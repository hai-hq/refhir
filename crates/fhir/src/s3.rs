//! Download packages from an S3-compatible store: AWS S3, MinIO, Cloudflare R2, Ceph,
//! Backblaze B2, Wasabi and anything else that accepts AWS Signature V4.
//!
//! A package source is `s3://bucket/prefix`. The objects use the same layout as a package
//! registry: `<prefix>/<name>/<version>` holds the `.tgz`. Only `GET` is needed, so the
//! signer is a few lines instead of an SDK, and the request goes through the same HTTP client
//! (timeouts, retries, size limit) as a plain registry or mirror.
//!
//! Settings, all from the environment so secrets stay out of the config file:
//!
//! - `REFHIR_S3_ACCESS_KEY_ID` / `REFHIR_S3_SECRET_ACCESS_KEY` (and `REFHIR_S3_SESSION_TOKEN`
//!   for temporary keys); the `AWS_*` names work too. Without keys the request is unsigned,
//!   which suits a public bucket.
//! - `REFHIR_S3_ENDPOINT` (or `AWS_ENDPOINT_URL_S3` / `AWS_ENDPOINT_URL`): the store's URL.
//!   Requests are then path-style, which MinIO and most others need. Unset means AWS S3.
//! - `REFHIR_S3_REGION` (or `AWS_REGION`): part of the signature. Default `us-east-1`; some
//!   stores want a specific value, such as `auto` for R2.

use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::collections::BTreeMap;

use crate::error::FhirError;
use crate::npm::{hex, sha256_hex};

const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

#[derive(Clone, PartialEq, Eq)]
struct Keys {
    access_key: String,
    secret_key: String,
    session_token: Option<String>,
}

impl std::fmt::Debug for Keys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Keys")
    }
}

/// Everything read from the environment, so it can be tested without touching it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Settings {
    region: String,
    /// `https://host[:port]` of an S3-compatible service.
    endpoint: Option<String>,
    keys: Option<Keys>,
}

impl Settings {
    fn from_lookup(get: &dyn Fn(&str) -> Option<String>) -> Result<Self, FhirError> {
        let first = |names: &[&str]| {
            names
                .iter()
                .find_map(|name| get(name).filter(|value| !value.is_empty()))
        };
        let keys = match (
            first(&["REFHIR_S3_ACCESS_KEY_ID", "AWS_ACCESS_KEY_ID"]),
            first(&["REFHIR_S3_SECRET_ACCESS_KEY", "AWS_SECRET_ACCESS_KEY"]),
        ) {
            (Some(access_key), Some(secret_key)) => Some(Keys {
                access_key,
                secret_key,
                session_token: first(&["REFHIR_S3_SESSION_TOKEN", "AWS_SESSION_TOKEN"]),
            }),
            (None, None) => None,
            _ => {
                return Err(FhirError::Invalid(
                    "S3 credentials need both an access key id and a secret access key".into(),
                ));
            }
        };
        let endpoint = first(&[
            "REFHIR_S3_ENDPOINT",
            "AWS_ENDPOINT_URL_S3",
            "AWS_ENDPOINT_URL",
        ])
        .map(|value| value.trim_end_matches('/').to_string());
        if let Some(endpoint) = &endpoint {
            if !endpoint.starts_with("http://") && !endpoint.starts_with("https://") {
                return Err(FhirError::Invalid(format!(
                    "S3 endpoint {endpoint} must start with http:// or https://"
                )));
            }
        }
        Ok(Self {
            region: first(&["REFHIR_S3_REGION", "AWS_REGION", "AWS_DEFAULT_REGION"])
                .unwrap_or_else(|| "us-east-1".into()),
            endpoint,
            keys,
        })
    }
}

#[derive(Clone, Debug)]
pub(crate) struct S3Source {
    bucket: String,
    /// Empty, or ends with `/`.
    prefix: String,
    settings: Settings,
}

/// One object to fetch: where, and what to sign.
pub(crate) struct S3Object {
    pub url: String,
    host: String,
    path: String,
}

impl S3Source {
    /// `Some` when `location` is an `s3://` URL.
    pub(crate) fn from_location(location: &str) -> Option<Result<Self, FhirError>> {
        let rest = location.strip_prefix("s3://")?;
        Some(Self::parse(rest, &|name| std::env::var(name).ok()))
    }

    fn parse(rest: &str, env: &dyn Fn(&str) -> Option<String>) -> Result<Self, FhirError> {
        let (bucket, prefix) = rest.split_once('/').unwrap_or((rest, ""));
        if bucket.is_empty() {
            return Err(FhirError::Invalid(
                "package registry s3:// needs a bucket: s3://bucket/prefix".into(),
            ));
        }
        let prefix = prefix.trim_matches('/');
        Ok(Self {
            bucket: bucket.to_string(),
            prefix: if prefix.is_empty() {
                String::new()
            } else {
                format!("{prefix}/")
            },
            settings: Settings::from_lookup(env)?,
        })
    }

    /// The object `<prefix><name>/<version>`.
    pub(crate) fn object(&self, name: &str, version: &str) -> S3Object {
        let key = format!("{}{name}/{version}", self.prefix);
        let encoded = uri_encode_path(&key);
        let region = &self.settings.region;
        let (origin, path) = match &self.settings.endpoint {
            Some(endpoint) => {
                // A gateway may serve S3 under a path prefix. It is part of the request path
                // and so of what is signed; only the authority is the host.
                let (origin, base) = split_endpoint(endpoint);
                (
                    origin,
                    format!("{base}/{}/{encoded}", uri_encode(&self.bucket)),
                )
            }
            // Dots in a bucket name break the wildcard certificate of virtual-hosted style.
            None if self.bucket.contains('.') => (
                format!("https://s3.{region}.amazonaws.com"),
                format!("/{}/{encoded}", uri_encode(&self.bucket)),
            ),
            None => (
                format!("https://{}.s3.{region}.amazonaws.com", self.bucket),
                format!("/{encoded}"),
            ),
        };
        let host = origin
            .split_once("://")
            .map(|(_, host)| host)
            .unwrap_or(&origin)
            .to_string();
        S3Object {
            url: format!("{origin}{path}"),
            host,
            path,
        }
    }

    /// Request headers for a `GET` of `object` at `now` (Unix seconds). Empty when unsigned.
    pub(crate) fn headers(&self, object: &S3Object, now: u64) -> Vec<(String, String)> {
        let Some(keys) = &self.settings.keys else {
            return Vec::new();
        };
        let region = &self.settings.region;
        let (amz_date, date) = amz_timestamps(now);
        let mut headers = BTreeMap::new();
        headers.insert("host".to_string(), object.host.clone());
        headers.insert("x-amz-content-sha256".to_string(), EMPTY_SHA256.to_string());
        headers.insert("x-amz-date".to_string(), amz_date.clone());
        if let Some(token) = &keys.session_token {
            headers.insert("x-amz-security-token".to_string(), token.clone());
        }
        let (signature, signed) = sign(keys, region, &object.path, &headers, &amz_date, &date);
        let authorization = format!(
            "AWS4-HMAC-SHA256 Credential={}/{date}/{region}/s3/aws4_request, SignedHeaders={signed}, Signature={signature}",
            keys.access_key
        );
        // `host` is added by the HTTP client; everything else is sent as signed.
        headers
            .into_iter()
            .filter(|(name, _)| name != "host")
            .chain([("authorization".to_string(), authorization)])
            .collect()
    }
}

/// Split `scheme://host[:port][/path]` into the origin (`scheme://host[:port]`) and the path
/// prefix (empty, or starting with `/` and without a trailing one). A default port is dropped
/// because HTTP clients do not send it in the Host header, so it must not be signed either.
fn split_endpoint(endpoint: &str) -> (String, String) {
    let (scheme, rest) = endpoint.split_once("://").unwrap_or(("https", endpoint));
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    let default_port = if scheme == "http" { ":80" } else { ":443" };
    let authority = authority.strip_suffix(default_port).unwrap_or(authority);
    let path = path.trim_matches('/');
    let path = if path.is_empty() {
        String::new()
    } else {
        format!("/{path}")
    };
    (format!("{scheme}://{authority}"), path)
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("hmac accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

/// AWS Signature V4 for a `GET` with no query string. `headers` are lower-case names with
/// trimmed values and must include `host`. Returns the signature and the signed header list.
fn sign(
    keys: &Keys,
    region: &str,
    canonical_path: &str,
    headers: &BTreeMap<String, String>,
    amz_date: &str,
    date: &str,
) -> (String, String) {
    let signed = headers.keys().cloned().collect::<Vec<_>>().join(";");
    let canonical_headers: String = headers
        .iter()
        .map(|(name, value)| format!("{name}:{}\n", value.trim()))
        .collect();
    let payload = headers
        .get("x-amz-content-sha256")
        .map(String::as_str)
        .unwrap_or(EMPTY_SHA256);
    let canonical_request =
        format!("GET\n{canonical_path}\n\n{canonical_headers}\n{signed}\n{payload}");
    let scope = format!("{date}/{region}/s3/aws4_request");
    let to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        sha256_hex(canonical_request.as_bytes())
    );
    let mut key = hmac_sha256(
        format!("AWS4{}", keys.secret_key).as_bytes(),
        date.as_bytes(),
    );
    for part in [region, "s3", "aws4_request"] {
        key = hmac_sha256(&key, part.as_bytes());
    }
    (hex(&hmac_sha256(&key, to_sign.as_bytes())), signed)
}

/// `(YYYYMMDDTHHMMSSZ, YYYYMMDD)` for a Unix time in seconds.
fn amz_timestamps(secs: u64) -> (String, String) {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    let date = format!("{year:04}{month:02}{day:02}");
    let stamp = format!(
        "{date}T{:02}{:02}{:02}Z",
        rem / 3_600,
        rem % 3_600 / 60,
        rem % 60
    );
    (stamp, date)
}

fn uri_encode(segment: &str) -> String {
    let mut out = String::new();
    for byte in segment.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn uri_encode_path(key: &str) -> String {
    key.split('/').map(uri_encode).collect::<Vec<_>>().join("/")
}

#[cfg(test)]
impl S3Source {
    /// A source for `endpoint` with fixed keys, so a test never reads the environment.
    pub(crate) fn for_test(endpoint: &str) -> Self {
        Self {
            bucket: "packages".into(),
            prefix: "fhir/".into(),
            settings: Settings {
                region: "eu-west-1".into(),
                endpoint: Some(endpoint.into()),
                keys: Some(Keys {
                    access_key: "TESTKEY".into(),
                    secret_key: "testsecret".into(),
                    session_token: None,
                }),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect();
        move |name| map.get(name).cloned()
    }

    fn example_keys() -> Keys {
        Keys {
            access_key: "AKIAIOSFODNN7EXAMPLE".into(),
            secret_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into(),
            session_token: None,
        }
    }

    #[test]
    fn signature_matches_the_aws_documented_get_object_example() {
        // https://docs.aws.amazon.com/AmazonS3/latest/API/sig-v4-header-based-auth.html
        let headers = BTreeMap::from([
            (
                "host".to_string(),
                "examplebucket.s3.amazonaws.com".to_string(),
            ),
            ("range".to_string(), "bytes=0-9".to_string()),
            ("x-amz-content-sha256".to_string(), EMPTY_SHA256.to_string()),
            ("x-amz-date".to_string(), "20130524T000000Z".to_string()),
        ]);
        let (signature, signed) = sign(
            &example_keys(),
            "us-east-1",
            "/test.txt",
            &headers,
            "20130524T000000Z",
            "20130524",
        );
        assert_eq!(signed, "host;range;x-amz-content-sha256;x-amz-date");
        assert_eq!(
            signature,
            "f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        );
    }

    #[test]
    fn timestamps_are_formatted_in_utc() {
        assert_eq!(
            amz_timestamps(1_369_353_600),
            ("20130524T000000Z".into(), "20130524".into())
        );
        // A leap day, and the end of a day.
        assert_eq!(amz_timestamps(1_709_251_199).0, "20240229T235959Z");
        assert_eq!(amz_timestamps(0).0, "19700101T000000Z");
    }

    fn source(rest: &str, endpoint: Option<&str>, keys: bool) -> S3Source {
        let mut source = S3Source::parse(rest, &env(&[])).unwrap();
        source.settings.endpoint = endpoint.map(str::to_string);
        source.settings.region = "eu-west-1".into();
        source.settings.keys = keys.then(example_keys);
        source
    }

    #[test]
    fn objects_use_the_registry_layout_under_the_prefix() {
        let aws = source("fhir-packages/r4/", None, true).object("hl7.fhir.r4.core", "4.0.1");
        assert_eq!(
            aws.url,
            "https://fhir-packages.s3.eu-west-1.amazonaws.com/r4/hl7.fhir.r4.core/4.0.1"
        );
        let dotted = source("my.bucket", None, true).object("a.b", "1.0.0-rc.1");
        assert_eq!(
            dotted.url,
            "https://s3.eu-west-1.amazonaws.com/my.bucket/a.b/1.0.0-rc.1"
        );
        let minio = source("packages/x", Some("http://127.0.0.1:9000"), true).object("a", "1");
        assert_eq!(minio.url, "http://127.0.0.1:9000/packages/x/a/1");
        assert_eq!(minio.host, "127.0.0.1:9000");
        // A path prefix is part of the request path (and so of the signature), not of the host.
        let gateway =
            source("packages", Some("https://gw.example.com/minio/"), true).object("a", "1");
        assert_eq!(gateway.url, "https://gw.example.com/minio/packages/a/1");
        assert_eq!(
            (gateway.host.as_str(), gateway.path.as_str()),
            ("gw.example.com", "/minio/packages/a/1")
        );
        // A default port is not sent in the Host header, so it must not be signed either.
        let https = source("b", Some("https://s3.example.com:443"), true).object("a", "1");
        assert_eq!(
            (https.url.as_str(), https.host.as_str()),
            ("https://s3.example.com/b/a/1", "s3.example.com")
        );
    }

    #[test]
    fn unsigned_without_keys_and_signed_with_them() {
        let object = source("b", None, false).object("a", "1");
        assert!(source("b", None, false).headers(&object, 0).is_empty());
        let headers = source("b", None, true).headers(&object, 1_369_353_600);
        let get = |name: &str| {
            headers
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        };
        assert_eq!(get("x-amz-date").as_deref(), Some("20130524T000000Z"));
        assert_eq!(get("x-amz-content-sha256").as_deref(), Some(EMPTY_SHA256));
        let authorization = get("authorization").unwrap();
        assert!(
            authorization.starts_with(
                "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/eu-west-1/s3/aws4_request"
            ),
            "{authorization}"
        );
        assert!(authorization.contains("SignedHeaders=host;x-amz-content-sha256;x-amz-date,"));
    }

    #[test]
    fn a_session_token_is_signed_and_sent() {
        let mut with_token = source("b", None, true);
        with_token.settings.keys.as_mut().unwrap().session_token = Some("tok".into());
        let object = with_token.object("a", "1");
        let headers = with_token.headers(&object, 0);
        assert!(headers
            .iter()
            .any(|(k, v)| k == "x-amz-security-token" && v == "tok"));
        let authorization = &headers
            .iter()
            .find(|(k, _)| k == "authorization")
            .unwrap()
            .1;
        assert!(
            authorization.contains("x-amz-date;x-amz-security-token,"),
            "{authorization}"
        );
    }

    #[test]
    fn settings_come_from_the_environment() {
        let settings = Settings::from_lookup(&env(&[
            ("REFHIR_S3_ENDPOINT", "http://127.0.0.1:9000/"),
            ("REFHIR_S3_REGION", "auto"),
            ("REFHIR_S3_ACCESS_KEY_ID", "key"),
            ("REFHIR_S3_SECRET_ACCESS_KEY", "secret"),
        ]))
        .unwrap();
        assert_eq!(settings.endpoint.as_deref(), Some("http://127.0.0.1:9000"));
        assert_eq!(settings.region, "auto");
        assert_eq!(
            settings.keys.as_ref().map(|k| k.access_key.as_str()),
            Some("key")
        );
        // The standard AWS names work as a fallback; REFHIR_S3_ wins when both are set.
        let aws = Settings::from_lookup(&env(&[
            ("AWS_ACCESS_KEY_ID", "a"),
            ("AWS_SECRET_ACCESS_KEY", "b"),
            ("AWS_ENDPOINT_URL_S3", "http://s3.local"),
            ("REFHIR_S3_ENDPOINT", "http://mine"),
        ]))
        .unwrap();
        assert_eq!(aws.endpoint.as_deref(), Some("http://mine"));
        assert_eq!(aws.region, "us-east-1");
        assert!(aws.keys.is_some());
        assert!(Settings::from_lookup(&env(&[])).unwrap().keys.is_none());
    }

    #[test]
    fn an_endpoint_without_a_scheme_is_rejected() {
        let err = Settings::from_lookup(&env(&[("REFHIR_S3_ENDPOINT", "minio:9000")]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("http://"), "{err}");
        assert!(
            Settings::from_lookup(&env(&[("REFHIR_S3_ENDPOINT", "https://s3.example.com")]))
                .is_ok()
        );
    }

    #[test]
    fn half_a_key_pair_is_an_error() {
        assert!(Settings::from_lookup(&env(&[("REFHIR_S3_ACCESS_KEY_ID", "only")])).is_err());
        assert!(Settings::from_lookup(&env(&[("AWS_SECRET_ACCESS_KEY", "only")])).is_err());
    }

    #[test]
    fn a_location_needs_a_bucket_and_normalises_the_prefix() {
        assert!(S3Source::from_location("https://packages.fhir.org").is_none());
        assert!(S3Source::from_location("s3://").unwrap().is_err());
        let source = S3Source::parse("bucket//r4/x/", &env(&[])).unwrap();
        assert_eq!(
            (source.bucket.as_str(), source.prefix.as_str()),
            ("bucket", "r4/x/")
        );
        assert_eq!(S3Source::parse("bucket", &env(&[])).unwrap().prefix, "");
    }
}
