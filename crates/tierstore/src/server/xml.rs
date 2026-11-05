//! The XML bodies of S3: the error body, the answers of ListObjectsV2, ListMultipartUploads and
//! the other calls of a multipart upload, and the one body that is read, `CompleteMultipartUpload`.

use roxmltree::{Document, Node};

use crate::{Listed, OpenUpload};

use super::time::iso8601;

/// The namespace of the documents S3 sends.
const NAMESPACE: &str = "http://s3.amazonaws.com/doc/2006-03-01/";

const PROLOG: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>";

/// `text` with the characters XML gives meaning to escaped.
pub(crate) fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
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

/// `text` as S3 writes it for `encoding-type=url`: every byte but the unreserved characters of
/// RFC 3986 (letters, digits, `-`, `.`, `_`, `~`) and `/` as `%` and two capital hexadecimal
/// digits, and a space as `+`.
pub(crate) fn url_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                out.push(char::from(byte));
            }
            b' ' => out.push('+'),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
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
    pub(crate) objects: &'a [Listed],
    /// The common prefixes of the page (none without a delimiter).
    pub(crate) prefixes: &'a [String],
    /// The continuation token of the next page, if the listing goes on.
    pub(crate) next: Option<String>,
}

impl Listing<'_> {
    pub(crate) fn to_xml(&self) -> String {
        // The text of a name field: percent-encoded if the request asked, escaped as XML.
        let text = |name: &str| {
            escape(&if self.url {
                url_encode(name)
            } else {
                name.to_owned()
            })
        };
        let bucket = escape(self.bucket);
        let (count, max_keys) = (self.objects.len() + self.prefixes.len(), self.max_keys);
        let truncated = self.next.is_some();
        let mut xml = format!("{PROLOG}<ListBucketResult xmlns=\"{NAMESPACE}\">");
        xml.push_str(&element("Name", &bucket));
        xml.push_str(&element("Prefix", &text(self.prefix)));
        if let Some(delimiter) = self.delimiter {
            xml.push_str(&element("Delimiter", &text(delimiter)));
        }
        if self.url {
            xml.push_str(&element("EncodingType", "url"));
        }
        xml.push_str(&format!(
            "<KeyCount>{count}</KeyCount><MaxKeys>{max_keys}</MaxKeys>"
        ));
        xml.push_str(&format!("<IsTruncated>{truncated}</IsTruncated>"));
        if let Some(sent) = self.continuation_token {
            xml.push_str(&element("ContinuationToken", &escape(sent)));
        }
        xml.push_str(&next_token(self.next.as_deref()));
        if let Some(sent) = self.start_after {
            xml.push_str(&element("StartAfter", &text(sent)));
        }
        for object in self.objects {
            let key = text(&object.key);
            let (size, modified) = (object.size, iso8601(object.modified));
            xml.push_str(&format!(
                "<Contents><Key>{key}</Key><LastModified>{modified}</LastModified>\
                 <Size>{size}</Size><StorageClass>STANDARD</StorageClass></Contents>"
            ));
        }
        for prefix in self.prefixes {
            let prefix = text(prefix);
            xml.push_str(&format!(
                "<CommonPrefixes><Prefix>{prefix}</Prefix></CommonPrefixes>"
            ));
        }
        xml.push_str("</ListBucketResult>");
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
    /// The `key-marker` and the `upload-id-marker` of the request, as they were sent.
    pub(crate) key_marker: &'a str,
    pub(crate) id_marker: &'a str,
    pub(crate) max_uploads: usize,
    pub(crate) uploads: &'a [OpenUpload],
    /// The last upload of the page, if the listing goes on after it.
    pub(crate) next: Option<&'a OpenUpload>,
}

impl UploadListing<'_> {
    pub(crate) fn to_xml(&self) -> String {
        let mut xml = format!("{PROLOG}<ListMultipartUploadsResult xmlns=\"{NAMESPACE}\">");
        xml.push_str(&element("Bucket", &escape(self.bucket)));
        xml.push_str(&element("KeyMarker", &escape(self.key_marker)));
        xml.push_str(&element("UploadIdMarker", &escape(self.id_marker)));
        if let Some(last) = self.next {
            xml.push_str(&element("NextKeyMarker", &escape(&last.key)));
            xml.push_str(&element("NextUploadIdMarker", &escape(&last.id)));
        }
        xml.push_str(&element("Prefix", &escape(self.prefix)));
        let max_uploads = self.max_uploads;
        xml.push_str(&format!("<MaxUploads>{max_uploads}</MaxUploads>"));
        let truncated = self.next.is_some();
        xml.push_str(&format!("<IsTruncated>{truncated}</IsTruncated>"));
        for upload in self.uploads {
            let (key, id) = (escape(&upload.key), escape(&upload.id));
            let initiated = iso8601(upload.initiated);
            xml.push_str(&format!(
                "<Upload><Key>{key}</Key><UploadId>{id}</UploadId>\
                 <StorageClass>STANDARD</StorageClass><Initiated>{initiated}</Initiated></Upload>"
            ));
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
