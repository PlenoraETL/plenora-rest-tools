//! Fixture-backed handshake checks against the production TLS implementation.
// Compile the private implementation here without widening the public API.
#[path = "../src/tls.rs"]
mod tls;

use plenora_rest_core::{EngineError, TlsConfig};
use rustls::{
    RootCertStore,
    pki_types::{CertificateDer, PrivateKeyDer, ServerName, pem::PemObject},
};
use std::{io::Cursor, sync::Arc};
use tls::client_config;

fn server_roots(policy: &TlsConfig) -> Result<RootCertStore, rustls::Error> {
    let mut roots = RootCertStore::empty();
    for cert in CertificateDer::pem_slice_iter(policy.ca_bundle_pem.as_ref().unwrap().as_bytes()) {
        roots.add(cert.unwrap())?;
    }
    Ok(roots)
}

fn server_identity(policy: &TlsConfig) -> (Vec<CertificateDer<'static>>, PrivateKeyDer<'static>) {
    let pem = policy.client_identity_pem.as_ref().unwrap().as_bytes();
    (
        CertificateDer::pem_slice_iter(pem)
            .map(Result::unwrap)
            .collect(),
        PrivateKeyDer::from_pem_slice(pem).unwrap(),
    )
}

const CA: &str = include_str!("fixtures/tls/ca.pem");
// Public test-only identity, never a credential for a deployed service.
const IDENTITY: &str = include_str!("fixtures/tls/identity.pem");

fn handshake(
    policy: &TlsConfig,
    hostname: &'static str,
    mutual: bool,
) -> Result<(), rustls::Error> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .unwrap();
    let builder = if mutual {
        let trust = server_roots(&TlsConfig {
            ca_bundle_pem: Some(CA.into()),
            ..TlsConfig::default()
        })
        .unwrap();
        builder.with_client_cert_verifier(
            rustls::server::WebPkiClientVerifier::builder_with_provider(Arc::new(trust), provider)
                .build()
                .unwrap(),
        )
    } else {
        builder.with_no_client_auth()
    };
    let (certs, key) = server_identity(&TlsConfig {
        client_identity_pem: Some(IDENTITY.into()),
        ..TlsConfig::default()
    });
    let mut server_config = builder.with_single_cert(certs, key).unwrap();
    server_config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    let mut server = rustls::ServerConnection::new(Arc::new(server_config)).unwrap();
    let mut client = rustls::ClientConnection::new(
        Arc::new(client_config(policy).unwrap()),
        ServerName::try_from(hostname).unwrap(),
    )
    .unwrap();
    for _ in 0..10 {
        let mut data = Vec::new();
        client.write_tls(&mut data).unwrap();
        let mut data = Cursor::new(data);
        while server.read_tls(&mut data).unwrap() != 0 {
            server.process_new_packets()?;
        }
        let mut data = Vec::new();
        server.write_tls(&mut data).unwrap();
        let mut data = Cursor::new(data);
        while client.read_tls(&mut data).unwrap() != 0 {
            client.process_new_packets()?;
        }
        if !client.is_handshaking() && !server.is_handshaking() {
            assert_eq!(client.alpn_protocol(), Some(b"h2".as_slice()));
            if mutual {
                assert!(server.peer_certificates().is_some());
            }
            return Ok(());
        }
    }
    panic!("local TLS handshake did not finish");
}

#[test]
fn local_tls_preserves_trust_hostname_bypass_and_mutual_authentication() {
    assert!(handshake(&TlsConfig::default(), "localhost", false).is_err());
    let trusted = TlsConfig {
        ca_bundle_pem: Some(CA.into()),
        ..TlsConfig::default()
    };
    handshake(&trusted, "localhost", false).unwrap();
    assert!(handshake(&trusted, "wrong.example", false).is_err());
    handshake(
        &TlsConfig {
            verify: false,
            ..TlsConfig::default()
        },
        "wrong.example",
        false,
    )
    .unwrap();
    assert!(handshake(&trusted, "localhost", true).is_err());
    handshake(
        &TlsConfig {
            client_identity_pem: Some(IDENTITY.into()),
            ..trusted
        },
        "localhost",
        true,
    )
    .unwrap();
}
