#[cfg(test)]
use admission::is_public_address;
#[cfg(any(test, feature = "fuzzing"))]
pub(crate) use artifacts::parse_content_range;
#[cfg(feature = "fuzzing")]
pub(crate) use artifacts::strong_etag;
#[cfg(feature = "fuzzing")]
pub(crate) use cache::{cache_max_age_ms, response_forbids_store, response_requires_revalidation};
#[cfg(any(test, feature = "fuzzing"))]
pub(crate) use sessions::BoundedJar;
#[cfg(test)]
use sessions::{SessionRefusal, SlotState};
mod artifacts;
use artifacts::{multipart_form, stream_body};
mod sessions;
use sessions::{CookieJar, SessionRegistry};
mod circuit;
use circuit::{CircuitKey, CircuitState};
mod cache;
use cache::{CacheStore, fingerprint};
mod admission;
use admission::{apply_query_auth, method, request_headers, set_request_header};
mod auth;

mod retry;
use retry::{AttemptOutcome, RetryDecision, RetrySession, is_retryable_transport_error};
#[cfg(test)]
use retry::{backoff_for, retry_delay, retry_wait};

use crate::error::{ExecutionEvent, FailureContext};
use std::{
    collections::{BTreeMap, HashMap},
    hash::{Hash, Hasher},
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    num::IntErrorKind,
    path::{Path, PathBuf},
    sync::Arc,
    sync::atomic::{AtomicU64, AtomicUsize, Ordering},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use crate::error::ErrorDetail;
#[cfg(test)]
use crate::injection::sleep;
use futures_util::StreamExt;
use reqwest::{
    Body, Client, Method, Proxy, Url,
    cookie::{CookieStore, Jar},
    header::{
        ACCEPT_ENCODING, CACHE_CONTROL, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, ETAG,
        HeaderMap, HeaderName, HeaderValue, IF_MODIFIED_SINCE, IF_NONE_MATCH, IF_RANGE,
        LAST_MODIFIED, LOCATION, RANGE, RETRY_AFTER, VARY,
    },
    multipart::{Form, Part},
    redirect::Policy,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
#[cfg(not(test))]
use tokio::time::sleep;
use tokio::{
    fs::{self, OpenOptions},
    io::AsyncWriteExt,
    net::lookup_host,
    sync::{Mutex, OwnedSemaphorePermit, Semaphore},
};
use tokio_util::io::ReaderStream;
use url::Host;

use crate::{
    ApiKeyLocation, AuthConfig, CachePolicy, CircuitBreakerPolicy, CookiePolicy, CookieSession,
    EngineConfig, EngineError, ErrorPhase, HttpMethod, OAuthClientAuth, ProxyConfig, RetryPolicy,
    TlsConfig,
};

static DOWNLOAD_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Requests sent and retries started by one execution, counted at the moment
/// they happen.
///
/// The metrics of a result are otherwise assembled from the responses that
/// came back, so a failure (transport error, timeout, deadline, cancellation)
/// would report zero requests while the remote side had received every
/// attempt. The tally survives the failure: `Engine::execute_with_control`
/// scopes it around the execution and reads it for the result.
///
/// The same count says whether a failure may have had a remote effect
/// (Typed Errors 1.0, ERR-014): once a request of the operation went out,
/// whatever its method or its answer, an error cannot claim that nothing
/// happened remotely. The event reducer retains that fact per input record, for the
/// operations whose errors belong to one record (enrich): a send is observed
/// for every record of the [`RECORD_SCOPE`] it was sent in.
///
/// `phases` holds the reduced events and the last public phase (prepare,
/// connect, read, write; ERR-003), for the execution and for each record of
/// the scope. It gives the phase of an error whose variant says `validate`,
/// "before any network activity", but that is raised after a request was
/// sent: the circuit breaker or the cache refusing the request after an
/// OAuth token was obtained (connect), a multipart body that cannot be built
/// (prepare), a redirect refused while reading a response (read).
///
/// Inside a [`RECORD_SCOPE`] only the phases of the scope's records change;
/// the phase of the execution changes only on paths outside any record. So
/// concurrent records never overwrite each other's phase or the execution's,
/// and no phase depends on the order in which records reach the transport.
#[derive(Default)]
pub(crate) struct ExecutionTally {
    pub(crate) requests: AtomicU64,
    pub(crate) retries: AtomicU64,
    phases: std::sync::Mutex<PhaseTrack>,
}

#[derive(Default)]
struct PhaseTrack {
    operation: FailureContext,
    records: BTreeMap<usize, FailureContext>,
}

impl ExecutionTally {
    pub(crate) fn failure_context(&self, index: Option<usize>) -> FailureContext {
        let track = self
            .phases
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match index {
            None => track.operation,
            Some(index) => track.records.get(&index).copied().unwrap_or_default(),
        }
    }

    #[cfg(test)]
    pub(crate) fn phase(&self, index: Option<usize>) -> Option<ErrorPhase> {
        self.failure_context(index).entered
    }

    fn observe(&self, event: ExecutionEvent) {
        let mut track = self
            .phases
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let in_records = RECORD_SCOPE.try_with(|indices| {
            for index in indices.iter() {
                track.records.entry(*index).or_default().observe(event);
            }
        });
        // Sends count for the whole operation even inside a record; phases
        // stay local to their scope, independent of record scheduling.
        if in_records.is_err() || matches!(event, ExecutionEvent::RequestSent) {
            track.operation.observe(event);
        }
    }
}

/// Records that the transport enters the public `phase`: for every record of
/// the current [`RECORD_SCOPE`], or for the execution outside any record.
pub(crate) fn enter_phase(phase: ErrorPhase) {
    tally(|tally| tally.observe(ExecutionEvent::Entered(phase)));
}

tokio::task_local! {
    pub(crate) static EXECUTION_TALLY: Arc<ExecutionTally>;
    /// Input records a request is sent for: one record in enrich, the
    /// records of the chunk in a batch enrich.
    pub(crate) static RECORD_SCOPE: Arc<[usize]>;
}

fn tally(update: impl FnOnce(&ExecutionTally)) {
    // Outside an execution (a best-effort remote cancellation after the
    // operation has ended) there is no result to account into.
    let _ = EXECUTION_TALLY.try_with(|tally| update(tally));
}

/// Upper bound for cached OAuth tokens; keeps secret material from accumulating
/// for an unbounded number of credential references.
const MAX_CACHED_TOKENS: usize = 256;

/// Longest `Set-Cookie` header the engine will accept. Well above any real
/// cookie; anything larger is a remote service pushing bulk data into
/// engine-held state.
pub(crate) const MAX_SET_COOKIE_BYTES: usize = 8 * 1024;

/// SHA-256 fingerprint used by the client, token, and cache isolation keys.
type Fingerprint = [u8; 32];

/// Ceiling for the half-open probe lease.
///
/// Retry, backoff, and `Retry-After` policies are caller supplied and have no
/// upper bound of their own, so an estimate derived from them could otherwise
/// become effectively permanent and keep a circuit open indefinitely.
const MAX_PROBE_LEASE: Duration = Duration::from_secs(15 * 60);

#[derive(Clone)]
pub(crate) struct PreparedFile {
    pub field_name: String,
    pub filename: String,
    pub content_type: Option<String>,
    pub source: PreparedFileSource,
}

#[derive(Clone)]
pub(crate) enum PreparedFileSource {
    Bytes(Vec<u8>),
    Path { path: PathBuf, length: u64 },
}

#[derive(Clone)]
pub(crate) struct PreparedStream {
    pub path: PathBuf,
    pub length: u64,
    pub content_type: Option<String>,
}

#[derive(Clone)]
pub(crate) enum PreparedBody {
    None,
    Json(Value),
    Form(Vec<(String, String)>),
    Multipart {
        fields: Vec<(String, String)>,
        files: Vec<PreparedFile>,
    },
    Raw(String),
    Stream(PreparedStream),
}

#[derive(Clone)]
pub(crate) struct PreparedRequest {
    pub url: Url,
    pub method: HttpMethod,
    pub headers: BTreeMap<String, String>,
    pub auth: AuthConfig,
    pub body: PreparedBody,
    pub timeout: Duration,
    pub allow_redirects: bool,
    pub max_redirects: usize,
    pub retry: RetryPolicy,
    pub cookies: CookiePolicy,
    /// Jar of the cookie session, resolved once when the operation is admitted
    /// and used for its whole duration.
    pub admitted_jar: Option<CookieJar>,
    /// Session named by the caller's request. Kept when a credential scope
    /// removes the cookies from a cross-origin follow-up, so that a request of
    /// an operation whose session has ended is still refused before it reaches
    /// the network.
    pub caller_session: Option<CookieSession>,
    pub cache: CachePolicy,
    pub circuit_breaker: CircuitBreakerPolicy,
    pub requests_per_second: Option<f64>,
    pub tls: TlsConfig,
    pub proxy: Option<ProxyConfig>,
}

#[derive(Clone)]
pub(crate) struct ResponseData {
    pub status: u16,
    pub body: Vec<u8>,
    pub final_url: Url,
    pub attempts: u32,
    pub network_requests: u64,
    pub auth_requests: u64,
    pub auth_retries: u64,
    pub rate_limit_wait_ms: u64,
    pub cache_hits: u64,
    pub cache_revalidations: u64,
    pub headers: BTreeMap<String, String>,
    retry_after_ms: Option<u64>,
}

pub(crate) struct DownloadTarget {
    pub path: PathBuf,
    pub overwrite: bool,
    pub resume: bool,
    pub max_bytes: u64,
    pub expected_sha256: Option<String>,
}

pub(crate) struct DownloadData {
    pub status: u16,
    pub final_url: Url,
    pub attempts: u32,
    pub network_requests: u64,
    pub auth_requests: u64,
    pub auth_retries: u64,
    pub rate_limit_wait_ms: u64,
    pub headers: BTreeMap<String, String>,
    pub bytes_written: u64,
    pub bytes_received: u64,
    pub sha256: String,
}

struct PendingResponse {
    response: reqwest::Response,
    final_url: Url,
    network_requests: u64,
    rate_limit_wait_ms: u64,
    permit: OwnedSemaphorePermit,
}

#[derive(Clone)]
pub(crate) struct Transport {
    config: EngineConfig,
    clients: Arc<Mutex<HashMap<ClientKey, PooledClient>>>,
    tokens: Arc<Mutex<HashMap<TokenKey, CachedToken>>>,
    token_refresh: Arc<Mutex<()>>,
    cache: Arc<Mutex<CacheStore>>,
    circuits: Arc<Mutex<HashMap<CircuitKey, CircuitState>>>,
    concurrency: Arc<Semaphore>,
    rate_state: Arc<Mutex<RateState>>,
    sequence: Arc<AtomicU64>,
    /// Cookie sessions, owned by the engine rather than by a pooled client.
    ///
    /// A jar living inside a client would vanish when that client is evicted,
    /// so a later request would silently start from an empty session. Owning
    /// them keeps a session alive for as long as its handle is valid,
    /// independently of how the connection pool churns.
    sessions: Arc<Mutex<SessionRegistry>>,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct ClientKey {
    host: String,
    port: u16,
    address: IpAddr,
    policy_fingerprint: Fingerprint,
    /// Jar instance, not just its id: a pooled client owns the `Arc<Jar>` it was
    /// built with, so a client created before the jar was evicted and recreated
    /// would keep sending the previous session's cookies.
    cookie_jar: Option<u64>,
}

#[derive(Clone)]
struct PooledClient {
    client: Client,
    sequence: u64,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct TokenKey {
    token_url: String,
    auth_fingerprint: Fingerprint,
    transport_fingerprint: Fingerprint,
}

struct CachedToken {
    token: String,
    expires_at: Instant,
    sequence: u64,
}

struct RateState {
    next_allowed: Instant,
}

#[derive(Default)]
struct RequestStats {
    network_requests: u64,
    retries: u64,
    rate_limit_wait_ms: u64,
}

impl Transport {
    pub fn new(config: EngineConfig) -> Self {
        let config_sessions = config.max_cookie_sessions;
        Self {
            // tokio admits at most Semaphore::MAX_PERMITS permits and panics
            // beyond. Engine::new cannot fail, so the value is kept in range
            // here and refused by every execution instead
            // (validate_engine_configuration): a semaphore built from an
            // out-of-range value never admits a request.
            concurrency: Arc::new(Semaphore::new(
                config
                    .max_concurrent_requests
                    .clamp(1, Semaphore::MAX_PERMITS),
            )),
            config,
            clients: Arc::new(Mutex::new(HashMap::new())),
            tokens: Arc::new(Mutex::new(HashMap::new())),
            token_refresh: Arc::new(Mutex::new(())),
            cache: Arc::new(Mutex::new(CacheStore::default())),
            circuits: Arc::new(Mutex::new(HashMap::new())),
            rate_state: Arc::new(Mutex::new(RateState {
                next_allowed: Instant::now(),
            })),
            sequence: Arc::new(AtomicU64::new(1)),
            sessions: Arc::new(Mutex::new(SessionRegistry::new(config_sessions))),
        }
    }

    /// Monotonic counter that gives every pooled entry a deterministic
    /// insertion order for eviction.
    fn next_sequence(&self) -> u64 {
        self.sequence.fetch_add(1, Ordering::Relaxed)
    }

    pub async fn execute(&self, mut request: PreparedRequest) -> Result<ResponseData, EngineError> {
        self.validate_request(&request)?;
        // Admitting the cookie session, obtaining the token and consulting the
        // cache and the circuit breaker are the connect phase of this request.
        enter_phase(ErrorPhase::Connect);
        let _jar_lease = self.admit_cookie_jar(&mut request).await?;
        let auth_stats = self.resolve_auth(&mut request).await?;
        // The token request, if any, ran its own phases; this request is back
        // to being admitted (cache, circuit breaker), part of connect.
        enter_phase(ErrorPhase::Connect);

        let mut response = self.execute_cached(&request).await?;
        response.auth_requests = auth_stats.network_requests;
        response.auth_retries = auth_stats.retries;
        response.network_requests = response
            .network_requests
            .saturating_add(auth_stats.network_requests);
        response.rate_limit_wait_ms = response
            .rate_limit_wait_ms
            .saturating_add(auth_stats.rate_limit_wait_ms);
        Ok(response)
    }

    async fn execute_authenticated(
        &self,
        request: &PreparedRequest,
    ) -> Result<ResponseData, EngineError> {
        let mut retries = RetrySession::new(&request.retry, &request.method)?;
        let mut network_requests = 0_u64;
        let mut rate_limit_wait_ms = 0_u64;

        for attempt in retries.attempts() {
            match self.send_once(request, attempt > 1).await {
                Ok(mut response) => {
                    network_requests = network_requests.saturating_add(response.network_requests);
                    rate_limit_wait_ms =
                        rate_limit_wait_ms.saturating_add(response.rate_limit_wait_ms);
                    if let RetryDecision::After(delay) = retries.decide(
                        attempt,
                        AttemptOutcome::Response {
                            status: response.status,
                            retry_after_ms: response.retry_after_ms,
                        },
                    ) {
                        sleep(delay).await;
                        continue;
                    }
                    response.attempts = attempt;
                    response.network_requests = network_requests;
                    response.rate_limit_wait_ms = rate_limit_wait_ms;
                    return Ok(response);
                }
                Err(error) => {
                    let RetryDecision::After(delay) =
                        retries.decide(attempt, AttemptOutcome::Failed(&error))
                    else {
                        return Err(error);
                    };
                    sleep(delay).await;
                }
            }
        }

        Err(EngineError::Runtime(ErrorDetail::from(
            "retry loop terminated unexpectedly",
        )))
    }

    async fn send_once(
        &self,
        request: &PreparedRequest,
        retry: bool,
    ) -> Result<ResponseData, EngineError> {
        let pending = self.send_once_response(request, retry).await?;
        self.read_response(pending).await
    }

    /// Sends `request`, following redirects. `retry` says this is a new
    /// attempt of a request already sent: it counts as a retry of the
    /// execution only once it actually goes out, so a backoff cut short by a
    /// deadline or a cancellation is not reported as a retry.
    async fn send_once_response(
        &self,
        request: &PreparedRequest,
        retry: bool,
    ) -> Result<PendingResponse, EngineError> {
        let origin = request.url.clone();
        let mut url = request.url.clone();
        let mut network_requests = 0_u64;
        let mut rate_limit_wait_ms = 0_u64;

        for redirects in 0..=request.max_redirects {
            enter_phase(ErrorPhase::Connect);
            let client = self
                .client_for(&url, &request.tls, request.proxy.as_ref(), request)
                .await?;
            enter_phase(ErrorPhase::Prepare);
            let headers = request_headers(request)?;
            let mut request_url = url.clone();
            apply_query_auth(&mut request_url, &request.auth);

            let mut builder = client
                .request(method(&request.method)?, request_url)
                .headers(headers)
                .timeout(request.timeout);
            builder = match &request.auth {
                AuthConfig::Bearer { token } => builder.bearer_auth(token),
                AuthConfig::Basic { username, password } => {
                    builder.basic_auth(username, Some(password))
                }
                AuthConfig::None | AuthConfig::ApiKey { .. } => builder,
                AuthConfig::OAuth2ClientCredentials { .. }
                | AuthConfig::OAuth2Password { .. }
                | AuthConfig::ArcgisToken { .. } => {
                    return Err(EngineError::Runtime(ErrorDetail::from(
                        "OAuth authentication was not resolved",
                    )));
                }
            };
            builder = match &request.body {
                PreparedBody::None => builder,
                PreparedBody::Json(value) => builder.json(value),
                PreparedBody::Form(values) => builder.form(values),
                PreparedBody::Multipart { fields, files } => {
                    builder.multipart(multipart_form(fields, files).await?)
                }
                PreparedBody::Raw(value) => builder.body(value.clone()),
                PreparedBody::Stream(stream) => stream_body(builder, stream).await?,
            };

            let (permit, waited_ms) = self.admit_request(request.requests_per_second).await?;
            network_requests = network_requests.saturating_add(1);
            enter_phase(ErrorPhase::Read);
            tally(|tally| {
                tally.requests.fetch_add(1, Ordering::Relaxed);
                if retry && redirects == 0 {
                    tally.retries.fetch_add(1, Ordering::Relaxed);
                }
                tally.observe(ExecutionEvent::RequestSent);
            });
            rate_limit_wait_ms = rate_limit_wait_ms.saturating_add(waited_ms);
            let response = builder.send().await.map_err(map_reqwest_error)?;
            if response.status().is_redirection() && request.allow_redirects {
                if redirects == request.max_redirects {
                    return Err(EngineError::InvalidResponse(ErrorDetail::from(
                        "redirect limit exceeded",
                    )));
                }
                let location = response
                    .headers()
                    .get(LOCATION)
                    .ok_or_else(|| {
                        EngineError::InvalidResponse(ErrorDetail::from(
                            "redirect response has no Location header",
                        ))
                    })?
                    .to_str()
                    .map_err(|_| {
                        EngineError::InvalidResponse(ErrorDetail::from(
                            "redirect Location is not valid text",
                        ))
                    })?;
                let next = url.join(location).map_err(|_| {
                    EngineError::InvalidUrl(ErrorDetail::from("invalid redirect target"))
                })?;
                if !same_origin(&origin, &next) {
                    return Err(EngineError::UnsafeAddress(ErrorDetail::from(
                        "cross-origin redirects are blocked",
                    )));
                }
                url = next;
                continue;
            }

            return Ok(PendingResponse {
                response,
                final_url: url,
                network_requests,
                rate_limit_wait_ms,
                permit,
            });
        }

        Err(EngineError::Runtime(ErrorDetail::from(
            "redirect loop terminated unexpectedly",
        )))
    }

    async fn read_response(&self, pending: PendingResponse) -> Result<ResponseData, EngineError> {
        let PendingResponse {
            response,
            final_url,
            network_requests,
            rate_limit_wait_ms,
            permit: _permit,
        } = pending;
        if response
            .headers()
            .get(CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<usize>().ok())
            .is_some_and(|length| length > self.config.max_response_bytes)
        {
            return Err(EngineError::ResponseTooLarge {
                limit_bytes: self.config.max_response_bytes,
            });
        }

        let status = response.status().as_u16();
        let retry_after_ms = response_retry_after(&response);
        let headers = response_headers(&response);
        let mut body = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(map_reqwest_error)?;
            if body.len().saturating_add(chunk.len()) > self.config.max_response_bytes {
                return Err(EngineError::ResponseTooLarge {
                    limit_bytes: self.config.max_response_bytes,
                });
            }
            body.extend_from_slice(&chunk);
        }

        Ok(ResponseData {
            status,
            body,
            final_url,
            attempts: 1,
            network_requests,
            auth_requests: 0,
            auth_retries: 0,
            rate_limit_wait_ms,
            cache_hits: 0,
            cache_revalidations: 0,
            headers,
            retry_after_ms,
        })
    }

    async fn admit_request(
        &self,
        connection_rate: Option<f64>,
    ) -> Result<(OwnedSemaphorePermit, u64), EngineError> {
        let rate = connection_rate.or_else(|| self.config.requests_per_second.map(f64::from));
        let wait = if let Some(requests_per_second) = rate {
            // Both rates are validated before any request; an interval that
            // cannot be represented here is an internal invariant failure.
            let interval = crate::exact::rate_interval(requests_per_second).ok_or_else(|| {
                EngineError::Runtime(ErrorDetail::from("request rate has no exact interval"))
            })?;
            let mut state = self.rate_state.lock().await;
            let now = Instant::now();
            let wait = state.next_allowed.saturating_duration_since(now);
            state.next_allowed = state.next_allowed.max(now) + interval;
            wait
        } else {
            Duration::ZERO
        };
        if !wait.is_zero() {
            sleep(wait).await;
        }
        let permit = self
            .concurrency
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| EngineError::Runtime(ErrorDetail::from("request limiter is closed")))?;
        Ok((permit, wait.as_millis().min(u128::from(u64::MAX)) as u64))
    }
}

/// The text of a header value, never dropped.
///
/// HTTP field values are bytes. Visible ASCII is the common case, but a
/// parameter such as a Link `title` may carry UTF-8, and obs-text is legal:
/// a value that is not valid UTF-8 is read byte for byte as ISO-8859-1, which
/// maps every byte to one character. Skipping such a value, as before, made a
/// Link header with `title="café"` disappear and pagination end in silence.
fn header_text(value: &reqwest::header::HeaderValue) -> String {
    match std::str::from_utf8(value.as_bytes()) {
        Ok(text) => text.to_owned(),
        Err(_) => value
            .as_bytes()
            .iter()
            .map(|byte| char::from(*byte))
            .collect(),
    }
}

fn response_headers(response: &reqwest::Response) -> BTreeMap<String, String> {
    let mut headers = BTreeMap::<String, String>::new();
    for (name, value) in response.headers() {
        let value = header_text(value);
        let value = value.as_str();
        headers
            .entry(name.as_str().to_owned())
            .and_modify(|existing| {
                existing.push_str(", ");
                existing.push_str(value);
            })
            .or_insert_with(|| value.to_owned());
    }
    headers
}

fn response_retry_after(response: &reqwest::Response) -> Option<u64> {
    response
        .headers()
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| parse_retry_after(value, SystemTime::now()))
}

fn number_as_u64(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
}

fn map_reqwest_error(error: reqwest::Error) -> EngineError {
    if error.is_timeout() {
        EngineError::Timeout
    } else if error.is_connect() {
        EngineError::Transport(ErrorDetail::from("connection failed"))
    } else if error.is_body() || error.is_decode() {
        EngineError::Transport(ErrorDetail::from("response transfer failed"))
    } else {
        EngineError::Transport(ErrorDetail::from("request failed"))
    }
}

pub(crate) fn parse_retry_after(value: &str, now: SystemTime) -> Option<u64> {
    let value = value.trim();
    // delay-seconds has no upper bound (RFC 9110, 10.2.3). A value too large
    // for u64 milliseconds is the longest possible wait, not an absent
    // header: it saturates, so it exceeds max_retry_after_ms and retry_wait
    // stops the retry. Reading it as absent fell back to the exponential
    // backoff and retried sooner than the service asked.
    match value.parse::<u64>() {
        Ok(seconds) => return Some(seconds.saturating_mul(1_000)),
        Err(error) if *error.kind() == IntErrorKind::PosOverflow => return Some(u64::MAX),
        Err(_) => {}
    }
    let date = httpdate::parse_http_date(value).ok()?;
    let delay = date.duration_since(now).unwrap_or(Duration::ZERO);
    Some(delay.as_millis().min(u128::from(u64::MAX)) as u64)
}

pub(crate) fn same_origin(left: &Url, right: &Url) -> bool {
    left.scheme() == right.scheme()
        && left
            .host_str()
            .zip(right.host_str())
            .is_some_and(|(left, right)| left.eq_ignore_ascii_case(right))
        && left.port_or_known_default() == right.port_or_known_default()
}

#[cfg(test)]
mod tests {
    include!("../tests/support/injected_transport.rs");
    include!("../tests/support/frozen_retry.rs");
    use std::{
        net::{IpAddr, Ipv4Addr, Ipv6Addr},
        time::{Duration, UNIX_EPOCH},
    };

    use super::{
        EXECUTION_TALLY, ExecutionTally, RECORD_SCOPE, enter_phase, is_public_address,
        parse_retry_after, same_origin,
    };
    use crate::ErrorPhase;
    use reqwest::Url;
    use std::sync::Arc;

    #[tokio::test]
    async fn records_never_move_the_phase_of_the_execution() {
        // Determinism: inside a record scope only the record's phase moves,
        // whatever order concurrent records run in; the execution's phase
        // moves only outside any record.
        let tally = Arc::new(ExecutionTally::default());
        EXECUTION_TALLY
            .scope(tally.clone(), async {
                enter_phase(ErrorPhase::Prepare);
                RECORD_SCOPE
                    .scope(Arc::from([0_usize]), async {
                        enter_phase(ErrorPhase::Read);
                    })
                    .await;
                RECORD_SCOPE
                    .scope(Arc::from([1_usize, 2]), async {
                        enter_phase(ErrorPhase::Connect);
                    })
                    .await;
            })
            .await;
        assert_eq!(tally.phase(None), Some(ErrorPhase::Prepare));
        assert_eq!(tally.phase(Some(0)), Some(ErrorPhase::Read));
        assert_eq!(tally.phase(Some(1)), Some(ErrorPhase::Connect));
        assert_eq!(tally.phase(Some(2)), Some(ErrorPhase::Connect));
        assert_eq!(tally.phase(Some(3)), None);
    }

    #[test]
    fn blocks_non_public_addresses() {
        assert!(!is_public_address(IpAddr::V4(Ipv4Addr::LOCALHOST)));
        assert!(!is_public_address(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))));
        assert!(!is_public_address(IpAddr::V6(Ipv6Addr::LOCALHOST)));
        assert!(is_public_address(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1))));
    }

    #[test]
    fn redirects_must_keep_the_origin() {
        let origin = Url::parse("https://example.com/path").unwrap();
        assert!(same_origin(
            &origin,
            &Url::parse("https://example.com/next").unwrap()
        ));
        assert!(!same_origin(
            &origin,
            &Url::parse("https://other.example/next").unwrap()
        ));
    }

    #[test]
    fn retry_after_accepts_seconds_and_http_dates() {
        let now = UNIX_EPOCH + Duration::from_secs(1_000_000);
        let later = now + Duration::from_millis(3_250);
        assert_eq!(parse_retry_after("7", now), Some(7_000));
        assert_eq!(
            parse_retry_after(&httpdate::fmt_http_date(later), now),
            Some(3_000)
        );
        assert_eq!(
            parse_retry_after(&httpdate::fmt_http_date(now - Duration::from_secs(1)), now),
            Some(0)
        );
    }

    #[test]
    fn the_cookie_header_does_not_depend_on_hash_order() {
        // Found by the remote_headers fuzz target: two jars holding the same
        // cookies sent them in different orders, because the inner jar
        // iterates hash maps with a per-instance random seed.
        use reqwest::cookie::CookieStore;
        let url = Url::parse("https://example.com/").unwrap();
        let headers = ["h=8", "c=3", "a=1", "g=7", "e=5", "b=2", "f=6", "d=4"]
            .map(reqwest::header::HeaderValue::from_static);
        for _ in 0..4 {
            let jar = super::BoundedJar::default();
            jar.set_cookies(&mut headers.iter(), &url);
            assert_eq!(
                jar.cookies(&url).unwrap(),
                "a=1; b=2; c=3; d=4; e=5; f=6; g=7; h=8"
            );
        }
    }

    #[test]
    fn retry_after_beyond_the_representable_wait_saturates() {
        // Found by the property test against RFC 9110: both values used to
        // read as "no Retry-After", so the retry came after the exponential
        // backoff instead of after max_retry_after_ms.
        let now = UNIX_EPOCH + Duration::from_secs(1_000_000);
        assert_eq!(parse_retry_after("18446744073709552", now), Some(u64::MAX));
        assert_eq!(
            parse_retry_after("99999999999999999999999", now),
            Some(u64::MAX)
        );
        let policy = crate::RetryPolicy::default();
        let mut backoff = super::backoff_for(&policy).unwrap();
        assert_eq!(
            super::retry_delay(
                &policy,
                &mut backoff,
                parse_retry_after("99999999999999999999999", now)
            ),
            Duration::from_millis(policy.max_retry_after_ms)
        );
    }

    #[tokio::test]
    async fn an_exhausted_generation_retires_the_slot_instead_of_wrapping() {
        let transport = super::Transport::new(crate::EngineConfig {
            allow_cookie_store: true,
            max_cookie_sessions: 1,
            ..crate::EngineConfig::default()
        });
        let first = transport.open_cookie_session().await.unwrap();
        transport.close_cookie_session(&first).await.unwrap();
        // Jump the only slot to the last generation it can represent.
        transport.sessions.lock().await.slots[0].generation = u64::MAX;
        let last = transport.open_cookie_session().await.unwrap();
        assert_eq!(last.generation, u64::MAX);
        transport.close_cookie_session(&last).await.unwrap();

        // Wrapping to generation 0 would make `first` look current again.
        assert!(matches!(
            transport.sessions.lock().await.slots[0].state,
            super::SlotState::Retired
        ));
        assert!(transport.close_cookie_session(&first).await.is_err());
        assert!(transport.close_cookie_session(&last).await.is_err());
        // With its only slot retired the engine refuses, explicitly.
        assert!(transport.open_cookie_session().await.is_err());
    }

    #[tokio::test]
    async fn a_reused_slot_refuses_every_earlier_generation() {
        let transport = super::Transport::new(crate::EngineConfig {
            allow_cookie_store: true,
            max_cookie_sessions: 1,
            ..crate::EngineConfig::default()
        });
        let first = transport.open_cookie_session().await.unwrap();
        transport.close_cookie_session(&first).await.unwrap();
        let second = transport.open_cookie_session().await.unwrap();
        assert_eq!(second.slot, first.slot);
        assert!(second.generation > first.generation);
        // The generation alone refuses the old session, even with the current
        // random part: it does not rely on the nonce being different.
        let earlier = crate::CookieSession {
            generation: first.generation,
            ..second.clone()
        };
        let mut registry = transport.sessions.lock().await;
        assert_eq!(
            registry.resolve(&earlier).err(),
            Some(super::SessionRefusal::Stale)
        );
        assert!(registry.resolve(&second).is_ok());
    }

    #[tokio::test]
    async fn eviction_moves_the_slot_to_a_new_generation() {
        let transport = super::Transport::new(crate::EngineConfig {
            allow_cookie_store: true,
            max_cookie_sessions: 1,
            ..crate::EngineConfig::default()
        });
        let evicted = transport.open_cookie_session().await.unwrap();
        // The only slot is taken and idle, so opening evicts it.
        let current = transport.open_cookie_session().await.unwrap();
        assert_eq!(current.slot, evicted.slot);
        assert!(current.generation > evicted.generation);
        let earlier = crate::CookieSession {
            generation: evicted.generation,
            ..current.clone()
        };
        assert_eq!(
            transport.sessions.lock().await.resolve(&earlier).err(),
            Some(super::SessionRefusal::Stale)
        );
    }

    #[tokio::test]
    async fn a_handle_naming_another_engine_is_refused_as_foreign() {
        let transport = super::Transport::new(crate::EngineConfig {
            allow_cookie_store: true,
            ..crate::EngineConfig::default()
        });
        let current = transport.open_cookie_session().await.unwrap();
        // Everything matches but the engine identifier.
        let foreign = crate::CookieSession {
            engine: current.engine.wrapping_add(1),
            ..current.clone()
        };
        let mut registry = transport.sessions.lock().await;
        assert_eq!(
            registry.resolve(&foreign).err(),
            Some(super::SessionRefusal::ForeignEngine)
        );
        assert!(registry.resolve(&current).is_ok());
    }
}
