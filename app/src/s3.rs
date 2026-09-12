//! A small S3 client: enough to put, list, get and delete backup objects at
//! any S3-compatible endpoint, signed with AWS Signature Version 4.
//!
//! The AWS SDK is not worth its weight here. It brings forty-odd crates and
//! an async runtime to do what four requests need, and it speaks only to
//! AWS. Signing by hand is about two hundred lines and reaches every store
//! a listener is likely to own:
//!
//! | store              | endpoint                                     | path style |
//! |--------------------|----------------------------------------------|------------|
//! | AWS S3             | `https://s3.<region>.amazonaws.com`          | no         |
//! | Supabase Storage   | `https://<ref>.supabase.co/storage/v1/s3`    | yes        |
//! | Backblaze B2       | `https://s3.<region>.backblazeb2.com`        | no         |
//! | Cloudflare R2      | `https://<account>.r2.cloudflarestorage.com` | yes        |
//! | Wasabi             | `https://s3.<region>.wasabisys.com`          | no         |
//! | MinIO (self-hosted)| `http://host:9000`                           | yes        |
//!
//! The signing functions are pure and tested against the published AWS
//! `get-vanilla` test vector, so a wrong signature fails here rather than as
//! an opaque 403 from a bucket.

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use std::time::Duration;

type HmacSha256 = Hmac<Sha256>;

/// Objects larger than this go up in parts. Also the part size.
pub const PART: u64 = 64 * 1024 * 1024;

#[derive(Clone, Debug, Default)]
pub struct Bucket {
    /// `https://s3.us-east-2.amazonaws.com`, or a provider URL that carries
    /// a path of its own (`https://<ref>.supabase.co/storage/v1/s3`).
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    /// `bucket/key` in the path instead of `bucket.` in the host. Required
    /// by Supabase, R2 and MinIO; optional on AWS.
    pub path_style: bool,
    pub access: String,
    pub secret: String,
}

/// One object in a bucket listing.
#[derive(Clone, Debug, PartialEq)]
pub struct Object {
    pub key: String,
    pub bytes: u64,
    pub modified: String,
}

// ---------------------------------------------------------------- signing

fn mac(key: &[u8], msg: &[u8]) -> Vec<u8> {
    let mut m = <HmacSha256 as Mac>::new_from_slice(key).expect("hmac takes any key length");
    m.update(msg);
    m.finalize().into_bytes().to_vec()
}

pub fn sha256_hex(b: &[u8]) -> String {
    crate::library::hex(&Sha256::digest(b))
}

/// The four-step chain that turns a secret into a key good for one day, one
/// region and one service.
pub fn signing_key(secret: &str, date: &str, region: &str, service: &str) -> Vec<u8> {
    let k = mac(format!("AWS4{secret}").as_bytes(), date.as_bytes());
    let k = mac(&k, region.as_bytes());
    let k = mac(&k, service.as_bytes());
    mac(&k, b"aws4_request")
}

/// RFC 3986 encoding, as SigV4 wants it: unreserved characters pass, every
/// other byte becomes uppercase percent-hex. `/` survives in a path but not
/// in a query value.
pub fn uri_encode(s: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            b'/' if keep_slash => out.push('/'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn canonical_query(q: &[(String, String)]) -> String {
    let mut pairs: Vec<(String, String)> = q
        .iter()
        .map(|(k, v)| (uri_encode(k, false), uri_encode(v, false)))
        .collect();
    pairs.sort();
    pairs
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

/// The canonical request and its signed-header list.
pub fn canonical_request(
    method: &str,
    uri: &str,
    query: &[(String, String)],
    headers: &[(String, String)],
    payload_hash: &str,
) -> (String, String) {
    let mut h: Vec<(String, String)> = headers
        .iter()
        .map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    h.sort();
    let signed = h
        .iter()
        .map(|(k, _)| k.clone())
        .collect::<Vec<_>>()
        .join(";");
    let canon_headers: String = h.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let req = format!(
        "{method}\n{}\n{}\n{canon_headers}\n{signed}\n{payload_hash}",
        uri_encode(uri, true),
        canonical_query(query),
    );
    (req, signed)
}

/// The `Authorization` header value for a request. `amz_date` is the basic
/// ISO 8601 stamp (`20150830T123600Z`); its first eight characters are the
/// date the key is scoped to.
#[allow(clippy::too_many_arguments)]
pub fn authorization(
    method: &str,
    uri: &str,
    query: &[(String, String)],
    headers: &[(String, String)],
    payload_hash: &str,
    access: &str,
    secret: &str,
    region: &str,
    service: &str,
    amz_date: &str,
) -> String {
    let date = &amz_date[..8];
    let (canon, signed) = canonical_request(method, uri, query, headers, payload_hash);
    let scope = format!("{date}/{region}/{service}/aws4_request");
    let to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        sha256_hex(canon.as_bytes())
    );
    let sig = crate::library::hex(&mac(
        &signing_key(secret, date, region, service),
        to_sign.as_bytes(),
    ));
    format!(
        "AWS4-HMAC-SHA256 Credential={access}/{scope}, SignedHeaders={signed}, Signature={sig}"
    )
}

// ---------------------------------------------------------------- requests

/// Where a key lives: the host to talk to and the path to sign.
struct Target {
    url: String,
    host: String,
    uri: String,
}

fn target(b: &Bucket, key: &str) -> Result<Target, String> {
    let ep = b.endpoint.trim().trim_end_matches('/');
    let (scheme, rest) = ep
        .split_once("://")
        .ok_or("the endpoint needs to start with https:// or http://")?;
    let (host, base) = match rest.split_once('/') {
        Some((h, p)) => (h.to_string(), format!("/{p}")),
        None => (rest.to_string(), String::new()),
    };
    if b.bucket.trim().is_empty() {
        return Err("no bucket name".into());
    }
    let (host, uri) = if b.path_style {
        (host, format!("{base}/{}/{key}", b.bucket))
    } else {
        (format!("{}.{host}", b.bucket), format!("{base}/{key}"))
    };
    Ok(Target {
        url: format!("{scheme}://{host}{}", uri_encode(&uri, true)),
        host,
        uri,
    })
}

fn agent(timeout_secs: u64) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(timeout_secs)))
        .http_status_as_error(false)
        .build()
        .into()
}

fn stamp() -> String {
    chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string()
}

/// A signed request with a body held in memory. Returns status, response
/// headers and body text.
#[allow(clippy::type_complexity)]
fn send(
    b: &Bucket,
    method: &str,
    key: &str,
    query: &[(String, String)],
    body: &[u8],
    timeout_secs: u64,
) -> Result<(u16, Vec<(String, String)>, String), String> {
    let t = target(b, key)?;
    let hash = sha256_hex(body);
    let date = stamp();
    let mut headers = vec![
        ("host".to_string(), t.host.clone()),
        ("x-amz-content-sha256".to_string(), hash.clone()),
        ("x-amz-date".to_string(), date.clone()),
    ];
    if !body.is_empty() {
        headers.push(("content-length".into(), body.len().to_string()));
    }
    let auth = authorization(
        method,
        &t.uri,
        query,
        &headers,
        &hash,
        &b.access,
        &b.secret,
        &b.region,
        "s3",
        &date,
    );
    let q = canonical_query(query);
    let url = if q.is_empty() {
        t.url
    } else {
        format!("{}?{q}", t.url)
    };
    if !matches!(method, "GET" | "PUT" | "POST" | "DELETE") {
        return Err(format!("{method} is not a method this client sends"));
    }
    // Built as an `http::Request` rather than through ureq's per-method
    // builders, whose with-body and without-body types cannot share one
    // `match` arm.
    let mut rb = ureq::http::Request::builder()
        .method(method)
        .uri(&url)
        .header("x-amz-content-sha256", &hash)
        .header("x-amz-date", &date)
        .header("Authorization", &auth)
        .header("User-Agent", "HoosierSDR");
    if !body.is_empty() {
        rb = rb.header("Content-Length", body.len().to_string());
    }
    let req = rb.body(body).map_err(|e| e.to_string())?;
    let mut r = agent(timeout_secs).run(req).map_err(|e| e.to_string())?;
    let status = r.status().as_u16();
    let hdrs: Vec<(String, String)> = r
        .headers()
        .iter()
        .map(|(k, v)| {
            (
                k.as_str().to_ascii_lowercase(),
                v.to_str().unwrap_or("").to_string(),
            )
        })
        .collect();
    let text = r.body_mut().read_to_string().unwrap_or_default();
    Ok((status, hdrs, text))
}

/// The message inside an S3 error body, which is XML and says more than the
/// status code does.
fn why(status: u16, body: &str) -> String {
    let pick = |tag: &str| {
        body.split_once(&format!("<{tag}>"))
            .and_then(|(_, r)| r.split_once(&format!("</{tag}>")))
            .map(|(v, _)| v.trim().to_string())
    };
    match (pick("Code"), pick("Message")) {
        (Some(c), Some(m)) => format!("{status} {c}: {m}"),
        (Some(c), None) => format!("{status} {c}"),
        _ if body.trim().is_empty() => format!("HTTP {status}"),
        _ => format!("HTTP {status}: {}", body.chars().take(200).collect::<String>()),
    }
}

fn ok(status: u16) -> bool {
    (200..300).contains(&status)
}

/// Stream the object to a file, so a restore does not need the archive in
/// memory. Returns the bytes written.
pub fn get_to_file(b: &Bucket, key: &str, to: &std::path::Path) -> Result<u64, String> {
    let t = target(b, key)?;
    let hash = sha256_hex(b"");
    let date = stamp();
    let headers = vec![
        ("host".to_string(), t.host.clone()),
        ("x-amz-content-sha256".to_string(), hash.clone()),
        ("x-amz-date".to_string(), date.clone()),
    ];
    let auth = authorization(
        "GET", &t.uri, &[], &headers, &hash, &b.access, &b.secret, &b.region, "s3", &date,
    );
    let mut r = agent(6 * 3600)
        .get(&t.url)
        .header("x-amz-content-sha256", &hash)
        .header("x-amz-date", &date)
        .header("Authorization", &auth)
        .header("User-Agent", "HoosierSDR")
        .call()
        .map_err(|e| e.to_string())?;
    let status = r.status().as_u16();
    if !ok(status) {
        return Err(why(status, &r.body_mut().read_to_string().unwrap_or_default()));
    }
    let mut f = std::fs::File::create(to).map_err(|e| e.to_string())?;
    std::io::copy(&mut r.body_mut().as_reader(), &mut f).map_err(|e| e.to_string())
}

pub fn delete(b: &Bucket, key: &str) -> Result<(), String> {
    let (s, _, body) = send(b, "DELETE", key, &[], &[], 60)?;
    if ok(s) {
        Ok(())
    } else {
        Err(why(s, &body))
    }
}

/// Objects under a prefix. Follows the continuation token, so a bucket with
/// more than a thousand keys still lists fully.
pub fn list(b: &Bucket, prefix: &str) -> Result<Vec<Object>, String> {
    let mut out = Vec::new();
    let mut token: Option<String> = None;
    loop {
        let mut q = vec![
            ("list-type".to_string(), "2".to_string()),
            ("prefix".to_string(), prefix.to_string()),
        ];
        if let Some(t) = &token {
            q.push(("continuation-token".into(), t.clone()));
        }
        let (s, _, body) = send(b, "GET", "", &q, &[], 120)?;
        if !ok(s) {
            return Err(why(s, &body));
        }
        out.extend(parse_listing(&body));
        match tag(&body, "NextContinuationToken") {
            Some(t) if tag(&body, "IsTruncated").as_deref() == Some("true") => token = Some(t),
            _ => break,
        }
    }
    Ok(out)
}

fn tag(xml: &str, name: &str) -> Option<String> {
    xml.split_once(&format!("<{name}>"))
        .and_then(|(_, r)| r.split_once(&format!("</{name}>")))
        .map(|(v, _)| v.trim().to_string())
}

/// The `<Contents>` blocks of a ListObjectsV2 response. Written by hand
/// rather than with an XML crate: the shape is fixed and shallow.
pub fn parse_listing(xml: &str) -> Vec<Object> {
    xml.split("<Contents>")
        .skip(1)
        .filter_map(|block| {
            let block = block.split("</Contents>").next()?;
            Some(Object {
                key: tag(block, "Key")?,
                bytes: tag(block, "Size").and_then(|s| s.parse().ok()).unwrap_or(0),
                modified: tag(block, "LastModified").unwrap_or_default(),
            })
        })
        .collect()
}

/// Send a file, in one PUT if it is small and in parts if it is not.
/// `progress` is called with bytes sent so far.
pub fn put_file(
    b: &Bucket,
    key: &str,
    path: &std::path::Path,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<(), String> {
    use std::io::Read;
    let size = std::fs::metadata(path).map_err(|e| e.to_string())?.len();
    if size <= PART {
        let body = std::fs::read(path).map_err(|e| e.to_string())?;
        let (s, _, text) = send(b, "PUT", key, &[], &body, 3600)?;
        if !ok(s) {
            return Err(why(s, &text));
        }
        progress(size, size);
        return Ok(());
    }
    // Multipart: create, send each part, complete. An error between those
    // leaves parts billing in the bucket, so abort on the way out.
    let (s, _, text) = send(b, "POST", key, &[("uploads".into(), String::new())], &[], 120)?;
    if !ok(s) {
        return Err(why(s, &text));
    }
    let upload = tag(&text, "UploadId").ok_or("the store did not return an upload id")?;
    let mut f = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut etags: Vec<(u32, String)> = Vec::new();
    let mut done = 0u64;
    let mut n = 0u32;
    let result = loop {
        let mut buf = vec![0u8; PART as usize];
        let mut filled = 0usize;
        let mut trouble = None;
        while filled < buf.len() {
            match f.read(&mut buf[filled..]) {
                Ok(0) => break,
                Ok(k) => filled += k,
                Err(e) => {
                    trouble = Some(e.to_string());
                    break;
                }
            }
        }
        if let Some(e) = trouble {
            break Err(e);
        }
        if filled == 0 {
            break Ok(());
        }
        buf.truncate(filled);
        n += 1;
        let q = vec![
            ("partNumber".to_string(), n.to_string()),
            ("uploadId".to_string(), upload.clone()),
        ];
        match send(b, "PUT", key, &q, &buf, 3600) {
            Ok((s, hdrs, text)) if ok(s) => {
                let etag = hdrs
                    .iter()
                    .find(|(k, _)| k == "etag")
                    .map(|(_, v)| v.clone())
                    .unwrap_or_default();
                if etag.is_empty() {
                    break Err(format!("part {n} came back without an ETag: {text}"));
                }
                etags.push((n, etag));
            }
            Ok((s, _, text)) => break Err(why(s, &text)),
            Err(e) => break Err(e),
        }
        done += filled as u64;
        progress(done, size);
    };
    if let Err(e) = result {
        let _ = send(
            b,
            "DELETE",
            key,
            &[("uploadId".to_string(), upload.clone())],
            &[],
            60,
        );
        return Err(e);
    }
    let body = format!(
        "<CompleteMultipartUpload>{}</CompleteMultipartUpload>",
        etags
            .iter()
            .map(|(n, e)| format!("<Part><PartNumber>{n}</PartNumber><ETag>{e}</ETag></Part>"))
            .collect::<String>()
    );
    let (s, _, text) = send(
        b,
        "POST",
        key,
        &[("uploadId".to_string(), upload.clone())],
        body.as_bytes(),
        600,
    )?;
    if !ok(s) {
        let _ = send(
            b,
            "DELETE",
            key,
            &[("uploadId".to_string(), upload)],
            &[],
            60,
        );
        return Err(why(s, &text));
    }
    // A 200 can still carry an error body for this call.
    if text.contains("<Error>") {
        return Err(why(200, &text));
    }
    progress(size, size);
    Ok(())
}

/// Can we reach the bucket and write to it? Puts and deletes one tiny key,
/// so a wrong region or a read-only key is caught at setup rather than at
/// the first backup.
pub fn check(b: &Bucket, prefix: &str) -> Result<String, String> {
    let key = format!("{prefix}.hoosier-write-test");
    let (s, _, text) = send(b, "PUT", &key, &[], b"hoosier", 60)?;
    if !ok(s) {
        return Err(why(s, &text));
    }
    let _ = delete(b, &key);
    let n = list(b, prefix)?.len();
    Ok(format!("Wrote and removed a test object. {n} already there."))
}

#[cfg(test)]
mod tests {
    use super::*;

    // The published AWS SigV4 test vector `get-vanilla`: a bare GET with
    // only host and date signed. The expected header is from the suite, so
    // this pins the whole chain — canonical request, string to sign and key
    // derivation — not just one step of it.
    #[test]
    fn the_published_test_vector_signs_byte_for_byte() {
        let headers = vec![
            ("Host".to_string(), "example.amazonaws.com".to_string()),
            ("X-Amz-Date".to_string(), "20150830T123600Z".to_string()),
        ];
        let got = authorization(
            "GET",
            "/",
            &[],
            &headers,
            &sha256_hex(b""),
            "AKIDEXAMPLE",
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            "us-east-1",
            "service",
            "20150830T123600Z",
        );
        assert_eq!(
            got,
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, \
             SignedHeaders=host;x-amz-date, \
             Signature=5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
        );
    }

    #[test]
    fn the_canonical_request_is_built_the_way_the_spec_says() {
        let (canon, signed) = canonical_request(
            "GET",
            "/",
            &[
                ("Param2".to_string(), "value2".to_string()),
                ("Param1".to_string(), "value1".to_string()),
            ],
            &[
                ("Host".to_string(), "example.amazonaws.com".to_string()),
                ("X-Amz-Date".to_string(), "20150830T123600Z".to_string()),
            ],
            &sha256_hex(b""),
        );
        assert_eq!(signed, "host;x-amz-date");
        // Query parameters sort by name, not by the order they were given.
        assert!(canon.contains("Param1=value1&Param2=value2"), "{canon}");
        assert!(canon.starts_with("GET\n/\n"), "{canon}");
    }

    #[test]
    fn encoding_follows_the_unreserved_set() {
        assert_eq!(uri_encode("a/b c+d~e", true), "a/b%20c%2Bd~e");
        assert_eq!(uri_encode("a/b", false), "a%2Fb");
        // The empty payload hash is a constant worth recognising.
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn path_style_puts_the_bucket_in_the_path_and_virtual_style_in_the_host() {
        let mut b = Bucket {
            endpoint: "https://s3.us-east-2.amazonaws.com".into(),
            bucket: "my-logs".into(),
            ..Default::default()
        };
        let t = target(&b, "backups/a.tar.gz.age").unwrap();
        assert_eq!(t.host, "my-logs.s3.us-east-2.amazonaws.com");
        assert_eq!(t.uri, "/backups/a.tar.gz.age");

        b.path_style = true;
        let t = target(&b, "backups/a.tar.gz.age").unwrap();
        assert_eq!(t.host, "s3.us-east-2.amazonaws.com");
        assert_eq!(t.uri, "/my-logs/backups/a.tar.gz.age");
    }

    // Supabase hangs its S3 API off a path, which has to end up in the
    // signed URI or every request is a 403.
    #[test]
    fn an_endpoint_with_a_path_of_its_own_keeps_it() {
        let b = Bucket {
            endpoint: "https://abcdefgh.supabase.co/storage/v1/s3".into(),
            bucket: "radio".into(),
            path_style: true,
            ..Default::default()
        };
        let t = target(&b, "b/x.age").unwrap();
        assert_eq!(t.host, "abcdefgh.supabase.co");
        assert_eq!(t.uri, "/storage/v1/s3/radio/b/x.age");
        assert_eq!(t.url, "https://abcdefgh.supabase.co/storage/v1/s3/radio/b/x.age");
    }

    #[test]
    fn a_listing_is_read_back_into_objects() {
        let xml = "<?xml version=\"1.0\"?><ListBucketResult><IsTruncated>false</IsTruncated>\
            <Contents><Key>bk/2026-09-12.tar.gz.age</Key><LastModified>2026-09-12T04:00:00.000Z</LastModified><Size>12345</Size></Contents>\
            <Contents><Key>bk/2026-09-11.tar.gz.age</Key><LastModified>2026-09-11T04:00:00.000Z</LastModified><Size>9</Size></Contents>\
            </ListBucketResult>";
        let got = parse_listing(xml);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].key, "bk/2026-09-12.tar.gz.age");
        assert_eq!(got[0].bytes, 12345);
        assert_eq!(got[1].bytes, 9);
    }

    #[test]
    fn an_error_body_is_read_rather_than_shown_as_a_bare_number() {
        let xml = "<Error><Code>SignatureDoesNotMatch</Code><Message>The request signature we calculated does not match</Message></Error>";
        assert!(why(403, xml).starts_with("403 SignatureDoesNotMatch: The request signature"));
        assert_eq!(why(500, ""), "HTTP 500");
    }

    #[test]
    fn an_endpoint_without_a_scheme_is_refused_before_anything_is_sent() {
        let b = Bucket {
            endpoint: "s3.amazonaws.com".into(),
            bucket: "x".into(),
            ..Default::default()
        };
        assert!(target(&b, "k").is_err());
    }
}
