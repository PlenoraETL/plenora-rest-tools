#![no_main]

//! Gli header remoti che il motore interpreta, oltre a `Link`: il primo byte
//! sceglie l'header, il resto è il valore come arriva dal servizio.
//!
//! - `Retry-After`: secondi interi (saturati, mai persi) o data HTTP;
//!   deterministico a parità di istante; un istante successivo non allunga
//!   mai l'attesa.
//! - `Content-Range`: accettato solo se `inizio <= fine < totale`; il valore
//!   canonico riletto dà la stessa terna; un rifiuto è `INVALID_RESPONSE`
//!   senza dati.
//! - `ETag`: il validatore forte è il valore senza spazi ai bordi, tra
//!   virgolette, mai debole.
//! - `Cache-Control`: `no-store`, `no-cache` solo se scritti; `max-age` in
//!   millisecondi interi di secondo.
//! - `Set-Cookie`: il jar limitato memorizza esattamente ciò che memorizza
//!   dopo aver tolto a mano gli header oltre il limite (oracolo del limite).
//!   Il secondo byte fissa una lunghezza minima a cui gli header vengono
//!   allungati, per superare il limite oltre `-max_len`.

use std::time::{Duration, UNIX_EPOCH};

use libfuzzer_sys::fuzz_target;
use plenora_rest_core::{EngineError, fuzzing};

#[path = "comune/esiti.rs"]
mod esiti;

fn secondi_saturati(digits: &str) -> u64 {
    digits
        .bytes()
        .try_fold(0_u64, |total, digit| {
            total
                .checked_mul(10)
                .and_then(|total| total.checked_add(u64::from(digit - b'0')))
        })
        .map_or(u64::MAX, |seconds| seconds.saturating_mul(1_000))
}

fn retry_after(value: &str) {
    let now = UNIX_EPOCH + Duration::from_millis(1_700_000_000_250);
    let later = now + Duration::from_secs(1);
    let attesa = fuzzing::parse_retry_after(value, now);
    assert_eq!(
        attesa,
        fuzzing::parse_retry_after(value, now),
        "Retry-After non deterministico"
    );
    let trimmed = value.trim();
    if !trimmed.is_empty() && trimmed.bytes().all(|byte| byte.is_ascii_digit()) {
        assert_eq!(
            attesa,
            Some(secondi_saturati(trimmed)),
            "secondi letti male"
        );
    }
    let dopo = fuzzing::parse_retry_after(value, later);
    assert_eq!(attesa.is_some(), dopo.is_some());
    assert!(dopo <= attesa, "un istante successivo allunga l'attesa");
}

fn content_range(value: &str) {
    match fuzzing::parse_content_range(value) {
        Ok((start, end, total)) => {
            assert!(
                start <= end && end < total,
                "intervallo incoerente accettato"
            );
            assert_eq!(
                fuzzing::parse_content_range(&format!("bytes {start}-{end}/{total}")).ok(),
                Some((start, end, total))
            );
        }
        Err(error) => {
            assert!(matches!(error, EngineError::InvalidResponse(_)));
            esiti::controlla_errore(&error);
        }
    }
}

fn etag(value: &str) {
    if let Some(validator) = fuzzing::strong_etag(value) {
        assert_eq!(validator, value.trim());
        assert!(validator.len() >= 2 && validator.starts_with('"') && validator.ends_with('"'));
        assert!(!validator.starts_with("W/"));
    }
}

fn cache_control(value: &str) {
    let (no_store, no_cache, max_age) = fuzzing::cache_control(value);
    assert_eq!((no_store, no_cache, max_age), fuzzing::cache_control(value));
    let lower = value.to_ascii_lowercase();
    assert!(!no_store || lower.contains("no-store"));
    assert!(!no_cache || lower.contains("no-cache"));
    if let Some(ms) = max_age {
        assert!(lower.contains("max-age"));
        assert_eq!(ms % 1_000, 0);
    }
}

fn set_cookie(lunghezza: usize, value: &[u8]) {
    let headers = value
        .split(|byte| *byte == b'\n')
        .map(|header| {
            let mut header = header.to_vec();
            if header.len() < lunghezza {
                header.resize(lunghezza, b'a');
            }
            header
        })
        .collect::<Vec<_>>();
    let tutti = headers.iter().map(Vec::as_slice).collect::<Vec<_>>();
    let entro_il_limite = tutti
        .iter()
        .copied()
        .filter(|header| header.len() <= fuzzing::MAX_SET_COOKIE_BYTES)
        .collect::<Vec<_>>();
    for url in ["https://example.com/", "http://example.com/a/b"] {
        let memorizzati = fuzzing::stored_cookies(&tutti, url).expect("URL valido");
        assert_eq!(
            memorizzati,
            fuzzing::stored_cookies(&tutti, url).expect("URL valido")
        );
        assert_eq!(
            memorizzati,
            fuzzing::stored_cookies(&entro_il_limite, url).expect("URL valido"),
            "un Set-Cookie oltre il limite è entrato nel jar"
        );
    }
}

fuzz_target!(|dati: &[u8]| {
    let [selettore, parametro, resto @ ..] = dati else {
        return;
    };
    if selettore % 5 == 4 {
        set_cookie(usize::from(*parametro) * 64, resto);
        return;
    }
    let Ok(value) = std::str::from_utf8(resto) else {
        return;
    };
    match selettore % 5 {
        0 => retry_after(value),
        1 => content_range(value),
        2 => etag(value),
        _ => cache_control(value),
    }
});
