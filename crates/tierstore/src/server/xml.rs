//! The XML bodies of S3: the error body, the answers of ListObjectsV2, ListMultipartUploads and
//! the other calls of a multipart upload, and the one body that is read, `CompleteMultipartUpload`.

use roxmltree::{Document, Node};

use crate::{Listed, OpenUpload};

use super::time::iso8601;

/// The namespace of the documents S3 sends.
const NAMESPACE: &str = "http://s3.amazonaws.com/doc/2006-03-01/";

const PROLOG: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>";

/// `text` with the characters XML gives meaning to escaped. A carriage return is written as a
/// character reference, since a parser reads a bare one as a line feed (a tab and a line feed
/// are read as they are written).
pub(crate) fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('\r', "&#13;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn element(name: &str, text: &str) -> String {
    format!("<{name}>{text}</{name}>")
}

/// The body of an error answer. `resource` is the path of the request.
pub(crate) fn error_body(code: &str, message: &str, resource: &str, request_id: &str) -> String {
    let (message, resource) = (escape(message), escape(resource));
    format!(
        "{PROLOG}<Error><Code>{code}</Code><Message>{message}</Message>\
         <Resource>{resource}</Resource><RequestId>{request_id}</RequestId></Error>"
    )
}

/// `text` as S3 writes it for `encoding-type=url`: every byte but letters, digits, `-`, `.`, `_`,
/// `*` and `/` as `%` and two capital hexadecimal digits, and a space as `+`. (The S3 reference
/// does not list the characters it leaves; the set is what a listing of MinIO gave, which is
/// like `URLEncoder` of Java but for the slash, and so `~` is `%7E` and `*` stays.)
pub(crate) fn url_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'*' | b'/' => {
                out.push(char::from(byte));
            }
            b' ' => out.push('+'),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// The answer of GetBucketLocation: the `LocationConstraint` of the bucket, which S3 leaves
/// empty for `us-east-1`.
pub(crate) fn location(region: &str) -> String {
    let named = if region == "us-east-1" { "" } else { region };
    let text = escape(named);
    format!("{PROLOG}<LocationConstraint xmlns=\"{NAMESPACE}\">{text}</LocationConstraint>")
}

/// The answer to a read of a setting that S3 answers with an empty document when it is not set.
pub(crate) fn empty_setting(root: &str) -> String {
    format!("{PROLOG}<{root} xmlns=\"{NAMESPACE}\"/>")
}

/// The answer of GetBucketRequestPayment: the bucket owner pays.
pub(crate) fn payer() -> String {
    format!(
        "{PROLOG}<RequestPaymentConfiguration xmlns=\"{NAMESPACE}\">\
         <Payer>BucketOwner</Payer></RequestPaymentConfiguration>"
    )
}

/// The answer of GetBucketAcl: the owner, with full control, and no other grant.
pub(crate) fn acl() -> String {
    let who = "<ID>tierstore</ID><DisplayName>tierstore</DisplayName>";
    let schema = "http://www.w3.org/2001/XMLSchema-instance";
    format!(
        "{PROLOG}<AccessControlPolicy xmlns=\"{NAMESPACE}\"><Owner>{who}</Owner>\
         <AccessControlList><Grant><Grantee xmlns:xsi=\"{schema}\" xsi:type=\"CanonicalUser\">\
         {who}</Grantee><Permission>FULL_CONTROL</Permission></Grant></AccessControlList>\
         </AccessControlPolicy>"
    )
}

/// The answer of ListBuckets: the owner, and the bucket (with its region and the time it was
/// made, `created_ms` since the Unix epoch) if `listed`, which is false when a filter of the call
/// left it out.
pub(crate) fn buckets(bucket: &str, region: &str, created_ms: u64, listed: bool) -> String {
    let found = if listed {
        let (name, region) = (escape(bucket), escape(region));
        let created = iso8601(created_ms);
        format!(
            "<Bucket><Name>{name}</Name><CreationDate>{created}</CreationDate>\
             <BucketRegion>{region}</BucketRegion></Bucket>"
        )
    } else {
        String::new()
    };
    format!(
        "{PROLOG}<ListAllMyBucketsResult xmlns=\"{NAMESPACE}\">{OWNER}\
         <Buckets>{found}</Buckets></ListAllMyBucketsResult>"
    )
}

/// The `Owner` S3 adds to the objects of a listing that asked for `fetch-owner=true`. The proxy
/// has no accounts: it serves one bucket to every caller it lets in, so every object has the
/// same owner, named for the proxy.
pub(crate) const OWNER: &str =
    "<Owner><ID>tierstore</ID><DisplayName>tierstore</DisplayName></Owner>";

/// The text of a name field: percent-encoded if the request asked, escaped as XML.
fn name_text(name: &str, url: bool) -> String {
    escape(&if url {
        url_encode(name)
    } else {
        name.to_owned()
    })
}

/// One page of a ListObjectsV2 answer.
pub(crate) struct Listing<'a> {
    pub(crate) bucket: &'a str,
    pub(crate) prefix: &'a str,
    /// The `delimiter` of the request, if it had one that is not empty.
    pub(crate) delimiter: Option<&'a str>,
    /// Whether the request asked for `encoding-type=url`.
    pub(crate) url: bool,
    /// The `start-after` and the `continuation-token` of the request, as they were sent.
    pub(crate) start_after: Option<&'a str>,
    pub(crate) continuation_token: Option<&'a str>,
    pub(crate) max_keys: usize,
    /// Whether the request asked for `fetch-owner=true`: each object then has an `Owner`.
    pub(crate) owner: bool,
    pub(crate) objects: &'a [Listed],
    /// The common prefixes of the page (none without a delimiter).
    pub(crate) prefixes: &'a [String],
    /// The continuation token of the next page, if the listing goes on.
    pub(crate) next: Option<String>,
}

impl Listing<'_> {
    pub(crate) fn to_xml(&self) -> String {
        let text = |name: &str| name_text(name, self.url);
        let bucket = escape(self.bucket);
        let (count, max_keys) = (self.objects.len() + self.prefixes.len(), self.max_keys);
        let truncated = self.next.is_some();
        // The order of the elements is the one of the S3 reference's examples.
        let mut xml = format!("{PROLOG}<ListBucketResult xmlns=\"{NAMESPACE}\">");
        xml.push_str(&element("Name", &bucket));
        xml.push_str(&element("Prefix", &text(self.prefix)));
        if let Some(sent) = self.start_after {
            xml.push_str(&element("StartAfter", &text(sent)));
        }
        if let Some(sent) = self.continuation_token {
            xml.push_str(&element("ContinuationToken", &escape(sent)));
        }
        xml.push_str(&next_token(self.next.as_deref()));
        xml.push_str(&format!(
            "<KeyCount>{count}</KeyCount><MaxKeys>{max_keys}</MaxKeys>"
        ));
        if let Some(delimiter) = self.delimiter {
            xml.push_str(&element("Delimiter", &text(delimiter)));
        }
        xml.push_str(&format!("<IsTruncated>{truncated}</IsTruncated>"));
        let owner = if self.owner { OWNER } else { "" };
        xml.push_str(&entries(self.objects, "Contents", "", owner, self.url));
        xml.push_str(&common_prefixes(self.prefixes, self.url));
        if self.url {
            xml.push_str(&element("EncodingType", "url"));
        }
        xml.push_str("</ListBucketResult>");
        xml
    }
}

/// The entries of the objects of a page, each an element `tag` that has the `Key`, then `after_key`
/// (the text of elements), the `LastModified`, `ETag` and `Size` of the object, `owner` (the
/// text of an `Owner` element, or nothing) and the `StorageClass`.
fn entries(objects: &[Listed], tag: &str, after_key: &str, owner: &str, url: bool) -> String {
    let mut xml = String::new();
    for object in objects {
        let key = name_text(&object.key, url);
        let (size, modified) = (object.size, iso8601(object.modified));
        // The tag in quotes, as the `ETag` header of a HEAD of the object has it.
        let etag = escape(&format!("\"{}\"", object.tag));
        xml.push_str(&format!(
            "<{tag}><Key>{key}</Key>{after_key}<LastModified>{modified}</LastModified>\
             <ETag>{etag}</ETag><Size>{size}</Size>{owner}\
             <StorageClass>STANDARD</StorageClass></{tag}>"
        ));
    }
    xml
}

/// The `CommonPrefixes` of a page.
fn common_prefixes(prefixes: &[String], url: bool) -> String {
    let mut xml = String::new();
    for prefix in prefixes {
        let prefix = name_text(prefix, url);
        xml.push_str(&format!(
            "<CommonPrefixes><Prefix>{prefix}</Prefix></CommonPrefixes>"
        ));
    }
    xml
}

/// The id of the one version every object of this server has, as S3 names the version of an
/// object in a bucket that is not versioned.
pub(crate) const NULL_VERSION: &str = "null";

/// One page of a ListObjectVersions answer, for a bucket that is not versioned: each object is
/// its one version, `null`, and the latest.
pub(crate) struct VersionListing<'a> {
    pub(crate) bucket: &'a str,
    pub(crate) prefix: &'a str,
    /// The `delimiter` of the request, if it had one that is not empty.
    pub(crate) delimiter: Option<&'a str>,
    /// Whether the request asked for `encoding-type=url`.
    pub(crate) url: bool,
    /// The `key-marker` and the `version-id-marker` of the request, as they were sent.
    pub(crate) key_marker: &'a str,
    pub(crate) id_marker: &'a str,
    pub(crate) max_keys: usize,
    pub(crate) objects: &'a [Listed],
    /// The common prefixes of the page (none without a delimiter).
    pub(crate) prefixes: &'a [String],
    /// The key and the version id (empty after a common prefix) that start the next page, if the
    /// listing goes on.
    pub(crate) next: Option<(&'a str, &'a str)>,
}

impl VersionListing<'_> {
    pub(crate) fn to_xml(&self) -> String {
        let text = |name: &str| name_text(name, self.url);
        let mut xml = format!("{PROLOG}<ListVersionsResult xmlns=\"{NAMESPACE}\">");
        xml.push_str(&element("Name", &escape(self.bucket)));
        xml.push_str(&element("Prefix", &text(self.prefix)));
        xml.push_str(&element("KeyMarker", &text(self.key_marker)));
        xml.push_str(&element("VersionIdMarker", &escape(self.id_marker)));
        if let Some((key, id)) = self.next {
            xml.push_str(&element("NextKeyMarker", &text(key)));
            xml.push_str(&element("NextVersionIdMarker", &escape(id)));
        }
        let max_keys = self.max_keys;
        xml.push_str(&format!("<MaxKeys>{max_keys}</MaxKeys>"));
        if let Some(delimiter) = self.delimiter {
            xml.push_str(&element("Delimiter", &text(delimiter)));
        }
        let truncated = self.next.is_some();
        xml.push_str(&format!("<IsTruncated>{truncated}</IsTruncated>"));
        let latest = format!("<VersionId>{NULL_VERSION}</VersionId><IsLatest>true</IsLatest>");
        xml.push_str(&entries(self.objects, "Version", &latest, OWNER, self.url));
        xml.push_str(&common_prefixes(self.prefixes, self.url));
        if self.url {
            xml.push_str(&element("EncodingType", "url"));
        }
        xml.push_str("</ListVersionsResult>");
        xml
    }
}

/// The element that carries the token of the next page, or nothing for the last page.
fn next_token(next: Option<&str>) -> String {
    next.map(|token| element("NextContinuationToken", &escape(token)))
        .unwrap_or_default()
}

/// One page of a ListMultipartUploads answer.
pub(crate) struct UploadListing<'a> {
    pub(crate) bucket: &'a str,
    pub(crate) prefix: &'a str,
    /// The `delimiter` of the request, if it had one that is not empty.
    pub(crate) delimiter: Option<&'a str>,
    /// Whether the request asked for `encoding-type=url`.
    pub(crate) url: bool,
    /// The `key-marker` and the `upload-id-marker` of the request, as they were sent.
    pub(crate) key_marker: &'a str,
    pub(crate) id_marker: &'a str,
    pub(crate) max_uploads: usize,
    pub(crate) uploads: &'a [OpenUpload],
    /// The common prefixes of the page (none without a delimiter).
    pub(crate) prefixes: &'a [String],
    /// The key and the upload id (empty after a common prefix) that start the next page, if the
    /// listing goes on.
    pub(crate) next: Option<(&'a str, &'a str)>,
}

impl UploadListing<'_> {
    pub(crate) fn to_xml(&self) -> String {
        let text = |name: &str| name_text(name, self.url);
        let mut xml = format!("{PROLOG}<ListMultipartUploadsResult xmlns=\"{NAMESPACE}\">");
        xml.push_str(&element("Bucket", &escape(self.bucket)));
        xml.push_str(&element("KeyMarker", &text(self.key_marker)));
        xml.push_str(&element("UploadIdMarker", &escape(self.id_marker)));
        if let Some((key, id)) = self.next {
            xml.push_str(&element("NextKeyMarker", &text(key)));
            xml.push_str(&element("NextUploadIdMarker", &escape(id)));
        }
        xml.push_str(&element("Prefix", &text(self.prefix)));
        if let Some(delimiter) = self.delimiter {
            xml.push_str(&element("Delimiter", &text(delimiter)));
        }
        let max_uploads = self.max_uploads;
        xml.push_str(&format!("<MaxUploads>{max_uploads}</MaxUploads>"));
        let truncated = self.next.is_some();
        xml.push_str(&format!("<IsTruncated>{truncated}</IsTruncated>"));
        for upload in self.uploads {
            let (key, id) = (text(&upload.key), escape(&upload.id));
            let initiated = iso8601(upload.initiated);
            xml.push_str(&format!(
                "<Upload><Key>{key}</Key><UploadId>{id}</UploadId>\
                 <StorageClass>STANDARD</StorageClass><Initiated>{initiated}</Initiated></Upload>"
            ));
        }
        for prefix in self.prefixes {
            let prefix = text(prefix);
            xml.push_str(&format!(
                "<CommonPrefixes><Prefix>{prefix}</Prefix></CommonPrefixes>"
            ));
        }
        if self.url {
            xml.push_str(&element("EncodingType", "url"));
        }
        xml.push_str("</ListMultipartUploadsResult>");
        xml
    }
}

/// The answer of CreateMultipartUpload.
pub(crate) fn initiate(bucket: &str, key: &str, upload_id: &str) -> String {
    let (bucket, key) = (escape(bucket), escape(key));
    format!(
        "{PROLOG}<InitiateMultipartUploadResult xmlns=\"{NAMESPACE}\"><Bucket>{bucket}</Bucket>\
         <Key>{key}</Key><UploadId>{upload_id}</UploadId></InitiateMultipartUploadResult>"
    )
}

/// The answer of CompleteMultipartUpload.
pub(crate) fn completed(bucket: &str, key: &str, etag: &str) -> String {
    let (bucket, key, etag) = (escape(bucket), escape(key), escape(etag));
    format!(
        "{PROLOG}<CompleteMultipartUploadResult xmlns=\"{NAMESPACE}\">\
         <Location>/{bucket}/{key}</Location><Bucket>{bucket}</Bucket><Key>{key}</Key>\
         <ETag>{etag}</ETag></CompleteMultipartUploadResult>"
    )
}

/// The text of the first child of `node` named `name`, in any namespace.
fn child_text<'a>(node: Node<'a, '_>, name: &str) -> Option<&'a str> {
    node.children()
        .find(|child| child.tag_name().name() == name)
        .and_then(|child| child.text())
}

/// The parts a `CompleteMultipartUpload` body names, in its order: the number of each and its
/// ETag without the quotes. `None` if the body is not that document.
pub(crate) fn parse_complete(body: &[u8]) -> Option<Vec<(u32, String)>> {
    let text = std::str::from_utf8(body).ok()?;
    let doc = Document::parse(text).ok()?;
    let root = doc.root_element();
    (root.tag_name().name() == "CompleteMultipartUpload").then_some(())?;
    root.children()
        .filter(|node| node.tag_name().name() == "Part")
        .map(|part| {
            let number = child_text(part, "PartNumber")?.trim().parse::<u32>().ok()?;
            let tag = child_text(part, "ETag")?.trim().trim_matches('"');
            Some((number, tag.to_owned()))
        })
        .collect()
}

/// One `Object` of a `Delete` document.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct DeleteObject {
    pub(crate) key: String,
    /// Whether the object names a version.
    pub(crate) versioned: bool,
}

/// What a `Delete` document asks for.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct DeleteRequest {
    /// Whether the answer leaves out the keys that were deleted.
    pub(crate) quiet: bool,
    pub(crate) objects: Vec<DeleteObject>,
}

/// The objects and the `Quiet` flag a `Delete` body names, in its order. `None` if the body is not
/// that document: not XML, another root, an object with no key or an empty one, or a `Quiet` that
/// is not `true` or `false`.
pub(crate) fn parse_delete(body: &[u8]) -> Option<DeleteRequest> {
    let text = std::str::from_utf8(body).ok()?;
    let doc = Document::parse(text).ok()?;
    let root = doc.root_element();
    (root.tag_name().name() == "Delete").then_some(())?;
    let quiet = match child_text(root, "Quiet").map(str::trim) {
        None => false,
        Some("true") => true,
        Some("false") => false,
        Some(_) => return None,
    };
    let objects = root
        .children()
        .filter(|node| node.tag_name().name() == "Object")
        .map(|node| {
            let key = child_text(node, "Key").filter(|key| !key.is_empty())?;
            let versioned = child_text(node, "VersionId").is_some();
            Some(DeleteObject {
                key: key.to_owned(),
                versioned,
            })
        })
        .collect::<Option<Vec<_>>>()?;
    Some(DeleteRequest { quiet, objects })
}

/// What became of one key of a `Delete` call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    Deleted,
    Failed { code: &'static str, message: String },
}

/// The answer of DeleteObjects: a `Deleted` for each key that was deleted (none if `quiet`) and
/// an `Error` for each that was not, in the order of the call.
pub(crate) fn delete_result(results: &[(&str, Outcome)], quiet: bool) -> String {
    let mut xml = format!("{PROLOG}<DeleteResult xmlns=\"{NAMESPACE}\">");
    for (key, outcome) in results {
        let key = escape(key);
        match outcome {
            Outcome::Deleted if quiet => {}
            Outcome::Deleted => xml.push_str(&format!("<Deleted><Key>{key}</Key></Deleted>")),
            Outcome::Failed { code, message } => {
                let message = escape(message);
                xml.push_str(&format!(
                    "<Error><Key>{key}</Key><Code>{code}</Code><Message>{message}</Message></Error>"
                ));
            }
        }
    }
    xml.push_str("</DeleteResult>");
    xml
}
