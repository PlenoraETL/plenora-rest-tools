#![no_main]

//! `RuntimeBinding::invoke_json` su un messaggio runtime arbitrario: envelope,
//! metadati, payload `ExecutionRequest`, risoluzione dei riferimenti, poi
//! l'esecuzione fino al primo rifiuto, senza rete.
//!
//! Se l'input è un `RuntimeMessage`, il payload passa per `senza_rete` (ogni
//! URL diventa un loopback letterale, che l'Engine di default rifiuta prima di
//! connettersi) e il messaggio viene riscritto; altrimenti il testo arriva
//! così com'è. Le risorse dell'host risolvono le credenziali in nessuna
//! autenticazione e rifiutano ogni artifact.
//!
//! Proprietà: mai panico; un testo che non è un `RuntimeMessage` è
//! `INVALID_INPUT` con una posizione dentro il testo; ogni altra risposta è
//! un envelope d'errore (mai un successo senza rete) con payload pubblico
//! senza dati della richiesta, correlazione e causazione ricopiate dalla
//! richiesta; stesso esito su due Engine nuovi, a parte l'identificativo
//! casuale del messaggio di risposta.

use std::path::PathBuf;

use libfuzzer_sys::fuzz_target;
use plenora_rest_core::{
    AuthConfig, CancellationToken, ERROR_CONTENT_TYPE, Engine, EngineConfig, EngineError,
    RuntimeBinding, RuntimeMessage, RuntimeMessageKind, RuntimeResources,
};
#[path = "comune/esiti.rs"]
mod esiti;

struct Risorse;

impl RuntimeResources for Risorse {
    fn resolve_credentials(&self, _reference: &str) -> Result<AuthConfig, EngineError> {
        Ok(AuthConfig::None)
    }

    fn resolve_artifact_source(&self, _reference: &str) -> Result<PathBuf, EngineError> {
        Err(EngineError::PolicyViolation(
            "artifact non autorizzato".into(),
        ))
    }

    fn resolve_artifact_sink(&self, _reference: &str) -> Result<PathBuf, EngineError> {
        Err(EngineError::PolicyViolation(
            "artifact non autorizzato".into(),
        ))
    }
}

fn invoca(testo: &str) -> Result<String, EngineError> {
    esiti::runtime().block_on(async {
        let engine = Engine::new(EngineConfig::default());
        RuntimeBinding::new(&engine, &Risorse)
            .invoke_json(testo, CancellationToken::new())
            .await
    })
}

fuzz_target!(|dati: &[u8]| {
    let Ok(testo) = std::str::from_utf8(dati) else {
        return;
    };
    let (testo, richiesta) = match serde_json::from_str::<RuntimeMessage>(testo) {
        Ok(mut message) => {
            esiti::senza_rete(&mut message.payload);
            let riscritto = serde_json::to_string(&message).expect("messaggio serializzabile");
            (riscritto, Some(message))
        }
        Err(_) => (testo.to_owned(), None),
    };

    let primo = invoca(&testo);
    let Some(richiesta) = richiesta else {
        let Err(error) = primo else {
            panic!("un testo che non è un RuntimeMessage è stato accettato");
        };
        let EngineError::InvalidInput(detail) = &error else {
            panic!("errore inatteso: {}", error.payload().code);
        };
        if let Some((line, _)) = detail.position() {
            assert!(
                line <= testo.lines().count() as u64 + 1,
                "riga oltre il testo"
            );
        }
        esiti::controlla_errore(&error);
        return;
    };
    let risposta = primo.expect("un RuntimeMessage riceve sempre un envelope");
    let message =
        serde_json::from_str::<RuntimeMessage>(&risposta).expect("la risposta è un RuntimeMessage");
    if message.kind == RuntimeMessageKind::Success {
        assert!(
            esiti::senza_lavoro(&richiesta.payload),
            "successo senza rete"
        );
        assert!(
            message.payload["errors"]
                .as_array()
                .is_some_and(Vec::is_empty)
        );
    } else {
        assert_eq!(message.kind, RuntimeMessageKind::Error);
        assert_eq!(message.content_type, ERROR_CONTENT_TYPE);
        esiti::controlla_payload(&message.payload);
        let code = message.payload["code"].as_str().expect("code");
        assert!(
            !esiti::CODICI_DI_RETE.contains(&code),
            "codice che richiede la rete"
        );
        assert!(
            message
                .payload
                .get("details")
                .and_then(|details| details.get("async_jobs"))
                .is_none()
        );
        // Solo l'envelope d'errore: il payload di un successo è il
        // risultato, che contiene legittimamente dati e nomi di campo.
        esiti::senza_dati(
            &message.payload.to_string(),
            &esiti::stringhe_distintive(&serde_json::to_value(&richiesta).expect("richiesta")),
        );
    }
    // Identità del risultato (Runtime Binding 1.0 RT-019 e RT-020, proposti
    // in plenora-contracts #21): correlazione e operazione sono copiate byte
    // per byte solo se canoniche, altrimenti omesse; mai inventate. La
    // causazione è l'identificativo della richiesta, solo se canonico.
    let correlazione = richiesta.metadata.get("plenora.trace.correlation_id");
    assert_eq!(
        message.metadata.get("plenora.trace.correlation_id"),
        correlazione.filter(|valore| uuid_canonico(valore))
    );
    let operazione = richiesta.metadata.get("plenora.capability.operation");
    assert_eq!(
        message.metadata.get("plenora.capability.operation"),
        operazione.filter(|valore| operazione_canonica(valore))
    );
    let id_richiesta = richiesta.metadata.get("plenora.message.id");
    assert_eq!(
        message.metadata.get("plenora.message.causation_id"),
        id_richiesta.filter(|valore| uuid_canonico(valore))
    );
    // L'identificativo del risultato è sempre nuovo e canonico.
    let id = message
        .metadata
        .get("plenora.message.id")
        .expect("identificativo generato");
    assert!(uuid_canonico(id), "identificativo generato non canonico");
    assert_ne!(Some(id), id_richiesta);

    // Determinismo, a parte gli identificativi casuali e una scadenza che
    // può cadere tra le due invocazioni.
    let scadenza = richiesta
        .metadata
        .contains_key("plenora.execution.deadline")
        || richiesta
            .payload
            .pointer("/options/deadline")
            .is_some_and(|value| !value.is_null());
    let secondo = serde_json::from_str::<RuntimeMessage>(&invoca(&testo).expect("envelope"))
        .expect("la risposta è un RuntimeMessage");
    if !scadenza {
        let senza_id = |message: &RuntimeMessage| {
            let mut metadata = message.metadata.clone();
            for chiave in id_casuali {
                metadata.remove(*chiave);
            }
            (metadata, message.payload.clone())
        };
        assert_eq!(
            senza_id(&message),
            senza_id(&secondo),
            "invocazione non deterministica"
        );
    }
});

/// UUID in forma canonica: 36 caratteri, trattini nelle posizioni 8, 13, 18
/// e 23, cifre esadecimali minuscole altrove.
fn uuid_canonico(valore: &str) -> bool {
    valore.len() == 36
        && valore.bytes().enumerate().all(|(indice, byte)| {
            if matches!(indice, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
            }
        })
}

/// `^[a-z][a-z0-9_-]*(\.[a-z][a-z0-9_-]*)+$`.
fn operazione_canonica(valore: &str) -> bool {
    let segmenti = valore.split('.').collect::<Vec<_>>();
    segmenti.len() >= 2
        && segmenti.iter().all(|segmento| {
            let mut byte = segmento.bytes();
            byte.next().is_some_and(|primo| primo.is_ascii_lowercase())
                && byte.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
        })
}
