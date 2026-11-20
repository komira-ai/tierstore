//! The farm's object store, SeaweedFS: the answer of the filer's `?metadata=true` for an object
//! ([`parse_entry`]), the volume servers' `/status` pages gathered into the map from a volume to
//! the nodes and drives that hold its copies ([`VolumeMap`]), and the names the read path gives
//! those drives ([`drive_id`], [`device`], [`device_label`]).

use std::collections::BTreeMap;

use serde::Deserialize;
use tierstore_fetch_api::upstream::DeviceRef;
use tierstore_format::ids::{DriveId, NodeId};

use crate::Error;

/// One piece of an object, stored as one needle of one volume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    /// The file id: `<volume id>,<needle hex>`.
    pub fid: String,
    /// The volume the needle is in (the id before the comma of `fid`).
    pub volume: u32,
    /// The offset of the piece in the object.
    pub offset: u64,
    /// The length of the piece.
    pub size: u64,
}

/// An object as the filer lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The object's length.
    pub size: u64,
    /// The object's entity tag without quotes, when the answer names one.
    pub etag: Option<String>,
    /// The pieces, ascending by offset, covering `0..size` exactly once.
    pub chunks: Vec<Chunk>,
}

#[derive(Deserialize)]
struct RawEntry {
    #[serde(rename = "FileSize")]
    file_size: u64,
    #[serde(rename = "ETag")]
    etag: Option<String>,
    chunks: Option<Vec<RawChunk>>,
}

#[derive(Deserialize)]
struct RawChunk {
    file_id: String,
    #[serde(default)]
    offset: u64,
    size: u64,
    e_tag: Option<String>,
}

/// The entry of the filer's `?metadata=true` answer for `key`.
///
/// The entity tag is the answer's `ETag` when it has one, else the `e_tag` of the object's only
/// chunk. A missing chunk list is an empty object.
///
/// # Errors
/// `json` is not an entry, a file id is not `<volume id>,<needle>`, or the chunks leave a gap,
/// overlap, or do not end at the object's size. The error names `key`.
pub fn parse_entry(key: &str, json: &[u8]) -> Result<Entry, Error> {
    let raw: RawEntry =
        serde_json::from_slice(json).map_err(|e| Error::parse(key, e.to_string()))?;
    let mut raw_chunks = raw.chunks.unwrap_or_default();
    raw_chunks.sort_by_key(|c| c.offset);
    let mut chunks = Vec::with_capacity(raw_chunks.len());
    let mut next = 0u64;
    for c in &raw_chunks {
        if c.offset != next {
            let why = if c.offset < next {
                "overlaps"
            } else {
                "leaves a gap before"
            };
            return Err(Error::parse(
                key,
                format!(
                    "chunk {} at offset {} {why} offset {next}",
                    c.file_id, c.offset
                ),
            ));
        }
        next = next
            .checked_add(c.size)
            .ok_or_else(|| Error::parse(key, format!("chunk {} runs past 2^64", c.file_id)))?;
        chunks.push(Chunk {
            volume: volume_of(key, &c.file_id)?,
            fid: c.file_id.clone(),
            offset: c.offset,
            size: c.size,
        });
    }
    if next != raw.file_size {
        return Err(Error::parse(
            key,
            format!("chunks cover {next} bytes of {}", raw.file_size),
        ));
    }
    let etag = raw.etag.or_else(|| match raw_chunks.as_slice() {
        [only] => only.e_tag.clone(),
        _ => None,
    });
    Ok(Entry {
        size: raw.file_size,
        etag: etag.filter(|t| !t.is_empty()),
        chunks,
    })
}

/// The volume id of a file id `<volume id>,<needle hex>`.
fn volume_of(key: &str, fid: &str) -> Result<u32, Error> {
    let bad = || Error::parse(key, format!("file id {fid:?} is not <volume id>,<needle>"));
    let (vid, needle) = fid.split_once(',').ok_or_else(bad)?;
    if needle.is_empty() {
        return Err(bad());
    }
    vid.parse().map_err(|_| bad())
}

/// A volume server of the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeServer {
    /// The node it runs on, as the harness numbers the nodes.
    pub node: u8,
    /// Its `host:port`.
    pub addr: String,
}

/// A command line argument `<node id>=<host:port>`, such as `2=10.77.0.2:8080`.
///
/// # Errors
/// The argument has no `=`, the node is not a number below 256, or the address is not a host and
/// a port.
pub fn parse_server(arg: &str) -> Result<VolumeServer, Error> {
    let bad = |why: &str| Error::Usage(format!("volume server {arg:?}: {why}"));
    let (node, addr) = arg
        .split_once('=')
        .ok_or_else(|| bad("expected <node id>=<host:port>"))?;
    let node = node
        .parse()
        .map_err(|_| bad("the node id is not a number from 0 to 255"))?;
    let port_ok = addr
        .rsplit_once(':')
        .is_some_and(|(host, port)| !host.is_empty() && port.parse::<u16>().is_ok());
    if !port_ok {
        return Err(bad("the address is not <host>:<port>"));
    }
    Ok(VolumeServer {
        node,
        addr: addr.to_string(),
    })
}

/// One copy of a volume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Replica {
    /// The node that holds the copy.
    pub node: u8,
    /// The `host:port` of the node's volume server.
    pub addr: String,
    /// The disk of the node the copy is on (the volume's `DiskId`).
    pub disk: u32,
}

#[derive(Deserialize)]
struct RawStatus {
    #[serde(rename = "Volumes")]
    volumes: Option<Vec<RawVolume>>,
}

#[derive(Deserialize)]
struct RawVolume {
    #[serde(rename = "Id")]
    id: u32,
    #[serde(rename = "DiskId", default)]
    disk: u32,
}

/// Which nodes and disks hold the copies of each volume.
#[derive(Debug, Clone, Default)]
pub struct VolumeMap {
    volumes: BTreeMap<u32, Vec<Replica>>,
}

impl VolumeMap {
    /// Adds the volumes `server`'s `/status` page `json` lists. A server added twice replaces its
    /// earlier copies.
    ///
    /// # Errors
    /// `json` is not a status page; the error names the server.
    pub fn add_status(&mut self, server: &VolumeServer, json: &[u8]) -> Result<(), Error> {
        let status: RawStatus = serde_json::from_slice(json)
            .map_err(|e| Error::parse(format!("{} /status", server.addr), e.to_string()))?;
        for v in status.volumes.unwrap_or_default() {
            let copies = self.volumes.entry(v.id).or_default();
            let replica = Replica {
                node: server.node,
                addr: server.addr.clone(),
                disk: v.disk,
            };
            match copies.binary_search_by_key(&server.node, |r| r.node) {
                Ok(at) => copies[at] = replica,
                Err(at) => copies.insert(at, replica),
            }
        }
        Ok(())
    }

    /// The copies of `volume`, ordered by node; none for a volume no server listed.
    #[must_use]
    pub fn replicas(&self, volume: u32) -> &[Replica] {
        self.volumes.get(&volume).map_or(&[], Vec::as_slice)
    }

    /// How many volumes the map knows.
    #[must_use]
    pub fn volumes(&self) -> usize {
        self.volumes.len()
    }

    /// The audit of copies: for each number of copies, how many volumes have that many.
    #[must_use]
    pub fn histogram(&self) -> BTreeMap<usize, u64> {
        let mut h = BTreeMap::new();
        for copies in self.volumes.values() {
            *h.entry(copies.len()).or_insert(0) += 1;
        }
        h
    }
}

/// The id the harness gives disk `disk` of node `node`: the node, then the disk in four bytes
/// big-endian, then zeros.
#[must_use]
pub fn drive_id(node: u8, disk: u32) -> DriveId {
    let mut b = [0u8; 16];
    b[0] = node;
    b[1..5].copy_from_slice(&disk.to_be_bytes());
    DriveId(b)
}

/// The device of the read path a copy is on.
#[must_use]
pub fn device(r: &Replica) -> DeviceRef {
    DeviceRef::Drive(NodeId(r.node), drive_id(r.node, r.disk))
}

/// A short name of a device: `n2d1` for disk 1 of node 2 (as [`drive_id`] made it), `n2hot` for
/// node 2's hot device.
#[must_use]
pub fn device_label(d: DeviceRef) -> String {
    match d {
        DeviceRef::Hot(n) => format!("n{}hot", n.0),
        DeviceRef::Drive(n, id) => {
            let disk = u32::from_be_bytes([id.0[1], id.0[2], id.0[3], id.0[4]]);
            format!("n{}d{disk}", n.0)
        }
    }
}

#[cfg(test)]
#[path = "swfs_tests.rs"]
mod tests;
