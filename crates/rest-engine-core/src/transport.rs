use std::{
    collections::{BTreeMap, HashMap},
    hash::{DefaultHasher, Hash, Hasher},
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use futures_util::StreamExt;
use reqwest::{
    Certificate, Client, Identity, Method, Proxy, Url,
    header::{
        CONTENT_LENGTH, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue, LOCATION, RETRY_AFTER,
    },
    multipart::{Form, Part},
    redirect::Policy,
};
use serde_json::Value;
use tokio::{
    net::lookup_host,
    sync::{Mutex, OwnedSemaphorePermit, Semaphore},
    time::sleep,
};
use url::Host;

use crate::{
    ApiKeyLocation, AuthConfig, EngineConfig, EngineError, HttpMethod, OAuthClientAuth,
    ProxyConfig, RetryPolicy, TlsConfig,
};

#[derive(Clone)]
pub(crate) struct PreparedFile {
    pub field_name: String,
    pub filename: String,
    pub content_type: Option<String>,
    pub data: Vec<u8>,
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
    pub requests_per_second: Option<f64>,
    pub tls: TlsConfig,
    pub proxy: Option<ProxyConfig>,
}

pub(crate) struct ResponseData {
    pub status: u16,
    pub body: Vec<u8>,
    pub final_url: Url,
    pub attempts: u32,
    pub network_requests: u64,
    pub auth_requests: u64,
    pub auth_retries: u64,
    pub rate_limit_wait_ms: u64,
    pub headers: BTreeMap<String, String>,
    retry_after_ms: Option<u64>,
}

#[derive(Clone)]
pub(crate) struct Transport {
    config: EngineConfig,
    clients: Arc<Mutex<HashMap<ClientKey, Client>>>,
    tokens: Arc<Mutex<HashMap<TokenKey, CachedToken>>>,
    token_refresh: Arc<Mutex<()>>,
    concurrency: Arc<Semaphore>,
    rate_state: Arc<Mutex<RateState>>,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct ClientKey {
    host: String,
    port: u16,
    address: IpAddr,
    policy_fingerprint: u64,
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
            rate_state: Arc::new(Mutex::new(RateState {
                next_allowed: Instant::now(),
            })),
        }
    }

    pub async fn execute(&self, mut request: PreparedRequest) -> Result<ResponseData, EngineError> {
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
        let auth_stats = if matches!(
            &request.auth,
            AuthConfig::OAuth2ClientCredentials { .. }
                | AuthConfig::OAuth2Password { .. }
                | AuthConfig::ArcgisToken { .. }
        ) {
            let (token, stats) = self.oauth_token(&request).await?;
            request.auth = AuthConfig::Bearer { token };
            stats
        } else {
            RequestStats::default()
        };

        let mut response = self.execute_authenticated(&request).await?;
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
        let origin = request.url.clone();
        let mut url = request.url.clone();
        let mut network_requests = 0_u64;
        let mut rate_limit_wait_ms = 0_u64;

        for redirects in 0..=request.max_redirects {
            let client = self
                .client_for(&url, &request.tls, request.proxy.as_ref())
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
                    builder.multipart(multipart_form(fields, files)?)
                }
                PreparedBody::Raw(value) => builder.body(value.clone()),
            };

            let (_permit, waited_ms) = self.admit_request(request.requests_per_second).await?;
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

            return self
                .read_response(response, url, network_requests, rate_limit_wait_ms)
                .await;
        }

        Err(EngineError::Runtime(
            "redirect loop terminated unexpectedly".to_owned(),
        ))
    }

    async fn read_response(
        &self,
        response: reqwest::Response,
        final_url: Url,
        network_requests: u64,
        rate_limit_wait_ms: u64,
    ) -> Result<ResponseData, EngineError> {
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
        let retry_after_ms = response
            .headers()
            .get(RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| parse_retry_after(value, SystemTime::now()));
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
            headers,
            retry_after_ms,
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

    if !headers.contains_key(CONTENT_TYPE) {
        let content_type = match request.body {
            PreparedBody::Json(_) => Some("application/json"),
            PreparedBody::Form(_) => Some("application/x-www-form-urlencoded"),
            PreparedBody::Multipart { .. } => {
                if headers.contains_key(CONTENT_TYPE) {
                    return Err(EngineError::InvalidHeader(
                        "Content-Type must be generated by the multipart encoder".to_owned(),
                    ));
                }
                None
            }
            PreparedBody::Raw(_) | PreparedBody::None => None,
        };
        if let Some(content_type) = content_type {
            headers.insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
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

fn multipart_form(
    fields: &[(String, String)],
    files: &[PreparedFile],
) -> Result<Form, EngineError> {
    let mut form = Form::new();
    for (name, value) in fields {
        form = form.text(name.clone(), value.clone());
    }
    for file in files {
        let mut part = Part::bytes(file.data.clone()).file_name(file.filename.clone());
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
