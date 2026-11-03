//! Implicit-TLS client connections (IMAPS 993, SMTPS 465).

use hatch_core::{Error, Result};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

pub type TlsStream = rustls::StreamOwned<rustls::ClientConnection, TcpStream>;

pub fn connect(host: &str, port: u16, timeout: Duration) -> Result<TlsStream> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| Error::Connector(format!("tls config: {e}")))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    let name = rustls::pki_types::ServerName::try_from(host.to_string())
        .map_err(|e| Error::Connector(format!("bad host {host}: {e}")))?;
    let conn = rustls::ClientConnection::new(Arc::new(config), name)
        .map_err(|e| Error::Connector(format!("tls: {e}")))?;
    let addr = (host, port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| Error::Connector(format!("cannot resolve {host}")))?;
    let tcp = TcpStream::connect_timeout(&addr, timeout)?;
    tcp.set_read_timeout(Some(timeout))?;
    tcp.set_write_timeout(Some(timeout))?;
    Ok(rustls::StreamOwned::new(conn, tcp))
}
