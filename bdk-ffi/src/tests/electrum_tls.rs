//! TALIA FORK: the TLS behaviour of the vendored electrum-client 0.25.0, against a
//! local TLS server on 127.0.0.1 that answers the client's `server.version` request.
//!
//! - validate_domain = true: the server's certificate must chain to the bundled Mozilla roots, so
//!   a self-signed one is refused.
//! - validate_domain = false (personal nodes on the LAN or Tor): the certificate itself is not
//!   checked, but the server's TLS 1.2/1.3 handshake signature still is, against the key in the
//!   certificate it sent. Upstream 0.25.0 accepted any signature there.
//! - TLS 1.2 and 1.3 both work (rustls's safe defaults).
//! - The crypto provider is chosen per connection: connecting installs no process-wide default.
//!
//! `tls/server-cert.pem` is a self-signed certificate for localhost and 127.0.0.1 with the key
//! `tls/server-key.pem`; `tls/other-key.pem` is an unrelated key. All three were generated for
//! these tests only and protect nothing.

use crate::electrum::ElectrumClient;

use bdk_wallet::serde_json;
use rustls::crypto::ring::default_provider;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use rustls::{ServerConfig, ServerConnection, StreamOwned, SupportedProtocolVersion};

use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;

const SERVER_CERT: &[u8] = include_bytes!("tls/server-cert.pem");
const SERVER_KEY: &[u8] = include_bytes!("tls/server-key.pem");
const OTHER_KEY: &[u8] = include_bytes!("tls/other-key.pem");

/// Always presents `tls/server-cert.pem`, signing the handshake with `key`.
#[derive(Debug)]
struct FixedCert(Arc<CertifiedKey>);

impl ResolvesServerCert for FixedCert {
    fn resolve(&self, _: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(Arc::clone(&self.0))
    }
}

fn server_config(
    signing_key_pem: &[u8],
    version: &'static SupportedProtocolVersion,
) -> ServerConfig {
    let provider = Arc::new(default_provider());
    let cert = CertificateDer::from_pem_slice(SERVER_CERT).unwrap();
    let key = provider
        .key_provider
        .load_private_key(PrivateKeyDer::from_pem_slice(signing_key_pem).unwrap())
        .unwrap();
    ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[version])
        .unwrap()
        .with_no_client_auth()
        .with_cert_resolver(Arc::new(FixedCert(Arc::new(CertifiedKey::new(
            vec![cert],
            key,
        )))))
}

/// Accept TLS connections on 127.0.0.1 and answer one Electrum `server.version` request on each.
fn serve(config: ServerConfig) -> u16 {
    let config = Arc::new(config);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for tcp in listener.incoming().flatten() {
            let config = Arc::clone(&config);
            thread::spawn(move || {
                // A refused handshake ends here with an error, which is the point of some tests.
                let _ = answer(config, tcp);
            });
        }
    });
    port
}

fn answer(config: Arc<ServerConfig>, tcp: TcpStream) -> io::Result<()> {
    let connection = ServerConnection::new(config).map_err(io::Error::other)?;
    let mut tls = StreamOwned::new(connection, tcp);
    let mut request = Vec::new();
    let mut byte = [0u8; 1];
    while request.last() != Some(&b'\n') {
        if tls.read(&mut byte)? == 0 {
            return Ok(());
        }
        request.push(byte[0]);
    }
    let request: serde_json::Value = serde_json::from_slice(&request)?;
    let response = serde_json::json!({
        "jsonrpc": "2.0",
        "id": request["id"],
        "result": ["TaliaTest 1.0", "1.4"],
    });
    tls.write_all(format!("{response}\n").as_bytes())?;
    tls.flush()
}

fn connect(url: String, validate_domain: bool) -> Result<ElectrumClient, String> {
    ElectrumClient::new(url, None, Some(10), None, validate_domain).map_err(|e| e.to_string())
}

#[test]
fn test_unvalidated_tls_accepts_a_self_signed_server_on_tls12_and_tls13() {
    for version in [&rustls::version::TLS12, &rustls::version::TLS13] {
        let port = serve(server_config(SERVER_KEY, version));
        let result = connect(format!("ssl://127.0.0.1:{port}"), false);
        assert!(result.is_ok(), "{version:?}: {:?}", result.err());
    }
    // Every connection chose its provider itself.
    assert!(rustls::crypto::CryptoProvider::get_default().is_none());
}

#[test]
fn test_unvalidated_tls_still_checks_the_handshake_signature() {
    // The server sends the right certificate but signs the handshake with another key, as
    // someone in the middle without the server's key would have to.
    for version in [&rustls::version::TLS12, &rustls::version::TLS13] {
        let port = serve(server_config(OTHER_KEY, version));
        let result = connect(format!("ssl://127.0.0.1:{port}"), false);
        let error = result.err().unwrap_or_else(|| {
            panic!(
                "{:?}: a handshake signed with the wrong key was accepted",
                version
            )
        });
        assert!(
            error.to_lowercase().contains("signature"),
            "{:?}: {}",
            version,
            error
        );
    }
}

#[test]
fn test_validated_tls_refuses_a_self_signed_server() {
    let port = serve(server_config(SERVER_KEY, &rustls::version::TLS13));
    let error = connect(format!("ssl://localhost:{port}"), true)
        .err()
        .expect("a self-signed certificate must not pass validation");
    assert!(error.contains("UnknownIssuer"), "{}", error);
}

// validate_domain = true checks a public server's certificate against the root list the vendored
// client really uses. A wrong or empty list would refuse every public server, Talia's default
// sync path, while the self-signed test above still passed; so check the list itself: webpki-roots
// 1.0's copy of Mozilla's (about 150 roots), including ISRG Root X1, the root of Let's Encrypt.
#[test]
fn test_validated_tls_uses_the_bundled_mozilla_roots() {
    let store = bdk_electrum::electrum_client::raw_client::default_root_store();
    assert!(store.roots.len() >= 100, "{} roots", store.roots.len());
    let has_subject = |name: &[u8]| {
        store.roots.iter().any(|root| {
            root.subject
                .as_ref()
                .windows(name.len())
                .any(|window| window == name)
        })
    };
    assert!(has_subject(b"ISRG Root X1"));
    assert!(has_subject(b"Internet Security Research Group"));
}

// The same against a real public Electrum server (needs the internet, so not part of `cargo
// test`): `cargo test --locked -- --ignored public_electrum_server`.
#[test]
#[ignore]
fn test_validated_tls_reaches_a_public_electrum_server() {
    let client = connect("ssl://electrum.blockstream.info:60002".to_string(), true)
        .expect("a public server with a valid certificate");
    assert!(client.ping().is_ok());
}
