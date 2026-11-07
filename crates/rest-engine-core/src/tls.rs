//! Per-client TLS policy, independent of process-wide rustls defaults.

use std::{io::Cursor, sync::Arc};

use rustls::{
    ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime, pem::SectionKind},
};

use crate::{EngineError, TlsConfig};

fn build_error() -> EngineError {
    EngineError::Runtime("HTTP client could not be built".into())
}

fn identity_error() -> EngineError {
    EngineError::InvalidInput("invalid TLS client identity PEM".into())
}

fn roots(policy: &TlsConfig) -> Result<RootCertStore, EngineError> {
    use rustls::pki_types::pem::PemObject;

    let mut roots = RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    if let Some(pem) = &policy.ca_bundle_pem {
        // As in reqwest 0.12, all PEM certificates supplement Mozilla roots.
        // Decoding and validation errors are hidden behind the static build error.
        for cert in CertificateDer::pem_slice_iter(pem.as_bytes()) {
            roots
                .add(cert.map_err(|_| build_error())?)
                .map_err(|_| build_error())?;
        }
    }
    Ok(roots)
}

type ClientIdentity = (Vec<CertificateDer<'static>>, PrivateKeyDer<'static>);

fn identity(policy: &TlsConfig) -> Result<Option<ClientIdentity>, EngineError> {
    if let Some(pem) = &policy.client_identity_pem {
        let mut reader = Cursor::new(pem.as_bytes());
        let mut certificates = Vec::new();
        let mut key = None;
        while let Some((kind, data)) =
            rustls::pki_types::pem::from_buf(&mut reader).map_err(|_| identity_error())?
        {
            match kind {
                SectionKind::Certificate => certificates.push(CertificateDer::from(data)),
                SectionKind::PrivateKey => key = Some(PrivateKeyDer::Pkcs8(data.into())),
                SectionKind::RsaPrivateKey => key = Some(PrivateKeyDer::Pkcs1(data.into())),
                SectionKind::EcPrivateKey => key = Some(PrivateKeyDer::Sec1(data.into())),
                _ => return Err(identity_error()),
            }
        }
        if certificates.is_empty() {
            return Err(identity_error());
        }
        Ok(Some((certificates, key.ok_or_else(identity_error)?)))
    } else {
        Ok(None)
    }
}

pub(crate) fn client_config(policy: &TlsConfig) -> Result<ClientConfig, EngineError> {
    let identity = identity(policy)?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|_| build_error())?;
    let roots = roots(policy)?;
    let builder = if policy.verify {
        builder.with_root_certificates(roots)
    } else {
        // Preserve the explicit verify=false policy, including disabled signature
        // checks. No verifier is installed globally or used by verified clients.
        builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerification))
    };
    let mut config = if let Some((certificates, key)) = identity {
        builder
            .with_client_auth_cert(certificates, key)
            .map_err(|_| build_error())?
    } else {
        builder.with_no_client_auth()
    };
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(config)
}

#[derive(Debug)]
struct NoVerification;

impl ServerCertVerifier for NoVerification {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        // Match reqwest's verify=false advertisement, including legacy schemes.
        vec![
            SignatureScheme::RSA_PKCS1_SHA1,
            SignatureScheme::ECDSA_SHA1_Legacy,
            SignatureScheme::RSA_PKCS1_SHA256,
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::RSA_PKCS1_SHA384,
            SignatureScheme::ECDSA_NISTP384_SHA384,
            SignatureScheme::RSA_PKCS1_SHA512,
            SignatureScheme::ECDSA_NISTP521_SHA512,
            SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::RSA_PSS_SHA384,
            SignatureScheme::RSA_PSS_SHA512,
            SignatureScheme::ED25519,
            SignatureScheme::ED448,
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_roots_and_protocols_are_explicit() {
        let policy = TlsConfig::default();
        let config = client_config(&policy).unwrap();
        let ring = rustls::crypto::ring::default_provider();
        assert!(std::ptr::eq(
            config.crypto_provider().secure_random,
            ring.secure_random
        ));
        assert!(std::ptr::eq(
            config.crypto_provider().key_provider,
            ring.key_provider
        ));
        assert_eq!(
            roots(&policy).unwrap().roots,
            webpki_roots::TLS_SERVER_ROOTS
        );
        assert_eq!(
            config.alpn_protocols,
            [b"h2".to_vec(), b"http/1.1".to_vec()]
        );
        reqwest::Client::builder()
            .tls_backend_preconfigured(config)
            .build()
            .unwrap();
    }

    #[test]
    fn malformed_tls_input_keeps_static_errors() {
        let policy = TlsConfig {
            client_identity_pem: Some("PRIVATE_CANARY".into()),
            ..TlsConfig::default()
        };
        assert_eq!(
            client_config(&policy).unwrap_err().to_string(),
            identity_error().to_string()
        );
        let policy = TlsConfig {
            ca_bundle_pem: Some(
                "-----BEGIN CERTIFICATE-----\nPRIVATE_CANARY\n-----END CERTIFICATE-----".into(),
            ),
            ..TlsConfig::default()
        };
        for verify in [true, false] {
            let policy = TlsConfig {
                verify,
                ..policy.clone()
            };
            assert_eq!(
                client_config(&policy).unwrap_err().to_string(),
                build_error().to_string()
            );
        }
        let policy = TlsConfig {
            client_identity_pem: Some("PRIVATE_CANARY".into()),
            ..policy
        };
        assert_eq!(
            client_config(&policy).unwrap_err().to_string(),
            identity_error().to_string()
        );
    }
}
