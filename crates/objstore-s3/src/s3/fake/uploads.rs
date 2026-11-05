//! ListMultipartUploads of the fake service.

use super::{Request, State, Upload, answer, error, iso_date, unescape};

/// ListParts: the parts of the upload `id` of `key` numbered above `part-number-marker`, in
/// ascending order of number, at most `max-parts` of them (1000 where the request has none), with
/// the marker of the next page if there is one. An upload that is not there, or is for another
/// key, is `NoSuchUpload`.
pub(super) fn list_parts(state: &State, request: &Request, key: &str, id: &str) -> Vec<u8> {
    let query = |name: &str| request.query.get(name).map(|value| unescape(value));
    let upload = state.uploads.get(id).filter(|up| up.key == key);
    let Some(upload) = upload else {
        return error("404 Not Found", "NoSuchUpload");
    };
    let max: usize = query("max-parts")
        .and_then(|v| v.parse().ok())
        .unwrap_or(1000);
    let marker: u32 = query("part-number-marker")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let matching: Vec<(&u32, &(String, super::Kept))> = upload
        .parts
        .iter()
        .filter(|(number, _)| **number > marker)
        .collect();
    let page = &matching[..matching.len().min(max)];
    let mut body = format!("<ListPartsResult><UploadId>{id}</UploadId>");
    for (number, (etag, kept)) in page {
        let stored = upload.stored_at.get(number).copied().unwrap_or_default();
        body.push_str(&format!(
            "<Part><PartNumber>{number}</PartNumber><LastModified>{}</LastModified>\
             <ETag>{etag}</ETag><Size>{}</Size></Part>",
            iso_date(stored),
            kept.len
        ));
    }
    let more = page.last().filter(|_| matching.len() > page.len());
    let tail = match more {
        Some((number, _)) => format!(
            "<IsTruncated>true</IsTruncated><NextPartNumberMarker>{number}</NextPartNumberMarker>"
        ),
        None => "<IsTruncated>false</IsTruncated>".to_owned(),
    };
    body.push_str(&tail);
    body.push_str("</ListPartsResult>");
    answer("200 OK", &[], body.as_bytes())
}

/// ListMultipartUploads: the uploads under `prefix`, by key and then by id, after the
/// markers, at most `max-uploads` of them, with the markers of the next page if there is one.
/// A key marker alone skips every upload of that key; with an upload id marker it skips the
/// uploads of the key up to that id.
pub(super) fn list_uploads(state: &State, request: &Request) -> Vec<u8> {
    let query = |name: &str| request.query.get(name).map(|value| unescape(value));
    let prefix = query("prefix").unwrap_or_default();
    let max: usize = query("max-uploads")
        .and_then(|v| v.parse().ok())
        .unwrap_or(1000);
    let (key_marker, id_marker) = (query("key-marker"), query("upload-id-marker"));
    let mut matching: Vec<(&String, &String, &Upload)> = state
        .uploads
        .iter()
        .map(|(id, upload)| (&upload.key, id, upload))
        .filter(|(key, _, _)| key.starts_with(&prefix))
        .filter(|(key, id, _)| match (&key_marker, &id_marker) {
            (None, _) => true,
            (Some(marker), None) => key.as_str() > marker.as_str(),
            (Some(marker), Some(after)) => {
                (key.as_str(), id.as_str()) > (marker.as_str(), after.as_str())
            }
        })
        .collect();
    matching.sort_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));
    let page = &matching[..matching.len().min(max)];
    let mut body = String::from("<ListMultipartUploadsResult>");
    for (key, id, upload) in page {
        body.push_str(&format!(
            "<Upload><Key>{key}</Key><UploadId>{id}</UploadId>\
             <Initiated>{}</Initiated></Upload>",
            iso_date(upload.initiated)
        ));
    }
    let more = page.last().filter(|_| matching.len() > page.len());
    let tail = match more {
        Some((key, id, _)) => format!(
            "<IsTruncated>true</IsTruncated><NextKeyMarker>{key}</NextKeyMarker>\
             <NextUploadIdMarker>{id}</NextUploadIdMarker>"
        ),
        None => "<IsTruncated>false</IsTruncated>".to_owned(),
    };
    body.push_str(&tail);
    body.push_str("</ListMultipartUploadsResult>");
    answer("200 OK", &[], body.as_bytes())
}
