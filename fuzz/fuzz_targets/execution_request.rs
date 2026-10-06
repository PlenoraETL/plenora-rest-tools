#![no_main]

//! `ExecutionRequest` dal JSON del chiamante, poi `Engine::execute` fino al
//! primo rifiuto, senza rete.
//!
//! Prima di eseguire, ogni URL della richiesta (`url`, `*_url`) diventa un
//! loopback letterale e il rate limit per connessione sparisce
//! (`comune/esiti.rs`, `senza_rete`): l'Engine di default blocca le reti
//! private, quindi nessuna connessione e nessuna risoluzione DNS sono
//! possibili. Il resto della richiesta resta quello generato.
//!
//! Proprietà: mai panico; una richiesta accettata sopravvive al round-trip
//! (serializzata e riletta dà lo stesso JSON); l'esecuzione fallisce sempre
//! prima della rete (nessuna richiesta, risposta, retry o polling contati,
//! nessun codice che esista solo dopo la rete); stesso esito su due Engine
//! nuovi; ogni errore ha messaggio statico, `details` solo numerici e nessuna
//! stringa distintiva della richiesta.

use libfuzzer_sys::fuzz_target;
use plenora_rest_core::{Engine, EngineConfig, ExecutionRequest, ExecutionStatus};
use serde_json::Value;

#[path = "comune/esiti.rs"]
mod esiti;

fuzz_target!(|dati: &[u8]| {
    let Ok(mut json) = serde_json::from_slice::<Value>(dati) else {
        return;
    };
    // Il round-trip si verifica sulla richiesta così come arriva.
    if let Ok(request) = serde_json::from_value::<ExecutionRequest>(json.clone()) {
        let serializzata = serde_json::to_value(&request).expect("richiesta serializzabile");
        let riletta = serde_json::from_value::<ExecutionRequest>(serializzata.clone())
            .expect("una richiesta serializzata deve rileggersi");
        assert_eq!(
            serde_json::to_value(&riletta).expect("richiesta serializzabile"),
            serializzata,
            "round-trip di ExecutionRequest non stabile"
        );
    }

    esiti::senza_rete(&mut json);
    let Ok(request) = serde_json::from_value::<ExecutionRequest>(json.clone()) else {
        return;
    };
    let scadenza = request.options.deadline.is_some();
    let distintive = esiti::stringhe_distintive(&json);

    let esegui = |request: ExecutionRequest| {
        esiti::runtime().block_on(async {
            let engine = Engine::new(EngineConfig::default());
            engine.execute(request).await
        })
    };
    let primo = esegui(request.clone());
    let secondo = esegui(request);

    if esiti::senza_lavoro(&json) && primo.errors.is_empty() {
        assert_eq!(primo.status, ExecutionStatus::Success);
    } else {
        assert_eq!(
            primo.status,
            ExecutionStatus::Failed,
            "esecuzione riuscita senza rete"
        );
        assert!(!primo.errors.is_empty());
    }
    assert!(primo.responses.is_empty(), "una risposta senza rete");
    assert!(primo.recoveries.is_empty());
    let metriche = &primo.metrics;
    assert_eq!(
        (
            metriche.requests,
            metriche.retries,
            metriche.auth_requests,
            metriche.poll_requests
        ),
        (0, 0, 0, 0),
        "richieste di rete contate"
    );
    let errori = serde_json::to_value(&primo.errors).expect("errori serializzabili");
    for errore in errori.as_array().expect("array") {
        esiti::controlla_payload(errore);
        let code = errore["code"].as_str().expect("code");
        assert!(
            !esiti::CODICI_DI_RETE.contains(&code),
            "codice che richiede la rete"
        );
        assert!(
            code != "TIMEOUT" || scadenza,
            "timeout senza scadenza e senza rete"
        );
    }
    esiti::senza_dati(&errori.to_string(), &distintive);

    // Determinismo: stesso stato e stessi errori su un Engine nuovo, salvo
    // una scadenza che può cadere tra le due esecuzioni.
    let errori_secondo = serde_json::to_value(&secondo.errors).expect("errori serializzabili");
    if !scadenza {
        assert_eq!(primo.status, secondo.status);
        assert_eq!(errori, errori_secondo, "esecuzione non deterministica");
    }
});
