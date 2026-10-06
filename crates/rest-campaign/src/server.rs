//! Server HTTP locale della campagna, in-process.
//!
//! Ogni percorso è uno scenario; il secondo segmento è la chiave
//! dell'operazione, scelta dal driver e unica, così i contatori lato server
//! (richieste, corpi ricevuti, submit, polling, cancellazioni, richieste con
//! `Range`) si leggono per singola operazione e vengono rimossi subito dopo
//! la verifica: la memoria del server non cresce con la durata della campagna.
//!
//! Percorsi (`<k>` è la chiave):
//!
//! | percorso | comportamento |
//! | --- | --- |
//! | `/ok/<k>` | 200 JSON |
//! | `/slow/<k>/<ms>` | 200 JSON dopo `ms` millisecondi |
//! | `/hang/<k>` | nessuna risposta finché il client non chiude |
//! | `/status/<k>/<n>/<code>/<retry_after>` | `code` per le prime `n` richieste, poi 200 |
//! | `/drop/<k>` | chiude la connessione senza rispondere |
//! | `/sink_drop/<k>` | legge tutto il corpo, poi chiude senza rispondere |
//! | `/truncated/<k>` | dichiara 4096 byte, ne invia 128 e chiude |
//! | `/pages/<k>/<mode>/<total>/<size>/<fail_page>/<fail_times>` | paginazione offset, cursor o link |
//! | `/enrich/<k>/<flaky_mod>/<id>` | `{"remote": id}` con ritardo variabile; 503 una volta per gli id multipli di `flaky_mod` |
//! | `/jobs/<k>/submit/<polls>/<fail_poll>` | crea un job (202) |
//! | `/jobs/<k>` | GET: stato del job; DELETE: cancellazione |
//! | `/download/<k>/<size>/<mode>` | contenuto deterministico in streaming, `Range` con ETag forte; mode `plain`, `cut`, `corrupt` |
//! | `/upload/<k>` | conta byte e SHA-256 del corpo |
//! | `/cookie/<k>/set`, `/cookie/<k>/check` | imposta e verifica un cookie |
//! | `/auth/<k>` | 200 solo con il bearer atteso |

use std::{
    collections::{BTreeMap, BTreeSet},
    net::SocketAddr,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::{io::AsyncWriteExt, net::TcpListener, sync::watch, task::JoinHandle};

use crate::{
    CampaignError,
    http::{Connection, ReadOutcome, Request, Response, hex, response_head, write_response},
};

/// Header con cui ogni richiesta della campagna dichiara l'Engine che la
/// invia: i limiti di concorrenza e di rate si verificano per Engine.
pub const ENGINE_LABEL_HEADER: &str = "x-campaign-engine";

const DOWNLOAD_CHUNK: u64 = 64 * 1024;

/// Contatori di una singola operazione.
#[derive(Clone, Debug, Default)]
pub struct KeyCounters {
    /// Richieste ricevute per la chiave (tutte le route).
    pub hits: u64,
    /// Corpi ricevuti per intero da `sink_drop`.
    pub bodies: u64,
    pub submits: u64,
    pub polls: u64,
    pub cancels: u64,
    pub range_requests: u64,
    fails_done: u64,
    seen: BTreeSet<u64>,
    polls_needed: u64,
    fail_poll: u64,
    pub upload_bytes: u64,
    pub upload_sha256: String,
    pub cookie_seen: bool,
}

/// Richieste per Engine viste dal server.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct LabelStats {
    pub requests: u64,
    /// Massimo di richieste contemporaneamente in servizio (dalla lettura
    /// della richiesta all'inizio della scrittura della risposta). Esclude
    /// `/hang`, la cui fine dipende dal client e arriverebbe in ritardo
    /// rispetto al rilascio del permesso del motore.
    pub peak_in_flight: u64,
    /// Massimo di richieste arrivate nella stessa finestra di un secondo
    /// (finestre allineate all'avvio del server).
    pub max_per_second: u64,
    #[serde(skip)]
    in_flight: u64,
    #[serde(skip)]
    current_second: u64,
    #[serde(skip)]
    current_count: u64,
}

pub struct ServerState {
    started: Instant,
    hang_limit: Duration,
    bearer: String,
    keys: Mutex<BTreeMap<String, KeyCounters>>,
    labels: Mutex<BTreeMap<String, LabelStats>>,
    malformed: AtomicU64,
    requests: AtomicU64,
    connections: AtomicU64,
}

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // Un panic con il lock preso è già contato dal gancio dei panic; i
    // contatori restano leggibili.
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl ServerState {
    fn with_key<R>(&self, key: &str, update: impl FnOnce(&mut KeyCounters) -> R) -> R {
        let mut keys = locked(&self.keys);
        update(keys.entry(key.to_owned()).or_default())
    }

    fn arrive(&self, label: &str, tracked: bool) {
        let second = self.started.elapsed().as_secs();
        let mut labels = locked(&self.labels);
        let stats = labels.entry(label.to_owned()).or_default();
        stats.requests += 1;
        if stats.current_second != second {
            stats.current_second = second;
            stats.current_count = 0;
        }
        stats.current_count += 1;
        stats.max_per_second = stats.max_per_second.max(stats.current_count);
        if tracked {
            stats.in_flight += 1;
            stats.peak_in_flight = stats.peak_in_flight.max(stats.in_flight);
        }
    }

    fn depart(&self, label: &str) {
        let mut labels = locked(&self.labels);
        if let Some(stats) = labels.get_mut(label) {
            stats.in_flight = stats.in_flight.saturating_sub(1);
        }
    }
}

pub struct TestServer {
    address: SocketAddr,
    state: Arc<ServerState>,
    stop: watch::Sender<bool>,
    accept: JoinHandle<()>,
}

impl TestServer {
    /// Avvia il server su una porta effimera di 127.0.0.1.
    pub async fn start(bearer: String, hang_limit: Duration) -> Result<Self, CampaignError> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|_| CampaignError::new("avvio del server di prova non riuscito"))?;
        let address = listener
            .local_addr()
            .map_err(|_| CampaignError::new("indirizzo del server di prova non leggibile"))?;
        let state = Arc::new(ServerState {
            started: Instant::now(),
            hang_limit,
            bearer,
            keys: Mutex::new(BTreeMap::new()),
            labels: Mutex::new(BTreeMap::new()),
            malformed: AtomicU64::new(0),
            requests: AtomicU64::new(0),
            connections: AtomicU64::new(0),
        });
        let (stop, mut stopped) = watch::channel(false);
        let accept_state = Arc::clone(&state);
        let accept = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = stopped.changed() => break,
                    accepted = listener.accept() => {
                        let Ok((stream, _)) = accepted else { continue };
                        accept_state.connections.fetch_add(1, Ordering::Relaxed);
                        let state = Arc::clone(&accept_state);
                        tokio::spawn(serve_connection(state, Connection::new(stream)));
                    }
                }
            }
        });
        Ok(Self {
            address,
            state,
            stop,
            accept,
        })
    }

    pub fn base_url(&self) -> String {
        format!("http://{}", self.address)
    }

    pub fn port(&self) -> u16 {
        self.address.port()
    }

    /// Rimuove e restituisce i contatori di una chiave (vuoti se il server non
    /// l'ha mai vista).
    pub fn take(&self, key: &str) -> KeyCounters {
        locked(&self.state.keys).remove(key).unwrap_or_default()
    }

    /// Copia dei contatori di una chiave, senza rimuoverli.
    pub fn peek(&self, key: &str) -> KeyCounters {
        locked(&self.state.keys)
            .get(key)
            .cloned()
            .unwrap_or_default()
    }

    /// Chiavi ancora registrate (operazioni il cui traffico è arrivato dopo
    /// la verifica).
    pub fn pending_keys(&self) -> usize {
        locked(&self.state.keys).len()
    }

    pub fn label_stats(&self) -> BTreeMap<String, LabelStats> {
        locked(&self.state.labels).clone()
    }

    pub fn malformed_connections(&self) -> u64 {
        self.state.malformed.load(Ordering::Relaxed)
    }

    pub fn requests(&self) -> u64 {
        self.state.requests.load(Ordering::Relaxed)
    }

    pub fn connections(&self) -> u64 {
        self.state.connections.load(Ordering::Relaxed)
    }

    /// Smette di accettare connessioni. Le connessioni aperte finiscono da
    /// sole quando il client chiude (o al limite di `/hang`).
    pub fn shutdown(&self) {
        let _ = self.stop.send(true);
        self.accept.abort();
    }
}

/// Seed deterministico del contenuto di una chiave (FNV-1a a 64 bit).
pub fn key_seed(key: &str) -> u64 {
    key.bytes().fold(0xCBF2_9CE4_8422_2325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01B3)
    })
}

/// Byte `index` del contenuto deterministico di `seed`.
pub fn content_byte(seed: u64, index: u64) -> u8 {
    let mixed = (index ^ seed).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    (mixed >> 56) as u8
}

/// `len` byte di contenuto a partire da `start`.
pub fn content_chunk(seed: u64, start: u64, len: u64) -> Vec<u8> {
    (start..start.saturating_add(len))
        .map(|index| content_byte(seed, index))
        .collect()
}

/// SHA-256 esadecimale dei primi `size` byte del contenuto di `seed`.
pub fn content_sha256(seed: u64, size: u64) -> String {
    let mut digest = Sha256::new();
    let mut start = 0;
    while start < size {
        let len = DOWNLOAD_CHUNK.min(size - start);
        digest.update(content_chunk(seed, start, len));
        start += len;
    }
    hex(&digest.finalize())
}

enum Flow {
    KeepOpen,
    Close,
}

async fn serve_connection(state: Arc<ServerState>, mut connection: Connection) {
    loop {
        let request = match connection.read_request().await {
            ReadOutcome::Request(request) => request,
            ReadOutcome::Closed => break,
            ReadOutcome::Malformed => {
                state.malformed.fetch_add(1, Ordering::Relaxed);
                break;
            }
        };
        state.requests.fetch_add(1, Ordering::Relaxed);
        let label = request
            .header(ENGINE_LABEL_HEADER)
            .unwrap_or("senza-etichetta")
            .to_owned();
        let tracked = !request.path.starts_with("/hang/");
        state.arrive(&label, tracked);
        let mut in_flight = InFlight {
            state: &state,
            label: &label,
            active: tracked,
        };
        let flow = route(&state, &mut connection, &mut in_flight, &request).await;
        in_flight.release();
        if matches!(flow, Flow::Close) || request.wants_close() {
            break;
        }
    }
    let _ = connection.stream.shutdown().await;
}

/// Richiesta in servizio per il conteggio della concorrenza. Si rilascia
/// prima di scrivere la risposta: da quel momento il client può completare e
/// liberare il permesso del motore, quindi la finestra contata dal server è
/// sempre contenuta in quella del permesso e il picco osservato non supera il
/// limite per un ritardo di scheduling del server. Un download in streaming
/// conta fino alla testata, non per tutto il corpo.
struct InFlight<'a> {
    state: &'a ServerState,
    label: &'a str,
    active: bool,
}

impl InFlight<'_> {
    fn release(&mut self) {
        if self.active {
            self.active = false;
            self.state.depart(self.label);
        }
    }
}

fn number(segment: Option<&&str>) -> Option<u64> {
    segment.and_then(|value| value.parse().ok())
}

async fn respond(
    connection: &mut Connection,
    in_flight: &mut InFlight<'_>,
    request: &Request,
    response: Response,
) -> Flow {
    in_flight.release();
    let keep_alive = !request.wants_close();
    match write_response(&mut connection.stream, &response, keep_alive).await {
        Ok(()) => Flow::KeepOpen,
        Err(_) => Flow::Close,
    }
}

fn not_found() -> Response {
    Response::json(404, &json!({"error": "unknown route"}))
}

fn injected(status: u16) -> Response {
    Response::json(status, &json!({"error": "injected"}))
}

async fn route(
    state: &ServerState,
    connection: &mut Connection,
    in_flight: &mut InFlight<'_>,
    request: &Request,
) -> Flow {
    let segments: Vec<&str> = request.path.trim_start_matches('/').split('/').collect();
    let (Some(route), Some(key)) = (segments.first().copied(), segments.get(1).copied()) else {
        return respond(connection, in_flight, request, not_found()).await;
    };
    let key = key.to_owned();
    let hits = state.with_key(&key, |counters| {
        counters.hits += 1;
        counters.hits
    });
    match route {
        "ok" => {
            respond(
                connection,
                in_flight,
                request,
                Response::json(200, &json!({"ok": true})),
            )
            .await
        }
        "slow" => {
            let delay = number(segments.get(2)).unwrap_or(0);
            tokio::time::sleep(Duration::from_millis(delay)).await;
            respond(
                connection,
                in_flight,
                request,
                Response::json(200, &json!({"ok": true})),
            )
            .await
        }
        "hang" => {
            connection.wait_for_close(state.hang_limit).await;
            Flow::Close
        }
        "status" => {
            let failures = number(segments.get(2)).unwrap_or(0);
            let status = number(segments.get(3))
                .and_then(|value| u16::try_from(value).ok())
                .unwrap_or(503);
            let retry_after = segments.get(4).copied().unwrap_or("-");
            let response = if hits <= failures {
                let response = injected(status);
                if retry_after == "-" {
                    response
                } else {
                    response.with_header("Retry-After", retry_after.to_owned())
                }
            } else {
                Response::json(200, &json!({"ok": true}))
            };
            respond(connection, in_flight, request, response).await
        }
        "drop" => Flow::Close,
        "sink_drop" => {
            state.with_key(&key, |counters| counters.bodies += 1);
            Flow::Close
        }
        "truncated" => {
            let head = response_head(
                200,
                &[("Content-Type".to_owned(), "application/json".to_owned())],
                4_096,
                false,
            );
            let mut partial = br#"{"payload":""#.to_vec();
            partial.resize(128, b'x');
            in_flight.release();
            let _ = connection.stream.write_all(head.as_bytes()).await;
            let _ = connection.stream.write_all(&partial).await;
            let _ = connection.stream.flush().await;
            Flow::Close
        }
        "pages" => {
            respond(
                connection,
                in_flight,
                request,
                pages(state, &key, &segments, request),
            )
            .await
        }
        "enrich" => {
            let flaky_mod = number(segments.get(2)).unwrap_or(0);
            let Some(id) = number(segments.get(3)) else {
                return respond(connection, in_flight, request, not_found()).await;
            };
            let delay = (id.wrapping_mul(7_919) ^ key_seed(&key)) % 17;
            tokio::time::sleep(Duration::from_millis(delay)).await;
            let fail = flaky_mod > 0
                && id % flaky_mod == 0
                && state.with_key(&key, |counters| counters.seen.insert(id));
            let response = if fail {
                injected(503)
            } else {
                Response::json(200, &json!({"remote": id}))
            };
            respond(connection, in_flight, request, response).await
        }
        "jobs" => {
            respond(
                connection,
                in_flight,
                request,
                jobs(state, &key, &segments, request),
            )
            .await
        }
        "download" => download(state, connection, in_flight, request, &key, &segments, hits).await,
        "upload" => {
            state.with_key(&key, |counters| {
                counters.upload_bytes = request.body_bytes;
                counters.upload_sha256 = request.body_sha256.clone();
            });
            let body = json!({"bytes": request.body_bytes, "sha256": request.body_sha256});
            respond(connection, in_flight, request, Response::json(200, &body)).await
        }
        "cookie" => {
            let response = match segments.get(2).copied() {
                Some("set") => Response::json(200, &json!({"set": true}))
                    .with_header("Set-Cookie", format!("campaign={key}; Path=/; HttpOnly")),
                Some("check") => {
                    let expected = format!("campaign={key}");
                    let seen = request
                        .header("cookie")
                        .is_some_and(|cookies| cookies.split(';').any(|c| c.trim() == expected));
                    state.with_key(&key, |counters| counters.cookie_seen |= seen);
                    Response::json(200, &json!({"has_cookie": seen}))
                }
                _ => not_found(),
            };
            respond(connection, in_flight, request, response).await
        }
        "auth" => {
            let expected = format!("Bearer {}", state.bearer);
            let authorized = request.header("authorization") == Some(expected.as_str());
            let status = if authorized { 200 } else { 401 };
            let response = Response::json(status, &json!({"authorized": authorized}));
            respond(connection, in_flight, request, response).await
        }
        _ => respond(connection, in_flight, request, not_found()).await,
    }
}

fn pages(state: &ServerState, key: &str, segments: &[&str], request: &Request) -> Response {
    let mode = segments.get(2).copied().unwrap_or("");
    let (Some(total), Some(size)) = (number(segments.get(3)), number(segments.get(4))) else {
        return not_found();
    };
    if size == 0 {
        return not_found();
    }
    let fail_page = number(segments.get(5));
    let fail_times = number(segments.get(6)).unwrap_or(0);
    let query_number = |name: &str| request.query.get(name).and_then(|value| value.parse().ok());
    let (start, limit, page) = match mode {
        "offset" => {
            let offset: u64 = query_number("offset").unwrap_or(0);
            let limit: u64 = query_number("limit").unwrap_or(size);
            (offset, limit, offset / size)
        }
        "cursor" => {
            let page = request
                .query
                .get("cursor")
                .and_then(|cursor| cursor.strip_prefix('c'))
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(0);
            (page.saturating_mul(size), size, page)
        }
        "link" => {
            let page: u64 = query_number("page").unwrap_or(0);
            (page.saturating_mul(size), size, page)
        }
        _ => return not_found(),
    };
    if fail_page == Some(page) {
        let fail = state.with_key(key, |counters| {
            if counters.fails_done < fail_times {
                counters.fails_done += 1;
                true
            } else {
                false
            }
        });
        if fail {
            return injected(503);
        }
    }
    let end = start.saturating_add(limit).min(total);
    let items: Vec<serde_json::Value> = (start.min(end)..end).map(|n| json!({"n": n})).collect();
    let more = end < total;
    let next_cursor = more.then(|| format!("c{}", page + 1));
    let next = more.then(|| {
        let host = request.header("host").unwrap_or("127.0.0.1");
        format!(
            "http://{host}/pages/{key}/link/{total}/{size}/{}/{fail_times}?page={}",
            segments.get(5).copied().unwrap_or("-"),
            page + 1
        )
    });
    Response::json(
        200,
        &json!({"items": items, "next_cursor": next_cursor, "next": next}),
    )
}

fn jobs(state: &ServerState, key: &str, segments: &[&str], request: &Request) -> Response {
    if segments.get(2).copied() == Some("submit") && request.method == "POST" {
        let polls_needed = number(segments.get(3)).unwrap_or(1).max(1);
        let fail_poll = number(segments.get(4)).unwrap_or(0);
        state.with_key(key, |counters| {
            counters.submits += 1;
            counters.polls_needed = polls_needed;
            counters.fail_poll = fail_poll;
        });
        return Response::json(202, &json!({"id": key, "status": "queued"}));
    }
    if segments.len() != 2 {
        return not_found();
    }
    match request.method.as_str() {
        "GET" => {
            enum Poll {
                Unknown,
                Fail,
                Running,
                Completed,
            }
            let outcome = state.with_key(key, |counters| {
                if counters.submits == 0 {
                    return Poll::Unknown;
                }
                counters.polls += 1;
                if counters.fail_poll > 0 && counters.polls == counters.fail_poll {
                    return Poll::Fail;
                }
                let failed =
                    u64::from(counters.fail_poll > 0 && counters.polls > counters.fail_poll);
                if counters.polls - failed >= counters.polls_needed {
                    Poll::Completed
                } else {
                    Poll::Running
                }
            });
            match outcome {
                Poll::Unknown => not_found(),
                Poll::Fail => injected(503),
                Poll::Running => Response::json(200, &json!({"status": "running"})),
                Poll::Completed => Response::json(
                    200,
                    &json!({"status": "completed", "result": {"job": key, "answer": 42}}),
                ),
            }
        }
        "DELETE" => {
            state.with_key(key, |counters| counters.cancels += 1);
            Response::json(200, &json!({"cancelled": true}))
        }
        _ => not_found(),
    }
}

#[allow(clippy::too_many_arguments)]
async fn download(
    state: &ServerState,
    connection: &mut Connection,
    in_flight: &mut InFlight<'_>,
    request: &Request,
    key: &str,
    segments: &[&str],
    hits: u64,
) -> Flow {
    let Some(size) = number(segments.get(2)) else {
        return respond(connection, in_flight, request, not_found()).await;
    };
    let mode = segments.get(3).copied().unwrap_or("plain");
    let seed = key_seed(key);
    let etag = format!("\"{key}\"");
    let range_start = request
        .header("range")
        .and_then(|range| range.strip_prefix("bytes="))
        .and_then(|range| range.strip_suffix('-'))
        .and_then(|start| start.parse::<u64>().ok());
    let if_range_ok = request.header("if-range").is_none_or(|value| value == etag);
    let start = match range_start {
        Some(start) if if_range_ok && start < size => {
            state.with_key(key, |counters| counters.range_requests += 1);
            start
        }
        _ => 0,
    };
    let length = size - start;
    let mut headers = vec![
        (
            "Content-Type".to_owned(),
            "application/octet-stream".to_owned(),
        ),
        ("ETag".to_owned(), etag),
        ("Accept-Ranges".to_owned(), "bytes".to_owned()),
    ];
    let status = if start > 0 {
        headers.push((
            "Content-Range".to_owned(),
            format!("bytes {start}-{}/{size}", size.saturating_sub(1)),
        ));
        206
    } else {
        200
    };
    // Il guasto `cut` interrompe soltanto la prima risposta: la ripresa deve
    // poter completare il file.
    let cut_at = (mode == "cut" && hits == 1).then_some(size / 2);
    let corrupt_at = (mode == "corrupt").then_some(size / 2);
    let keep_alive = !request.wants_close() && cut_at.is_none();
    let head = response_head(status, &headers, length, keep_alive);
    in_flight.release();
    if connection.stream.write_all(head.as_bytes()).await.is_err() {
        return Flow::Close;
    }
    let stop = cut_at.unwrap_or(size).max(start);
    let mut position = start;
    while position < stop {
        let len = DOWNLOAD_CHUNK.min(stop - position);
        let mut chunk = content_chunk(seed, position, len);
        if let Some(corrupt) = corrupt_at
            && (position..position + len).contains(&corrupt)
        {
            let offset = usize::try_from(corrupt - position).unwrap_or(0);
            if let Some(byte) = chunk.get_mut(offset) {
                *byte ^= 0xFF;
            }
        }
        if connection.stream.write_all(&chunk).await.is_err() {
            return Flow::Close;
        }
        position += len;
    }
    let _ = connection.stream.flush().await;
    if cut_at.is_some() || !keep_alive {
        Flow::Close
    } else {
        Flow::KeepOpen
    }
}

#[cfg(test)]
mod tests {
    use super::{content_chunk, content_sha256, key_seed};
    use sha2::{Digest, Sha256};

    #[test]
    fn content_is_deterministic_and_hash_matches_chunks() {
        let seed = key_seed("dl-1");
        assert_eq!(seed, key_seed("dl-1"));
        assert_ne!(seed, key_seed("dl-2"));
        let whole = content_chunk(seed, 0, 200_000);
        let digest = crate::http::hex(&Sha256::digest(&whole));
        assert_eq!(content_sha256(seed, 200_000), digest);
        assert_eq!(content_chunk(seed, 100, 10), whole[100..110].to_vec());
    }
}
