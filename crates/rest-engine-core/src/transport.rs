use std::{
    collections::{BTreeMap, HashMap},
    hash::{DefaultHasher, Hash, Hasher},
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::Arc,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use futures_util::StreamExt;
use reqwest::{
    Body, Certificate, Client, Identity, Method, Proxy, Url,
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
    ApiKeyLocation, AuthConfig, CachePolicy, CircuitBreakerPolicy, CookiePolicy, EngineConfig,
    EngineError, HttpMethod, OAuthClientAuth, ProxyConfig, RetryPolicy, TlsConfig,
};

static DOWNLOAD_SEQUENCE: AtomicU64 = AtomicU64::new(0);

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
    clients: Arc<Mutex<HashMap<ClientKey, Client>>>,
    tokens: Arc<Mutex<HashMap<TokenKey, CachedToken>>>,
    token_refresh: Arc<Mutex<()>>,
    cache: Arc<Mutex<CacheStore>>,
    circuits: Arc<Mutex<HashMap<CircuitKey, CircuitState>>>,
    concurrency: Arc<Semaphore>,
    rate_state: Arc<Mutex<RateState>>,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct ClientKey {
    host: String,
    port: u16,
    address: IpAddr,
    policy_fingerprint: u64,
    cookie_jar_id: Option<String>,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct TokenKey {
    token_url: String,
    auth_fingerprint: u64,
    transport_fingerprint: u64,
}

struct CachedToken {
    token: String,
    expires_at: Instant,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct CacheKey {
    method: String,
    url: String,
    request_fingerprint: u64,
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

#[derive(Default)]
struct CircuitState {
    consecutive_failures: u32,
    opened_at: Option<Instant>,
    half_open_in_flight: bool,
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
        }
    }

    pub async fn execute(&self, mut request: PreparedRequest) -> Result<ResponseData, EngineError> {
        self.validate_request(&request)?;
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
        if target.resume {
            if request.method != HttpMethod::Get {
                return Err(EngineError::InvalidInput(
                    "resumable downloads require the GET method".to_owned(),
                ));
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
                return Err(EngineError::InvalidInput(
                    "managed resume cannot be combined with Range or If-Range headers".to_owned(),
                ));
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
            return Err(EngineError::PolicyViolation(format!(
                "custom HTTP method '{}' is not in allowed_custom_methods",
                request.method.as_str()
            )));
        }
        if request.cookies.enabled {
            if !self.config.allow_cookie_store {
                return Err(EngineError::PolicyViolation(
                    "cookie storage is not enabled for this engine".to_owned(),
                ));
            }
            if request.cookies.jar_id.trim().is_empty() {
                return Err(EngineError::InvalidInput(
                    "cookie jar_id cannot be empty".to_owned(),
                ));
            }
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
            return Err(EngineError::InvalidInput(
                "HTTP cache is supported only for GET and HEAD".to_owned(),
            ));
        }
        if self.config.max_cache_entries == 0 || self.config.max_cache_bytes == 0 {
            return Err(EngineError::PolicyViolation(
                "HTTP cache capacity is disabled by this engine".to_owned(),
            ));
        }
        let authenticated = !matches!(request.auth, AuthConfig::None)
            || request
                .headers
                .keys()
                .any(|name| name.eq_ignore_ascii_case("authorization"))
            || request.cookies.enabled;
        if authenticated && !request.cache.allow_authenticated {
            return Err(EngineError::PolicyViolation(
                "authenticated HTTP caching requires allow_authenticated".to_owned(),
            ));
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
            };
            self.store_cache_entry(key, cached).await;
        }
        Ok(response)
    }

    async fn execute_with_circuit(
        &self,
        request: &PreparedRequest,
    ) -> Result<ResponseData, EngineError> {
        let key = self.admit_circuit(request).await?;
        let result = self.execute_authenticated(request).await;
        if let Some(key) = key {
            let failed = match &result {
                Ok(response) => request
                    .circuit_breaker
                    .failure_statuses
                    .contains(&response.status),
                Err(error) => is_retryable_transport_error(error),
            };
            self.record_circuit(&key, &request.circuit_breaker, failed)
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
                .min_by_key(|(_, value)| value.last_access)
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
    ) -> Result<Option<CircuitKey>, EngineError> {
        let policy = &request.circuit_breaker;
        if !policy.enabled {
            return Ok(None);
        }
        if policy.failure_threshold == 0 {
            return Err(EngineError::InvalidInput(
                "circuit breaker failure_threshold must be greater than zero".to_owned(),
            ));
        }
        if policy.group.trim().is_empty() {
            return Err(EngineError::InvalidInput(
                "circuit breaker group cannot be empty".to_owned(),
            ));
        }
        if self.config.max_circuit_origins == 0 {
            return Err(EngineError::PolicyViolation(
                "circuit breaker state is disabled by this engine".to_owned(),
            ));
        }
        let key = CircuitKey {
            origin: request.url.origin().ascii_serialization(),
            group: policy.group.clone(),
        };
        let mut circuits = self.circuits.lock().await;
        if !circuits.contains_key(&key) && circuits.len() >= self.config.max_circuit_origins {
            if let Some(oldest) = circuits.keys().next().cloned() {
                circuits.remove(&oldest);
            }
        }
        let state = circuits.entry(key.clone()).or_default();
        if let Some(opened_at) = state.opened_at {
            let recovery = Duration::from_millis(policy.recovery_timeout_ms);
            if opened_at.elapsed() < recovery || state.half_open_in_flight {
                return Err(EngineError::CircuitOpen { origin: key.origin });
            }
            state.half_open_in_flight = true;
        }
        Ok(Some(key))
    }

    async fn record_circuit(&self, key: &CircuitKey, policy: &CircuitBreakerPolicy, failed: bool) {
        let mut circuits = self.circuits.lock().await;
        let state = circuits.entry(key.clone()).or_default();
        if failed {
            state.consecutive_failures = state.consecutive_failures.saturating_add(1);
            if state.half_open_in_flight || state.consecutive_failures >= policy.failure_threshold {
                state.opened_at = Some(Instant::now());
            }
        } else {
            state.consecutive_failures = 0;
            state.opened_at = None;
        }
        state.half_open_in_flight = false;
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
            match self.send_once(request).await {
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

        Err(EngineError::Runtime(
            "retry loop terminated unexpectedly".to_owned(),
        ))
    }

    async fn download_with_circuit(
        &self,
        request: &PreparedRequest,
        target: &DownloadTarget,
        success_statuses: &[u16],
    ) -> Result<DownloadData, EngineError> {
        let key = self.admit_circuit(request).await?;
        let result = self
            .download_authenticated(request, target, success_statuses)
            .await;
        if let Some(key) = key {
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
            self.record_circuit(&key, &request.circuit_breaker, failed)
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
            return Err(EngineError::FileIo(format!(
                "destination '{}' already exists",
                target.path.display()
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
            let pending = match self.send_once_response(&attempt_request).await {
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

        Err(EngineError::Runtime(
            "download retry loop terminated unexpectedly".to_owned(),
        ))
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

        let url = Url::parse(&token_url)
            .map_err(|error| EngineError::InvalidUrl(format!("OAuth token URL: {error}")))?;
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
            return Err(EngineError::Authentication(format!(
                "token endpoint returned HTTP {}",
                response.status
            )));
        }

        let payload: Value = serde_json::from_slice(&response.body).map_err(|_| {
            EngineError::Authentication("token endpoint returned invalid JSON".to_owned())
        })?;
        if is_arcgis && payload.get("error").is_some() {
            return Err(EngineError::Authentication(
                "ArcGIS token endpoint returned an error".to_owned(),
            ));
        }
        let token_field = if is_arcgis { "token" } else { "access_token" };
        let token = payload
            .get(token_field)
            .and_then(Value::as_str)
            .filter(|token| !token.is_empty())
            .ok_or_else(|| {
                EngineError::Authentication(format!("token response has no {token_field}"))
            })?
            .to_owned();
        if !is_arcgis
            && payload
                .get("token_type")
                .and_then(Value::as_str)
                .is_some_and(|token_type| !token_type.eq_ignore_ascii_case("bearer"))
        {
            return Err(EngineError::Authentication(
                "only bearer OAuth tokens are supported".to_owned(),
            ));
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
        self.tokens.lock().await.insert(
            key,
            CachedToken {
                token: token.clone(),
                expires_at: Instant::now() + Duration::from_secs(ttl),
            },
        );
        Ok((token, stats))
    }

    async fn send_once(&self, request: &PreparedRequest) -> Result<ResponseData, EngineError> {
        let pending = self.send_once_response(request).await?;
        self.read_response(pending).await
    }

    async fn send_once_response(
        &self,
        request: &PreparedRequest,
    ) -> Result<PendingResponse, EngineError> {
        let origin = request.url.clone();
        let mut url = request.url.clone();
        let mut network_requests = 0_u64;
        let mut rate_limit_wait_ms = 0_u64;

        for redirects in 0..=request.max_redirects {
            let client = self
                .client_for(&url, &request.tls, request.proxy.as_ref(), &request.cookies)
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
                    return Err(EngineError::Runtime(
                        "OAuth authentication was not resolved".to_owned(),
                    ));
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
            rate_limit_wait_ms = rate_limit_wait_ms.saturating_add(waited_ms);
            let response = builder.send().await.map_err(map_reqwest_error)?;
            if response.status().is_redirection() && request.allow_redirects {
                if redirects == request.max_redirects {
                    return Err(EngineError::InvalidResponse(format!(
                        "redirect limit ({}) exceeded",
                        request.max_redirects
                    )));
                }
                let location = response
                    .headers()
                    .get(LOCATION)
                    .ok_or_else(|| {
                        EngineError::InvalidResponse(
                            "redirect response has no Location header".to_owned(),
                        )
                    })?
                    .to_str()
                    .map_err(|_| {
                        EngineError::InvalidResponse(
                            "redirect Location is not valid text".to_owned(),
                        )
                    })?;
                let next = url.join(location).map_err(|error| {
                    EngineError::InvalidUrl(format!("invalid redirect target: {error}"))
                })?;
                if !same_origin(&origin, &next) {
                    return Err(EngineError::UnsafeAddress(
                        "cross-origin redirects are blocked".to_owned(),
                    ));
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

        Err(EngineError::Runtime(
            "redirect loop terminated unexpectedly".to_owned(),
        ))
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
            return Err(EngineError::InvalidResponse(format!(
                "resumed download returned HTTP {status}; expected 200 or 206"
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
                    EngineError::InvalidResponse(
                        "download Content-Range has invalid bounds".to_owned(),
                    )
                })?;
            if content_length.is_some_and(|length| length != segment_length) {
                return Err(EngineError::InvalidResponse(
                    "download Content-Length does not match Content-Range".to_owned(),
                ));
            }
            if resumed {
                if content_range.start != state.bytes_written {
                    return Err(EngineError::InvalidResponse(format!(
                        "resumed download started at byte {}, expected {}",
                        content_range.start, state.bytes_written
                    )));
                }
                if response_etag.as_deref() != state.etag.as_deref() {
                    return Err(EngineError::InvalidResponse(
                        "resumed download returned a missing or different strong ETag".to_owned(),
                    ));
                }
                if state
                    .expected_total
                    .is_some_and(|expected| expected != total)
                {
                    return Err(EngineError::InvalidResponse(
                        "resumed download changed the complete representation length".to_owned(),
                    ));
                }
            } else if content_range.start != 0 || content_range.end.checked_add(1) != Some(total) {
                return Err(EngineError::InvalidResponse(
                    "partial response cannot be promoted as a complete download".to_owned(),
                ));
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
                EngineError::Runtime("download chunk length overflowed u64".to_owned())
            })?;
            if state.bytes_written.saturating_add(chunk_length) > target.max_bytes {
                return Err(EngineError::FileTooLarge {
                    limit_bytes: target.max_bytes,
                });
            }
            state
                .file
                .as_mut()
                .ok_or_else(|| EngineError::Runtime("download staging file is closed".to_owned()))?
                .write_all(&chunk)
                .await
                .map_err(file_io)?;
            state.digest.update(&chunk);
            state.bytes_written = state.bytes_written.saturating_add(chunk_length);
            state.bytes_received = state.bytes_received.saturating_add(chunk_length);
        }
        let attempt_bytes = state.bytes_written.saturating_sub(attempt_start);
        if content_length.is_some_and(|length| length != attempt_bytes) {
            return Err(EngineError::InvalidResponse(
                "download body length does not match Content-Length".to_owned(),
            ));
        }
        if expected_total.is_some_and(|total| state.bytes_written != total) {
            return Err(EngineError::InvalidResponse(
                "download body length does not match the complete representation".to_owned(),
            ));
        }
        let file = state
            .file
            .as_mut()
            .ok_or_else(|| EngineError::Runtime("download staging file is closed".to_owned()))?;
        file.flush().await.map_err(file_io)?;
        file.sync_all().await.map_err(file_io)?;
        drop(state.file.take());

        let sha256 = format!("{:x}", state.digest.clone().finalize());
        if let Some(expected) = &target.expected_sha256 {
            if !sha256.eq_ignore_ascii_case(expected) {
                return Err(EngineError::ChecksumMismatch {
                    expected: expected.clone(),
                    actual: sha256,
                });
            }
        }
        persist_download(&state.temporary, &target.path, target.overwrite).await?;
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
            .map_err(|_| EngineError::Runtime("request limiter is closed".to_owned()))?;
        Ok((permit, wait.as_millis().min(u128::from(u64::MAX)) as u64))
    }

    async fn client_for(
        &self,
        url: &Url,
        tls: &TlsConfig,
        proxy: Option<&ProxyConfig>,
        cookies: &CookiePolicy,
    ) -> Result<Client, EngineError> {
        if !tls.verify && !self.config.allow_insecure_tls {
            return Err(EngineError::PolicyViolation(
                "TLS verification cannot be disabled by this engine".to_owned(),
            ));
        }
        if proxy.is_some() && !self.config.allow_proxies {
            return Err(EngineError::PolicyViolation(
                "proxies are not enabled for this engine".to_owned(),
            ));
        }

        let (host, address, should_pin, port) = self.resolve_endpoint(url).await?;
        let proxy_endpoint = match proxy {
            Some(proxy) => {
                let url = Url::parse(&proxy.url)
                    .map_err(|_| EngineError::InvalidInput("invalid proxy URL".to_owned()))?;
                Some((proxy, url.clone(), self.resolve_endpoint(&url).await?))
            }
            None => None,
        };
        let key = ClientKey {
            host: host.clone(),
            port,
            address,
            policy_fingerprint: fingerprint(&(tls, proxy)),
            cookie_jar_id: cookies.enabled.then(|| cookies.jar_id.clone()),
        };
        if let Some(client) = self.clients.lock().await.get(&key).cloned() {
            return Ok(client);
        }

        let mut builder = Client::builder()
            .connect_timeout(Duration::from_millis(self.config.connect_timeout_ms))
            .pool_idle_timeout(Duration::from_millis(self.config.pool_idle_timeout_ms))
            .pool_max_idle_per_host(self.config.pool_max_idle_per_host)
            .redirect(Policy::none())
            .no_proxy()
            .user_agent(&self.config.user_agent);
        if cookies.enabled {
            builder = builder.cookie_store(true);
        }
        if !self.config.automatic_decompression {
            builder = builder.no_brotli().no_deflate().no_gzip().no_zstd();
        }
        if !tls.verify {
            builder = builder.danger_accept_invalid_certs(true);
        }
        if let Some(pem) = &tls.ca_bundle_pem {
            let certificate = Certificate::from_pem(pem.as_bytes())
                .map_err(|_| EngineError::InvalidInput("invalid TLS CA bundle PEM".to_owned()))?;
            builder = builder.add_root_certificate(certificate);
        }
        if let Some(pem) = &tls.client_identity_pem {
            let identity = Identity::from_pem(pem.as_bytes()).map_err(|_| {
                EngineError::InvalidInput("invalid TLS client identity PEM".to_owned())
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
                .map_err(|_| EngineError::InvalidInput("invalid proxy URL".to_owned()))?;
            match (&config.username, &config.password) {
                (Some(username), password) => {
                    configured = configured.basic_auth(username, password.as_deref().unwrap_or(""));
                }
                (None, Some(_)) => {
                    return Err(EngineError::InvalidInput(
                        "proxy password requires a username".to_owned(),
                    ));
                }
                (None, None) => {}
            }
            builder = builder.proxy(configured);
            if proxy_pin {
                builder = builder.resolve(&proxy_host, SocketAddr::new(proxy_address, proxy_port));
            }
        }
        let client = builder
            .build()
            .map_err(|error| EngineError::Runtime(error.to_string()))?;
        if self.config.max_pooled_origins > 0 {
            let mut clients = self.clients.lock().await;
            if clients.len() >= self.config.max_pooled_origins {
                if let Some(oldest_key) = clients.keys().next().cloned() {
                    clients.remove(&oldest_key);
                }
            }
            clients.insert(key, client.clone());
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
            .ok_or_else(|| EngineError::InvalidUrl("URL has no usable port".to_owned()))?;
        let (host, address, should_pin) = match url
            .host()
            .ok_or_else(|| EngineError::InvalidUrl("URL must include a host".to_owned()))?
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
                    .map_err(|error| EngineError::DnsResolution(format!("{domain}: {error}")))?
                    .map(|socket| socket.ip())
                    .collect();
                if addresses.is_empty() {
                    return Err(EngineError::DnsResolution(format!(
                        "{domain}: no addresses returned"
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
        Err(EngineError::UnsafeAddress(address.to_string()))
    }
}

fn validate_url(url: &Url) -> Result<(), EngineError> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(EngineError::InvalidUrl(
            "only http and https URLs are supported".to_owned(),
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(EngineError::InvalidUrl(
            "credentials embedded in URLs are not allowed".to_owned(),
        ));
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
            .map_err(|_| EngineError::InvalidHeader(name.clone()))?;
        let value = HeaderValue::from_str(value)
            .map_err(|_| EngineError::InvalidHeader(name.to_string()))?;
        headers.insert(name, value);
    }

    if let AuthConfig::ApiKey {
        key_name,
        key_value,
        location: ApiKeyLocation::Header,
    } = &request.auth
    {
        let name = HeaderName::from_bytes(key_name.as_bytes())
            .map_err(|_| EngineError::InvalidHeader(key_name.clone()))?;
        let value = HeaderValue::from_str(key_value)
            .map_err(|_| EngineError::InvalidHeader(key_name.clone()))?;
        headers.insert(name, value);
    }

    if matches!(request.body, PreparedBody::Multipart { .. }) && headers.contains_key(CONTENT_TYPE)
    {
        return Err(EngineError::InvalidHeader(
            "Content-Type must be generated by the multipart encoder".to_owned(),
        ));
    }
    if matches!(
        request.body,
        PreparedBody::Multipart { .. } | PreparedBody::Stream(_)
    ) && headers.contains_key(CONTENT_LENGTH)
    {
        return Err(EngineError::InvalidHeader(
            "Content-Length must be generated by the streaming encoder".to_owned(),
        ));
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
                .map_err(|_| EngineError::InvalidHeader("content-type".to_owned()))?;
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
                EngineError::InvalidInput(format!(
                    "invalid multipart content type for '{}'",
                    file.field_name
                ))
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

fn response_headers(response: &reqwest::Response) -> BTreeMap<String, String> {
    let mut headers = BTreeMap::<String, String>::new();
    for (name, value) in response.headers() {
        let Ok(value) = value.to_str() else {
            continue;
        };
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
    let value = response.headers().get(ETAG)?.to_str().ok()?.trim();
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

struct ContentRange {
    start: u64,
    end: u64,
    total: u64,
}

fn satisfied_content_range(response: &reqwest::Response) -> Result<ContentRange, EngineError> {
    let value = response
        .headers()
        .get(CONTENT_RANGE)
        .ok_or_else(|| {
            EngineError::InvalidResponse("206 response has no Content-Range header".to_owned())
        })?
        .to_str()
        .map_err(|_| {
            EngineError::InvalidResponse("download Content-Range is not valid text".to_owned())
        })?;
    let (unit, range_and_total) = value.trim().split_once(' ').ok_or_else(|| {
        EngineError::InvalidResponse("download Content-Range is malformed".to_owned())
    })?;
    if !unit.eq_ignore_ascii_case("bytes") {
        return Err(EngineError::InvalidResponse(
            "download Content-Range must use byte units".to_owned(),
        ));
    }
    let (range, total) = range_and_total.split_once('/').ok_or_else(|| {
        EngineError::InvalidResponse("download Content-Range is malformed".to_owned())
    })?;
    let (start, end) = range.split_once('-').ok_or_else(|| {
        EngineError::InvalidResponse(
            "download Content-Range does not describe a satisfied range".to_owned(),
        )
    })?;
    let start = start.parse::<u64>().map_err(|_| {
        EngineError::InvalidResponse("download Content-Range start is invalid".to_owned())
    })?;
    let end = end.parse::<u64>().map_err(|_| {
        EngineError::InvalidResponse("download Content-Range end is invalid".to_owned())
    })?;
    let total = total.parse::<u64>().map_err(|_| {
        EngineError::InvalidResponse("download Content-Range total is invalid".to_owned())
    })?;
    if start > end || end >= total {
        return Err(EngineError::InvalidResponse(
            "download Content-Range bounds are invalid".to_owned(),
        ));
    }
    Ok(ContentRange { start, end, total })
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
    let (temporary, file) = create_download_file(target).await?;
    Ok(DownloadState {
        temporary,
        file: Some(file),
        bytes_written: 0,
        bytes_received: 0,
        digest: Sha256::new(),
        etag: None,
        expected_total: None,
    })
}

async fn reset_download_state(state: &mut DownloadState) -> Result<(), EngineError> {
    drop(state.file.take());
    let file = OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(&state.temporary)
        .await
        .map_err(file_io)?;
    state.file = Some(file);
    state.bytes_written = 0;
    state.digest = Sha256::new();
    state.etag = None;
    state.expected_total = None;
    Ok(())
}

async fn discard_download_state(state: &mut DownloadState) {
    drop(state.file.take());
    let _ = fs::remove_file(&state.temporary).await;
}

async fn create_download_file(target: &Path) -> Result<(PathBuf, fs::File), EngineError> {
    for _ in 0..16 {
        let temporary = download_temporary_path(target);
        match OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)
            .await
        {
            Ok(file) => return Ok((temporary, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(file_io(error)),
        }
    }
    Err(EngineError::FileIo(
        "could not allocate a unique temporary download file".to_owned(),
    ))
}

async fn persist_download(
    temporary: &Path,
    target: &Path,
    overwrite: bool,
) -> Result<(), EngineError> {
    if !overwrite {
        fs::hard_link(temporary, target).await.map_err(file_io)?;
        fs::remove_file(temporary).await.map_err(file_io)?;
        return Ok(());
    }
    match fs::rename(temporary, target).await {
        Ok(()) => Ok(()),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::AlreadyExists | std::io::ErrorKind::PermissionDenied
            ) && fs::try_exists(target).await.map_err(file_io)? =>
        {
            fs::remove_file(target).await.map_err(file_io)?;
            fs::rename(temporary, target).await.map_err(file_io)
        }
        Err(error) => Err(file_io(error)),
    }
}

fn file_io(error: std::io::Error) -> EngineError {
    EngineError::FileIo(error.to_string())
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
                return Err(EngineError::InvalidInput(
                    "ArcGIS client must be requestip, referer, or ip".to_owned(),
                ));
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
                        EngineError::InvalidInput(
                            "ArcGIS referer client requires referer".to_owned(),
                        )
                    })?,
                );
            } else if client == "ip" {
                form.insert(
                    "ip".to_owned(),
                    ip.clone().ok_or_else(|| {
                        EngineError::InvalidInput("ArcGIS ip client requires ip".to_owned())
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
        _ => Err(EngineError::Runtime(
            "token requested for non-token authentication".to_owned(),
        )),
    }
}

fn number_as_u64(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
}

fn fingerprint(value: &(impl Hash + ?Sized)) -> u64 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

fn cache_key(request: &PreparedRequest) -> Result<CacheKey, EngineError> {
    let mut hasher = DefaultHasher::new();
    request.headers.hash(&mut hasher);
    request.auth.hash(&mut hasher);
    request.cookies.hash(&mut hasher);
    match &request.body {
        PreparedBody::None => 0_u8.hash(&mut hasher),
        PreparedBody::Json(value) => {
            1_u8.hash(&mut hasher);
            serde_json::to_vec(value)
                .map_err(|error| EngineError::InvalidInput(error.to_string()))?
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
                        return Err(EngineError::InvalidInput(
                            "HTTP cache cannot fingerprint streaming multipart files".to_owned(),
                        ));
                    }
                }
            }
        }
        PreparedBody::Raw(value) => {
            4_u8.hash(&mut hasher);
            value.hash(&mut hasher);
        }
        PreparedBody::Stream(_) => {
            return Err(EngineError::InvalidInput(
                "HTTP cache cannot fingerprint streaming request bodies".to_owned(),
            ));
        }
    }
    Ok(CacheKey {
        method: request.method.as_str().to_owned(),
        url: request.url.as_str().to_owned(),
        request_fingerprint: hasher.finish(),
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
        attempts: 0,
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

fn response_forbids_store(headers: &BTreeMap<String, String>) -> bool {
    header_has_directive(headers, CACHE_CONTROL.as_str(), "no-store")
        || headers
            .get(VARY.as_str())
            .is_some_and(|value| value.split(',').any(|value| value.trim() == "*"))
}

fn response_requires_revalidation(headers: &BTreeMap<String, String>) -> bool {
    header_has_directive(headers, CACHE_CONTROL.as_str(), "no-cache")
}

fn cache_max_age_ms(headers: &BTreeMap<String, String>) -> Option<u64> {
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
        HttpMethod::Custom(value) => Method::from_bytes(value.as_bytes())
            .map_err(|_| EngineError::InvalidInput("invalid custom HTTP method".to_owned())),
    }
}

fn map_reqwest_error(error: reqwest::Error) -> EngineError {
    if error.is_timeout() {
        EngineError::Timeout
    } else if error.is_connect() {
        EngineError::Transport("connection failed".to_owned())
    } else if error.is_body() || error.is_decode() {
        EngineError::Transport("response transfer failed".to_owned())
    } else {
        EngineError::Transport("request failed".to_owned())
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

fn parse_retry_after(value: &str, now: SystemTime) -> Option<u64> {
    let value = value.trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return seconds.checked_mul(1_000);
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
}
