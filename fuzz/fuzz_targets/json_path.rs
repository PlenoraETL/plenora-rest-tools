#![no_main]

//! Percorso JSON: la prima riga è il percorso, il resto il documento JSON
//! (una risposta remota). È lo stesso `get` di cursori, link nel body,
//! mapping, iterazioni, batch e polling.
//!
//! Proprietà: mai panico; stesso esito a ogni chiamata; il valore trovato è
//! un nodo del documento (identità di puntatore, mai una copia o un valore
//! costruito); `$` iniziale e spazi ai bordi non cambiano il risultato;
//! `$` e il percorso vuoto selezionano la radice.

use libfuzzer_sys::fuzz_target;
use plenora_rest_core::fuzzing;
use serde_json::Value;

fn nodi<'a>(value: &'a Value, out: &mut Vec<&'a Value>) {
    out.push(value);
    match value {
        Value::Array(items) => items.iter().for_each(|item| nodi(item, out)),
        Value::Object(map) => map.values().for_each(|item| nodi(item, out)),
        _ => {}
    }
}

fuzz_target!(|dati: &[u8]| {
    let Ok(testo) = std::str::from_utf8(dati) else {
        return;
    };
    let (path, documento) = testo.split_once('\n').unwrap_or((testo, "null"));
    let Ok(document) = serde_json::from_str::<Value>(documento) else {
        return;
    };

    let trovato = fuzzing::json_path_get(&document, path);
    assert_eq!(
        trovato,
        fuzzing::json_path_get(&document, path),
        "get non deterministico"
    );

    if let Some(found) = trovato {
        let mut tutti = Vec::new();
        nodi(&document, &mut tutti);
        assert!(
            tutti.iter().any(|node| std::ptr::eq(*node, found)),
            "il valore trovato non è un nodo del documento"
        );
    }

    // `$` iniziale opzionale e spazi ai bordi ignorati.
    let normalizzato = path.trim();
    if !normalizzato.starts_with('$') {
        let con_radice = format!(" ${normalizzato} ");
        assert_eq!(
            fuzzing::json_path_get(&document, &con_radice).map(|value| value as *const Value),
            trovato.map(|value| value as *const Value),
            "il `$` iniziale cambia il risultato"
        );
    }
    for radice in ["", "$", " $ "] {
        assert!(std::ptr::eq(
            fuzzing::json_path_get(&document, radice).expect("radice"),
            &document
        ));
    }
});
