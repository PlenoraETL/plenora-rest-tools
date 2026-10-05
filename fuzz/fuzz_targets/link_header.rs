#![no_main]

//! Header `Link` della paginazione: la prima riga è la relazione cercata, il
//! resto il valore dell'header così come arriva dal servizio remoto.
//!
//! Proprietà: mai panico; stesso esito a ogni chiamata; un rifiuto è
//! `INVALID_RESPONSE` con testo statico, senza posizione e senza byte
//! dell'header; un target trovato è il contenuto esatto di una coppia `<…>`
//! dell'header; la relazione cercata si confronta senza distinguere le
//! maiuscole solo se non è un URI.

use libfuzzer_sys::fuzz_target;
use plenora_rest_core::{EngineError, fuzzing};

#[path = "comune/esiti.rs"]
mod esiti;

fuzz_target!(|dati: &[u8]| {
    let Ok(testo) = std::str::from_utf8(dati) else {
        return;
    };
    let (relation, header) = testo.split_once('\n').unwrap_or(("next", testo));

    let esito = fuzzing::link_header_target(header, relation);
    let ripetuto = fuzzing::link_header_target(header, relation);
    match (&esito, &ripetuto) {
        (Ok(first), Ok(second)) => assert_eq!(first, second, "Link non deterministico"),
        (Err(first), Err(second)) => assert_eq!(first.payload().code, second.payload().code),
        _ => panic!("Link non deterministico"),
    }

    match esito {
        Ok(Some(target)) => {
            assert!(
                header.contains(&format!("<{target}>")),
                "il target non è una coppia <…> dell'header"
            );
            assert!(!target.contains('>'));
            // Una relazione registrata non distingue le maiuscole.
            if !relation.contains(':') {
                assert_eq!(
                    fuzzing::link_header_target(header, &relation.to_ascii_uppercase()).ok(),
                    Some(Some(target.clone()))
                );
            }
        }
        Ok(None) => {}
        Err(error) => {
            let EngineError::InvalidResponse(detail) = &error else {
                panic!("errore inatteso dall'header Link: {}", error.payload().code);
            };
            assert_eq!(detail.position(), None);
            esiti::controlla_errore(&error);
        }
    }
});
