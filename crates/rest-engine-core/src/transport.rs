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
use futures_util::StreamExt;
use reqwest::{
    Body, Certificate, Client, Identity, Method, Proxy, Url,
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
use tokio::{
    fs::{self, OpenOptions},
    io::AsyncWriteExt,
    net::lookup_host,
    sync::{Mutex, OwnedSemaphorePermit, Semaphore},
    time::sleep,
};
use tokio_util::io::ReaderStream;
use url::Host;

use crate::{
    ApiKeyLocation, AuthConfig, CachePolicy, CircuitBreakerPolicy, CookiePolicy, CookieSession,
    EngineConfig, EngineError, HttpMethod, OAuthClientAuth, ProxyConfig, RetryPolicy, TlsConfig,
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
#[derive(Default)]
pub(crate) struct ExecutionTally {
    pub(crate) requests: AtomicU64,
    pub(crate) retries: AtomicU64,
}

tokio::task_local! {
    pub(crate) static EXECUTION_TALLY: Arc<ExecutionTally>;
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

struct DownloadState {
    temporary: PathBuf,
    file: Option<fs::File>,
    bytes_written: u64,
    bytes_received: u64,
    digest: Sha256,
    etag: Option<String>,
    expected_total: Option<u64>,
    /// Cleared once the staging file has been persisted or explicitly discarded.
    /// While set, dropping the state removes the partial file, which is what makes
    /// a cancelled or timed out download cancellation-safe.
    cleanup: bool,
}

impl Drop for DownloadState {
    fn drop(&mut self) {
        if !self.cleanup {
            return;
        }
        drop(self.file.take());
        let _ = std::fs::remove_file(&self.temporary);
    }
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

/// Fixed set of cookie session slots.
///
/// Memory is bounded by the slot count alone: nothing is remembered about
/// sessions that ended. A handle stays recognisable as stale because the slot
/// it names has moved to a later generation, not because the engine keeps a
/// list of what it evicted.
struct SessionRegistry {
    /// Random per engine, so a handle issued by another engine is refused.
    /// `None` when the system random source failed at construction: every
    /// session operation is then refused rather than run with a guessable id.
    engine: Option<u64>,
    capacity: usize,
    slots: Vec<SessionSlot>,
}

struct SessionSlot {
    /// Generation a handle must carry to use this slot. It moves forward every
    /// time a session in the slot ends, so no handle outlives its session.
    generation: u64,
    state: SlotState,
}

enum SlotState {
    Free,
    Open(OpenSession),
    /// Closed while operations still held it. The handle is already refused
    /// for new requests; the slot is freed, and its generation advanced, only
    /// once the last of those operations has released its lease.
    Closing(CookieJar),
    /// The generation counter is exhausted. The slot is never used again rather
    /// than restarting from a generation an old handle might still carry.
    Retired,
}

struct OpenSession {
    /// Random per session, 128 bits: a handle cannot be assembled from a slot
    /// number and a guessed generation.
    nonce: u128,
    jar: CookieJar,
    /// Order of last use, for evicting the least recently used session.
    last_used: u64,
}

/// Why a cookie session handle was not honoured.
#[derive(Debug, PartialEq, Eq)]
enum SessionRefusal {
    /// Issued by another engine.
    ForeignEngine,
    /// Closed, evicted, or never issued by this engine.
    Stale,
}

/// 128 random bits from the operating system's generator.
fn random_u128() -> Result<u128, EngineError> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| random_source_error())?;
    Ok(u128::from_le_bytes(bytes))
}

fn random_source_error() -> EngineError {
    EngineError::Runtime(ErrorDetail::from("the system random source failed"))
}

impl SessionRegistry {
    fn new(capacity: usize) -> Self {
        Self {
            engine: getrandom::u64().ok(),
            capacity,
            slots: Vec::new(),
        }
    }

    /// The open session `handle` names, if the handle is current.
    fn resolve(&mut self, handle: &CookieSession) -> Result<&mut OpenSession, SessionRefusal> {
        if Some(handle.engine) != self.engine {
            return Err(SessionRefusal::ForeignEngine);
        }
        let slot = usize::try_from(handle.slot)
            .ok()
            .and_then(|index| self.slots.get_mut(index))
            .ok_or(SessionRefusal::Stale)?;
        match &mut slot.state {
            SlotState::Open(session)
                if slot.generation == handle.generation && session.nonce == handle.nonce =>
            {
                Ok(session)
            }
            _ => Err(SessionRefusal::Stale),
        }
    }

    /// Ends the session in `index`, moving the slot to its next generation or
    /// retiring it when the generation cannot advance.
    fn end(&mut self, index: usize) -> Option<u64> {
        let slot = &mut self.slots[index];
        let incarnation = match std::mem::replace(&mut slot.state, SlotState::Free) {
            SlotState::Open(session) => Some(session.jar.incarnation),
            SlotState::Closing(jar) => Some(jar.incarnation),
            SlotState::Free | SlotState::Retired => None,
        };
        match slot.generation.checked_add(1) {
            Some(next) => slot.generation = next,
            None => slot.state = SlotState::Retired,
        }
        incarnation
    }
}
/// Cookie store that drops implausibly long `Set-Cookie` headers.
///
/// A resource bound, not a vulnerability mitigation: a remote service should not
/// be able to push arbitrarily large values into engine-held state one header at
/// a time. A legitimate `Set-Cookie` is far below this limit.
#[derive(Default)]
pub(crate) struct BoundedJar {
    inner: Jar,
}

impl CookieStore for BoundedJar {
    fn set_cookies(&self, cookie_headers: &mut dyn Iterator<Item = &HeaderValue>, url: &Url) {
        let accepted = cookie_headers
            .filter(|value| value.len() <= MAX_SET_COOKIE_BYTES)
            .cloned()
            .collect::<Vec<_>>();
        if accepted.is_empty() {
            return;
        }
        self.inner.set_cookies(&mut accepted.iter(), url);
    }

    fn cookies(&self, url: &Url) -> Option<HeaderValue> {
        // The inner jar keeps its cookies in hash maps, so the order of the
        // pairs in the Cookie header changed from one jar to the next for the
        // same cookies. Sorted, the header depends only on the cookies. A
        // pair never contains `;`: the cookie-octet grammar excludes it.
        let value = self.inner.cookies(url)?;
        let mut pairs = value
            .as_bytes()
            .split(|byte| *byte == b';')
            .map(<[u8]>::trim_ascii)
            .filter(|pair| !pair.is_empty())
            .collect::<Vec<_>>();
        pairs.sort_unstable();
        Some(HeaderValue::from_bytes(&pairs.join(&b"; "[..])).unwrap_or(value))
    }
}

#[derive(Clone)]
pub(crate) struct CookieJar {
    jar: Arc<BoundedJar>,
    /// Operations currently holding this jar, counted by this crate rather than
    /// inferred from `Arc::strong_count`.
    ///
    /// How many references a pooled HTTP client keeps to its cookie store is an
    /// implementation detail of that client; counting reservations here is
    /// independent of it, and a pooled client alone never makes a jar look busy.
    leases: Arc<AtomicUsize>,
    /// Identifies this jar instance. Never reused, so a pooled client built for
    /// an earlier session in the same slot is never handed to a later one.
    incarnation: u64,
}

/// Keeps a jar reserved for as long as an operation needs it.
///
/// Taken while the registry lock is held, so a jar cannot be evicted between
/// the lookup that found it and the reservation that protects it. Released on
/// drop, including when the operation fails or is cancelled.
struct JarLease {
    leases: Arc<AtomicUsize>,
}

impl Drop for JarLease {
    fn drop(&mut self) {
        // Released without the registry lock, so the atomic itself has to carry
        // the ordering that lets a later eviction see the count reach zero.
        self.leases.fetch_sub(1, Ordering::Release);
    }
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

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct CacheKey {
    method: String,
    url: String,
    request_fingerprint: Fingerprint,
}

#[derive(Clone)]
struct CachedResponse {
    status: u16,
    body: Vec<u8>,
    final_url: Url,
    headers: BTreeMap<String, String>,
    validated_at: Instant,
    last_access: Instant,
    must_revalidate: bool,
    server_fresh_for_ms: Option<u64>,
    size_bytes: usize,
    sequence: u64,
}

#[derive(Default)]
struct CacheStore {
    entries: HashMap<CacheKey, CachedResponse>,
    size_bytes: usize,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct CircuitKey {
    origin: String,
    group: String,
}

/// Outcome of admitting a request past the circuit breaker. `probe` is set only
/// when this request is the half-open probe, and carries the generation that
/// entitles it to record the result. `epoch` is the state generation the
/// admission decision was based on.
struct CircuitAdmission {
    key: CircuitKey,
    probe: Option<u64>,
    epoch: u64,
}

#[derive(Default)]
struct CircuitState {
    consecutive_failures: u32,
    opened_at: Option<Instant>,
    /// Current half-open probe, if any: its generation and its start.
    ///
    /// The start is a lease, so a cancelled or abandoned probe cannot keep the
    /// circuit open forever. The generation makes the lease safe: once it
    /// expires and a new probe is admitted, the abandoned one may still finish,
    /// and only the probe owning the current generation may record an outcome.
    half_open_probe: Option<(u64, Instant)>,
    /// Bumped whenever the circuit opens or a probe starts.
    ///
    /// A request admitted while the circuit was closed carries no probe
    /// generation, yet it can still finish after the circuit opened and a probe
    /// began. Recording its outcome would reset `opened_at` and let ordinary
    /// traffic through while the probe is still in flight, so an admission from
    /// a superseded epoch is ignored.
    epoch: u64,
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
            concurrency: Arc::new(Semaphore::new(config.max_concurrent_requests.max(1))),
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

    /// Opens a cookie session and returns its handle.
    ///
    /// Uses a free slot when there is one. Otherwise the least recently used
    /// session that no operation is holding is evicted: its slot moves to the
    /// next generation, so the evicted handle fails from then on instead of
    /// reaching the new, empty session. When every session is held by a
    /// running operation the call is refused.
    pub async fn open_cookie_session(&self) -> Result<CookieSession, EngineError> {
        if !self.config.allow_cookie_store {
            return Err(EngineError::PolicyViolation(ErrorDetail::from(
                "cookie storage is not enabled for this engine",
            )));
        }
        let incarnation = self.next_sequence();
        let last_used = self.next_sequence();
        let nonce = random_u128()?;
        let mut registry = self.sessions.lock().await;
        let engine = registry.engine.ok_or_else(random_source_error)?;
        let index = loop {
            if let Some(index) = registry
                .slots
                .iter()
                .position(|slot| matches!(slot.state, SlotState::Free))
            {
                break index;
            }
            // A closed session whose last operation has finished: its slot can
            // move to the next generation now. Leases are released without this
            // lock, so the count is read here rather than acted on at release.
            if let Some(index) = registry.slots.iter().position(|slot| {
                matches!(&slot.state, SlotState::Closing(jar) if jar.leases.load(Ordering::Acquire) == 0)
            }) {
                if let Some(incarnation) = registry.end(index) {
                    self.drop_session_clients(incarnation).await;
                }
                continue;
            }
            if registry.slots.len() < registry.capacity {
                registry.slots.push(SessionSlot {
                    generation: 0,
                    state: SlotState::Free,
                });
                break registry.slots.len() - 1;
            }
            // Only a session no operation holds may go: evicting one in use
            // would split it between the running request and the next one.
            // Leases are taken and read under this lock, so a zero count here
            // cannot be stale.
            let victim = registry
                .slots
                .iter()
                .enumerate()
                .filter_map(|(index, slot)| match &slot.state {
                    SlotState::Open(session) if session.jar.leases.load(Ordering::Acquire) == 0 => {
                        Some((index, session.last_used))
                    }
                    _ => None,
                })
                .min_by_key(|(_, last_used)| *last_used)
                .map(|(index, _)| index);
            let Some(victim) = victim else {
                return Err(EngineError::PolicyViolation(ErrorDetail::from(
                    "every cookie session is held by a running operation",
                )));
            };
            if let Some(incarnation) = registry.end(victim) {
                self.drop_session_clients(incarnation).await;
            }
            // A retired slot is skipped by the next iteration; the loop ends
            // because every iteration either returns, or frees or retires one
            // of finitely many slots.
        };
        let slot = &mut registry.slots[index];
        slot.state = SlotState::Open(OpenSession {
            nonce,
            jar: CookieJar {
                jar: Arc::new(BoundedJar::default()),
                leases: Arc::new(AtomicUsize::new(0)),
                incarnation,
            },
            last_used,
        });
        Ok(CookieSession {
            engine,
            slot: u32::try_from(index).map_err(|_| {
                EngineError::Runtime(ErrorDetail::from("cookie session slot index overflowed"))
            })?,
            generation: slot.generation,
            nonce,
        })
    }

    /// Closes the session `handle` names.
    ///
    /// The handle, and every copy of it, is refused for new requests from now
    /// on. Operations already admitted keep the jar they were admitted with
    /// until they finish; the slot is freed and moves to its next generation
    /// only after the last of them has released it, so a closed session is
    /// never handed to a new one while still in use. Closing a handle that is
    /// already stale is an error rather than a no-op, so a caller that lost
    /// track of a session finds out.
    pub async fn close_cookie_session(&self, handle: &CookieSession) -> Result<(), EngineError> {
        let mut registry = self.sessions.lock().await;
        let session = registry.resolve(handle).map_err(session_refusal_error)?;
        let held = session.jar.leases.load(Ordering::Acquire) > 0;
        let incarnation = session.jar.incarnation;
        let index = usize::try_from(handle.slot).map_err(|_| {
            EngineError::Runtime(ErrorDetail::from("cookie session slot index overflowed"))
        })?;
        if held {
            let slot = &mut registry.slots[index];
            slot.state = match std::mem::replace(&mut slot.state, SlotState::Free) {
                SlotState::Open(session) => SlotState::Closing(session.jar),
                other => other,
            };
            // Idle pooled clients go now; one built by an operation still in
            // flight is removed when the slot is reclaimed.
            self.drop_session_clients(incarnation).await;
        } else if let Some(incarnation) = registry.end(index) {
            self.drop_session_clients(incarnation).await;
        }
        Ok(())
    }

    /// Removes the pooled clients built on a session that ended.
    ///
    /// Called with the session registry locked; locks are always taken
    /// sessions then clients, as in `client_for`, so the two cannot cross.
    async fn drop_session_clients(&self, incarnation: u64) {
        self.clients
            .lock()
            .await
            .retain(|key, _| key.cookie_jar != Some(incarnation));
    }

    /// Reserves the session this request needs, before any step that can reach
    /// the network.
    ///
    /// Doing this up front is what keeps the error honest: refusing later, from
    /// `client_for`, would report a policy violation with no remote effect after
    /// an OAuth token had already been fetched. The returned lease keeps the
    /// session from being evicted while this operation authenticates and
    /// resolves DNS.
    ///
    /// The jar resolved here is the one the whole operation uses: it is stored
    /// in the request and `client_for` never resolves the handle again, so a
    /// session closed while the operation is in flight cannot turn into a
    /// refusal after network activity has already happened.
    async fn admit_cookie_jar(
        &self,
        request: &mut PreparedRequest,
    ) -> Result<Option<JarLease>, EngineError> {
        // A follow-up whose cookies were stripped still belongs to the caller's
        // session: it must not run once that session has ended.
        if request.cookies.session.is_none() {
            self.check_cookie_session(request.caller_session.as_ref())
                .await?;
        }
        Ok(match self.cookie_jar(&request.cookies).await? {
            Some((jar, lease)) => {
                request.admitted_jar = Some(jar);
                Some(lease)
            }
            None => None,
        })
    }

    /// The jar of the session `cookies` names, with a lease that keeps the
    /// session from being evicted while the caller holds it.
    ///
    /// `Ok(None)` when the request uses no session. A stale handle, or one from
    /// another engine, fails: the session it named is gone, and an empty one in
    /// its place would quietly log the caller out.
    async fn cookie_jar(
        &self,
        cookies: &CookiePolicy,
    ) -> Result<Option<(CookieJar, JarLease)>, EngineError> {
        let Some(handle) = &cookies.session else {
            return Ok(None);
        };
        let last_used = self.next_sequence();
        let mut registry = self.sessions.lock().await;
        let session = registry.resolve(handle).map_err(session_refusal_error)?;
        session.last_used = last_used;
        let jar = session.jar.clone();
        jar.leases.fetch_add(1, Ordering::Acquire);
        let lease = JarLease {
            leases: jar.leases.clone(),
        };
        Ok(Some((jar, lease)))
    }

    /// Refuses a handle that is stale, foreign, or used on an engine without a
    /// cookie store. `None` is accepted: the request names no session.
    pub async fn check_cookie_session(
        &self,
        session: Option<&CookieSession>,
    ) -> Result<(), EngineError> {
        let Some(handle) = session else {
            return Ok(());
        };
        if !self.config.allow_cookie_store {
            return Err(EngineError::PolicyViolation(ErrorDetail::from(
                "cookie storage is not enabled for this engine",
            )));
        }
        self.sessions
            .lock()
            .await
            .resolve(handle)
            .map(|_| ())
            .map_err(session_refusal_error)
    }

    /// Monotonic counter that gives every pooled entry a deterministic
    /// insertion order for eviction.
    fn next_sequence(&self) -> u64 {
        self.sequence.fetch_add(1, Ordering::Relaxed)
    }

    pub async fn execute(&self, mut request: PreparedRequest) -> Result<ResponseData, EngineError> {
        self.validate_request(&request)?;
        let _jar_lease = self.admit_cookie_jar(&mut request).await?;
        let auth_stats = self.resolve_auth(&mut request).await?;

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

    pub async fn download(
        &self,
        mut request: PreparedRequest,
        target: &DownloadTarget,
        success_statuses: &[u16],
    ) -> Result<DownloadData, EngineError> {
        self.validate_request(&request)?;
        let _jar_lease = self.admit_cookie_jar(&mut request).await?;
        if target.resume {
            if request.method != HttpMethod::Get {
                return Err(EngineError::InvalidInput(ErrorDetail::from(
                    "resumable downloads require the GET method",
                )));
            }
            if request
                .headers
                .keys()
                .any(|name| name.eq_ignore_ascii_case(RANGE.as_str()))
                || request
                    .headers
                    .keys()
                    .any(|name| name.eq_ignore_ascii_case(IF_RANGE.as_str()))
            {
                return Err(EngineError::InvalidInput(ErrorDetail::from(
                    "managed resume cannot be combined with Range or If-Range headers",
                )));
            }
            set_request_header(
                &mut request.headers,
                ACCEPT_ENCODING.as_str(),
                "identity".to_owned(),
            );
        }
        let auth_stats = self.resolve_auth(&mut request).await?;
        let mut response = self
            .download_with_circuit(&request, target, success_statuses)
            .await?;
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

    fn validate_request(&self, request: &PreparedRequest) -> Result<(), EngineError> {
        if request.method.is_custom()
            && !self
                .config
                .allowed_custom_methods
                .iter()
                .any(|allowed| allowed == request.method.as_str())
        {
            return Err(EngineError::PolicyViolation(ErrorDetail::from(
                "custom HTTP method is not in allowed_custom_methods",
            )));
        }
        // Refused rather than approximated, and refused here so that a request
        // that cannot run produces no effect at all — not even an OAuth token
        // acquisition, which `execute` performs before consulting the cache.
        //
        // A cached entry belongs to the session that produced it, but the cookie
        // jar lives inside the HTTP client and changes while the operation runs:
        // a retry or a redirect can pick up a new session after the key was
        // computed, and cookies expire on their own with nothing to observe.
        // Keying the cache on a snapshot of the jar would therefore describe a
        // session that may not be the one that answered.
        if request.cookies.is_enabled() && request.cache.enabled {
            return Err(EngineError::PolicyViolation(ErrorDetail::from(
                "the HTTP cache cannot be combined with the cookie store",
            )));
        }
        if request.cookies.is_enabled() && !self.config.allow_cookie_store {
            return Err(EngineError::PolicyViolation(ErrorDetail::from(
                "cookie storage is not enabled for this engine",
            )));
        }
        Ok(())
    }

    async fn resolve_auth(
        &self,
        request: &mut PreparedRequest,
    ) -> Result<RequestStats, EngineError> {
        if matches!(
            &request.auth,
            AuthConfig::OAuth2ClientCredentials { .. }
                | AuthConfig::OAuth2Password { .. }
                | AuthConfig::ArcgisToken { .. }
        ) {
            let (token, stats) = self.oauth_token(request).await?;
            request.auth = AuthConfig::Bearer { token };
            Ok(stats)
        } else {
            Ok(RequestStats::default())
        }
    }

    async fn execute_cached(&self, request: &PreparedRequest) -> Result<ResponseData, EngineError> {
        if !request.cache.enabled {
            return self.execute_with_circuit(request).await;
        }
        if !matches!(request.method, HttpMethod::Get | HttpMethod::Head) {
            return Err(EngineError::InvalidInput(ErrorDetail::from(
                "HTTP cache is supported only for GET and HEAD",
            )));
        }
        if self.config.max_cache_entries == 0 || self.config.max_cache_bytes == 0 {
            return Err(EngineError::PolicyViolation(ErrorDetail::from(
                "HTTP cache capacity is disabled by this engine",
            )));
        }
        // Defensive: `validate_request` already refused this combination before
        // anything reached the network.
        if request.cookies.is_enabled() {
            return Err(EngineError::PolicyViolation(ErrorDetail::from(
                "the HTTP cache cannot be combined with the cookie store",
            )));
        }
        // A client certificate authenticates the request just as much as a
        // bearer token does, so caching it needs the same explicit opt-in.
        let authenticated = !matches!(request.auth, AuthConfig::None)
            || request
                .headers
                .keys()
                .any(|name| name.eq_ignore_ascii_case("authorization"))
            || request.cookies.is_enabled()
            || request.tls.client_identity_pem.is_some();
        if authenticated && !request.cache.allow_authenticated {
            return Err(EngineError::PolicyViolation(ErrorDetail::from(
                "authenticated HTTP caching requires allow_authenticated",
            )));
        }

        let key = cache_key(request)?;
        let cached = {
            let mut store = self.cache.lock().await;
            match store.entries.get_mut(&key) {
                Some(cached) => {
                    cached.last_access = Instant::now();
                    Some(cached.clone())
                }
                None => None,
            }
        };
        if let Some(cached) = &cached {
            let fresh_for_ms = cached
                .server_fresh_for_ms
                .map_or(request.cache.fresh_for_ms, |server| {
                    request.cache.fresh_for_ms.min(server)
                });
            let fresh = !cached.must_revalidate
                && fresh_for_ms > 0
                && cached.validated_at.elapsed() <= Duration::from_millis(fresh_for_ms);
            if fresh {
                return Ok(cached_response(cached, 1, 0));
            }
        }

        let mut conditional = request.clone();
        if let Some(cached) = &cached {
            add_conditional_headers(&mut conditional.headers, &cached.headers);
        }
        let response = self.execute_with_circuit(&conditional).await?;
        if response.status == 304 {
            let Some(mut cached) = cached else {
                return Ok(response);
            };
            merge_headers(&mut cached.headers, &response.headers);
            cached.validated_at = Instant::now();
            cached.last_access = cached.validated_at;
            cached.must_revalidate = response_requires_revalidation(&cached.headers);
            cached.server_fresh_for_ms = cache_max_age_ms(&cached.headers);
            cached.size_bytes = cached_response_size(&cached.body, &cached.headers);
            self.store_cache_entry(key, cached.clone()).await;
            let mut resolved = cached_response(&cached, 1, 1);
            resolved.attempts = response.attempts;
            resolved.network_requests = response.network_requests;
            resolved.rate_limit_wait_ms = response.rate_limit_wait_ms;
            return Ok(resolved);
        }

        if (200..300).contains(&response.status)
            && !response_forbids_store(&response.headers)
            && response.body.len() <= self.config.max_cache_bytes
        {
            let now = Instant::now();
            let cached = CachedResponse {
                status: response.status,
                body: response.body.clone(),
                final_url: response.final_url.clone(),
                headers: response.headers.clone(),
                validated_at: now,
                last_access: now,
                must_revalidate: response_requires_revalidation(&response.headers),
                server_fresh_for_ms: cache_max_age_ms(&response.headers),
                size_bytes: cached_response_size(&response.body, &response.headers),
                sequence: self.next_sequence(),
            };
            self.store_cache_entry(key, cached).await;
        }
        Ok(response)
    }

    async fn execute_with_circuit(
        &self,
        request: &PreparedRequest,
    ) -> Result<ResponseData, EngineError> {
        let admission = self.admit_circuit(request).await?;
        let result = self.execute_authenticated(request).await;
        if let Some(admission) = admission {
            let failed = match &result {
                Ok(response) => request
                    .circuit_breaker
                    .failure_statuses
                    .contains(&response.status),
                Err(error) => is_retryable_transport_error(error),
            };
            self.record_circuit(&admission, &request.circuit_breaker, failed)
                .await;
        }
        result
    }

    async fn store_cache_entry(&self, key: CacheKey, entry: CachedResponse) {
        if entry.size_bytes > self.config.max_cache_bytes {
            return;
        }
        let mut store = self.cache.lock().await;
        if let Some(previous) = store.entries.remove(&key) {
            store.size_bytes = store.size_bytes.saturating_sub(previous.size_bytes);
        }
        while store.entries.len() >= self.config.max_cache_entries
            || store.size_bytes.saturating_add(entry.size_bytes) > self.config.max_cache_bytes
        {
            let Some(oldest) = store
                .entries
                .iter()
                .min_by_key(|(_, value)| (value.last_access, value.sequence))
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            if let Some(removed) = store.entries.remove(&oldest) {
                store.size_bytes = store.size_bytes.saturating_sub(removed.size_bytes);
            }
        }
        if store.entries.len() < self.config.max_cache_entries
            && store.size_bytes.saturating_add(entry.size_bytes) <= self.config.max_cache_bytes
        {
            store.size_bytes = store.size_bytes.saturating_add(entry.size_bytes);
            store.entries.insert(key, entry);
        }
    }

    async fn admit_circuit(
        &self,
        request: &PreparedRequest,
    ) -> Result<Option<CircuitAdmission>, EngineError> {
        let policy = &request.circuit_breaker;
        if !policy.enabled {
            return Ok(None);
        }
        if policy.failure_threshold == 0 {
            return Err(EngineError::InvalidInput(ErrorDetail::from(
                "circuit breaker failure_threshold must be greater than zero",
            )));
        }
        if policy.group.trim().is_empty() {
            return Err(EngineError::InvalidInput(ErrorDetail::from(
                "circuit breaker group cannot be empty",
            )));
        }
        if self.config.max_circuit_origins == 0 {
            return Err(EngineError::PolicyViolation(ErrorDetail::from(
                "circuit breaker state is disabled by this engine",
            )));
        }
        let key = CircuitKey {
            origin: request.url.origin().ascii_serialization(),
            group: policy.group.clone(),
        };
        let sequence = self.next_sequence();
        let recovery = Duration::from_millis(policy.recovery_timeout_ms);
        // A probe whose future was cancelled never reaches `record_circuit`, so
        // the in-flight marker is a lease that expires instead of a sticky flag.
        //
        // The lease is an upper-bound *estimate*, not a guarantee: a probe runs a
        // whole retry loop, and its real duration also includes `Retry-After`
        // waits, redirects, rate limiting, and name resolution. Correctness does
        // not rest on it — the epoch and the probe generation already stop a
        // superseded probe from recording anything. The lease exists only so an
        // abandoned probe cannot wedge the circuit open forever, so it is
        // estimated generously and then clamped: too short would admit a second
        // probe next to a healthy one, too long would bring back the wedge it is
        // meant to prevent.
        let attempts = request.retry.max_attempts.max(1);
        let per_attempt = request.timeout.saturating_add(Duration::from_millis(
            request
                .retry
                .max_backoff_ms
                .max(request.retry.backoff_base_ms)
                .max(if request.retry.respect_retry_after {
                    request.retry.max_retry_after_ms
                } else {
                    0
                }),
        ));
        let probe_lease = recovery
            .max(per_attempt.saturating_mul(attempts))
            .min(MAX_PROBE_LEASE);
        let mut circuits = self.circuits.lock().await;
        if !circuits.contains_key(&key) && circuits.len() >= self.config.max_circuit_origins {
            let oldest = circuits
                .iter()
                .min_by_key(|(_, state)| state.sequence)
                .map(|(key, _)| key.clone());
            if let Some(oldest) = oldest {
                circuits.remove(&oldest);
            }
        }
        let state = circuits.entry(key.clone()).or_default();
        if state.sequence == 0 {
            state.sequence = sequence;
        }
        let mut probe = None;
        if let Some(opened_at) = state.opened_at {
            let probing = state
                .half_open_probe
                .is_some_and(|(_, started)| started.elapsed() < probe_lease);
            if opened_at.elapsed() < recovery || probing {
                return Err(EngineError::CircuitOpen);
            }
            let generation = self.next_sequence();
            state.half_open_probe = Some((generation, Instant::now()));
            state.epoch = state.epoch.saturating_add(1);
            probe = Some(generation);
        }
        Ok(Some(CircuitAdmission {
            key,
            probe,
            epoch: state.epoch,
        }))
    }

    async fn record_circuit(
        &self,
        admission: &CircuitAdmission,
        policy: &CircuitBreakerPolicy,
        failed: bool,
    ) {
        let sequence = self.next_sequence();
        let mut circuits = self.circuits.lock().await;
        let state = circuits.entry(admission.key.clone()).or_default();
        if state.sequence == 0 {
            state.sequence = sequence;
        }
        // An admission from a superseded epoch describes a decision that no
        // longer holds: the circuit has opened, or a probe has started, since it
        // was let through. Recording it would close or reopen the circuit on
        // behalf of a request nobody is waiting for.
        if admission.epoch != state.epoch {
            return;
        }
        // A probe whose lease expired has likewise been superseded.
        let current_probe = state.half_open_probe.map(|(generation, _)| generation);
        if let Some(generation) = admission.probe {
            if current_probe != Some(generation) {
                return;
            }
        }
        if failed {
            state.consecutive_failures = state.consecutive_failures.saturating_add(1);
            if admission.probe.is_some() || state.consecutive_failures >= policy.failure_threshold {
                if state.opened_at.is_none() {
                    state.epoch = state.epoch.saturating_add(1);
                }
                state.opened_at = Some(Instant::now());
            }
        } else {
            state.consecutive_failures = 0;
            state.opened_at = None;
        }
        if admission.probe.is_some() {
            state.half_open_probe = None;
        }
    }

    async fn execute_authenticated(
        &self,
        request: &PreparedRequest,
    ) -> Result<ResponseData, EngineError> {
        let max_attempts = request.retry.max_attempts.max(1);
        let can_retry = request.method.is_idempotent() || request.retry.retry_non_idempotent;
        let mut network_requests = 0_u64;
        let mut rate_limit_wait_ms = 0_u64;

        for attempt in 1..=max_attempts {
            match self.send_once(request, attempt > 1).await {
                Ok(mut response) => {
                    network_requests = network_requests.saturating_add(response.network_requests);
                    rate_limit_wait_ms =
                        rate_limit_wait_ms.saturating_add(response.rate_limit_wait_ms);
                    let retry_status = request.retry.retry_on_status.contains(&response.status);
                    if can_retry && retry_status && attempt < max_attempts {
                        sleep(retry_delay(
                            &request.retry,
                            attempt,
                            response.retry_after_ms,
                        ))
                        .await;
                        continue;
                    }
                    response.attempts = attempt;
                    response.network_requests = network_requests;
                    response.rate_limit_wait_ms = rate_limit_wait_ms;
                    return Ok(response);
                }
                Err(error)
                    if can_retry
                        && attempt < max_attempts
                        && is_retryable_transport_error(&error) =>
                {
                    sleep(retry_delay(&request.retry, attempt, None)).await;
                }
                Err(error) => return Err(error),
            }
        }

        Err(EngineError::Runtime(ErrorDetail::from(
            "retry loop terminated unexpectedly",
        )))
    }

    async fn download_with_circuit(
        &self,
        request: &PreparedRequest,
        target: &DownloadTarget,
        success_statuses: &[u16],
    ) -> Result<DownloadData, EngineError> {
        let admission = self.admit_circuit(request).await?;
        let result = self
            .download_authenticated(request, target, success_statuses)
            .await;
        if let Some(admission) = admission {
            let failed = match &result {
                Ok(response) => request
                    .circuit_breaker
                    .failure_statuses
                    .contains(&response.status),
                Err(EngineError::HttpStatus { status, .. }) => {
                    request.circuit_breaker.failure_statuses.contains(status)
                }
                Err(error) => is_retryable_transport_error(error),
            };
            self.record_circuit(&admission, &request.circuit_breaker, failed)
                .await;
        }
        result
    }

    async fn download_authenticated(
        &self,
        request: &PreparedRequest,
        target: &DownloadTarget,
        success_statuses: &[u16],
    ) -> Result<DownloadData, EngineError> {
        if !target.overwrite && fs::try_exists(&target.path).await.map_err(file_io)? {
            return Err(EngineError::FileIo(ErrorDetail::from(
                "destination already exists",
            )));
        }
        let mut state = create_download_state(&target.path).await?;
        let result = self
            .download_authenticated_with_state(request, target, success_statuses, &mut state)
            .await;
        if result.is_err() {
            discard_download_state(&mut state).await;
        }
        result
    }

    async fn download_authenticated_with_state(
        &self,
        request: &PreparedRequest,
        target: &DownloadTarget,
        success_statuses: &[u16],
        state: &mut DownloadState,
    ) -> Result<DownloadData, EngineError> {
        let max_attempts = request.retry.max_attempts.max(1);
        let can_retry = request.method.is_idempotent() || request.retry.retry_non_idempotent;
        let mut network_requests = 0_u64;
        let mut rate_limit_wait_ms = 0_u64;

        for attempt in 1..=max_attempts {
            let mut attempt_request = request.clone();
            let resume_etag = if target.resume && state.bytes_written > 0 {
                state.etag.clone()
            } else {
                None
            };
            let resumed = resume_etag.is_some();
            if let Some(etag) = resume_etag {
                set_request_header(
                    &mut attempt_request.headers,
                    RANGE.as_str(),
                    format!("bytes={}-", state.bytes_written),
                );
                set_request_header(&mut attempt_request.headers, IF_RANGE.as_str(), etag);
            }
            let pending = match self.send_once_response(&attempt_request, attempt > 1).await {
                Ok(pending) => pending,
                Err(error)
                    if can_retry
                        && attempt < max_attempts
                        && is_retryable_transport_error(&error) =>
                {
                    sleep(retry_delay(&request.retry, attempt, None)).await;
                    continue;
                }
                Err(error) => return Err(error),
            };
            network_requests = network_requests.saturating_add(pending.network_requests);
            rate_limit_wait_ms = rate_limit_wait_ms.saturating_add(pending.rate_limit_wait_ms);
            let status = pending.response.status().as_u16();
            let retry_after_ms = response_retry_after(&pending.response);
            if can_retry
                && request.retry.retry_on_status.contains(&status)
                && attempt < max_attempts
            {
                sleep(retry_delay(&request.retry, attempt, retry_after_ms)).await;
                continue;
            }

            match self
                .write_download_attempt(pending, target, success_statuses, state, resumed)
                .await
            {
                Ok(mut response) => {
                    response.attempts = attempt;
                    response.network_requests = network_requests;
                    response.rate_limit_wait_ms = rate_limit_wait_ms;
                    return Ok(response);
                }
                Err(error)
                    if can_retry
                        && attempt < max_attempts
                        && is_retryable_transport_error(&error) =>
                {
                    if !(target.resume
                        && state.bytes_written > 0
                        && state.etag.as_deref().is_some())
                    {
                        reset_download_state(state).await?;
                    }
                    sleep(retry_delay(&request.retry, attempt, None)).await;
                }
                Err(error) => return Err(error),
            }
        }

        Err(EngineError::Runtime(ErrorDetail::from(
            "download retry loop terminated unexpectedly",
        )))
    }

    async fn oauth_token(
        &self,
        original: &PreparedRequest,
    ) -> Result<(String, RequestStats), EngineError> {
        let (token_url, form, request_auth, is_arcgis, fallback_ttl) =
            token_request_parts(&original.auth)?;
        let key = TokenKey {
            token_url: token_url.clone(),
            auth_fingerprint: fingerprint(&original.auth),
            transport_fingerprint: fingerprint(&(&original.tls, &original.proxy)),
        };
        if let Some(token) = self
            .tokens
            .lock()
            .await
            .get(&key)
            .filter(|token| token.expires_at > Instant::now())
            .map(|token| token.token.clone())
        {
            return Ok((token, RequestStats::default()));
        }
        let _refresh = self.token_refresh.lock().await;
        if let Some(token) = self
            .tokens
            .lock()
            .await
            .get(&key)
            .filter(|token| token.expires_at > Instant::now())
            .map(|token| token.token.clone())
        {
            return Ok((token, RequestStats::default()));
        }

        let url = Url::parse(&token_url).map_err(|_| {
            EngineError::InvalidUrl(ErrorDetail::from("OAuth token URL is invalid"))
        })?;
        let token_request = PreparedRequest {
            url,
            method: HttpMethod::Post,
            headers: BTreeMap::new(),
            auth: request_auth,
            body: PreparedBody::Form(form.into_iter().collect()),
            timeout: Duration::from_millis(self.config.request_timeout_ms),
            allow_redirects: false,
            max_redirects: 0,
            retry: RetryPolicy {
                max_attempts: 2,
                retry_non_idempotent: true,
                ..RetryPolicy::default()
            },
            cookies: CookiePolicy::default(),
            admitted_jar: None,
            caller_session: None,
            cache: CachePolicy::default(),
            circuit_breaker: CircuitBreakerPolicy::default(),
            requests_per_second: original.requests_per_second,
            tls: original.tls.clone(),
            proxy: original.proxy.clone(),
        };
        let response = self.execute_authenticated(&token_request).await?;
        let stats = RequestStats {
            network_requests: response.network_requests,
            retries: u64::from(response.attempts.saturating_sub(1)),
            rate_limit_wait_ms: response.rate_limit_wait_ms,
        };
        if !(200..300).contains(&response.status) {
            return Err(EngineError::Authentication(ErrorDetail::from(
                "token endpoint returned an unsuccessful HTTP status",
            )));
        }

        let payload: Value = serde_json::from_slice(&response.body).map_err(|_| {
            EngineError::Authentication(ErrorDetail::from("token endpoint returned invalid JSON"))
        })?;
        if is_arcgis && payload.get("error").is_some() {
            return Err(EngineError::Authentication(ErrorDetail::from(
                "ArcGIS token endpoint returned an error",
            )));
        }
        let token_field = if is_arcgis { "token" } else { "access_token" };
        let token = payload
            .get(token_field)
            .and_then(Value::as_str)
            .filter(|token| !token.is_empty())
            .ok_or_else(|| {
                EngineError::Authentication(ErrorDetail::from("token response has no token field"))
            })?
            .to_owned();
        if !is_arcgis
            && payload
                .get("token_type")
                .and_then(Value::as_str)
                .is_some_and(|token_type| !token_type.eq_ignore_ascii_case("bearer"))
        {
            return Err(EngineError::Authentication(ErrorDetail::from(
                "only bearer OAuth tokens are supported",
            )));
        }
        let expires_in = if is_arcgis {
            payload
                .get("expires")
                .and_then(number_as_u64)
                .and_then(|expires_ms| {
                    let now_ms = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .ok()?
                        .as_millis() as u64;
                    expires_ms.checked_sub(now_ms).map(|value| value / 1_000)
                })
                .unwrap_or(fallback_ttl)
        } else {
            payload
                .get("expires_in")
                .and_then(number_as_u64)
                .unwrap_or(fallback_ttl)
        };
        let refresh_margin = (expires_in / 10).clamp(1, 30);
        let ttl = expires_in.saturating_sub(refresh_margin).max(1);
        let sequence = self.next_sequence();
        {
            let mut tokens = self.tokens.lock().await;
            let now = Instant::now();
            tokens.retain(|_, cached| cached.expires_at > now);
            while tokens.len() >= MAX_CACHED_TOKENS {
                let oldest = tokens
                    .iter()
                    .min_by_key(|(_, cached)| (cached.expires_at, cached.sequence))
                    .map(|(key, _)| key.clone());
                let Some(oldest) = oldest else {
                    break;
                };
                tokens.remove(&oldest);
            }
            tokens.insert(
                key,
                CachedToken {
                    token: token.clone(),
                    expires_at: now + Duration::from_secs(ttl),
                    sequence,
                },
            );
        }
        Ok((token, stats))
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
            let client = self
                .client_for(&url, &request.tls, request.proxy.as_ref(), request)
                .await?;
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
            tally(|tally| {
                tally.requests.fetch_add(1, Ordering::Relaxed);
                if retry && redirects == 0 {
                    tally.retries.fetch_add(1, Ordering::Relaxed);
                }
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

    async fn write_download_attempt(
        &self,
        pending: PendingResponse,
        target: &DownloadTarget,
        success_statuses: &[u16],
        state: &mut DownloadState,
        resumed: bool,
    ) -> Result<DownloadData, EngineError> {
        let PendingResponse {
            response,
            final_url,
            network_requests,
            rate_limit_wait_ms,
            permit: _permit,
        } = pending;
        let status = response.status().as_u16();
        let headers = response_headers(&response);
        if !(200..300).contains(&status) && !success_statuses.contains(&status) {
            return Err(EngineError::HttpStatus { status });
        }
        if resumed && !matches!(status, 200 | 206) {
            return Err(EngineError::InvalidResponse(ErrorDetail::from(
                "resumed download returned a status other than 200 or 206",
            )));
        }
        let content_length = response_content_length(&response);
        let response_etag = strong_response_etag(&response);
        let expected_total = if status == 206 {
            let content_range = satisfied_content_range(&response)?;
            let total = content_range.total;
            let segment_length = content_range
                .end
                .checked_sub(content_range.start)
                .and_then(|length| length.checked_add(1))
                .ok_or_else(|| {
                    EngineError::InvalidResponse(ErrorDetail::from(
                        "download Content-Range has invalid bounds",
                    ))
                })?;
            if content_length.is_some_and(|length| length != segment_length) {
                return Err(EngineError::InvalidResponse(ErrorDetail::from(
                    "download Content-Length does not match Content-Range",
                )));
            }
            if resumed {
                if content_range.start != state.bytes_written {
                    return Err(EngineError::InvalidResponse(ErrorDetail::from(
                        "resumed download started at an unexpected byte",
                    )));
                }
                if response_etag.as_deref() != state.etag.as_deref() {
                    return Err(EngineError::InvalidResponse(ErrorDetail::from(
                        "resumed download returned a missing or different strong ETag",
                    )));
                }
                if state
                    .expected_total
                    .is_some_and(|expected| expected != total)
                {
                    return Err(EngineError::InvalidResponse(ErrorDetail::from(
                        "resumed download changed the complete representation length",
                    )));
                }
            } else if content_range.start != 0 || content_range.end.checked_add(1) != Some(total) {
                return Err(EngineError::InvalidResponse(ErrorDetail::from(
                    "partial response cannot be promoted as a complete download",
                )));
            } else {
                state.etag = response_etag;
            }
            state.expected_total = Some(total);
            Some(total)
        } else {
            if state.bytes_written > 0 {
                reset_download_state(state).await?;
            }
            state.etag = response_etag;
            state.expected_total = content_length;
            content_length
        };
        if expected_total.is_some_and(|length| length > target.max_bytes) {
            return Err(EngineError::FileTooLarge {
                limit_bytes: target.max_bytes,
            });
        }

        let attempt_start = state.bytes_written;
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(map_reqwest_error)?;
            let chunk_length = u64::try_from(chunk.len()).map_err(|_| {
                EngineError::Runtime(ErrorDetail::from("download chunk length overflowed u64"))
            })?;
            if state.bytes_written.saturating_add(chunk_length) > target.max_bytes {
                return Err(EngineError::FileTooLarge {
                    limit_bytes: target.max_bytes,
                });
            }
            state
                .file
                .as_mut()
                .ok_or_else(|| {
                    EngineError::Runtime(ErrorDetail::from("download staging file is closed"))
                })?
                .write_all(&chunk)
                .await
                .map_err(download_write_io)?;
            state.digest.update(&chunk);
            state.bytes_written = state.bytes_written.saturating_add(chunk_length);
            state.bytes_received = state.bytes_received.saturating_add(chunk_length);
        }
        let attempt_bytes = state.bytes_written.saturating_sub(attempt_start);
        if content_length.is_some_and(|length| length != attempt_bytes) {
            return Err(EngineError::InvalidResponse(ErrorDetail::from(
                "download body length does not match Content-Length",
            )));
        }
        if expected_total.is_some_and(|total| state.bytes_written != total) {
            return Err(EngineError::InvalidResponse(ErrorDetail::from(
                "download body length does not match the complete representation",
            )));
        }
        let file = state.file.as_mut().ok_or_else(|| {
            EngineError::Runtime(ErrorDetail::from("download staging file is closed"))
        })?;
        file.flush().await.map_err(download_write_io)?;
        file.sync_all().await.map_err(download_write_io)?;
        drop(state.file.take());

        let sha256 = format!("{:x}", state.digest.clone().finalize());
        if let Some(expected) = &target.expected_sha256 {
            if !sha256.eq_ignore_ascii_case(expected) {
                return Err(EngineError::ChecksumMismatch);
            }
        }
        persist_download(&state.temporary, &target.path, target.overwrite).await?;
        state.cleanup = false;
        Ok(DownloadData {
            status,
            final_url,
            attempts: 1,
            network_requests,
            auth_requests: 0,
            auth_retries: 0,
            rate_limit_wait_ms,
            headers,
            bytes_written: state.bytes_written,
            bytes_received: state.bytes_received,
            sha256,
        })
    }

    async fn admit_request(
        &self,
        connection_rate: Option<f64>,
    ) -> Result<(OwnedSemaphorePermit, u64), EngineError> {
        let rate = connection_rate
            .filter(|rate| rate.is_finite() && *rate > 0.0)
            .or_else(|| self.config.requests_per_second.map(f64::from));
        let wait = if let Some(requests_per_second) = rate {
            let interval = Duration::from_nanos(
                (1_000_000_000_f64 / requests_per_second).clamp(0.0, u64::MAX as f64) as u64,
            );
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

    async fn client_for(
        &self,
        url: &Url,
        tls: &TlsConfig,
        proxy: Option<&ProxyConfig>,
        request: &PreparedRequest,
    ) -> Result<Client, EngineError> {
        if !tls.verify && !self.config.allow_insecure_tls {
            return Err(EngineError::PolicyViolation(ErrorDetail::from(
                "TLS verification cannot be disabled by this engine",
            )));
        }
        if proxy.is_some() && !self.config.allow_proxies {
            return Err(EngineError::PolicyViolation(ErrorDetail::from(
                "proxies are not enabled for this engine",
            )));
        }

        let (host, address, should_pin, port) = self.resolve_endpoint(url).await?;
        let proxy_endpoint = match proxy {
            Some(proxy) => {
                let url = Url::parse(&proxy.url).map_err(|_| {
                    EngineError::InvalidInput(ErrorDetail::from("invalid proxy URL"))
                })?;
                Some((proxy, url.clone(), self.resolve_endpoint(&url).await?))
            }
            None => None,
        };
        // The jar admitted with the operation, never a fresh resolution of the
        // handle: the operation's lease keeps it alive, and resolving again
        // here could refuse a session closed in the meantime after an OAuth
        // token had already been fetched.
        let jar = match (&request.cookies.session, &request.admitted_jar) {
            (None, _) => None,
            (Some(_), Some(jar)) => Some(jar),
            (Some(_), None) => {
                return Err(EngineError::Runtime(ErrorDetail::from(
                    "a cookie session was used without being admitted",
                )));
            }
        };
        let key = ClientKey {
            host: host.clone(),
            port,
            address,
            policy_fingerprint: fingerprint(&(tls, proxy)),
            cookie_jar: jar.map(|jar| jar.incarnation),
        };
        if let Some(pooled) = self.clients.lock().await.get(&key) {
            return Ok(pooled.client.clone());
        }

        let mut builder = Client::builder()
            .connect_timeout(Duration::from_millis(self.config.connect_timeout_ms))
            .pool_idle_timeout(Duration::from_millis(self.config.pool_idle_timeout_ms))
            .pool_max_idle_per_host(self.config.pool_max_idle_per_host)
            .redirect(Policy::none())
            .no_proxy()
            .user_agent(&self.config.user_agent);
        if let Some(jar) = jar {
            builder = builder.cookie_provider(jar.jar.clone());
        }
        if !self.config.automatic_decompression {
            builder = builder.no_brotli().no_deflate().no_gzip().no_zstd();
        }
        if !tls.verify {
            builder = builder.danger_accept_invalid_certs(true);
        }
        if let Some(pem) = &tls.ca_bundle_pem {
            let certificate = Certificate::from_pem(pem.as_bytes()).map_err(|_| {
                EngineError::InvalidInput(ErrorDetail::from("invalid TLS CA bundle PEM"))
            })?;
            builder = builder.add_root_certificate(certificate);
        }
        if let Some(pem) = &tls.client_identity_pem {
            let identity = Identity::from_pem(pem.as_bytes()).map_err(|_| {
                EngineError::InvalidInput(ErrorDetail::from("invalid TLS client identity PEM"))
            })?;
            builder = builder.identity(identity);
        }
        if should_pin {
            builder = builder.resolve(&host, SocketAddr::new(address, port));
        }
        if let Some((config, _, (proxy_host, proxy_address, proxy_pin, proxy_port))) =
            proxy_endpoint
        {
            let mut configured = Proxy::all(&config.url)
                .map_err(|_| EngineError::InvalidInput(ErrorDetail::from("invalid proxy URL")))?;
            match (&config.username, &config.password) {
                (Some(username), password) => {
                    configured = configured.basic_auth(username, password.as_deref().unwrap_or(""));
                }
                (None, Some(_)) => {
                    return Err(EngineError::InvalidInput(ErrorDetail::from(
                        "proxy password requires a username",
                    )));
                }
                (None, None) => {}
            }
            builder = builder.proxy(configured);
            if proxy_pin {
                builder = builder.resolve(&proxy_host, SocketAddr::new(proxy_address, proxy_port));
            }
        }
        let client = builder.build().map_err(|_| {
            EngineError::Runtime(ErrorDetail::from("HTTP client could not be built"))
        })?;
        if self.config.max_pooled_origins > 0 {
            let sequence = self.next_sequence();
            let mut clients = self.clients.lock().await;
            if clients.len() >= self.config.max_pooled_origins {
                let oldest = clients
                    .iter()
                    .min_by_key(|(_, pooled)| pooled.sequence)
                    .map(|(key, _)| key.clone());
                if let Some(oldest) = oldest {
                    clients.remove(&oldest);
                }
            }
            clients.insert(
                key,
                PooledClient {
                    client: client.clone(),
                    sequence,
                },
            );
        }
        Ok(client)
    }

    async fn resolve_endpoint(
        &self,
        url: &Url,
    ) -> Result<(String, IpAddr, bool, u16), EngineError> {
        validate_url(url)?;
        let port = url
            .port_or_known_default()
            .ok_or_else(|| EngineError::InvalidUrl(ErrorDetail::from("URL has no usable port")))?;
        let (host, address, should_pin) = match url
            .host()
            .ok_or_else(|| EngineError::InvalidUrl(ErrorDetail::from("URL must include a host")))?
        {
            Host::Ipv4(address) => {
                let address = IpAddr::V4(address);
                self.validate_address(address)?;
                (address.to_string(), address, false)
            }
            Host::Ipv6(address) => {
                let address = IpAddr::V6(address);
                self.validate_address(address)?;
                (address.to_string(), address, false)
            }
            Host::Domain(domain) => {
                let addresses: Vec<IpAddr> = lookup_host((domain, port))
                    .await
                    .map_err(|_| {
                        EngineError::DnsResolution(ErrorDetail::from("DNS resolution failed"))
                    })?
                    .map(|socket| socket.ip())
                    .collect();
                if addresses.is_empty() {
                    return Err(EngineError::DnsResolution(ErrorDetail::from(
                        "no addresses returned",
                    )));
                }
                for address in &addresses {
                    self.validate_address(*address)?;
                }
                (domain.to_owned(), addresses[0], true)
            }
        };
        Ok((host, address, should_pin, port))
    }

    fn validate_address(&self, address: IpAddr) -> Result<(), EngineError> {
        if self.config.allow_private_networks || is_public_address(address) {
            return Ok(());
        }
        Err(EngineError::UnsafeAddress(ErrorDetail::from(
            "the resolved address is private, loopback, or otherwise not public",
        )))
    }
}

fn validate_url(url: &Url) -> Result<(), EngineError> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(EngineError::InvalidUrl(ErrorDetail::from(
            "only http and https URLs are supported",
        )));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(EngineError::InvalidUrl(ErrorDetail::from(
            "credentials embedded in URLs are not allowed",
        )));
    }
    Ok(())
}

fn set_request_header(headers: &mut BTreeMap<String, String>, name: &str, value: String) {
    headers.retain(|existing, _| !existing.eq_ignore_ascii_case(name));
    headers.insert(name.to_owned(), value);
}

fn request_headers(request: &PreparedRequest) -> Result<HeaderMap, EngineError> {
    let mut headers = HeaderMap::new();
    for (name, value) in &request.headers {
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| EngineError::InvalidHeader(ErrorDetail::from("header name is invalid")))?;
        let value = HeaderValue::from_str(value).map_err(|_| {
            EngineError::InvalidHeader(ErrorDetail::from("header value is invalid"))
        })?;
        headers.insert(name, value);
    }

    if let AuthConfig::ApiKey {
        key_name,
        key_value,
        location: ApiKeyLocation::Header,
    } = &request.auth
    {
        let name = HeaderName::from_bytes(key_name.as_bytes()).map_err(|_| {
            EngineError::InvalidHeader(ErrorDetail::from("API key header name is invalid"))
        })?;
        let value = HeaderValue::from_str(key_value).map_err(|_| {
            EngineError::InvalidHeader(ErrorDetail::from("API key header value is invalid"))
        })?;
        headers.insert(name, value);
    }

    if matches!(request.body, PreparedBody::Multipart { .. }) && headers.contains_key(CONTENT_TYPE)
    {
        return Err(EngineError::InvalidHeader(ErrorDetail::from(
            "Content-Type must be generated by the multipart encoder",
        )));
    }
    if matches!(
        request.body,
        PreparedBody::Multipart { .. } | PreparedBody::Stream(_)
    ) && headers.contains_key(CONTENT_LENGTH)
    {
        return Err(EngineError::InvalidHeader(ErrorDetail::from(
            "Content-Length must be generated by the streaming encoder",
        )));
    }
    if !headers.contains_key(CONTENT_TYPE) {
        let content_type = match &request.body {
            PreparedBody::Json(_) => Some("application/json"),
            PreparedBody::Form(_) => Some("application/x-www-form-urlencoded"),
            PreparedBody::Multipart { .. } => None,
            PreparedBody::Stream(stream) => stream.content_type.as_deref(),
            PreparedBody::Raw(_) | PreparedBody::None => None,
        };
        if let Some(content_type) = content_type {
            let content_type = HeaderValue::from_str(content_type)
                .map_err(|_| EngineError::InvalidHeader(ErrorDetail::from("content-type")))?;
            headers.insert(CONTENT_TYPE, content_type);
        }
    }
    Ok(headers)
}

fn apply_query_auth(url: &mut Url, auth: &AuthConfig) {
    if let AuthConfig::ApiKey {
        key_name,
        key_value,
        location: ApiKeyLocation::Query,
    } = auth
    {
        url.query_pairs_mut().append_pair(key_name, key_value);
    }
}

async fn multipart_form(
    fields: &[(String, String)],
    files: &[PreparedFile],
) -> Result<Form, EngineError> {
    let mut form = Form::new();
    for (name, value) in fields {
        form = form.text(name.clone(), value.clone());
    }
    for file in files {
        let part = match &file.source {
            PreparedFileSource::Bytes(data) => Part::bytes(data.clone()),
            PreparedFileSource::Path { path, length } => {
                let source = fs::File::open(path).await.map_err(file_io)?;
                let body = Body::wrap_stream(ReaderStream::new(source));
                Part::stream_with_length(body, *length)
            }
        };
        let mut part = part.file_name(file.filename.clone());
        if let Some(content_type) = &file.content_type {
            part = part.mime_str(content_type).map_err(|_| {
                EngineError::InvalidInput(ErrorDetail::from("invalid multipart content type"))
            })?;
        }
        form = form.part(file.field_name.clone(), part);
    }
    Ok(form)
}

async fn stream_body(
    builder: reqwest::RequestBuilder,
    stream: &PreparedStream,
) -> Result<reqwest::RequestBuilder, EngineError> {
    let source = fs::File::open(&stream.path).await.map_err(file_io)?;
    let body = Body::wrap_stream(ReaderStream::new(source));
    Ok(builder.header(CONTENT_LENGTH, stream.length).body(body))
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

fn response_content_length(response: &reqwest::Response) -> Option<u64> {
    response
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
}

fn strong_response_etag(response: &reqwest::Response) -> Option<String> {
    strong_etag(response.headers().get(ETAG)?.to_str().ok()?)
}

/// The strong validator carried by an `ETag` value, if it is one.
pub(crate) fn strong_etag(value: &str) -> Option<String> {
    let value = value.trim();
    if value.starts_with("W/")
        || value.len() < 2
        || !value.starts_with('"')
        || !value.ends_with('"')
    {
        None
    } else {
        Some(value.to_owned())
    }
}

pub(crate) struct ContentRange {
    pub(crate) start: u64,
    pub(crate) end: u64,
    pub(crate) total: u64,
}

fn satisfied_content_range(response: &reqwest::Response) -> Result<ContentRange, EngineError> {
    let value = response
        .headers()
        .get(CONTENT_RANGE)
        .ok_or_else(|| {
            EngineError::InvalidResponse(ErrorDetail::from(
                "206 response has no Content-Range header",
            ))
        })?
        .to_str()
        .map_err(|_| {
            EngineError::InvalidResponse(ErrorDetail::from(
                "download Content-Range is not valid text",
            ))
        })?;
    parse_content_range(value)
}

/// A satisfied `Content-Range` value: `bytes <start>-<end>/<total>`.
pub(crate) fn parse_content_range(value: &str) -> Result<ContentRange, EngineError> {
    let (unit, range_and_total) = value.trim().split_once(' ').ok_or_else(|| {
        EngineError::InvalidResponse(ErrorDetail::from("download Content-Range is malformed"))
    })?;
    if !unit.eq_ignore_ascii_case("bytes") {
        return Err(EngineError::InvalidResponse(ErrorDetail::from(
            "download Content-Range must use byte units",
        )));
    }
    let (range, total) = range_and_total.split_once('/').ok_or_else(|| {
        EngineError::InvalidResponse(ErrorDetail::from("download Content-Range is malformed"))
    })?;
    let (start, end) = range.split_once('-').ok_or_else(|| {
        EngineError::InvalidResponse(ErrorDetail::from(
            "download Content-Range does not describe a satisfied range",
        ))
    })?;
    let start = start.parse::<u64>().map_err(|_| {
        EngineError::InvalidResponse(ErrorDetail::from("download Content-Range start is invalid"))
    })?;
    let end = end.parse::<u64>().map_err(|_| {
        EngineError::InvalidResponse(ErrorDetail::from("download Content-Range end is invalid"))
    })?;
    let total = total.parse::<u64>().map_err(|_| {
        EngineError::InvalidResponse(ErrorDetail::from("download Content-Range total is invalid"))
    })?;
    if start > end || end >= total {
        return Err(EngineError::InvalidResponse(ErrorDetail::from(
            "download Content-Range bounds are invalid",
        )));
    }
    Ok(ContentRange { start, end, total })
}

fn session_refusal_error(refusal: SessionRefusal) -> EngineError {
    EngineError::PolicyViolation(ErrorDetail::from(match refusal {
        SessionRefusal::ForeignEngine => "the cookie session handle was issued by another engine",
        SessionRefusal::Stale => "the cookie session was closed or evicted; open a new session",
    }))
}

fn download_temporary_path(target: &Path) -> PathBuf {
    let sequence = DOWNLOAD_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let filename = target
        .file_name()
        .map(|value| value.to_string_lossy())
        .unwrap_or_else(|| "download".into());
    target.with_file_name(format!(
        ".{filename}.rest-engine-{}-{sequence}.part",
        std::process::id()
    ))
}

async fn create_download_state(target: &Path) -> Result<DownloadState, EngineError> {
    // No `.await` between creating the staging file and arming its Drop guard:
    // a cancellation can only drop this future before the file exists or after
    // the guard owns it.
    let (temporary, file) = create_download_file(target)?;
    Ok(DownloadState {
        temporary,
        file: Some(file),
        bytes_written: 0,
        bytes_received: 0,
        digest: Sha256::new(),
        etag: None,
        expected_total: None,
        cleanup: true,
    })
}

async fn reset_download_state(state: &mut DownloadState) -> Result<(), EngineError> {
    drop(state.file.take());
    let file = OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(&state.temporary)
        .await
        // A retry of a download whose request already went out: the remote
        // side may have acted, so this is not a failure without effect.
        .map_err(download_write_io)?;
    state.file = Some(file);
    state.bytes_written = 0;
    state.digest = Sha256::new();
    state.etag = None;
    state.expected_total = None;
    Ok(())
}

async fn discard_download_state(state: &mut DownloadState) {
    drop(state.file.take());
    // Only disarm when the staging file is really gone; otherwise leave the
    // Drop guard to try again rather than leaking a partial download.
    if fs::remove_file(&state.temporary).await.is_ok() {
        state.cleanup = false;
    }
}

/// Creates the staging file synchronously.
///
/// `tokio::fs` runs the open on a blocking thread, and that thread finishes the
/// call even when the awaiting future is dropped: a download cancelled during
/// the open left a `.part` file that no guard owned. Creating an empty file is a
/// single short system call, so it is done inline instead, and the caller arms
/// the cleanup guard before yielding.
fn create_download_file(target: &Path) -> Result<(PathBuf, fs::File), EngineError> {
    for _ in 0..16 {
        let temporary = download_temporary_path(target);
        match std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)
        {
            Ok(file) => return Ok((temporary, fs::File::from_std(file))),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(file_io(error)),
        }
    }
    Err(EngineError::FileIo(ErrorDetail::from(
        "could not allocate a unique temporary download file",
    )))
}

async fn persist_download(
    temporary: &Path,
    target: &Path,
    overwrite: bool,
) -> Result<(), EngineError> {
    if !overwrite {
        fs::hard_link(temporary, target)
            .await
            .map_err(download_write_io)?;
        // The sink now holds the download: a failure from here on leaves the
        // publication in place and only the staging file behind.
        fs::remove_file(temporary)
            .await
            .map_err(|error| EngineError::CleanupAfterPublish(crate::error::io_detail(&error)))?;
        return Ok(());
    }
    match fs::rename(temporary, target).await {
        Ok(()) => Ok(()),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::AlreadyExists | std::io::ErrorKind::PermissionDenied
            ) && fs::try_exists(target).await.map_err(download_write_io)? =>
        {
            fs::remove_file(target).await.map_err(download_write_io)?;
            fs::rename(temporary, target)
                .await
                .map_err(download_write_io)
        }
        Err(error) => Err(download_write_io(error)),
    }
}

fn file_io(error: std::io::Error) -> EngineError {
    EngineError::FileIo(crate::error::io_detail(&error))
}

/// A local write of a download, after its HTTP request went out. The request
/// may have had a remote effect (a download is not restricted to safe
/// methods), so the failure cannot claim that nothing happened remotely.
fn download_write_io(error: std::io::Error) -> EngineError {
    EngineError::DownloadWrite(crate::error::io_detail(&error))
}

type TokenRequestParts = (String, BTreeMap<String, String>, AuthConfig, bool, u64);

fn token_request_parts(auth: &AuthConfig) -> Result<TokenRequestParts, EngineError> {
    match auth {
        AuthConfig::OAuth2ClientCredentials {
            token_url,
            client_id,
            client_secret,
            scope,
            audience,
            extra_params,
            client_auth,
        } => {
            let mut form = extra_params.clone();
            form.insert("grant_type".to_owned(), "client_credentials".to_owned());
            if let Some(scope) = scope {
                form.insert("scope".to_owned(), scope.clone());
            }
            if let Some(audience) = audience {
                form.insert("audience".to_owned(), audience.clone());
            }
            let request_auth = match client_auth {
                OAuthClientAuth::Basic => AuthConfig::Basic {
                    username: client_id.clone(),
                    password: client_secret.clone(),
                },
                OAuthClientAuth::Body => {
                    form.insert("client_id".to_owned(), client_id.clone());
                    form.insert("client_secret".to_owned(), client_secret.clone());
                    AuthConfig::None
                }
            };
            Ok((token_url.clone(), form, request_auth, false, 300))
        }
        AuthConfig::OAuth2Password {
            token_url,
            username,
            password,
            client_id,
            client_secret,
            scope,
            extra_params,
        } => {
            let mut form = extra_params.clone();
            form.insert("grant_type".to_owned(), "password".to_owned());
            form.insert("username".to_owned(), username.clone());
            form.insert("password".to_owned(), password.clone());
            if let Some(client_id) = client_id {
                form.insert("client_id".to_owned(), client_id.clone());
            }
            if let Some(client_secret) = client_secret {
                form.insert("client_secret".to_owned(), client_secret.clone());
            }
            if let Some(scope) = scope {
                form.insert("scope".to_owned(), scope.clone());
            }
            Ok((token_url.clone(), form, AuthConfig::None, false, 300))
        }
        AuthConfig::ArcgisToken {
            token_url,
            username,
            password,
            client,
            referer,
            ip,
            expiration,
        } => {
            if !matches!(client.as_str(), "requestip" | "referer" | "ip") {
                return Err(EngineError::InvalidInput(ErrorDetail::from(
                    "ArcGIS client must be requestip, referer, or ip",
                )));
            }
            let mut form = BTreeMap::from([
                ("username".to_owned(), username.clone()),
                ("password".to_owned(), password.clone()),
                ("client".to_owned(), client.clone()),
                ("expiration".to_owned(), expiration.to_string()),
                ("f".to_owned(), "json".to_owned()),
            ]);
            if client == "referer" {
                form.insert(
                    "referer".to_owned(),
                    referer.clone().ok_or_else(|| {
                        EngineError::InvalidInput(ErrorDetail::from(
                            "ArcGIS referer client requires referer",
                        ))
                    })?,
                );
            } else if client == "ip" {
                form.insert(
                    "ip".to_owned(),
                    ip.clone().ok_or_else(|| {
                        EngineError::InvalidInput(ErrorDetail::from("ArcGIS ip client requires ip"))
                    })?,
                );
            }
            Ok((
                token_url.clone(),
                form,
                AuthConfig::None,
                true,
                u64::from(*expiration).saturating_mul(60),
            ))
        }
        _ => Err(EngineError::Runtime(ErrorDetail::from(
            "token requested for non-token authentication",
        ))),
    }
}

fn number_as_u64(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
}

/// Feeds `Hash` output into SHA-256 so isolation keys keep a collision resistant
/// fingerprint instead of a 64 bit `DefaultHasher` value.
#[derive(Default)]
struct DigestHasher {
    digest: Sha256,
}

impl DigestHasher {
    fn fingerprint(self) -> Fingerprint {
        self.digest.finalize().into()
    }
}

impl Hasher for DigestHasher {
    fn write(&mut self, bytes: &[u8]) {
        self.digest.update(bytes);
    }

    fn finish(&self) -> u64 {
        self.digest
            .clone()
            .finalize()
            .iter()
            .take(8)
            .fold(0_u64, |accumulator, byte| {
                (accumulator << 8) | u64::from(*byte)
            })
    }
}

fn fingerprint(value: &(impl Hash + ?Sized)) -> Fingerprint {
    let mut hasher = DigestHasher::default();
    value.hash(&mut hasher);
    hasher.fingerprint()
}

fn cache_key(request: &PreparedRequest) -> Result<CacheKey, EngineError> {
    let mut hasher = DigestHasher::default();
    request.headers.hash(&mut hasher);
    request.auth.hash(&mut hasher);
    request.cookies.hash(&mut hasher);
    // The transport identity is part of who is asking, not just of how the
    // connection is made: two requests that differ only by client certificate,
    // trust anchor, or proxy are different requests and must not share an entry.
    // The connection pool already isolates them; the cache is consulted before
    // the pool, so it needs the same isolation of its own.
    request.tls.hash(&mut hasher);
    request.proxy.hash(&mut hasher);
    // The redirect policy decides which resource actually answered: a request
    // that followed a redirect must not serve its answer to one that forbids
    // them.
    request.allow_redirects.hash(&mut hasher);
    request.max_redirects.hash(&mut hasher);
    match &request.body {
        PreparedBody::None => 0_u8.hash(&mut hasher),
        PreparedBody::Json(value) => {
            1_u8.hash(&mut hasher);
            serde_json::to_vec(value)
                .map_err(|_| {
                    EngineError::InvalidInput(ErrorDetail::from(
                        "JSON body could not be serialized for the cache key",
                    ))
                })?
                .hash(&mut hasher);
        }
        PreparedBody::Form(values) => {
            2_u8.hash(&mut hasher);
            values.hash(&mut hasher);
        }
        PreparedBody::Multipart { fields, files } => {
            3_u8.hash(&mut hasher);
            fields.hash(&mut hasher);
            for file in files {
                file.field_name.hash(&mut hasher);
                file.filename.hash(&mut hasher);
                file.content_type.hash(&mut hasher);
                match &file.source {
                    PreparedFileSource::Bytes(data) => data.hash(&mut hasher),
                    PreparedFileSource::Path { .. } => {
                        return Err(EngineError::InvalidInput(ErrorDetail::from(
                            "HTTP cache cannot fingerprint streaming multipart files",
                        )));
                    }
                }
            }
        }
        PreparedBody::Raw(value) => {
            4_u8.hash(&mut hasher);
            value.hash(&mut hasher);
        }
        PreparedBody::Stream(_) => {
            return Err(EngineError::InvalidInput(ErrorDetail::from(
                "HTTP cache cannot fingerprint streaming request bodies",
            )));
        }
    }
    Ok(CacheKey {
        method: request.method.as_str().to_owned(),
        url: request.url.as_str().to_owned(),
        request_fingerprint: hasher.fingerprint(),
    })
}

fn cached_response(
    cached: &CachedResponse,
    cache_hits: u64,
    cache_revalidations: u64,
) -> ResponseData {
    ResponseData {
        status: cached.status,
        body: cached.body.clone(),
        final_url: cached.final_url.clone(),
        // The public contract requires `attempts >= 1`; a cache hit still
        // resolves one request attempt. `network_requests` stays zero because
        // that is the metric describing actual network traffic.
        attempts: 1,
        network_requests: 0,
        auth_requests: 0,
        auth_retries: 0,
        rate_limit_wait_ms: 0,
        cache_hits,
        cache_revalidations,
        headers: cached.headers.clone(),
        retry_after_ms: None,
    }
}

fn add_conditional_headers(
    request_headers: &mut BTreeMap<String, String>,
    cached_headers: &BTreeMap<String, String>,
) {
    if !contains_header(request_headers, IF_NONE_MATCH.as_str()) {
        if let Some(etag) = cached_headers.get(ETAG.as_str()) {
            request_headers.insert(IF_NONE_MATCH.as_str().to_owned(), etag.clone());
        }
    }
    if !contains_header(request_headers, IF_MODIFIED_SINCE.as_str()) {
        if let Some(last_modified) = cached_headers.get(LAST_MODIFIED.as_str()) {
            request_headers.insert(IF_MODIFIED_SINCE.as_str().to_owned(), last_modified.clone());
        }
    }
}

fn contains_header(headers: &BTreeMap<String, String>, name: &str) -> bool {
    headers
        .keys()
        .any(|existing| existing.eq_ignore_ascii_case(name))
}

fn merge_headers(cached: &mut BTreeMap<String, String>, revalidated: &BTreeMap<String, String>) {
    for (name, value) in revalidated {
        if !name.eq_ignore_ascii_case(CONTENT_LENGTH.as_str()) {
            cached.insert(name.clone(), value.clone());
        }
    }
}

pub(crate) fn response_forbids_store(headers: &BTreeMap<String, String>) -> bool {
    header_has_directive(headers, CACHE_CONTROL.as_str(), "no-store")
        || headers
            .get(VARY.as_str())
            .is_some_and(|value| value.split(',').any(|value| value.trim() == "*"))
}

pub(crate) fn response_requires_revalidation(headers: &BTreeMap<String, String>) -> bool {
    header_has_directive(headers, CACHE_CONTROL.as_str(), "no-cache")
}

pub(crate) fn cache_max_age_ms(headers: &BTreeMap<String, String>) -> Option<u64> {
    headers
        .get(CACHE_CONTROL.as_str())?
        .split(',')
        .find_map(|directive| {
            let (name, value) = directive.trim().split_once('=')?;
            name.trim()
                .eq_ignore_ascii_case("max-age")
                .then(|| value.trim().trim_matches('"').parse::<u64>().ok())
                .flatten()
        })
        .and_then(|seconds| seconds.checked_mul(1_000))
}

fn header_has_directive(headers: &BTreeMap<String, String>, header: &str, directive: &str) -> bool {
    headers.get(header).is_some_and(|value| {
        value
            .split(',')
            .filter_map(|value| {
                value
                    .trim()
                    .split_once('=')
                    .map_or_else(|| Some(value.trim()), |(name, _)| Some(name.trim()))
            })
            .any(|value| value.eq_ignore_ascii_case(directive))
    })
}

fn cached_response_size(body: &[u8], headers: &BTreeMap<String, String>) -> usize {
    headers.iter().fold(body.len(), |size, (name, value)| {
        size.saturating_add(name.len()).saturating_add(value.len())
    })
}

fn method(method: &HttpMethod) -> Result<Method, EngineError> {
    match method {
        HttpMethod::Get => Ok(Method::GET),
        HttpMethod::Head => Ok(Method::HEAD),
        HttpMethod::Post => Ok(Method::POST),
        HttpMethod::Put => Ok(Method::PUT),
        HttpMethod::Patch => Ok(Method::PATCH),
        HttpMethod::Delete => Ok(Method::DELETE),
        HttpMethod::Options => Ok(Method::OPTIONS),
        HttpMethod::Custom(value) => Method::from_bytes(value.as_bytes()).map_err(|_| {
            EngineError::InvalidInput(ErrorDetail::from("invalid custom HTTP method"))
        }),
    }
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

fn is_retryable_transport_error(error: &EngineError) -> bool {
    matches!(
        error,
        EngineError::Timeout | EngineError::Transport(_) | EngineError::DnsResolution(_)
    )
}

fn retry_delay(policy: &RetryPolicy, attempt: u32, retry_after_ms: Option<u64>) -> Duration {
    if policy.respect_retry_after {
        if let Some(delay) = retry_after_ms {
            return Duration::from_millis(delay.min(policy.max_retry_after_ms));
        }
    }

    let exponent = attempt.saturating_sub(1) as i32;
    let delay = (policy.backoff_base_ms as f64 * policy.backoff_factor.max(1.0).powi(exponent))
        .min(policy.max_backoff_ms as f64)
        .max(0.0) as u64;
    Duration::from_millis(delay)
}

pub(crate) fn parse_retry_after(value: &str, now: SystemTime) -> Option<u64> {
    let value = value.trim();
    // delay-seconds has no upper bound (RFC 9110, 10.2.3). A value too large
    // for u64 milliseconds is the longest possible wait, not an absent
    // header: it saturates, and retry_delay then caps it at
    // max_retry_after_ms. Reading it as absent fell back to the exponential
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

fn is_public_address(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => is_public_ipv4(address),
        IpAddr::V6(address) => is_public_ipv6(address),
    }
}

fn is_public_ipv4(address: Ipv4Addr) -> bool {
    let octets = address.octets();
    !(address.is_private()
        || address.is_loopback()
        || address.is_link_local()
        || address.is_broadcast()
        || address.is_documentation()
        || address.is_unspecified()
        || address.is_multicast()
        || octets[0] == 0
        || (octets[0] == 100 && (64..=127).contains(&octets[1]))
        || (octets[0] == 192 && octets[1] == 0 && octets[2] == 0)
        || (octets[0] == 198 && (18..=19).contains(&octets[1]))
        || octets[0] >= 240)
}

fn is_public_ipv6(address: Ipv6Addr) -> bool {
    if let Some(mapped) = address.to_ipv4_mapped() {
        return is_public_ipv4(mapped);
    }
    let segments = address.segments();
    !(address.is_loopback()
        || address.is_unspecified()
        || address.is_multicast()
        || (segments[0] & 0xfe00) == 0xfc00
        || (segments[0] & 0xffc0) == 0xfe80
        || (segments[0] == 0x2001 && segments[1] == 0x0db8))
}

#[cfg(test)]
mod tests {
    use std::{
        net::{IpAddr, Ipv4Addr, Ipv6Addr},
        time::{Duration, UNIX_EPOCH},
    };

    use super::{is_public_address, parse_retry_after, same_origin};
    use reqwest::Url;

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
        assert_eq!(
            super::retry_delay(
                &policy,
                1,
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
