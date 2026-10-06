#![no_main]

//! Corpo di risposta remoto: il primo byte sceglie il formato, il secondo il
//! delimitatore CSV, il resto è il body così come arriva dalla rete.
//!
//! Proprietà: mai panico; stesso esito a ogni lettura; un rifiuto è
//! `INVALID_RESPONSE` (o `INVALID_INPUT` per un delimitatore che non è un
//! byte ASCII), con testo statico e al più una posizione dentro il body;
//! round-trip dove esiste (JSON e NDJSON riserializzati e riletti uguali,
//! CSV riscritto e riletto con le stesse righe, testo e binario byte per
//! byte); forma documentata dell'XML (una radice, `@attributi`, `#text` non
//! vuoto, figli ripetuti in array, testo senza spazi ai bordi).

use base64::{Engine as _, engine::general_purpose::STANDARD};
use libfuzzer_sys::fuzz_target;
use plenora_rest_core::{EngineError, ResponseConfig, ResponseFormat, fuzzing};
use serde_json::{Map, Value};

#[path = "comune/esiti.rs"]
mod esiti;

const FORMATI: [ResponseFormat; 6] = [
    ResponseFormat::Json,
    ResponseFormat::Ndjson,
    ResponseFormat::Csv,
    ResponseFormat::Xml,
    ResponseFormat::Text,
    ResponseFormat::Binary,
];

fn configurazione(format: ResponseFormat, delimiter: &str) -> ResponseConfig {
    ResponseConfig {
        format,
        delimiter: delimiter.to_owned(),
        ..ResponseConfig::default()
    }
}

/// Un rifiuto confrontabile: codice, testo statico e posizione.
type Rifiuto = (String, &'static str, Option<(u64, u64)>);

/// Esito confrontabile: il valore, o codice, testo e posizione dell'errore.
fn esito(result: &Result<Value, EngineError>) -> Result<Value, Rifiuto> {
    match result {
        Ok(value) => Ok(value.clone()),
        Err(error) => Err((error.payload().code, testo(error), posizione(error))),
    }
}

fn testo(error: &EngineError) -> &'static str {
    match error {
        EngineError::InvalidResponse(detail) | EngineError::InvalidInput(detail) => detail.text(),
        other => panic!("errore inatteso dal parser: {}", other.payload().code),
    }
}

fn posizione(error: &EngineError) -> Option<(u64, u64)> {
    match error {
        EngineError::InvalidResponse(detail) | EngineError::InvalidInput(detail) => {
            detail.position()
        }
        _ => None,
    }
}

fn controlla_rifiuto(error: &EngineError, body: &[u8], configurazione_valida: bool) {
    match error {
        EngineError::InvalidResponse(_) => {
            assert!(
                configurazione_valida,
                "configurazione non valida letta come body non valido"
            );
        }
        EngineError::InvalidInput(_) => {
            assert!(
                !configurazione_valida,
                "INVALID_INPUT con una configurazione valida"
            );
        }
        other => panic!("errore inatteso dal parser: {}", other.payload().code),
    }
    esiti::controlla_errore(error);
    if let Some((line, column)) = posizione(error) {
        // Una posizione localizza senza citare: deve stare dentro il body.
        let lines = body.iter().filter(|byte| **byte == b'\n').count() as u64 + 1;
        assert!(line <= lines, "riga oltre il body");
        assert!(column <= body.len() as u64 + 1, "colonna oltre il body");
    }
}

/// Scrittura RFC 4180 con tutti i campi tra virgolette.
fn scrivi_csv(righe: &[Vec<String>], delimiter: char) -> String {
    let mut out = String::new();
    for riga in righe {
        let campi = riga
            .iter()
            .map(|campo| format!("\"{}\"", campo.replace('"', "\"\"")))
            .collect::<Vec<_>>();
        out.push_str(&campi.join(&delimiter.to_string()));
        out.push('\n');
    }
    out
}

fn controlla_xml(value: &Value, profondita: usize) {
    assert!(profondita <= 130, "XML oltre il limite di annidamento");
    match value {
        Value::String(text) => assert_eq!(text.trim(), text, "testo XML con spazi ai bordi"),
        Value::Object(map) => {
            assert!(!map.is_empty());
            for (key, child) in map {
                if key.starts_with('@') {
                    assert!(child.is_string(), "attributo non stringa");
                } else if key == "#text" {
                    let text = child.as_str().expect("#text stringa");
                    assert!(!text.is_empty() && text.trim() == text);
                } else if let Value::Array(items) = child {
                    assert!(items.len() >= 2, "array di un solo figlio");
                    for item in items {
                        assert!(!item.is_array());
                        controlla_xml(item, profondita + 1);
                    }
                } else {
                    controlla_xml(child, profondita + 1);
                }
            }
        }
        _ => panic!("valore XML né stringa né oggetto"),
    }
}

fuzz_target!(|dati: &[u8]| {
    let [formato, delimitatore, body @ ..] = dati else {
        return;
    };
    let format = FORMATI[usize::from(*formato) % FORMATI.len()];
    // Un byte alto produce un delimitatore di due byte UTF-8, che il motore
    // deve rifiutare come configurazione non valida.
    let delimiter = char::from(*delimitatore).to_string();
    let delimitatore_valido = delimitatore.is_ascii();
    let config = configurazione(format, &delimiter);

    let primo = fuzzing::parse_response_body(body, &config);
    let secondo = fuzzing::parse_response_body(body, &config);
    assert_eq!(esito(&primo), esito(&secondo), "parse non deterministico");

    // Solo il CSV usa il delimitatore, e un delimitatore non valido è sempre
    // rifiutato prima di leggere il body.
    let configurazione_valida = format != ResponseFormat::Csv || delimitatore_valido;
    let value = match primo {
        Ok(value) => {
            assert!(configurazione_valida, "delimitatore non valido accettato");
            value
        }
        Err(error) => {
            controlla_rifiuto(&error, body, configurazione_valida);
            return;
        }
    };

    match format {
        ResponseFormat::Json => {
            let testo = serde_json::to_vec(&value).expect("JSON serializzabile");
            let riletto = fuzzing::parse_response_body(&testo, &config).expect("round-trip JSON");
            assert_eq!(
                riletto, value,
                "il JSON accettato non sopravvive al round-trip"
            );
        }
        ResponseFormat::Ndjson => {
            let Value::Array(records) = &value else {
                panic!("NDJSON non array");
            };
            let righe = records
                .iter()
                .map(|record| serde_json::to_string(record).expect("record"))
                .collect::<Vec<_>>()
                .join("\n");
            let riletto =
                fuzzing::parse_response_body(righe.as_bytes(), &config).expect("round-trip NDJSON");
            assert_eq!(
                riletto, value,
                "l'NDJSON accettato non sopravvive al round-trip"
            );
        }
        ResponseFormat::Csv => {
            let Value::Array(rows) = &value else {
                panic!("CSV non array");
            };
            if let Some(Value::Object(first)) = rows.first() {
                let headers = first.keys().cloned().collect::<Vec<_>>();
                let mut righe = vec![headers.clone()];
                for row in rows {
                    let Value::Object(row) = row else {
                        panic!("riga CSV non oggetto");
                    };
                    assert_eq!(
                        row.keys().collect::<Vec<_>>(),
                        headers.iter().collect::<Vec<_>>()
                    );
                    righe.push(
                        row.values()
                            .map(|cell| cell.as_str().expect("cella stringa").to_owned())
                            .collect(),
                    );
                }
                let delimiter_char = char::from(*delimitatore);
                if delimiter_char != '"' && delimiter_char != '\n' && delimiter_char != '\r' {
                    let testo = scrivi_csv(&righe, delimiter_char);
                    let riletto = fuzzing::parse_response_body(testo.as_bytes(), &config)
                        .expect("round-trip CSV");
                    assert_eq!(
                        riletto, value,
                        "il CSV accettato non sopravvive al round-trip"
                    );
                }
            }
        }
        ResponseFormat::Xml => {
            let Value::Object(root) = &value else {
                panic!("XML non oggetto");
            };
            assert_eq!(root.len(), 1, "XML con più di una radice");
            for child in root.values() {
                controlla_xml(child, 1);
            }
        }
        ResponseFormat::Text => {
            assert_eq!(
                value.as_str().map(str::as_bytes),
                Some(body),
                "testo alterato"
            );
        }
        ResponseFormat::Binary => {
            let dati = STANDARD
                .decode(value["data_base64"].as_str().expect("base64"))
                .expect("base64 valido");
            assert_eq!(dati, body, "binario alterato");
            assert_eq!(value["size"].as_u64(), Some(body.len() as u64));
            assert_eq!(value.as_object().map(Map::len), Some(2));
        }
    }
});
