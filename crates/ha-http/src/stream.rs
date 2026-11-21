//! A raw outbound TLS stream, for protocols other than HTTP (MQTT): TCP,
//! then TLS with the same compiled-in Mozilla roots as the HTTP client.
//! The TLS settings are built once per process, on first use.

use crate::Error;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::net::TcpStream;

/// A TLS stream over TCP.
pub type TlsStream = tokio_rustls::client::TlsStream<TcpStream>;

fn config() -> Arc<rustls::ClientConfig> {
    static CONFIG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let roots = rustls::RootCertStore {
                roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
            };
            let config = rustls::ClientConfig::builder_with_provider(Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .expect("ring supports the default versions")
            .with_root_certificates(roots)
            .with_no_client_auth();
            Arc::new(config)
        })
        .clone()
}

/// Connect to `host:port` over TCP, within `timeout`.
pub async fn connect_tcp(host: &str, port: u16, timeout: Duration) -> Result<TcpStream, Error> {
    let tcp = tokio::time::timeout(timeout, TcpStream::connect((host, port)))
        .await
        .map_err(|_| Error::Timeout(timeout))?
        .map_err(|e| Error::Request(e.to_string()))?;
    tcp.set_nodelay(true).ok();
    Ok(tcp)
}

/// Connect to `host:port` and complete a TLS handshake for `host`, the
/// whole of it within `timeout`.
pub async fn connect_tls(host: &str, port: u16, timeout: Duration) -> Result<TlsStream, Error> {
    let name = rustls::pki_types::ServerName::try_from(host.to_owned())
        .map_err(|_| Error::Url(host.to_owned()))?;
    let connect = async {
        let tcp = connect_tcp(host, port, timeout).await?;
        tokio_rustls::TlsConnector::from(config())
            .connect(name, tcp)
            .await
            .map_err(|e| Error::Request(e.to_string()))
    };
    tokio::time::timeout(timeout, connect)
        .await
        .map_err(|_| Error::Timeout(timeout))?
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A listener that accepts and then says nothing: the handshake times
    /// out instead of hanging.
    #[tokio::test]
    async fn a_silent_handshake_times_out() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let _held = tokio::spawn(async move {
            let (s, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_secs(5)).await;
            drop(s);
        });
        let r = connect_tls("localhost", port, Duration::from_millis(200)).await;
        assert!(matches!(r, Err(Error::Timeout(_))), "{r:?}");
    }

    #[test]
    fn the_settings_are_made_once() {
        assert!(Arc::ptr_eq(&config(), &config()));
    }
}
