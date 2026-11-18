//! `Model`: an object store in memory that conforms, and that a test can break in one way at a
//! time ([`Defect`]) to prove that the suite finds that defect.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::SystemTime;

use bytes::Bytes;
use futures::TryStreamExt as _;
use objstore_conformance::IdenticalPut;
use objstore_s3::{
    BodyStream, ByteRange, Capabilities, KeyPrefix, ListPage, ListToken, ObjectInfo, ObjectKey,
    ObjectStore, ObjectStoreError, PageSize, gather, too_short,
};

/// One way a store breaks the contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Defect {
    /// Claims no conditional writes.
    NoConditionalPut,
    /// Claims Object Lock.
    ObjectLock,
    /// Takes a retention date it cannot keep, and writes the object.
    KeepsRetention,
    /// Refuses a retention date as if the key existed.
    RetentionAlreadyExists,
    /// `put_new` replaces a used key.
    Overwrites,
    /// `put_stream` replaces a used key (`put_new` does not).
    StreamOverwrites,
    /// Every `put_new` fails with a `503`.
    PutFails,
    /// `put_new` checks the key, yields, and then writes: concurrent writers all succeed.
    RacyCheck,
    /// A streamed body that ends short is stored as far as it went.
    KeepsShortBody,
    /// A streamed body that runs long is cut and stored.
    CutsLongBody,
    /// A read that runs past the end loses its last byte.
    CutsOneShort,
    /// A read gives its bytes with the byte at offset 3 changed.
    FlipsByte,
    /// A read that starts at the end gives no bytes instead of `InvalidRange`.
    EmptyAtEnd,
    /// A read of an absent key is a `403`.
    AbsentForbidden,
    /// HEAD gives one byte more than the object holds.
    LongHead,
    /// HEAD fails with a `500`.
    HeadFails,
    /// `delete` of an absent key is `NotFound`.
    DeleteAbsentFails,
    /// `delete` leaves the key in listings.
    ListsDeleted,
    /// Listings leave out keys that are not ASCII.
    DropsNonAscii,
    /// A listing gives its keys in reverse order.
    Unordered,
    /// A page holds every key, whatever `max_keys` says.
    IgnoresMaxKeys,
    /// A page always says there is another, starting where it started.
    NeverEnds,
    /// A listing after a key a caller names gives that key too (pages, whose tokens the store
    /// issued, are right).
    IncludesAfter,
}

/// An object store in memory, with at most one defect.
pub struct Model {
    objects: Mutex<BTreeMap<ObjectKey, Bytes>>,
    deleted: Mutex<Vec<ObjectKey>>,
    identical: IdenticalPut,
    defect: Option<Defect>,
}

impl Model {
    /// A store that answers an identical write as `identical` says, with `defect`.
    pub fn new(identical: IdenticalPut, defect: Option<Defect>) -> Self {
        Self {
            objects: Mutex::new(BTreeMap::new()),
            deleted: Mutex::new(Vec::new()),
            identical,
            defect,
        }
    }

    /// The keys the store holds.
    pub fn keys(&self) -> Vec<String> {
        let objects = self.objects.lock().unwrap();
        objects.keys().map(|k| k.as_str().to_owned()).collect()
    }

    fn has(&self, defect: Defect) -> bool {
        self.defect == Some(defect)
    }

    /// The create-only write of `body` under `key`.
    fn create(&self, key: &ObjectKey, body: Bytes, replace: bool) -> Result<(), ObjectStoreError> {
        let mut objects = self.objects.lock().unwrap();
        match objects.get(key) {
            Some(held) if !replace && *held == body && self.identical == IdenticalPut::Accepted => {
                Ok(())
            }
            Some(_) if !replace => Err(ObjectStoreError::AlreadyExists(key.clone())),
            _ => {
                objects.insert(key.clone(), body);
                self.deleted.lock().unwrap().retain(|k| k != key);
                Ok(())
            }
        }
    }
}

impl ObjectStore for Model {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            conditional_put: !self.has(Defect::NoConditionalPut),
            object_lock: self.has(Defect::ObjectLock),
        }
    }

    async fn put_new(
        &self,
        key: &ObjectKey,
        body: Bytes,
        retain_until: Option<SystemTime>,
    ) -> Result<(), ObjectStoreError> {
        if self.has(Defect::PutFails) {
            return Err(ObjectStoreError::Service {
                status: 503,
                code: "SlowDown".to_owned(),
                message: "the model is down".to_owned(),
            });
        }
        if retain_until.is_some() && self.has(Defect::RetentionAlreadyExists) {
            return Err(ObjectStoreError::AlreadyExists(key.clone()));
        }
        if retain_until.is_some() && !self.has(Defect::KeepsRetention) {
            return Err(ObjectStoreError::Unsupported("Object Lock"));
        }
        if self.has(Defect::RacyCheck) {
            let held = self.objects.lock().unwrap().contains_key(key);
            tokio::task::yield_now().await;
            return self.create(key, body, !held);
        }
        self.create(key, body, self.has(Defect::Overwrites))
    }

    async fn put_stream(
        &self,
        key: &ObjectKey,
        len: u64,
        body: BodyStream,
    ) -> Result<(), ObjectStoreError> {
        let replace = self.has(Defect::StreamOverwrites);
        if self.has(Defect::KeepsShortBody) || self.has(Defect::CutsLongBody) {
            let pieces: Vec<Bytes> = body.try_collect().await?;
            let all = Bytes::from(pieces.concat());
            if (all.len() as u64) < len {
                if self.has(Defect::KeepsShortBody) {
                    self.create(key, all.clone(), replace)?;
                }
                return Err(too_short(len, all.len() as u64));
            }
            return self.create(key, all.slice(..len as usize), replace);
        }
        let bytes = gather(body, len).await?;
        self.create(key, bytes, replace)
    }

    async fn get_range(
        &self,
        key: &ObjectKey,
        range: ByteRange,
    ) -> Result<Bytes, ObjectStoreError> {
        let objects = self.objects.lock().unwrap();
        let Some(bytes) = objects.get(key) else {
            if self.has(Defect::AbsentForbidden) {
                return Err(ObjectStoreError::Service {
                    status: 403,
                    code: "AccessDenied".to_owned(),
                    message: String::new(),
                });
            }
            return Err(ObjectStoreError::NotFound(key.clone()));
        };
        let len = bytes.len() as u64;
        if range.offset() >= len && !(self.has(Defect::EmptyAtEnd) && range.offset() == len) {
            return Err(ObjectStoreError::InvalidRange {
                key: key.clone(),
                range,
            });
        }
        let mut end = (range.offset() + range.size()).min(len);
        if self.has(Defect::CutsOneShort) && range.offset() + range.size() > len {
            end -= 1;
        }
        let got = bytes.slice(range.offset() as usize..end as usize);
        if self.has(Defect::FlipsByte) && got.len() > 3 {
            let mut flipped = got.to_vec();
            flipped[3] ^= 0xff;
            return Ok(flipped.into());
        }
        Ok(got)
    }

    async fn len(&self, key: &ObjectKey) -> Result<Option<u64>, ObjectStoreError> {
        if self.has(Defect::HeadFails) {
            return Err(ObjectStoreError::Service {
                status: 500,
                code: "InternalError".to_owned(),
                message: String::new(),
            });
        }
        let objects = self.objects.lock().unwrap();
        let extra = u64::from(self.has(Defect::LongHead));
        Ok(objects.get(key).map(|b| b.len() as u64 + extra))
    }

    async fn delete(&self, key: &ObjectKey) -> Result<(), ObjectStoreError> {
        let removed = self.objects.lock().unwrap().remove(key);
        match removed {
            None if self.has(Defect::DeleteAbsentFails) => {
                Err(ObjectStoreError::NotFound(key.clone()))
            }
            Some(_) if self.has(Defect::ListsDeleted) => {
                self.deleted.lock().unwrap().push(key.clone());
                Ok(())
            }
            _ => Ok(()),
        }
    }

    async fn list(
        &self,
        prefix: &KeyPrefix,
        after: Option<&ListToken>,
        max_keys: PageSize,
    ) -> Result<ListPage, ObjectStoreError> {
        // A page's token is `kept`; a token a caller made is a plain `after`.
        let named = after.is_some_and(|t| t.listing().is_none());
        let after = after.map(ListToken::local_after).transpose()?;
        let objects = self.objects.lock().unwrap();
        let ghosts = self.deleted.lock().unwrap();
        let mut all: Vec<ObjectInfo> = objects
            .iter()
            .map(|(k, b)| (k.clone(), b.len() as u64))
            .chain(ghosts.iter().map(|k| (k.clone(), 0)))
            .filter(|(k, _)| prefix.matches(k))
            .filter(|(k, _)| !self.has(Defect::DropsNonAscii) || k.as_str().is_ascii())
            .filter(|(k, _)| match after {
                Some(a) if named && self.has(Defect::IncludesAfter) => k >= a,
                Some(a) => k > a,
                None => true,
            })
            .map(|(key, size)| ObjectInfo { key, size })
            .collect();
        all.sort_by(|a, b| a.key.cmp(&b.key));
        if self.has(Defect::Unordered) {
            all.reverse();
        }
        if self.has(Defect::NeverEnds) {
            let next = after.map_or_else(
                || ListToken::kept(0, all[0].key.clone()),
                |a| ListToken::kept(0, a.clone()),
            );
            all.truncate(usize::from(max_keys.get()));
            return Ok(ListPage {
                objects: all,
                next: Some(next),
            });
        }
        let max = if self.has(Defect::IgnoresMaxKeys) {
            usize::MAX
        } else {
            usize::from(max_keys.get())
        };
        let more = all.len() > max;
        all.truncate(max);
        let next = more.then(|| ListToken::kept(0, all[all.len() - 1].key.clone()));
        Ok(ListPage { objects: all, next })
    }
}
