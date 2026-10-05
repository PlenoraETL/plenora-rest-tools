//! Controlli comuni ai target: errori senza dati e confine di rete.
//!
//! Incluso con `#[path]` da ogni target che lo usa; non tutti i target usano
//! ogni funzione.
#![allow(dead_code)]

use std::sync::OnceLock;

use plenora_rest_core::EngineError;
use serde_json::Value;

/// Le sole chiavi che `details` di un errore pubblico può avere: numeri
/// scelti dal motore o dal protocollo, mai testo.
const DETTAGLI_AMMESSI: [&str; 5] = [
    "received_version",
    "supported_version",
    "limit_bytes",
    "http_status",
    "poll_attempts",
];

/// Codici che esistono solo dopo aver parlato con la rete: in un target che
/// non deve mai aprire connessioni sono un buco dell'imbracatura.
pub const CODICI_DI_RETE: [&str; 6] = [
    "DNS_RESOLUTION_FAILED",
    "TRANSPORT_ERROR",
    "HTTP_STATUS",
    "INVALID_RESPONSE",
    "APPLICATION_ERROR",
    "CIRCUIT_OPEN",
];

/// URL che la validazione del motore rifiuta prima di qualsiasi
/// connessione: loopback letterale (nessuna risoluzione DNS) con le reti
/// private bloccate dalla configurazione di default.
pub const URL_SENZA_RETE: &str = "http://127.0.0.1:9/r/{id}";
pub const URL_AUSILIARIA_SENZA_RETE: &str = "http://127.0.0.1:9/token";

/// Un errore pubblico: `Display` è il messaggio statico del payload e il
/// payload rispetta [`controlla_payload`].
pub fn controlla_errore(error: &EngineError) {
    let payload = error.payload();
    assert_eq!(error.to_string(), payload.message);
    controlla_payload(&serde_json::to_value(&payload).expect("payload serializzabile"));
}

/// Un payload d'errore serializzato (`ErrorPayload` o `ExecutionError`):
/// codice e messaggio presenti, `details` solo con chiavi numeriche note.
pub fn controlla_payload(payload: &Value) {
    let code = payload["code"].as_str().expect("code");
    assert!(
        !code.is_empty()
            && code
                .bytes()
                .all(|byte| byte.is_ascii_uppercase() || byte == b'_')
    );
    assert!(!payload["message"].as_str().expect("message").is_empty());
    for (key, value) in payload["details"].as_object().expect("details") {
        assert!(
            DETTAGLI_AMMESSI.contains(&key.as_str()),
            "dettaglio inatteso"
        );
        assert!(value.is_u64(), "dettaglio non numerico");
    }
}

/// Il testo che il motore produce da sé in un errore pubblico: messaggi,
/// codici, nomi dei campi e valori delle enumerazioni di ogni variante. Una
/// stringa dell'input che vi compare non prova una fuga di dati.
fn vocabolario() -> &'static str {
    static VOCABOLARIO: OnceLock<String> = OnceLock::new();
    VOCABOLARIO.get_or_init(|| {
        let errori = [
            EngineError::InvalidInput("x".into()),
            EngineError::UnsupportedSchema {
                received: 0,
                supported: 0,
            },
            EngineError::InvalidUrl("x".into()),
            EngineError::UnsafeAddress("x".into()),
            EngineError::PolicyViolation("x".into()),
            EngineError::DnsResolution("x".into()),
            EngineError::InvalidHeader("x".into()),
            EngineError::Timeout,
            EngineError::Cancelled,
            EngineError::EngineClosed,
            EngineError::CircuitOpen,
            EngineError::Transport("x".into()),
            EngineError::ResponseTooLarge { limit_bytes: 0 },
            EngineError::RequestTooLarge { limit_bytes: 0 },
            EngineError::FileTooLarge { limit_bytes: 0 },
            EngineError::FileIo("x".into()),
            EngineError::ChecksumMismatch,
            EngineError::HttpStatus { status: 0 },
            EngineError::InvalidResponse("x".into()),
            EngineError::Application("x".into()),
            EngineError::MissingParameter("x".into()),
            EngineError::Authentication("x".into()),
            EngineError::IdempotencyConflict,
            EngineError::PollingTimeout { attempts: 0 },
            EngineError::Runtime("x".into()),
        ];
        let mut testo = String::from("input_index");
        for errore in errori {
            testo.push('\n');
            testo.push_str(&serde_json::to_string(&errore.payload()).expect("payload"));
        }
        testo
    })
}

/// Le stringhe dell'input (chiavi e valori) abbastanza lunghe da non
/// comparire per caso in un messaggio, e assenti dal vocabolario del motore.
pub fn stringhe_distintive(value: &Value) -> Vec<String> {
    fn aggiungi(text: &str, out: &mut Vec<String>) {
        if text.chars().count() >= 12 && !vocabolario().contains(text) {
            out.push(text.to_owned());
        }
    }
    fn raccogli(value: &Value, out: &mut Vec<String>) {
        match value {
            Value::String(text) => aggiungi(text, out),
            Value::Array(items) => items.iter().for_each(|item| raccogli(item, out)),
            Value::Object(map) => {
                for (key, item) in map {
                    aggiungi(key, out);
                    raccogli(item, out);
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    raccogli(value, &mut out);
    out
}

/// Nessuna stringa distintiva dell'input compare nel testo d'errore reso.
pub fn senza_dati(reso: &str, distintive: &[String]) {
    for stringa in distintive {
        // Confronto sul testo JSON: la stringa va cercata come il
        // serializzatore la scriverebbe.
        let serializzata = serde_json::to_string(stringa).expect("stringa");
        let interna = &serializzata[1..serializzata.len() - 1];
        assert!(
            !reso.contains(interna),
            "un errore pubblico riporta testo dell'input"
        );
    }
}

/// Rende innocua per la rete una richiesta JSON: ogni URL (`url`, `*_url`)
/// diventa un loopback letterale che la validazione rifiuta, e il rate
/// limit per connessione sparisce (un valore minuscolo farebbe solo
/// aspettare il fuzzer). Tutto il resto resta com'è.
pub fn senza_rete(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.remove("requests_per_second");
            for (key, child) in map.iter_mut() {
                if child.is_string() && key == "url" {
                    *child = Value::String(URL_SENZA_RETE.to_owned());
                } else if child.is_string() && key.ends_with("_url") {
                    *child = Value::String(URL_AUSILIARIA_SENZA_RETE.to_owned());
                } else {
                    senza_rete(child);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(senza_rete),
        _ => {}
    }
}

/// Una richiesta che non chiede alcuna richiesta HTTP: un enrich senza
/// record (anche in batch) termina con successo senza rete, ed è corretto.
pub fn senza_lavoro(request: &Value) -> bool {
    request["operation"] == "enrich"
        && request
            .pointer("/input/records")
            .and_then(Value::as_array)
            .is_none_or(Vec::is_empty)
}

/// Il runtime condiviso dai target asincroni: un solo thread, orologio
/// reale; nessuna attesa è raggiungibile senza rete.
pub fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime tokio")
    })
}
