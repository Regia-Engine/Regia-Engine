// SPDX-License-Identifier: MIT-0
//! Package download over HTTPS (`docs/specs/42-product-viewer.md` § 9). Plain `http://` is refused.
//! A LAN server usually has a self-signed certificate: the URL may pin it with a fragment,
//! `https://<lan-host>/products/a1/#sha256=<hex of the certificate's DER>`; the pin replaces
//! the public roots for that download. The TLS handshake signature is still verified.
//!
//! The file's hash is checked against the manifest either way (`crate::store`), so a pin protects
//! which manifest is believed, and the manifest protects the files.

use std::io::Read;
use std::sync::Arc;
use std::time::Duration;

use regia_crash::tls_guard::guarded_tls_connector;
use ureq::rustls;
use ureq::rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use ureq::rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use ureq::rustls::{DigitallySignedStruct, SignatureScheme};

use regia_viewer_pack::safe_path;

use crate::store::{PackageSource, StoreError};

/// How long a connection may take to establish.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a read may stall.
const READ_TIMEOUT: Duration = Duration::from_secs(20);

/// A parsed package URL: the folder, always ending in `/`, and the optional certificate pin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageUrl {
    /// `https://host[:port]/path/`.
    pub base: String,
    /// SHA-256 of the server certificate's DER, when pinned.
    pub pin: Option<[u8; 32]>,
}

impl PackageUrl {
    /// Parses a package folder URL. Only `https://`, no credentials, no query.
    pub fn parse(url: &str) -> Result<Self, StoreError> {
        let invalid = |why: &str| StoreError::new("package_invalid_url", why.to_owned());
        let (rest, fragment) = match url.split_once('#') {
            Some((rest, fragment)) => (rest, Some(fragment)),
            None => (url, None),
        };
        let Some(after) = rest.strip_prefix("https://") else {
            return Err(invalid("only https:// is accepted"));
        };
        let authority = after.split('/').next().unwrap_or("");
        if authority.is_empty() {
            return Err(invalid("no host"));
        }
        if authority.contains('@') {
            return Err(invalid("credentials are not accepted"));
        }
        if rest.contains('?') {
            return Err(invalid("a package folder has no query"));
        }
        if rest.split('/').skip(3).any(|segment| segment == "..") {
            return Err(invalid("no .. segments"));
        }
        let pin = match fragment {
            None | Some("") => None,
            Some(fragment) => {
                let hex = fragment
                    .strip_prefix("sha256=")
                    .ok_or_else(|| invalid("the fragment must be sha256=<hex>"))?;
                Some(parse_sha256(hex).ok_or_else(|| invalid("the pin is not 64 hex digits"))?)
            }
        };
        let mut base = rest.to_owned();
        if !base.ends_with('/') {
            base.push('/');
        }
        Ok(Self { base, pin })
    }
}

/// Parses 64 hex digits.
fn parse_sha256(hex: &str) -> Option<[u8; 32]> {
    let hex: Vec<u8> = hex.bytes().filter(|b| *b != b':').collect();
    if hex.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    let (pairs, _) = hex.as_chunks::<2>();
    for (byte, pair) in out.iter_mut().zip(pairs) {
        let text = std::str::from_utf8(pair).ok()?;
        *byte = u8::from_str_radix(text, 16).ok()?;
    }
    Some(out)
}

/// SHA-256 through the TLS library's own provider (no second hash crate).
fn sha256(bytes: &[u8]) -> Option<[u8; 32]> {
    let suite = rustls::crypto::ring::cipher_suite::TLS13_AES_128_GCM_SHA256;
    let digest = suite.tls13()?.common.hash_provider.hash(bytes);
    digest.as_ref().try_into().ok()
}

/// Accepts exactly one certificate, by the SHA-256 of its DER.
#[derive(Debug)]
struct PinnedCert {
    pin: [u8; 32],
    algorithms: rustls::crypto::WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for PinnedCert {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if sha256(end_entity.as_ref()) == Some(self.pin) {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(
                "server certificate does not match the pin".to_owned(),
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

/// The agent for a download: pinned or public roots, no redirects, no pooled connections, and the
/// guarded connector, so a malformed response head cannot abort the app (ADR-2151).
fn agent(pin: Option<[u8; 32]>) -> Result<ureq::Agent, StoreError> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|err| StoreError::fetch("tls", err))?;
    let config = match pin {
        Some(pin) => builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(PinnedCert {
                pin,
                algorithms: provider.signature_verification_algorithms,
            }))
            .with_no_client_auth(),
        None => builder
            .with_root_certificates(rustls::RootCertStore::from_iter(
                webpki_roots::TLS_SERVER_ROOTS.iter().cloned(),
            ))
            .with_no_client_auth(),
    };
    Ok(ureq::AgentBuilder::new()
        .tls_connector(guarded_tls_connector(Arc::new(config)))
        .timeout_connect(CONNECT_TIMEOUT)
        .timeout_read(READ_TIMEOUT)
        .redirects(0)
        .max_idle_connections(0)
        .build())
}

/// A package folder on an HTTPS server.
pub struct HttpsSource {
    url: PackageUrl,
    agent: ureq::Agent,
}

impl HttpsSource {
    /// A source for `url`.
    pub fn new(url: PackageUrl) -> Result<Self, StoreError> {
        let agent = agent(url.pin)?;
        Ok(Self { url, agent })
    }
}

impl PackageSource for HttpsSource {
    fn get(&mut self, path: &str, limit: u64) -> Result<Vec<u8>, StoreError> {
        if path != regia_viewer_pack::MANIFEST_FILE && !safe_path(path) {
            return Err(StoreError::new("pack_unsafe_path", path.to_owned()));
        }
        let url = format!("{}{path}", self.url.base);
        let response = self
            .agent
            .get(&url)
            .call()
            .map_err(|err| StoreError::fetch(path, err))?;
        if response.status() != 200 {
            return Err(StoreError::fetch(path, format!("status {}", response.status())));
        }
        let mut bytes = Vec::new();
        response
            .into_reader()
            .take(limit.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|err| StoreError::fetch(path, err))?;
        if bytes.len() as u64 > limit {
            return Err(StoreError::new(
                "pack_too_large",
                format!("{path} is over {limit} bytes"),
            ));
        }
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PIN: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";

    fn code(url: &str) -> Option<&'static str> {
        PackageUrl::parse(url).err().map(|e| e.code)
    }

    #[test]
    fn only_https_folders_parse() {
        for bad in [
            "http://host/a/",
            "ftp://host/a/",
            "host/a/",
            "https:///a/",
            "https://me:pw@host/a/",
            "https://host/a/?x=1",
            "https://host/a/../b/",
            "https://host/a/#md5=00",
            "https://host/a/#sha256=00",
        ] {
            assert_eq!(code(bad), Some("package_invalid_url"), "{bad}");
        }
    }

    #[test]
    fn a_url_gets_its_slash_and_its_pin() -> Result<(), StoreError> {
        let url = PackageUrl::parse(&format!("https://10.0.0.5:8443/products/a1#sha256={PIN}"))?;
        assert_eq!(url.base, "https://10.0.0.5:8443/products/a1/");
        assert_eq!(url.pin.map(|p| (p[0], p[31])), Some((0, 31)));
        assert_eq!(PackageUrl::parse("https://host/a/")?.pin, None);
        Ok(())
    }

    #[test]
    fn colons_in_a_pin_are_fine() -> Result<(), StoreError> {
        let spaced: Vec<String> = PIN
            .as_bytes()
            .chunks(2)
            .map(|c| String::from_utf8_lossy(c).into_owned())
            .collect();
        let url = PackageUrl::parse(&format!("https://h/a/#sha256={}", spaced.join(":")))?;
        assert_eq!(url.pin.map(|p| p[1]), Some(1));
        Ok(())
    }

    #[test]
    fn sha256_matches_the_published_vector() {
        // FIPS 180-2 "abc".
        let hex: String = sha256(b"abc")
            .map(|d| d.iter().map(|b| format!("{b:02x}")).collect())
            .unwrap_or_default();
        assert_eq!(
            hex,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
