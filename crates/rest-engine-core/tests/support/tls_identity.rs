//! Minimal test-only X.509 writer: P-256, one CA, localhost leaf, no input parser.
use base64::{Engine, engine::general_purpose::STANDARD};
use ring::{
    rand::SystemRandom,
    signature::{ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair, KeyPair},
};

pub struct Identity {
    pub ca: String,
    pub pem: String,
}

fn der(tag: u8, contents: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    if contents.len() < 128 {
        out.push(u8::try_from(contents.len()).unwrap());
    } else {
        let bytes = contents.len().to_be_bytes();
        let bytes = &bytes[bytes.iter().position(|b| *b != 0).unwrap()..];
        out.push(0x80 | u8::try_from(bytes.len()).unwrap());
        out.extend(bytes);
    }
    out.extend(contents);
    out
}

fn seq(parts: &[Vec<u8>]) -> Vec<u8> {
    der(0x30, &parts.concat())
}
fn oid(bytes: &[u8]) -> Vec<u8> {
    der(6, bytes)
}
fn bits(bytes: &[u8]) -> Vec<u8> {
    der(3, &[&[0][..], bytes].concat())
}
fn name(value: &[u8]) -> Vec<u8> {
    seq(&[der(0x31, &seq(&[oid(&[85, 4, 3]), der(12, value)]))])
}
fn pem(label: &str, data: &[u8]) -> String {
    format!(
        "-----BEGIN {label}-----\n{}\n-----END {label}-----\n",
        STANDARD.encode(data)
    )
}
fn extension(id: u8, value: Vec<u8>) -> Vec<u8> {
    seq(&[oid(&[85, 29, id]), der(4, &value)])
}
fn certificate(key: &EcdsaKeyPair, signer: &EcdsaKeyPair, ca: bool, rng: &SystemRandom) -> Vec<u8> {
    let algorithm = seq(&[oid(&[42, 134, 72, 206, 61, 4, 3, 2])]);
    let extensions = if ca {
        vec![seq(&[
            oid(&[85, 29, 19]),
            der(1, &[255]),
            der(4, &seq(&[der(1, &[255])])),
        ])]
    } else {
        vec![
            extension(17, seq(&[der(0x82, b"localhost")])),
            extension(
                37,
                seq(&[
                    oid(&[43, 6, 1, 5, 5, 7, 3, 1]),
                    oid(&[43, 6, 1, 5, 5, 7, 3, 2]),
                ]),
            ),
        ]
    };
    let tbs = seq(&[
        der(0xa0, &der(2, &[2])),
        der(2, &[if ca { 1 } else { 2 }]),
        algorithm.clone(),
        name(b"Ephemeral test CA"),
        seq(&[der(0x17, b"200101000000Z"), der(0x18, b"21200101000000Z")]),
        name(if ca {
            b"Ephemeral test CA"
        } else {
            b"localhost"
        }),
        seq(&[
            seq(&[
                oid(&[42, 134, 72, 206, 61, 2, 1]),
                oid(&[42, 134, 72, 206, 61, 3, 1, 7]),
            ]),
            bits(key.public_key().as_ref()),
        ]),
        der(0xa3, &seq(&extensions)),
    ]);
    let signature = signer.sign(rng, &tbs).unwrap();
    seq(&[tbs, algorithm, bits(signature.as_ref())])
}

impl Identity {
    pub fn generate() -> Self {
        let rng = SystemRandom::new();
        let generate = || {
            let pkcs8 =
                EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &rng).unwrap();
            let key =
                EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, pkcs8.as_ref(), &rng)
                    .unwrap();
            (pkcs8, key)
        };
        let (_, ca_key) = generate();
        let (pkcs8, leaf_key) = generate();
        Self {
            ca: pem("CERTIFICATE", &certificate(&ca_key, &ca_key, true, &rng)),
            pem: pem("CERTIFICATE", &certificate(&leaf_key, &ca_key, false, &rng))
                + &pem("PRIVATE KEY", pkcs8.as_ref()),
        }
    }
}
