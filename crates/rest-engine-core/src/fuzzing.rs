//! Punti d'ingresso per i target di `fuzz/`. **Non** è superficie pubblica.
//!
//! Il modulo esiste solo con la feature `fuzzing`, non di default, ed è
//! `#[doc(hidden)]`: non compare in `contracts/compatibility-v1.json`, non è
//! coperto dalla politica di compatibilità v1 e può cambiare o sparire in
//! qualsiasi versione. Espone ai target di fuzz i parser dell'input remoto che
//! il motore tiene privati, senza aprire connessioni: ogni funzione è un
//! involucro sottile attorno al percorso che il motore usa davvero.

use std::{collections::BTreeMap, time::SystemTime};

use reqwest::{Url, cookie::CookieStore, header::HeaderValue};
use serde_json::Value;

use crate::{EngineError, ResponseConfig};

/// Il body di una risposta letto come lo legge il motore (`ResponseConfig`).
pub fn parse_response_body(body: &[u8], config: &ResponseConfig) -> Result<Value, EngineError> {
    crate::response_body::parse(body, config)
}

/// Il valore che un percorso JSON seleziona, come per cursori, link,
/// mapping e polling.
pub fn json_path_get<'a>(root: &'a Value, path: &str) -> Option<&'a Value> {
    crate::json_path::get(root, path)
}

/// Il target dell'header `Link` con la relazione richiesta (paginazione per
/// header Link).
pub fn link_header_target(header: &str, relation: &str) -> Result<Option<String>, EngineError> {
    let headers = BTreeMap::from([("link".to_owned(), header.to_owned())]);
    crate::engine::link_header_target(&headers, relation)
}

/// L'attesa in millisecondi chiesta da un header `Retry-After`, rispetto a
/// `now`.
pub fn parse_retry_after(value: &str, now: SystemTime) -> Option<u64> {
    crate::transport::parse_retry_after(value, now)
}

/// Inizio, fine e totale di un header `Content-Range` soddisfatto (download
/// ripresi).
pub fn parse_content_range(value: &str) -> Result<(u64, u64, u64), EngineError> {
    crate::transport::parse_content_range(value).map(|range| (range.start, range.end, range.total))
}

/// Il validatore forte di un header `ETag`, se lo è.
pub fn strong_etag(value: &str) -> Option<String> {
    crate::transport::strong_etag(value)
}

/// Le decisioni di cache su un header `Cache-Control`: (`no-store`,
/// `no-cache`, `max-age` in millisecondi).
pub fn cache_control(value: &str) -> (bool, bool, Option<u64>) {
    let headers = BTreeMap::from([("cache-control".to_owned(), value.to_owned())]);
    (
        crate::transport::response_forbids_store(&headers),
        crate::transport::response_requires_revalidation(&headers),
        crate::transport::cache_max_age_ms(&headers),
    )
}

/// Gli header `Set-Cookie` passati a un jar limitato del motore, poi l'header
/// `Cookie` che il jar restituirebbe per `url`. `None` se `url` non è valido;
/// i valori che non sono header HTTP validi vengono scartati, come farebbe il
/// client HTTP prima di arrivare al jar.
pub fn stored_cookies(set_cookie: &[&[u8]], url: &str) -> Option<Option<String>> {
    let url = Url::parse(url).ok()?;
    let headers = set_cookie
        .iter()
        .filter_map(|value| HeaderValue::from_bytes(value).ok())
        .collect::<Vec<_>>();
    let jar = crate::transport::BoundedJar::default();
    jar.set_cookies(&mut headers.iter(), &url);
    Some(
        jar.cookies(&url)
            .map(|value| String::from_utf8_lossy(value.as_bytes()).into_owned()),
    )
}

/// Il limite in byte oltre il quale un `Set-Cookie` viene scartato.
pub const MAX_SET_COOKIE_BYTES: usize = crate::transport::MAX_SET_COOKIE_BYTES;

/// La validazione di un riferimento runtime opaco (`credential_ref`,
/// `artifact_source`, `artifact_sink`).
pub fn validate_runtime_reference(reference: &str) -> Result<(), EngineError> {
    crate::runtime::validate_reference(reference)
}
