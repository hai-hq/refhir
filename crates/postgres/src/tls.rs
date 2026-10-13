//! TLS for the Postgres connection, driven by the connection string the way libpq does it.
//!
//! `tokio-postgres` understands `sslmode=disable|prefer|require` and rejects everything else
//! (`verify-ca`, `verify-full`, `sslrootcert`, ...), and it does no verification of its own. So
//! the `ssl*` parameters are taken out of the connection string here, the rest goes to the
//! driver, and the TLS settings become a rustls client configuration.
//!
//! | `sslmode`                  | TLS                | server certificate                     |
//! |----------------------------|--------------------|----------------------------------------|
//! | absent, `disable`          | never              | -                                      |
//! | `prefer`                   | if the server does | not checked                            |
//! | `require`                  | always             | not checked (chain checked if `sslrootcert` is given, as libpq does) |
//! | `verify-ca`                | always             | chain checked against `sslrootcert`    |
//! | `verify-full`              | always             | chain and host name checked            |
//!
//! No `sslmode` means no TLS: that is what the connection did before TLS was supported, and
//! deployments that never set it must keep working.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::SystemTime;

use percent_encoding::percent_decode_str;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::WebPkiServerVerifier;
use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, CryptoProvider};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{
    CertificateError, ClientConfig, DigitallySignedStruct, Error as TlsError, RootCertStore,
    SignatureScheme,
};
use tokio_postgres_rustls::MakeRustlsConnect;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum SslMode {
    Disable,
    Prefer,
    Require,
    VerifyCa,
    VerifyFull,
}

impl SslMode {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "disable" => Ok(Self::Disable),
            "prefer" => Ok(Self::Prefer),
            "require" => Ok(Self::Require),
            "verify-ca" => Ok(Self::VerifyCa),
            "verify-full" => Ok(Self::VerifyFull),
            other => Err(format!(
                "unsupported sslmode `{other}`: use disable, prefer, require, verify-ca or verify-full"
            )),
        }
    }

    /// What the driver is told. It only decides whether TLS is attempted and whether a
    /// refusal is fatal; verification is ours.
    pub(crate) fn driver_mode(self) -> postgres::config::SslMode {
        match self {
            Self::Disable => postgres::config::SslMode::Disable,
            Self::Prefer => postgres::config::SslMode::Prefer,
            Self::Require | Self::VerifyCa | Self::VerifyFull => postgres::config::SslMode::Require,
        }
    }
}

/// Whether every host of the connection is a Unix socket (and there is at least one).
pub(crate) fn all_unix_sockets(config: &postgres::Config) -> bool {
    #[cfg(unix)]
    {
        let hosts = config.get_hosts();
        !hosts.is_empty()
            && hosts
                .iter()
                .all(|host| matches!(host, postgres::config::Host::Unix(_)))
    }
    #[cfg(not(unix))]
    {
        let _ = config;
        false
    }
}

/// Built client configurations by settings, with the modification times of the files they read.
type ConfigCache = HashMap<TlsSettings, (Vec<Option<SystemTime>>, ClientConfig)>;

/// The `ssl*` parameters of a connection string.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct TlsSettings {
    pub(crate) mode: SslMode,
    /// `sslrootcert`: a PEM file of the CA certificates that may have signed the server's.
    pub(crate) root_cert: Option<String>,
    /// `sslcert` and `sslkey`: a PEM client certificate chain and its private key.
    pub(crate) client_cert: Option<String>,
    pub(crate) client_key: Option<String>,
}

/// Splits a connection string (URL or `key=value` form) into what the driver parses and the
/// TLS settings. Nothing from `url` is put into an error: it can hold a password.
pub(crate) fn split(url: &str) -> Result<(String, TlsSettings), String> {
    let is_url = url.starts_with("postgres://") || url.starts_with("postgresql://");
    let (head, pairs) = if is_url {
        url_pairs(url)
    } else {
        key_value_pairs(url)?
    };

    let mut mode = None;
    let (mut root_cert, mut client_cert, mut client_key) = (None, None, None);
    let mut kept = Vec::new();
    for pair in pairs {
        let slot = match pair.key.as_str() {
            "sslmode" => {
                mode = Some(SslMode::parse(&pair.value)?);
                continue;
            }
            "sslrootcert" => &mut root_cert,
            "sslcert" => &mut client_cert,
            "sslkey" => &mut client_key,
            _ => {
                kept.push(pair.raw);
                continue;
            }
        };
        *slot = Some(pair.value).filter(|value| !value.is_empty());
    }

    let settings = TlsSettings {
        mode: mode.unwrap_or(SslMode::Disable),
        root_cert,
        client_cert,
        client_key,
    };
    settings.validate()?;

    let rest = if is_url {
        if kept.is_empty() {
            head
        } else {
            format!("{head}?{}", kept.join("&"))
        }
    } else {
        kept.join(" ")
    };
    Ok((rest, settings))
}

impl TlsSettings {
    fn validate(&self) -> Result<(), String> {
        let mode = match self.mode {
            SslMode::Disable => "disable",
            SslMode::Prefer => "prefer",
            SslMode::Require => "require",
            SslMode::VerifyCa => "verify-ca",
            SslMode::VerifyFull => "verify-full",
        };
        let uses_tls = self.mode != SslMode::Disable;
        if !uses_tls
            && (self.root_cert.is_some() || self.client_cert.is_some() || self.client_key.is_some())
        {
            return Err(format!(
                "sslrootcert, sslcert and sslkey need an sslmode that uses TLS, but the mode is `{mode}`; \
                 set sslmode=verify-full"
            ));
        }
        if self.mode == SslMode::Prefer && self.root_cert.is_some() {
            return Err(
                "sslrootcert has no effect with sslmode=prefer, which does not check the server's \
                 certificate: use verify-ca or verify-full"
                    .into(),
            );
        }
        if matches!(self.mode, SslMode::VerifyCa | SslMode::VerifyFull) && self.root_cert.is_none()
        {
            return Err(format!(
                "sslmode={mode} needs sslrootcert=<path to the CA certificate, PEM>"
            ));
        }
        if self.client_cert.is_some() != self.client_key.is_some() {
            return Err("sslcert and sslkey must be given together".into());
        }
        Ok(())
    }

    /// The verification that applies. As in libpq, `require` with a root certificate checks the
    /// chain: asking for encryption while handing over a CA is not a request to skip the check.
    fn verification(&self) -> Option<SslMode> {
        match self.mode {
            SslMode::Disable | SslMode::Prefer => None,
            SslMode::Require if self.root_cert.is_none() => None,
            SslMode::Require => Some(SslMode::VerifyCa),
            mode => Some(mode),
        }
    }

    /// The connector for these settings, or `None` when TLS is not used. The pool opens
    /// connections on demand, so the configuration is built once and reused until one of the
    /// certificate files changes on disk (a rotated certificate is picked up without a restart).
    pub(crate) fn connector(&self) -> Result<Option<MakeRustlsConnect>, String> {
        static CACHE: OnceLock<Mutex<ConfigCache>> = OnceLock::new();
        if self.mode == SslMode::Disable {
            return Ok(None);
        }
        let stamp: Vec<Option<SystemTime>> = [&self.root_cert, &self.client_cert, &self.client_key]
            .into_iter()
            .map(|path| {
                let path = path.as_ref()?;
                std::fs::metadata(path)
                    .and_then(|meta| meta.modified())
                    .ok()
            })
            .collect();
        let mut cache = CACHE
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some((seen, config)) = cache.get(self) {
            if *seen == stamp {
                return Ok(Some(MakeRustlsConnect::new(config.clone())));
            }
        }
        let config = self.client_config()?;
        cache.insert(self.clone(), (stamp, config.clone()));
        Ok(Some(MakeRustlsConnect::new(config)))
    }

    /// Reads the certificate files and builds the rustls configuration.
    fn client_config(&self) -> Result<ClientConfig, String> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let verifier: Arc<dyn ServerCertVerifier> = match (self.verification(), &self.root_cert) {
            (Some(mode), Some(path)) => {
                let mut roots = RootCertStore::empty();
                let mut count = 0;
                for cert in CertificateDer::pem_file_iter(path)
                    .map_err(|err| format!("sslrootcert {path}: {err}"))?
                {
                    let cert = cert.map_err(|err| format!("sslrootcert {path}: {err}"))?;
                    roots
                        .add(cert)
                        .map_err(|err| format!("sslrootcert {path}: {err}"))?;
                    count += 1;
                }
                if count == 0 {
                    return Err(format!("sslrootcert {path}: no certificate found"));
                }
                let inner =
                    WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider.clone())
                        .build()
                        .map_err(|err| format!("sslrootcert {path}: {err}"))?;
                if mode == SslMode::VerifyCa {
                    Arc::new(IgnoreHostName(inner))
                } else {
                    inner
                }
            }
            _ => Arc::new(AcceptAnyCertificate(provider.clone())),
        };

        let builder = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|err| format!("TLS configuration: {err}"))?
            .dangerous()
            .with_custom_certificate_verifier(verifier);
        let mut config = match (&self.client_cert, &self.client_key) {
            (Some(cert), Some(key)) => {
                let chain = CertificateDer::pem_file_iter(cert)
                    .map_err(|err| format!("sslcert {cert}: {err}"))?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|err| format!("sslcert {cert}: {err}"))?;
                let key = PrivateKeyDer::from_pem_file(key)
                    .map_err(|err| format!("sslkey {key}: {err}"))?;
                builder
                    .with_client_auth_cert(chain, key)
                    .map_err(|err| format!("sslcert {cert} and its sslkey: {err}"))?
            }
            _ => builder.with_no_client_auth(),
        };
        // What Postgres 17+ requires for `sslnegotiation=direct`; older servers ignore it.
        config.alpn_protocols = vec![b"postgresql".to_vec()];
        Ok(config)
    }
}

/// `verify-ca`: the certificate must chain to a trusted root, whatever host name it was issued
/// for. rustls checks the chain before the name, so a name mismatch means the chain was good.
#[derive(Debug)]
struct IgnoreHostName(Arc<WebPkiServerVerifier>);

impl ServerCertVerifier for IgnoreHostName {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        match self
            .0
            .verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)
        {
            Err(TlsError::InvalidCertificate(
                CertificateError::NotValidForName | CertificateError::NotValidForNameContext { .. },
            )) => Ok(ServerCertVerified::assertion()),
            other => other,
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        self.0.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        self.0.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.supported_verify_schemes()
    }
}

/// `prefer` and `require` without a root certificate: encrypt, and trust whoever answers. This
/// stops eavesdropping but not impersonation; `verify-full` is the mode for production.
#[derive(Debug)]
struct AcceptAnyCertificate(Arc<CryptoProvider>);

impl ServerCertVerifier for AcceptAnyCertificate {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

struct Pair {
    key: String,
    /// Decoded; what the parameter means.
    value: String,
    /// As written, so what is kept goes back to the driver untouched.
    raw: String,
}

/// `postgres://user@host/db?a=b&c=d` into `postgres://user@host/db` and its query pairs. Values
/// are percent-decoded and nothing else, as `tokio-postgres`'s own URL parser does it (it does
/// not turn `+` into a space).
fn url_pairs(url: &str) -> (String, Vec<Pair>) {
    let (head, query) = match url.split_once('?') {
        Some((head, query)) => (head, query),
        None => return (url.to_string(), Vec::new()),
    };
    let decode = |text: &str| percent_decode_str(text).decode_utf8_lossy().into_owned();
    let pairs = query
        .split('&')
        .filter(|raw| !raw.is_empty())
        .map(|raw| {
            let (key, value) = raw.split_once('=').unwrap_or((raw, ""));
            Pair {
                key: decode(key),
                value: decode(value),
                raw: raw.to_string(),
            }
        })
        .collect();
    (head.to_string(), pairs)
}

/// libpq's `host=db user=refhir password='a b'` form: `key = value` pairs, a value either bare
/// or in single quotes, with a backslash escaping the next character in both.
fn key_value_pairs(text: &str) -> Result<(String, Vec<Pair>), String> {
    let invalid = |why: &str| format!("invalid connection string: {why}");
    let mut chars = text.char_indices().peekable();
    let mut pairs = Vec::new();
    loop {
        while chars.next_if(|(_, c)| c.is_whitespace()).is_some() {}
        let Some(&(start, _)) = chars.peek() else {
            break;
        };
        let mut key = String::new();
        while let Some((_, c)) = chars.next_if(|(_, c)| *c != '=' && !c.is_whitespace()) {
            key.push(c);
        }
        while chars.next_if(|(_, c)| c.is_whitespace()).is_some() {}
        if chars.next_if(|(_, c)| *c == '=').is_none() {
            return Err(invalid(&format!("`{key}` has no value")));
        }
        while chars.next_if(|(_, c)| c.is_whitespace()).is_some() {}

        let mut value = String::new();
        let quoted = chars.next_if(|(_, c)| *c == '\'').is_some();
        // Where the pair ends in `text`: an escaped space is part of it, so it is not trimmed.
        let end = loop {
            match chars.next() {
                Some((_, '\\')) => match chars.next() {
                    Some((_, c)) => value.push(c),
                    None => return Err(invalid("a backslash ends the string")),
                },
                Some((at, '\'')) if quoted => break at + 1,
                None if quoted => return Err(invalid("a quoted value is not closed")),
                Some((at, c)) if !quoted && c.is_whitespace() => break at,
                Some((_, c)) => value.push(c),
                None => break text.len(),
            }
        };
        pairs.push(Pair {
            key,
            value,
            raw: text[start..end].to_string(),
        });
    }
    Ok((String::new(), pairs))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(url: &str) -> TlsSettings {
        split(url).unwrap().1
    }

    #[test]
    fn no_sslmode_means_no_tls_and_the_url_is_untouched() {
        for url in [
            "postgres://u:p@db:5432/refhir",
            "postgresql://u@db/refhir?application_name=refhir",
            "host=db user=u password=p dbname=refhir",
        ] {
            let (rest, tls) = split(url).unwrap();
            assert_eq!(rest, url);
            assert_eq!(tls.mode, SslMode::Disable);
            assert!(tls.connector().unwrap().is_none());
        }
        let (_, tls) = split("postgres://db/x?sslmode=disable").unwrap();
        assert_eq!(tls.mode, SslMode::Disable);
    }

    #[test]
    fn every_mode_is_read_from_a_url() {
        for (value, mode) in [
            ("disable", SslMode::Disable),
            ("prefer", SslMode::Prefer),
            ("require", SslMode::Require),
        ] {
            assert_eq!(
                settings(&format!("postgres://db/x?sslmode={value}")).mode,
                mode
            );
        }
        for (value, mode) in [
            ("verify-ca", SslMode::VerifyCa),
            ("verify-full", SslMode::VerifyFull),
        ] {
            let url = format!("postgres://db/x?sslmode={value}&sslrootcert=/etc/ca.pem");
            assert_eq!(settings(&url).mode, mode);
        }
    }

    #[test]
    fn ssl_parameters_are_taken_out_and_the_rest_is_kept_as_written() {
        let (rest, tls) = split(
            "postgresql://u:p%40ss@db:5432/refhir?application_name=a%20b&sslmode=verify-full\
             &sslrootcert=%2Fetc%2Ftls%2Fca.pem&sslcert=/c.crt&sslkey=/c.key&connect_timeout=5",
        )
        .unwrap();
        assert_eq!(
            rest,
            "postgresql://u:p%40ss@db:5432/refhir?application_name=a%20b&connect_timeout=5"
        );
        assert_eq!(
            tls,
            TlsSettings {
                mode: SslMode::VerifyFull,
                root_cert: Some("/etc/tls/ca.pem".into()),
                client_cert: Some("/c.crt".into()),
                client_key: Some("/c.key".into()),
            }
        );
        // Nothing left in the query: no dangling `?`.
        let (rest, _) = split("postgres://db/x?sslmode=require").unwrap();
        assert_eq!(rest, "postgres://db/x");
    }

    #[test]
    fn the_key_value_form_works_with_quotes() {
        let (rest, tls) = split(
            "host=db user=u password='a b\\'c' sslmode = verify-ca  sslrootcert='/my certs/ca.pem' dbname=x",
        )
        .unwrap();
        assert_eq!(rest, "host=db user=u password='a b\\'c' dbname=x");
        assert_eq!(tls.mode, SslMode::VerifyCa);
        assert_eq!(tls.root_cert.as_deref(), Some("/my certs/ca.pem"));

        // The CI form: no ssl parameters, nothing changes.
        let (rest, tls) =
            split("host=127.0.0.1 port=5432 user=postgres password=postgres").unwrap();
        assert_eq!(
            rest,
            "host=127.0.0.1 port=5432 user=postgres password=postgres"
        );
        assert_eq!(tls.mode, SslMode::Disable);
    }

    #[test]
    fn the_driver_is_asked_for_tls_only_when_a_mode_uses_it() {
        use postgres::config::SslMode as Driver;
        assert_eq!(SslMode::Disable.driver_mode(), Driver::Disable);
        assert_eq!(SslMode::Prefer.driver_mode(), Driver::Prefer);
        for mode in [SslMode::Require, SslMode::VerifyCa, SslMode::VerifyFull] {
            assert_eq!(mode.driver_mode(), Driver::Require);
        }
    }

    #[test]
    fn require_with_a_root_certificate_checks_the_chain_like_libpq() {
        assert_eq!(
            settings("postgres://db/x?sslmode=require").verification(),
            None
        );
        assert_eq!(
            settings("postgres://db/x?sslmode=prefer").verification(),
            None
        );
        assert_eq!(
            settings("postgres://db/x?sslmode=require&sslrootcert=/ca.pem").verification(),
            Some(SslMode::VerifyCa)
        );
        assert_eq!(
            settings("postgres://db/x?sslmode=verify-full&sslrootcert=/ca.pem").verification(),
            Some(SslMode::VerifyFull)
        );
    }

    #[test]
    fn mistakes_are_refused_instead_of_quietly_connecting_without_the_check() {
        for (url, expect) in [
            (
                "postgres://db/x?sslmode=allow",
                "unsupported sslmode `allow`",
            ),
            ("postgres://db/x?sslmode=verify-full", "needs sslrootcert"),
            ("postgres://db/x?sslmode=verify-ca", "needs sslrootcert"),
            (
                "postgres://db/x?sslrootcert=/ca.pem",
                "need an sslmode that uses TLS",
            ),
            (
                "postgres://db/x?sslmode=disable&sslcert=/c&sslkey=/k",
                "need an sslmode",
            ),
            (
                "postgres://db/x?sslmode=prefer&sslrootcert=/ca.pem",
                "no effect with sslmode=prefer",
            ),
            (
                "postgres://db/x?sslmode=require&sslcert=/c.crt",
                "given together",
            ),
            ("host=db sslmode=require sslkey=/c.key", "given together"),
            ("host=db sslmode", "has no value"),
            ("host=db password='open", "not closed"),
        ] {
            let err = split(url).unwrap_err();
            assert!(err.contains(expect), "{url}: {err}");
        }
    }

    #[test]
    fn an_error_never_repeats_the_connection_string() {
        let err = split("postgres://u:hunter2@db/x?sslmode=bogus").unwrap_err();
        assert!(!err.contains("hunter2"), "{err}");
    }

    #[test]
    fn unreadable_certificates_are_reported_with_their_path() {
        let missing = "postgres://db/x?sslmode=verify-full&sslrootcert=/nonexistent/ca.pem";
        let err = split(missing).unwrap().1.connector().err().unwrap();
        assert!(err.contains("sslrootcert /nonexistent/ca.pem"), "{err}");

        let empty =
            std::env::temp_dir().join(format!("refhir-empty-ca-{}.pem", std::process::id()));
        std::fs::write(&empty, "not a certificate\n").unwrap();
        let url = format!(
            "postgres://db/x?sslmode=verify-ca&sslrootcert={}",
            empty.display()
        );
        let err = split(&url).unwrap().1.connector().err().unwrap();
        let _ = std::fs::remove_file(&empty);
        assert!(err.contains("no certificate found"), "{err}");
    }

    #[test]
    fn an_escaped_space_at_the_end_of_a_value_is_kept() {
        let url = "host=db user=u password=abc\\ ";
        let (rest, _) = split(url).unwrap();
        assert_eq!(rest, url);
        let (rest, _) = split("host=db password=abc\\  sslmode=require").unwrap();
        assert_eq!(rest, "host=db password=abc\\ ");
    }

    /// What is handed to the driver must mean what the driver would have made of the original:
    /// the splitter only removes the ssl parameters and must not disturb anything else.
    #[test]
    fn the_driver_reads_the_rest_exactly_as_it_would_have_read_the_original() {
        fn parsed(text: &str) -> String {
            format!("{:?}", text.parse::<postgres::Config>().unwrap())
        }
        for (with_ssl, without) in [
            (
                "postgresql://u:p%40ss@db:5432/refhir?application_name=a+b&sslmode=require&connect_timeout=5",
                "postgresql://u:p%40ss@db:5432/refhir?application_name=a+b&connect_timeout=5",
            ),
            (
                "postgres://u@db/x?sslrootcert=%2Fca.pem&sslmode=verify-ca&application_name=x%20y",
                "postgres://u@db/x?application_name=x%20y",
            ),
            (
                "host=db user=u password='a b\\'c' sslmode=require dbname=x",
                "host=db user=u password='a b\\'c' dbname=x",
            ),
            ("sslmode=prefer host=db password=a\\ b", "host=db password=a\\ b"),
        ] {
            let (rest, _) = split(with_ssl).unwrap();
            assert_eq!(parsed(&rest), parsed(without), "{with_ssl}");
        }
    }

    #[test]
    fn a_plus_in_a_url_is_not_a_space_as_in_the_driver() {
        let (_, tls) = split("postgres://db/x?sslmode=require&sslrootcert=/a+b%20c.pem").unwrap();
        assert_eq!(tls.root_cert.as_deref(), Some("/a+b c.pem"));
    }

    #[test]
    fn unix_socket_hosts_are_recognised() {
        let all = |text: &str| all_unix_sockets(&text.parse().unwrap());
        assert!(all("postgres:///db?host=/var/run/postgresql"));
        assert!(all("host=/tmp user=u"));
        assert!(!all("postgres://db.example.com/x"));
        assert!(!all("host=/tmp,db.example.com user=u"));
        assert!(!all("user=u"));
    }

    #[test]
    fn require_and_prefer_build_a_connector_without_any_files() {
        for mode in ["prefer", "require"] {
            let tls = settings(&format!("postgres://db/x?sslmode={mode}"));
            assert!(tls.connector().unwrap().is_some());
        }
    }
}

/// Against a Postgres that accepts only TLS: `scripts/postgres-tls-test.sh up` starts one and
/// prints the two variables these read. Without them the tests are skipped, unless
/// `REFHIR_REQUIRE_POSTGRES_TLS` is set (CI), which makes a missing server a failure.
#[cfg(test)]
mod server {
    use crate::{connect_client, PostgresPersistence};

    struct Server {
        /// `localhost:port`. The server certificate is valid for that name, not for its address.
        addr: String,
        dir: String,
    }

    fn server() -> Option<Server> {
        match (
            std::env::var("REFHIR_TLS_POSTGRES_ADDR"),
            std::env::var("REFHIR_TLS_POSTGRES_DIR"),
        ) {
            (Ok(addr), Ok(dir)) => Some(Server { addr, dir }),
            _ if std::env::var("REFHIR_REQUIRE_POSTGRES_TLS").is_ok() => {
                panic!("REFHIR_REQUIRE_POSTGRES_TLS is set but the TLS Postgres is not configured")
            }
            _ => {
                eprintln!("REFHIR_TLS_POSTGRES_ADDR unset, skipping the TLS server test");
                None
            }
        }
    }

    impl Server {
        fn url(&self, query: &str) -> String {
            format!(
                "postgres://postgres:postgres@{}/postgres?{query}",
                self.addr
            )
        }

        /// Same server, reached by address: not a name its certificate is valid for.
        fn url_by_address(&self, query: &str) -> String {
            self.url(query).replace("localhost", "127.0.0.1")
        }

        fn file(&self, name: &str) -> String {
            format!("{}/{name}", self.dir)
        }

        fn verify(&self, mode: &str) -> String {
            format!("sslmode={mode}&sslrootcert={}", self.file("ca.pem"))
        }
    }

    /// Whether the session `url` opens is encrypted, as the server sees it.
    fn ssl_in_use(url: &str) -> Result<bool, String> {
        let mut client = connect_client(url).map_err(|err| err.to_string())?;
        let row = client
            .query_one(
                "SELECT ssl FROM pg_stat_ssl WHERE pid = pg_backend_pid()",
                &[],
            )
            .map_err(|err| err.to_string())?;
        Ok(row.get(0))
    }

    #[test]
    fn verify_full_connects_over_tls_when_the_name_and_the_ca_match() {
        let Some(pg) = server() else { return };
        assert_eq!(ssl_in_use(&pg.url(&pg.verify("verify-full"))), Ok(true));
        assert_eq!(ssl_in_use(&pg.url(&pg.verify("verify-ca"))), Ok(true));
    }

    #[test]
    fn require_and_prefer_encrypt_without_a_root_certificate() {
        let Some(pg) = server() else { return };
        assert_eq!(ssl_in_use(&pg.url("sslmode=require")), Ok(true));
        assert_eq!(ssl_in_use(&pg.url("sslmode=prefer")), Ok(true));
        // Reached by address, where the certificate does not match, and still fine: no check.
        assert_eq!(ssl_in_use(&pg.url_by_address("sslmode=require")), Ok(true));
    }

    #[test]
    fn verify_ca_ignores_the_host_name_and_verify_full_does_not() {
        let Some(pg) = server() else { return };
        assert_eq!(
            ssl_in_use(&pg.url_by_address(&pg.verify("verify-ca"))),
            Ok(true)
        );
        // `require` with a root certificate checks the chain too, and so also lets this through.
        assert_eq!(
            ssl_in_use(&pg.url_by_address(&pg.verify("require"))),
            Ok(true)
        );
        let err = ssl_in_use(&pg.url_by_address(&pg.verify("verify-full"))).unwrap_err();
        assert!(err.contains("not valid for name"), "{err}");
    }

    #[test]
    fn a_certificate_from_the_wrong_ca_is_refused_in_every_verifying_mode() {
        let Some(pg) = server() else { return };
        let wrong = |mode: &str| format!("sslmode={mode}&sslrootcert={}", pg.file("wrong-ca.pem"));
        for mode in ["verify-full", "verify-ca", "require"] {
            let err = ssl_in_use(&pg.url(&wrong(mode))).unwrap_err();
            assert!(err.contains("UnknownIssuer"), "{mode}: {err}");
        }
    }

    #[test]
    fn without_sslmode_the_connection_stays_unencrypted_and_this_server_refuses_it() {
        let Some(pg) = server() else { return };
        for url in [pg.url(""), pg.url("sslmode=disable")] {
            let err = ssl_in_use(&url).unwrap_err();
            assert!(err.contains("no encryption"), "{err}");
        }
    }

    #[test]
    fn a_client_certificate_authenticates_the_role_that_needs_one() {
        let Some(pg) = server() else { return };
        let by_cert = |query: &str| format!("postgres://refhir_cert@{}/tls_cert?{query}", pg.addr);
        let with_cert = format!(
            "{}&sslcert={}&sslkey={}",
            pg.verify("verify-full"),
            pg.file("client.crt"),
            pg.file("client.key")
        );
        let mut client = connect_client(&by_cert(&with_cert)).unwrap();
        let user: String = client.query_one("SELECT current_user", &[]).unwrap().get(0);
        assert_eq!(user, "refhir_cert");

        let err = ssl_in_use(&by_cert(&pg.verify("verify-full"))).unwrap_err();
        assert!(err.contains("certificate"), "{err}");
    }

    #[test]
    fn direct_negotiation_works_on_a_server_that_supports_it() {
        let Some(pg) = server() else { return };
        let url = pg.url(&pg.verify("verify-full"));
        let mut client = connect_client(&url).unwrap();
        let version: String = client
            .query_one("SHOW server_version_num", &[])
            .unwrap()
            .get(0);
        if version.parse::<u32>().unwrap() < 170000 {
            eprintln!("Postgres {version} has no direct TLS negotiation, skipping");
            return;
        }
        assert_eq!(
            ssl_in_use(&format!("{url}&sslnegotiation=direct")),
            Ok(true)
        );
    }

    #[test]
    fn the_server_opens_over_tls_end_to_end() {
        let Some(pg) = server() else { return };
        // Schema creation, the pool and its on-demand connections all go through connect_client.
        PostgresPersistence::connect(&pg.url(&pg.verify("verify-full"))).expect("connect over TLS");
    }
}

/// The verification itself, with certificates made on the spot and a TLS server in this
/// process, so these run everywhere and do not depend on an external Postgres.
#[cfg(test)]
mod handshake {
    use super::*;
    use rcgen::{
        BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, Issuer, KeyPair,
    };
    use rustls::server::WebPkiClientVerifier;
    use rustls::{ClientConnection, ServerConfig, ServerConnection};
    use std::io::Write;
    use std::net::{TcpListener, TcpStream};
    use std::path::PathBuf;

    struct Ca {
        params: CertificateParams,
        key: KeyPair,
        root_file: PathBuf,
    }

    impl Drop for Ca {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.root_file);
        }
    }

    fn name(common_name: &str) -> DistinguishedName {
        let mut name = DistinguishedName::new();
        name.push(DnType::CommonName, common_name);
        name
    }

    fn ca(common_name: &str) -> Ca {
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let mut params = CertificateParams::new(Vec::new()).unwrap();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.distinguished_name = name(common_name);
        let key = KeyPair::generate().unwrap();
        let root_file = std::env::temp_dir().join(format!(
            "refhir-tls-test-{}-{}.pem",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ));
        std::fs::write(&root_file, params.self_signed(&key).unwrap().pem()).unwrap();
        Ca {
            params,
            key,
            root_file,
        }
    }

    /// A certificate for `hosts` signed by `ca`, with its key.
    fn issue(ca: &Ca, common_name: &str, hosts: &[&str]) -> (rcgen::Certificate, KeyPair) {
        let mut params =
            CertificateParams::new(hosts.iter().map(|h| h.to_string()).collect::<Vec<_>>())
                .unwrap();
        params.distinguished_name = name(common_name);
        let key = KeyPair::generate().unwrap();
        let issuer = Issuer::from_params(&ca.params, &ca.key);
        (params.signed_by(&key, &issuer).unwrap(), key)
    }

    fn provider() -> Arc<CryptoProvider> {
        Arc::new(rustls::crypto::ring::default_provider())
    }

    /// A server whose certificate `ca` signed for `localhost`; with `client_ca` it also
    /// demands a client certificate signed by that CA.
    fn server(ca: &Ca, client_ca: Option<&Ca>) -> Arc<ServerConfig> {
        let (cert, key) = issue(ca, "server", &["localhost"]);
        let builder = ServerConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()
            .unwrap();
        let builder = match client_ca {
            None => builder.with_no_client_auth(),
            Some(client_ca) => {
                let mut roots = RootCertStore::empty();
                let pem = std::fs::read(&client_ca.root_file).unwrap();
                for root in CertificateDer::pem_slice_iter(&pem) {
                    roots.add(root.unwrap()).unwrap();
                }
                let verifier =
                    WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider())
                        .build()
                        .unwrap();
                builder.with_client_cert_verifier(verifier)
            }
        };
        let mut config = builder
            .with_single_cert(
                vec![cert.der().clone()],
                PrivateKeyDer::try_from(key.serialize_der()).unwrap(),
            )
            .unwrap();
        config.alpn_protocols = vec![b"postgresql".to_vec()];
        Arc::new(config)
    }

    /// Runs a TLS handshake between a client configured by `settings` and `server`, with the
    /// client believing it is talking to `host`. Returns the negotiated ALPN protocol, or what
    /// the client or the server refused.
    fn handshake(
        settings: &TlsSettings,
        server: &Arc<ServerConfig>,
        host: &str,
    ) -> Result<Option<Vec<u8>>, String> {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = server.clone();
        let peer = std::thread::spawn(move || {
            let (mut tcp, _) = listener.accept().unwrap();
            let mut conn = ServerConnection::new(server).unwrap();
            while conn.is_handshaking() {
                conn.complete_io(&mut tcp).map_err(|err| err.to_string())?;
            }
            Ok::<(), String>(())
        });
        let mut tcp = TcpStream::connect(addr).unwrap();
        tcp.set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .unwrap();
        let mut conn = ClientConnection::new(
            Arc::new(settings.client_config()?),
            ServerName::try_from(host.to_string()).unwrap(),
        )
        .unwrap();
        let result = loop {
            if !conn.is_handshaking() {
                break Ok(conn.alpn_protocol().map(<[u8]>::to_vec));
            }
            if let Err(err) = conn.complete_io(&mut tcp) {
                break Err(err.to_string());
            }
        };
        // A server that rejected the client (no certificate, say) says so only after the client
        // thinks it is done, so what the server saw counts too.
        if result.is_ok() {
            conn.writer().write_all(b"x").ok();
            conn.complete_io(&mut tcp).ok();
        }
        drop(tcp);
        let served = peer.join().unwrap();
        result.and_then(|alpn| served.map(|()| alpn))
    }

    fn settings(query: &str, root: Option<&Ca>) -> TlsSettings {
        let query = match root {
            Some(ca) => format!("{query}&sslrootcert={}", ca.root_file.display()),
            None => query.to_string(),
        };
        split(&format!("postgres://localhost/x?{query}")).unwrap().1
    }

    #[test]
    fn verify_full_needs_a_chain_to_the_root_and_the_right_name() {
        let (good, other) = (ca("good CA"), ca("other CA"));
        let srv = server(&good, None);
        let full = settings("sslmode=verify-full", Some(&good));
        assert!(handshake(&full, &srv, "localhost").is_ok());
        let err = handshake(&full, &srv, "127.0.0.1").unwrap_err();
        assert!(err.contains("not valid for name"), "{err}");
        let wrong_root = settings("sslmode=verify-full", Some(&other));
        let err = handshake(&wrong_root, &srv, "localhost").unwrap_err();
        assert!(err.contains("UnknownIssuer"), "{err}");
    }

    #[test]
    fn verify_ca_ignores_the_name_but_never_the_chain() {
        let (good, other) = (ca("good CA"), ca("other CA"));
        let srv = server(&good, None);
        let ca_only = settings("sslmode=verify-ca", Some(&good));
        assert!(handshake(&ca_only, &srv, "localhost").is_ok());
        assert!(handshake(&ca_only, &srv, "127.0.0.1").is_ok());
        let wrong_root = settings("sslmode=verify-ca", Some(&other));
        for host in ["localhost", "127.0.0.1"] {
            let err = handshake(&wrong_root, &srv, host).unwrap_err();
            assert!(err.contains("UnknownIssuer"), "{host}: {err}");
        }
    }

    #[test]
    fn require_checks_the_chain_only_when_given_a_root_certificate() {
        let (good, other) = (ca("good CA"), ca("other CA"));
        let srv = server(&good, None);
        let bare = settings("sslmode=require", None);
        assert!(handshake(&bare, &srv, "127.0.0.1").is_ok());
        assert!(handshake(&settings("sslmode=prefer", None), &srv, "127.0.0.1").is_ok());
        let with_root = settings("sslmode=require", Some(&good));
        assert!(handshake(&with_root, &srv, "127.0.0.1").is_ok());
        let wrong_root = settings("sslmode=require", Some(&other));
        let err = handshake(&wrong_root, &srv, "localhost").unwrap_err();
        assert!(err.contains("UnknownIssuer"), "{err}");
    }

    #[test]
    fn the_postgres_alpn_protocol_is_offered_for_direct_negotiation() {
        let good = ca("good CA");
        let negotiated = handshake(
            &settings("sslmode=verify-full", Some(&good)),
            &server(&good, None),
            "localhost",
        )
        .unwrap();
        assert_eq!(negotiated.as_deref(), Some(&b"postgresql"[..]));
    }

    #[test]
    fn a_client_certificate_is_sent_when_given_and_required_when_the_server_asks() {
        let good = ca("good CA");
        let srv = server(&good, Some(&good));
        let (cert, key) = issue(&good, "refhir_cert", &[]);
        let dir = std::env::temp_dir();
        let (crt, k) = (
            dir.join(format!("refhir-tls-test-{}-client.crt", std::process::id())),
            dir.join(format!("refhir-tls-test-{}-client.key", std::process::id())),
        );
        std::fs::write(&crt, cert.pem()).unwrap();
        std::fs::write(&k, key.serialize_pem()).unwrap();
        let base = "sslmode=verify-full";
        let with_cert = settings(
            &format!("{base}&sslcert={}&sslkey={}", crt.display(), k.display()),
            Some(&good),
        );
        let given = handshake(&with_cert, &srv, "localhost");
        let missing = handshake(&settings(base, Some(&good)), &srv, "localhost");
        let _ = (std::fs::remove_file(&crt), std::fs::remove_file(&k));
        assert!(given.is_ok(), "{given:?}");
        let err = missing.unwrap_err();
        assert!(err.to_lowercase().contains("certificate"), "{err}");
    }

    #[test]
    fn a_rotated_root_certificate_is_picked_up_and_an_unreadable_one_is_not_cached() {
        let good = ca("good CA");
        let tls = settings("sslmode=verify-full", Some(&good));
        assert!(tls.connector().unwrap().is_some());
        // Same settings, but the file now holds garbage and a newer modification time.
        std::fs::write(&good.root_file, "not a certificate\n").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&good.root_file)
            .unwrap()
            .set_modified(SystemTime::now() + std::time::Duration::from_secs(30))
            .unwrap();
        let err = tls.connector().err().unwrap();
        assert!(err.contains("no certificate found"), "{err}");
    }
}
