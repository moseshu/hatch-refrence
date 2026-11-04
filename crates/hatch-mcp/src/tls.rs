//! In-process TLS termination for the MCP HTTP server.
//!
//! rustls 0.23 is already in the workspace dependency tree (the LLM
//! client uses it), so no new dependency is needed: the server builds a
//! `rustls::ServerConfig` from a PEM certificate chain + private key
//! with the ring provider, the same pattern as
//! `hatch-connectors/src/tls.rs` (client side).
//!
//! The server loads the cert/key at bind time; a missing file, an
//! unparsable PEM block, or a key that does not match the certificate
//! is a loud startup error — the server never starts half-secured.
//! `rustls::StreamOwned` then wraps each accepted `TcpStream`; the
//! handshake runs lazily on first read/write and a failed handshake
//! just drops that connection.

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use std::path::Path;
use std::sync::Arc;

/// Build a rustls server config from a PEM certificate chain file and a
/// PEM private-key file (PKCS#8, PKCS#1, or SEC1 — ring accepts all
/// three). The cert file may hold a chain; every `CERTIFICATE` block
/// is used, in file order.
pub fn load_server_config(
    cert_path: &Path,
    key_path: &Path,
) -> Result<Arc<rustls::ServerConfig>, String> {
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(cert_path)
        .map_err(|e| {
            format!(
                "读 TLS 证书 {} 失败：{e}（需要 PEM 格式的证书链）",
                cert_path.display()
            )
        })?
        .collect::<Result<_, _>>()
        .map_err(|e| format!("解析 TLS 证书 {} 失败：{e}", cert_path.display()))?;
    if certs.is_empty() {
        return Err(format!(
            "TLS 证书 {} 里没有 CERTIFICATE 块",
            cert_path.display()
        ));
    }
    let key = PrivateKeyDer::from_pem_file(key_path).map_err(|e| {
        format!(
            "读 TLS 私钥 {} 失败：{e}（需要 PEM 格式的私钥；私钥文件权限建议 0600）",
            key_path.display()
        )
    })?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("TLS 协议版本不受支持：{e}"))?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| format!("TLS 证书与私钥不匹配或无效：{e}"))?;
    Ok(Arc::new(config))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Self-signed `CN=localhost` cert + key, generated only for tests
    /// (`openssl req -x509 -newkey rsa:2048 -nodes -subj /CN=localhost`).
    /// Never used outside tests.
    const TEST_CERT_PEM: &str = include_str!("../tests/fixtures/test-cert.pem");
    const TEST_KEY_PEM: &str = include_str!("../tests/fixtures/test-key.pem");

    fn write_fixture(dir: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
        let cert = dir.join("cert.pem");
        let key = dir.join("key.pem");
        std::fs::write(&cert, TEST_CERT_PEM).unwrap();
        std::fs::write(&key, TEST_KEY_PEM).unwrap();
        (cert, key)
    }

    #[test]
    fn loads_matching_cert_and_key() {
        let dir = tempfile::tempdir().unwrap();
        let (cert, key) = write_fixture(dir.path());
        assert!(load_server_config(&cert, &key).is_ok());
    }

    #[test]
    fn missing_files_and_mismatched_key_are_errors() {
        let dir = tempfile::tempdir().unwrap();
        let (cert, key) = write_fixture(dir.path());
        assert!(load_server_config(&dir.path().join("nope.pem"), &key).is_err());
        assert!(load_server_config(&cert, &dir.path().join("nope.pem")).is_err());
        // A different, valid key does not match the certificate.
        let other_key = dir.path().join("other-key.pem");
        std::fs::write(
            &other_key,
            include_str!("../tests/fixtures/test-other-key.pem"),
        )
        .unwrap();
        assert!(load_server_config(&cert, &other_key).is_err());
        // Garbage PEM is an error, not a panic.
        let bad = dir.path().join("bad.pem");
        std::fs::write(&bad, "not a pem file at all").unwrap();
        assert!(load_server_config(&bad, &key).is_err());
    }
}
