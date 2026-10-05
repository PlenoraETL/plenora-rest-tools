use std::{collections::BTreeMap, fmt};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};
use serde_json::{Map, Value};

use crate::error::{ErrorCategory, ErrorPhase, RemoteEffect, RetryAdvice};

/// Version of the execution request and result contracts this engine speaks.
///
/// [`ExecutionRequest::schema_version`] must equal it; any other value fails
/// the execution with `UNSUPPORTED_SCHEMA` before any network activity.
/// Every [`ExecutionResult`] carries it in `schema_version`.
pub const SCHEMA_VERSION: u32 = 1;
/// Contract identifier written into [`AsyncJobRecovery::contract`].
pub const ASYNC_JOB_RECOVERY_CONTRACT: &str = "plenora-rest-async-job-recovery-v1";
/// A JSON object: the shape of parameters, input records, and output records.
pub type JsonObject = Map<String, Value>;

/// Engine-wide limits and authorizations, fixed when an
/// [`Engine`](crate::Engine) is created.
///
/// Every field is optional in JSON (`#[serde(default)]`) and takes the value
/// of [`EngineConfig::default`]; unknown fields are rejected. The defaults are
/// restrictive: private networks, insecure TLS, proxies, file transfers, the
/// cookie store, and custom methods are all refused until enabled here, so a
/// request cannot grant itself any of them.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct EngineConfig {
    /// Timeout for establishing a TCP/TLS connection, in milliseconds.
    /// Default 5 000.
    pub connect_timeout_ms: u64,
    /// Timeout of one HTTP request, in milliseconds, used when
    /// `connection.request.timeout_ms` is not set; it also bounds OAuth and
    /// ArcGIS token requests. Expiry is a retryable `TIMEOUT`. Default 30 000.
    pub request_timeout_ms: u64,
    /// Largest request body the engine builds, in bytes, for JSON, form,
    /// multipart, and raw bodies (file uploads are bounded by
    /// `max_file_transfer_bytes` instead). A larger body fails with
    /// `REQUEST_TOO_LARGE` before it is sent. Default 32 MiB.
    pub max_request_bytes: usize,
    /// Largest response body the engine reads into memory, in bytes. A larger
    /// declared `Content-Length`, or a body that grows past the limit while
    /// streaming, fails with `RESPONSE_TOO_LARGE`. Downloads to a file are
    /// bounded by `max_file_transfer_bytes` instead. Default 32 MiB.
    pub max_response_bytes: usize,
    /// Upper bound, in bytes, for one file upload or download.
    /// `input.file.max_bytes` can only lower it. Exceeding it fails with
    /// `FILE_TOO_LARGE`; zero makes every file transfer a `POLICY_VIOLATION`.
    /// Default 1 GiB.
    pub max_file_transfer_bytes: u64,
    /// HTTP clients kept for reuse, one per origin and transport settings
    /// (TLS, proxy, cookie session). When full, the client created first is
    /// dropped; zero disables pooling, so every request builds a new client.
    /// Default 128.
    pub max_pooled_origins: usize,
    /// Idle connections each pooled client keeps per host. Default 50.
    pub pool_max_idle_per_host: usize,
    /// How long an idle pooled connection is kept, in milliseconds.
    /// Default 90 000.
    pub pool_idle_timeout_ms: u64,
    /// HTTP requests in flight at once across the whole engine; further
    /// requests wait for a slot. Zero is treated as one. Default 64.
    pub max_concurrent_requests: usize,
    /// Engine-wide request rate, in requests per second, applied when a
    /// connection does not set its own `requests_per_second`. One limiter is
    /// shared by every request of the engine; waiting time is reported in
    /// `metrics.rate_limit_wait_ms`. `None` (the default) means no limit.
    pub requests_per_second: Option<u32>,
    /// Allows connections to private, loopback, link-local, and other
    /// non-public addresses. When false (the default), every resolved address
    /// is checked before connecting and a non-public one fails with
    /// `UNSAFE_ADDRESS`.
    pub allow_private_networks: bool,
    /// Allows requests to set `connection.tls.verify` to false. When false
    /// (the default) such a request fails with `POLICY_VIOLATION`.
    pub allow_insecure_tls: bool,
    /// Allows requests to set `connection.proxy`. When false (the default)
    /// such a request fails with `POLICY_VIOLATION`. Proxies from the
    /// environment are never used.
    pub allow_proxies: bool,
    /// Allows the `download` and `upload` operations. They also need
    /// `file_root`; without both they fail with `POLICY_VIOLATION`.
    /// Default false.
    pub allow_file_transfers: bool,
    /// Directory that confines every local file transfer. A relative root is
    /// resolved against the process working directory; it must exist and be
    /// a directory (`FILE_IO` otherwise). Transfer paths, absolute or
    /// relative, must resolve inside it, or the request fails with
    /// `POLICY_VIOLATION`. Default `None`, which refuses all file transfers.
    pub file_root: Option<String>,
    /// Lets the HTTP client request and transparently decode gzip, deflate,
    /// brotli, and zstd responses. When false the body is read as sent.
    /// Default true.
    pub automatic_decompression: bool,
    /// HTTP methods outside GET, HEAD, POST, PUT, PATCH, DELETE, and OPTIONS
    /// that requests may use. Matching is exact and case-sensitive; any other
    /// custom method fails with `POLICY_VIOLATION`. Default empty.
    pub allowed_custom_methods: Vec<String>,
    /// Allows cookie sessions. When false (the default),
    /// [`Engine::open_cookie_session`](crate::Engine::open_cookie_session)
    /// and every request naming a session fail with `POLICY_VIOLATION`.
    pub allow_cookie_store: bool,
    /// Cookie sessions the engine keeps at once. Opening one more evicts the
    /// least recently used idle session.
    pub max_cookie_sessions: usize,
    /// Responses the HTTP cache keeps; the least recently used entry is
    /// evicted to make room. Zero, like a zero `max_cache_bytes`, makes a
    /// request with `connection.cache.enabled` fail with `POLICY_VIOLATION`.
    /// Default 1 024.
    pub max_cache_entries: usize,
    /// Total size of the HTTP cache, in bytes, counting bodies and headers.
    /// A response larger than this is not stored; least recently used entries
    /// are evicted to make room. Default 64 MiB.
    pub max_cache_bytes: usize,
    /// Circuit breaker states kept at once, one per origin and
    /// `circuit_breaker.group`. When full, the oldest state is forgotten.
    /// Zero makes a request with `circuit_breaker.enabled` fail with
    /// `POLICY_VIOLATION`. Default 256.
    pub max_circuit_origins: usize,
    /// Idempotency keys remembered to detect reuse with different input. A
    /// key seen again with a different request fails with
    /// `IDEMPOTENCY_CONFLICT`; when full, the oldest key is forgotten. Zero
    /// makes every request with `options.idempotency_key` fail with
    /// `POLICY_VIOLATION`. Default 4 096.
    pub max_idempotency_keys: usize,
    /// `User-Agent` header of every request. Default
    /// `rest-engine/<crate version>`.
    pub user_agent: String,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            connect_timeout_ms: 5_000,
            request_timeout_ms: 30_000,
            max_request_bytes: 32 * 1024 * 1024,
            max_response_bytes: 32 * 1024 * 1024,
            max_file_transfer_bytes: 1024 * 1024 * 1024,
            max_pooled_origins: 128,
            pool_max_idle_per_host: 50,
            pool_idle_timeout_ms: 90_000,
            max_concurrent_requests: 64,
            requests_per_second: None,
            allow_private_networks: false,
            allow_insecure_tls: false,
            allow_proxies: false,
            allow_file_transfers: false,
            file_root: None,
            automatic_decompression: true,
            allowed_custom_methods: Vec::new(),
            allow_cookie_store: false,
            max_cookie_sessions: 256,
            max_cache_entries: 1_024,
            max_cache_bytes: 64 * 1024 * 1024,
            max_circuit_origins: 256,
            max_idempotency_keys: 4_096,
            user_agent: format!("rest-engine/{}", env!("CARGO_PKG_VERSION")),
        }
    }
}

/// One execution: the `plenora-rest-execution-request-v1` contract (for
/// `download` and `upload`, the file transfer input contract).
///
/// Unknown fields are rejected. `Debug` is not implemented: the connection
/// can carry credentials.
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionRequest {
    /// Contract version; must equal [`SCHEMA_VERSION`], otherwise the result
    /// fails with `UNSUPPORTED_SCHEMA`.
    pub schema_version: u32,
    /// The operation to run.
    pub operation: ExecutionOperation,
    /// The remote service and how to talk to it.
    pub connection: ConnectionConfig,
    /// Parameters, records, and file transfer input. Optional; empty by
    /// default.
    #[serde(default)]
    pub input: ExecutionInput,
    /// Error handling, metadata capture, concurrency, deadline, and
    /// idempotency key. Optional; see [`ExecutionOptions::default`].
    #[serde(default)]
    pub options: ExecutionOptions,
}

/// The five normative operations, `snake_case` in JSON.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionOperation {
    /// `rest.test`: one request (plus polling, when configured) whose parsed
    /// response is returned as [`ExecutionOutput::Json`].
    Test,
    /// `rest.generate`: produces records from one response, or from every
    /// page when `connection.pagination` is set, as
    /// [`ExecutionOutput::Records`].
    Generate,
    /// `rest.enrich`: one request per input record (or per batch chunk when
    /// `connection.batch.enabled`), merging the mapped response fields into
    /// that record. Output order is input order.
    Enrich,
    /// `rest.download`: streams the response body into the file named by
    /// `input.file`, published only after the transfer and its checks
    /// complete.
    Download,
    /// `rest.upload`: streams the file named by `input.file` as a raw body or
    /// a multipart part.
    Upload,
}

/// The remote service and every policy that applies to talking to it.
///
/// Every field is optional in JSON and takes its [`Default`] value, but an
/// empty `url` fails with `INVALID_URL`. Unknown fields are rejected. `Debug`
/// is not implemented: headers and `auth` can carry credentials.
#[derive(Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ConnectionConfig {
    /// Request URL, `http` or `https` only. `{name}` placeholders are
    /// replaced by the percent-encoded value of the parameter with that name,
    /// which is then not sent anywhere else. An empty or unparsable result
    /// fails with `INVALID_URL`.
    pub url: String,
    /// HTTP method. Default GET.
    pub method: HttpMethod,
    /// Headers sent with every request of the operation, follow-ups included.
    /// When a follow-up leaves the original origin only representation
    /// headers (`Accept`, `Content-Type`, `User-Agent`, conditionals, …)
    /// are kept.
    pub headers: BTreeMap<String, String>,
    /// Inline authentication. Default [`AuthConfig::None`]. The runtime
    /// binding refuses it and uses `credential_ref` instead.
    pub auth: AuthConfig,
    /// Reference to credentials held by the host, resolved into `auth` by
    /// [`RuntimeResources`](crate::RuntimeResources) on the runtime boundary.
    /// Executing a request that still carries it directly on the
    /// [`Engine`](crate::Engine) fails with `INVALID_INPUT`.
    pub credential_ref: Option<String>,
    /// How parameters are resolved from the input and where they are sent.
    /// A resolved value without a spec is sent with
    /// [`ParameterLocation::Auto`].
    pub parameters: Vec<ParameterSpec>,
    /// Parameters sent with every request. `input.params`, and for `enrich`
    /// each record, override them by name.
    pub static_parameters: JsonObject,
    /// Request body, timeout, and redirect settings.
    pub request: RequestConfig,
    /// How the response is parsed, checked, and turned into records.
    pub response: ResponseConfig,
    /// Retry and backoff policy.
    pub retry: RetryPolicy,
    /// The cookie session the request uses, if any.
    pub cookies: CookiePolicy,
    /// HTTP response cache for GET and HEAD.
    pub cache: CachePolicy,
    /// Circuit breaker for the request origin.
    pub circuit_breaker: CircuitBreakerPolicy,
    /// Where `options.idempotency_key` is sent.
    pub idempotency: IdempotencyConfig,
    /// Pagination, used only by `generate`. Default `None`: a single request.
    pub pagination: Option<PaginationConfig>,
    /// Asynchronous job polling after the initial request. Default `None`.
    pub polling: Option<PollingConfig>,
    /// Batch requests for `enrich`, used only when `enabled`. Default `None`.
    pub batch: Option<BatchConfig>,
    /// HTTP statuses accepted as success in addition to 2xx. Any other status
    /// fails with `HTTP_STATUS`. Default empty.
    pub success_statuses: Vec<u16>,
    /// Request rate for this connection, in requests per second, replacing
    /// `EngineConfig::requests_per_second`. The engine has a single limiter,
    /// so the rate paces this request against every other one. A value that
    /// is not finite and positive is ignored and the engine rate applies.
    /// Default `None`.
    pub requests_per_second: Option<f64>,
    /// TLS verification, trusted roots, and client identity.
    pub tls: TlsConfig,
    /// Outbound proxy; needs `EngineConfig::allow_proxies`. Default `None`.
    pub proxy: Option<ProxyConfig>,
}

impl Default for ConnectionConfig {
    fn default() -> Self {
        Self {
            url: String::new(),
            method: HttpMethod::Get,
            headers: BTreeMap::new(),
            auth: AuthConfig::None,
            credential_ref: None,
            parameters: Vec::new(),
            static_parameters: JsonObject::new(),
            request: RequestConfig::default(),
            response: ResponseConfig::default(),
            retry: RetryPolicy::default(),
            cookies: CookiePolicy::default(),
            cache: CachePolicy::default(),
            circuit_breaker: CircuitBreakerPolicy::default(),
            idempotency: IdempotencyConfig::default(),
            pagination: None,
            polling: None,
            batch: None,
            success_statuses: Vec::new(),
            requests_per_second: None,
            tls: TlsConfig::default(),
            proxy: None,
        }
    }
}

/// Sends `enrich` records in chunks, one request per chunk.
///
/// The parameters each record resolves to are collected into an array sent
/// as the single parameter named `input_key`; pagination is not applied. The
/// response must hold one result per record sent, in order.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct BatchConfig {
    /// Turns batching on. When false the configuration is ignored and
    /// `enrich` sends one request per record. Default false.
    pub enabled: bool,
    /// Records per request. Zero fails with `INVALID_INPUT`. Default 100.
    pub max_size: usize,
    /// Name of the parameter carrying the array. Default `items`.
    pub input_key: String,
    /// What each record contributes to the array. Default
    /// [`BatchInputFormat::Array`].
    pub input_format: BatchInputFormat,
    /// JSON path of the result array in the response; empty (the default)
    /// means the whole response. A missing path, a value that is not an
    /// array, or an array whose length differs from the records sent fails
    /// every record of the chunk with `INVALID_RESPONSE`. A `null` result
    /// leaves its record unchanged.
    pub output_path: String,
    /// URL used for batch requests instead of `connection.url`.
    pub endpoint_override: Option<String>,
    /// Method used for batch requests instead of `connection.method`.
    pub method_override: Option<HttpMethod>,
}

impl Default for BatchConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_size: 100,
            input_key: "items".to_owned(),
            input_format: BatchInputFormat::Array,
            output_path: String::new(),
            endpoint_override: None,
            method_override: None,
        }
    }
}

/// Shape of the batch array, `snake_case` in JSON.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BatchInputFormat {
    /// One object per record, holding every parameter it resolved to.
    #[default]
    Array,
    /// One bare value per record. Each record must resolve to exactly one
    /// non-null parameter; any other record fails with `INVALID_INPUT` and is
    /// left out of the batch.
    FlatArray,
    /// One object per record; currently serialized exactly like `Array`.
    Object,
}

/// TLS settings of a connection.
///
/// `Debug` prints only `verify`; the PEM fields are redacted.
#[derive(Clone, Deserialize, Hash, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct TlsConfig {
    /// Verifies the server certificate and host name. Setting it to false
    /// needs `EngineConfig::allow_insecure_tls`, otherwise the request fails
    /// with `POLICY_VIOLATION`. Default true.
    pub verify: bool,
    /// PEM certificates trusted in addition to the built-in roots. Invalid
    /// PEM fails with `INVALID_INPUT`.
    pub ca_bundle_pem: Option<String>,
    /// PEM client certificate chain and private key for mutual TLS. It counts
    /// as authentication: caching needs `cache.allow_authenticated`, and it
    /// is not presented to an origin other than the original one. Invalid PEM
    /// fails with `INVALID_INPUT`.
    pub client_identity_pem: Option<String>,
}

impl Default for TlsConfig {
    fn default() -> Self {
        Self {
            verify: true,
            ca_bundle_pem: None,
            client_identity_pem: None,
        }
    }
}

impl fmt::Debug for TlsConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TlsConfig")
            .field("verify", &self.verify)
            .field(
                "ca_bundle_pem",
                &self.ca_bundle_pem.as_ref().map(|_| "<redacted>"),
            )
            .field(
                "client_identity_pem",
                &self.client_identity_pem.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

/// Outbound proxy for every request of a connection.
///
/// Needs `EngineConfig::allow_proxies`. `Debug` redacts every field.
#[derive(Clone, Deserialize, Hash, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProxyConfig {
    /// Proxy URL. An unparsable URL fails with `INVALID_INPUT`; its address
    /// is subject to the same private network policy as the target.
    pub url: String,
    /// User name for proxy Basic authentication.
    #[serde(default)]
    pub username: Option<String>,
    /// Password for proxy Basic authentication; without `username` the
    /// request fails with `INVALID_INPUT`.
    #[serde(default)]
    pub password: Option<String>,
}

impl fmt::Debug for ProxyConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProxyConfig")
            .field("url", &"<redacted-url>")
            .field("username", &self.username.as_ref().map(|_| "<redacted>"))
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// HTTP method, a string in JSON.
///
/// The seven standard names are recognized only in upper case; any other RFC
/// 9110 token, `get` included, is [`HttpMethod::Custom`]. A string that is not
/// a token is refused when the request is deserialized. GET, HEAD, PUT,
/// DELETE, and OPTIONS are idempotent and retried by default.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum HttpMethod {
    /// `GET`, the default.
    #[default]
    Get,
    /// `HEAD`.
    Head,
    /// `POST`.
    Post,
    /// `PUT`.
    Put,
    /// `PATCH`.
    Patch,
    /// `DELETE`.
    Delete,
    /// `OPTIONS`.
    Options,
    /// Any other method token, sent verbatim. It must appear in
    /// `EngineConfig::allowed_custom_methods`, otherwise the request fails
    /// with `POLICY_VIOLATION`; it is not idempotent for retries.
    Custom(String),
}

/// Inline authentication, an object tagged by `type` in JSON.
///
/// Token based variants (`oauth2_client_credentials`, `oauth2_password`,
/// `arcgis_token`) obtain a token with a form POST to `token_url` (at most two
/// attempts) and send it as a bearer token. Tokens are cached per token URL,
/// credentials, and TLS/proxy settings, and refreshed shortly before they
/// expire (by a tenth of their lifetime, between 1 and 30 seconds). A failing
/// token endpoint, a response without a token, or a non-bearer `token_type`
/// fails with `AUTHENTICATION_FAILED`. Credentials are never sent to an origin
/// other than the original one. `Debug` is not implemented.
#[derive(Clone, Default, Deserialize, Hash, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum AuthConfig {
    /// `none`: no authentication, the default.
    #[default]
    None,
    /// `bearer`: `Authorization: Bearer <token>`.
    Bearer {
        /// The bearer token.
        token: String,
    },
    /// `api_key`: a key sent as a header or a query parameter.
    ApiKey {
        /// Header or query parameter name. An invalid header name fails with
        /// `INVALID_HEADER`.
        key_name: String,
        /// The key. A value that is not a valid header value fails with
        /// `INVALID_HEADER` when sent as a header.
        key_value: String,
        /// Where the key goes. Default header.
        #[serde(default)]
        location: ApiKeyLocation,
    },
    /// `basic_auth` (alias `basic`): HTTP Basic authentication.
    #[serde(rename = "basic_auth", alias = "basic")]
    Basic {
        /// User name.
        username: String,
        /// Password.
        password: String,
    },
    /// `oauth2_client_credentials`: OAuth 2.0 client credentials grant.
    #[serde(rename = "oauth2_client_credentials")]
    OAuth2ClientCredentials {
        /// Token endpoint; an unparsable URL fails with `INVALID_URL`.
        token_url: String,
        /// Client identifier.
        client_id: String,
        /// Client secret.
        client_secret: String,
        /// `scope` form field, sent when set.
        #[serde(default)]
        scope: Option<String>,
        /// `audience` form field, sent when set.
        #[serde(default)]
        audience: Option<String>,
        /// Extra form fields; `grant_type`, `scope`, `audience`, and the
        /// client credentials sent in the body override fields of the same
        /// name.
        #[serde(default)]
        extra_params: BTreeMap<String, String>,
        /// How the client authenticates to the token endpoint. Default HTTP
        /// Basic.
        #[serde(default)]
        client_auth: OAuthClientAuth,
    },
    /// `oauth2_password`: OAuth 2.0 resource owner password grant.
    OAuth2Password {
        /// Token endpoint; an unparsable URL fails with `INVALID_URL`.
        token_url: String,
        /// Resource owner user name, sent as `username`.
        username: String,
        /// Resource owner password, sent as `password`.
        password: String,
        /// `client_id` form field, sent when set.
        #[serde(default)]
        client_id: Option<String>,
        /// `client_secret` form field, sent when set.
        #[serde(default)]
        client_secret: Option<String>,
        /// `scope` form field, sent when set.
        #[serde(default)]
        scope: Option<String>,
        /// Extra form fields; the fields above override those of the same
        /// name.
        #[serde(default)]
        extra_params: BTreeMap<String, String>,
    },
    /// `arcgis_token`: an ArcGIS `generateToken` request (`f=json`); the
    /// token is read from `token` and its expiry from `expires`, in epoch
    /// milliseconds. A response with an `error` member fails with
    /// `AUTHENTICATION_FAILED`.
    ArcgisToken {
        /// The `generateToken` endpoint.
        token_url: String,
        /// ArcGIS user name.
        username: String,
        /// ArcGIS password.
        password: String,
        /// Token binding: `requestip` (the default), `referer`, or `ip`. Any
        /// other value fails with `INVALID_INPUT`.
        #[serde(default = "default_arcgis_client")]
        client: String,
        /// Referer the token is bound to; required when `client` is
        /// `referer`, otherwise `INVALID_INPUT`.
        #[serde(default)]
        referer: Option<String>,
        /// IP address the token is bound to; required when `client` is `ip`,
        /// otherwise `INVALID_INPUT`.
        #[serde(default)]
        ip: Option<String>,
        /// Requested token lifetime, in minutes; also the assumed lifetime
        /// when the response has no `expires`. Default 60.
        #[serde(default = "default_arcgis_expiration")]
        expiration: u32,
    },
}

/// How an OAuth client authenticates to the token endpoint, `snake_case` in
/// JSON.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, Hash, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OAuthClientAuth {
    /// `client_id` and `client_secret` as HTTP Basic credentials, the default.
    #[default]
    Basic,
    /// `client_id` and `client_secret` as form fields in the request body.
    Body,
}

/// Where an API key is sent, `snake_case` in JSON.
#[derive(Clone, Copy, Debug, Default, Deserialize, Hash, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ApiKeyLocation {
    /// As the header `key_name`, the default.
    #[default]
    Header,
    /// As the query parameter `key_name`, appended to the URL.
    Query,
}

/// How one parameter is resolved and where it is sent.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ParameterSpec {
    /// Parameter name: the key of the resolved value, the name it is sent
    /// under, and the `{name}` placeholder it fills in the URL.
    pub name: String,
    /// Whether the value comes from the input or from `value`. Default
    /// mapped.
    #[serde(default)]
    pub mode: ParameterMode,
    /// For a mapped parameter, the input key or JSON path to read, looked up
    /// in `input.params` merged with the current record. Default: `name`.
    #[serde(default)]
    pub source: Option<String>,
    /// `None` when the field is absent, `Some(Value::Null)` when it is an
    /// explicit JSON `null`: a fixed parameter whose value is `null` sends
    /// `null`, it is not dropped as if it had no value.
    #[serde(
        default,
        deserialize_with = "explicit_value",
        skip_serializing_if = "Option::is_none"
    )]
    pub value: Option<Value>,
    /// A parameter without a value fails with `MISSING_PARAMETER` instead of
    /// being omitted. Default false.
    #[serde(default)]
    pub required: bool,
    /// Where the value is sent. Default [`ParameterLocation::Auto`].
    #[serde(default)]
    pub location: ParameterLocation,
    /// How an array or object value is written into the query string.
    /// Without it an array repeats the name once per element.
    #[serde(default)]
    pub query_serialization: Option<QuerySerialization>,
}

/// Where a parameter value comes from, `snake_case` in JSON.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ParameterMode {
    /// From the input at `source`, falling back to `value` when the input
    /// has none; the default.
    #[default]
    Mapped,
    /// Always `value`. A fixed parameter without `value` fails with
    /// `INVALID_INPUT`.
    Fixed,
}

/// Where a parameter is sent, `snake_case` in JSON.
///
/// A `null` value is refused with `INVALID_INPUT` everywhere except a JSON
/// body, because path, query, header, cookie, form, and multipart are text.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ParameterLocation {
    /// The URL when it has a matching `{name}` placeholder; otherwise the
    /// query for GET, HEAD, DELETE, and OPTIONS or when `body_type` is
    /// `none`, and the body for every other method. The default.
    #[default]
    Auto,
    /// The matching `{name}` URL placeholder; without one the request fails
    /// with `INVALID_INPUT`.
    Path,
    /// The query string.
    Query,
    /// A request header, replacing a configured header of the same name;
    /// array elements are joined with `,`.
    Header,
    /// The request body, encoded according to `request.body_type`.
    Body,
    /// The `Cookie` header. The name must be a valid cookie name and the
    /// value visible ASCII without `;` or `,`, otherwise `INVALID_INPUT`.
    Cookie,
}

/// OpenAPI-style serialization of an array or object query parameter.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct QuerySerialization {
    /// Serialization style. Default form.
    pub style: QueryStyle,
    /// For the form style, whether array elements and object members become
    /// separate query pairs (true) or one comma-joined value (false). Default
    /// true for form; the other styles ignore it.
    pub explode: Option<bool>,
}

impl Default for QuerySerialization {
    fn default() -> Self {
        Self {
            style: QueryStyle::Form,
            explode: None,
        }
    }
}

/// Query serialization style, `snake_case` in JSON with the OpenAPI
/// camelCase spellings accepted as aliases. Scalars are always sent as
/// `name=value`.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum QueryStyle {
    /// Exploded: `name=a&name=b` for arrays and `key=value` per member for
    /// objects. Not exploded: `name=a,b` and `name=key,value`. The default.
    #[default]
    Form,
    /// `name=a b`: array elements, or object keys and values, joined with a
    /// space.
    #[serde(alias = "spaceDelimited")]
    SpaceDelimited,
    /// `name=a|b`: array elements, or object keys and values, joined with
    /// `|`.
    #[serde(alias = "pipeDelimited")]
    PipeDelimited,
    /// `name[key]=value` per object member. A value that is not an object
    /// fails with `INVALID_INPUT`.
    #[serde(alias = "deepObject")]
    DeepObject,
}

/// Request body, timeout, and redirect settings.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct RequestConfig {
    /// How body parameters are encoded. Default json.
    pub body_type: BodyType,
    /// Body template for `body_type` raw: `{name}` placeholders are replaced
    /// with the parameter text, unencoded. The rendered body counts against
    /// `EngineConfig::max_request_bytes`.
    pub raw_body: Option<String>,
    /// Timeout of each HTTP request of this connection, in milliseconds,
    /// replacing `EngineConfig::request_timeout_ms`; `null` means unset.
    pub timeout_ms: Option<u64>,
    /// Follows redirects, which the engine handles itself and only within
    /// the original origin: a cross-origin redirect fails with
    /// `UNSAFE_ADDRESS`. When false (the default) a 3xx response is returned
    /// as is and fails with `HTTP_STATUS` unless listed in
    /// `success_statuses`.
    pub allow_redirects: bool,
    /// Redirects followed per request; one more fails with
    /// `INVALID_RESPONSE`. Default 5.
    pub max_redirects: usize,
}

impl Default for RequestConfig {
    fn default() -> Self {
        Self {
            body_type: BodyType::Json,
            raw_body: None,
            timeout_ms: None,
            allow_redirects: false,
            max_redirects: 5,
        }
    }
}

/// Request body encoding, `snake_case` in JSON.
///
/// A body is built when there are body parameters, when the method is not
/// GET, HEAD, DELETE, or OPTIONS, or for `raw` with a `raw_body`. Its size is
/// checked against `EngineConfig::max_request_bytes` (`REQUEST_TOO_LARGE`).
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BodyType {
    /// A JSON object of the body parameters (`application/json`), the
    /// default.
    #[default]
    Json,
    /// `application/x-www-form-urlencoded` fields; also accepted as
    /// `form-urlencoded`.
    #[serde(alias = "form-urlencoded")]
    FormUrlencoded,
    /// `multipart/form-data` fields; `upload` adds the file as a part. The
    /// `Content-Type` and `Content-Length` headers are generated and may not
    /// be configured (`INVALID_HEADER`).
    Multipart,
    /// The rendered `request.raw_body`; `upload` streams the file as the
    /// body instead.
    Raw,
    /// No body: parameters with the `auto` location go to the query.
    None,
}

/// How a response is parsed, checked, and turned into records.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ResponseConfig {
    /// How the body is parsed. Default json. An empty body is `null` whatever
    /// the format.
    pub format: ResponseFormat,
    /// Field delimiter for `csv`: exactly one ASCII byte, otherwise
    /// `INVALID_INPUT`. The default is empty, so a CSV response needs it set
    /// explicitly.
    pub delimiter: String,
    /// JSON path of the records in the parsed response; `null` or absent
    /// means the whole response. A path that does not resolve fails with
    /// `INVALID_RESPONSE`. The selected value must be an array (one record
    /// per element), an object (one record), or `null` (no records).
    pub records_path: Option<String>,
    /// Columns of each output record. Empty (the default) keeps an object
    /// record as is and wraps any other value as `{"value": …}`.
    pub output_mapping: Vec<OutputMapping>,
    /// Nested iteration that expands each selected value into one record per
    /// innermost element; empty (the default) means no iteration.
    pub iterate_on: Vec<IterationSpec>,
    /// Transforms applied in order to each mapped record. They are validated
    /// before any network activity; an invalid one fails with
    /// `INVALID_INPUT`, a value one cannot handle fails the record with
    /// `INVALID_RESPONSE`.
    pub transforms: Vec<ResponseTransform>,
    /// Condition the parsed response must satisfy, otherwise
    /// `APPLICATION_ERROR`. A boolean is used as is; an object is a test on
    /// the value at its `path` (the whole response without one): `exists`,
    /// `truthy`, `equals`, `not_equals`, `in`, `not_in`, or truthiness when
    /// none is given; an array requires every condition. `null` means no
    /// condition.
    pub success_when: Option<Value>,
    /// JSON path where the service reports an error: a value there that is
    /// neither `null` nor an empty string fails with `APPLICATION_ERROR`. The
    /// remote text is not captured.
    pub error_path: Option<String>,
}

/// Response body format, `snake_case` in JSON. A body that does not parse
/// fails with `INVALID_RESPONSE`.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ResponseFormat {
    /// A JSON document, the default.
    #[default]
    Json,
    /// CSV with a header row of unique, non-empty names: an array of
    /// objects whose values are strings. Every row must be as wide as the
    /// header.
    Csv,
    /// XML: an object keyed by the root element name; attributes become
    /// `@name` members, mixed text `#text`, and repeated elements arrays.
    /// Nesting is limited to 128 levels.
    Xml,
    /// Newline-delimited JSON: an array with one element per non-blank line.
    Ndjson,
    /// The body as one UTF-8 string.
    Text,
    /// `{"data_base64": …, "size": …}`: the body in standard Base64 and its
    /// length in bytes.
    Binary,
}

/// One level of nested iteration over the response.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct IterationSpec {
    /// JSON path, relative to the element selected by the previous level, of
    /// the array to iterate; empty (the default) means that element itself.
    /// A missing or `null` value yields no records; a scalar or object is
    /// iterated as a single element.
    #[serde(default)]
    pub path: String,
    /// Key, `as` in JSON, under which each element is placed in the record
    /// handed to `output_mapping`, together with the elements of the outer
    /// levels.
    #[serde(rename = "as")]
    pub alias: String,
}

/// A value transform applied to each mapped record.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResponseTransform {
    /// Column the result is written to. Must not be empty.
    pub column: String,
    /// Column of the record the transform reads; missing means `null`. Must
    /// not be empty.
    pub source: String,
    /// One of `add`, `subtract`, `multiply`, `divide`, `round`, `prefix`,
    /// `suffix`, `replace`, `uppercase`, `lowercase`, `default_if_null`,
    /// `kelvin_to_celsius`, or `celsius_to_kelvin`; anything else fails with
    /// `INVALID_INPUT`. Every operation except `default_if_null` maps `null`
    /// to `null`, and integer arithmetic is exact.
    pub operation: String,
    /// Absent and explicit `null` stay distinct, as for [`ParameterSpec::value`].
    #[serde(
        default,
        deserialize_with = "explicit_value",
        skip_serializing_if = "Option::is_none"
    )]
    pub value: Option<Value>,
    /// Applies the transform only when `column == 'literal'` or
    /// `column != 'literal'` holds for the record; a missing or `null` column
    /// satisfies neither. A condition of any other form fails with
    /// `INVALID_INPUT`.
    #[serde(default)]
    pub condition: Option<String>,
}

/// One output column read from each response record.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OutputMapping {
    /// JSON path of the value inside the record. A malformed path fails with
    /// `INVALID_INPUT` before any network activity.
    pub path: String,
    /// Name of the output column.
    pub column: String,
    /// Absent and explicit `null` stay distinct, as for [`ParameterSpec::value`].
    #[serde(
        default,
        deserialize_with = "explicit_value",
        skip_serializing_if = "Option::is_none"
    )]
    pub default: Option<Value>,
}

/// Deserializes a field whose JSON value may itself be `null`.
///
/// For `Option<Value>` serde reads a present `null` as `None`, which makes an
/// explicit `null` indistinguishable from an absent field. Where `null` is a
/// value the caller can mean — a parameter value, a transform argument, a
/// mapping default — that would silently drop it, so a present field is always
/// `Some`, `null` included. Absence is still `None` through `#[serde(default)]`,
/// which does not call this function.
fn explicit_value<'de, D>(deserializer: D) -> Result<Option<Value>, D::Error>
where
    D: Deserializer<'de>,
{
    Value::deserialize(deserializer).map(Some)
}

/// Retry and backoff policy.
///
/// A request is retried after a status in `retry_on_status` or after a
/// timeout, transport, or DNS error, only while attempts remain and only for
/// an idempotent method, unless `retry_non_idempotent` holds or an
/// idempotency key is sent.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct RetryPolicy {
    /// Attempts per request, the first included; zero is treated as one.
    /// Default 1, so nothing is retried.
    pub max_attempts: u32,
    /// Delay before the first retry, in milliseconds. Default 500.
    pub backoff_base_ms: u64,
    /// Multiplier applied to the delay after each retry; values below 1 are
    /// treated as 1. Default 2.0.
    pub backoff_factor: f64,
    /// Ceiling of the computed backoff delay, in milliseconds.
    /// Default 30 000.
    pub max_backoff_ms: u64,
    /// HTTP statuses that trigger a retry. Default 429, 500, 502, 503, 504.
    pub retry_on_status: Vec<u16>,
    /// Waits for the `Retry-After` header (seconds or HTTP date), when
    /// present, instead of the computed backoff. Default true.
    pub respect_retry_after: bool,
    /// Ceiling of a `Retry-After` wait, in milliseconds. Default 300 000.
    pub max_retry_after_ms: u64,
    /// Also retries POST, PATCH, and custom methods. Sending
    /// `options.idempotency_key` turns it on for the request, except where
    /// the key header is withheld from another origin. Default false.
    pub retry_non_idempotent: bool,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 1,
            backoff_base_ms: 500,
            backoff_factor: 2.0,
            max_backoff_ms: 30_000,
            retry_on_status: vec![429, 500, 502, 503, 504],
            respect_retry_after: true,
            max_retry_after_ms: 300_000,
            retry_non_idempotent: false,
        }
    }
}

/// Where `options.idempotency_key` is sent.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct IdempotencyConfig {
    /// Header, query parameter, or body field name: 1 to 256 non-control
    /// characters, otherwise `INVALID_INPUT`. A configured header or
    /// parameter of the same name with a different value fails with
    /// `INVALID_INPUT`. Default `Idempotency-Key`.
    pub name: String,
    /// Where the key goes. Default header.
    pub location: IdempotencyLocation,
}

impl Default for IdempotencyConfig {
    fn default() -> Self {
        Self {
            name: "Idempotency-Key".to_owned(),
            location: IdempotencyLocation::Header,
        }
    }
}

/// Where the idempotency key is sent, `snake_case` in JSON.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IdempotencyLocation {
    /// A request header, the default. It follows a request to another origin
    /// only when its name says it carries an idempotency key.
    #[default]
    Header,
    /// A query parameter.
    Query,
    /// A body field; needs `body_type` json, form_urlencoded, or multipart,
    /// otherwise `INVALID_INPUT`.
    Body,
}

/// Cookie handling for one request.
///
/// Cookies are kept only inside a session the caller opened with
/// [`Engine::open_cookie_session`](crate::Engine::open_cookie_session). Without
/// a session the request carries no engine-held cookies.
#[derive(Clone, Debug, Default, Deserialize, Hash, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct CookiePolicy {
    /// Handle of the session whose cookies the request sends and updates, as
    /// an opaque string in JSON. A closed or evicted session, or a handle of
    /// another engine, fails with `POLICY_VIOLATION` before any network
    /// activity; a malformed handle is `INVALID_INPUT`. It cannot be combined
    /// with `cache.enabled`. Default `None`: no engine-held cookies.
    pub session: Option<CookieSession>,
}

impl CookiePolicy {
    /// True when the request uses an engine-held cookie session.
    pub fn is_enabled(&self) -> bool {
        self.session.is_some()
    }
}

/// Handle of a cookie session opened by an [`Engine`](crate::Engine).
///
/// The engine issues it and only the engine interprets it. It names a slot
/// and the generation of that slot, so a handle outlives its session only as
/// a value that is refused: once the session is closed or evicted the slot's
/// generation moves on and the handle fails explicitly, before any network
/// activity, instead of reaching an empty session. It also carries the
/// issuing engine's random identifier and a random per-session value, so a
/// handle from another engine, or one assembled by hand, is refused too.
///
/// On the wire it is an opaque string. `Debug` does not print it: the handle
/// grants the use of the session's cookies.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct CookieSession {
    pub(crate) engine: u64,
    pub(crate) slot: u32,
    pub(crate) generation: u64,
    pub(crate) nonce: u128,
}

const COOKIE_SESSION_PREFIX: &str = "rcs1";

impl CookieSession {
    /// The opaque token, as it travels in JSON.
    pub fn to_token(&self) -> String {
        format!(
            "{COOKIE_SESSION_PREFIX}.{:016x}.{}.{}.{:032x}",
            self.engine, self.slot, self.generation, self.nonce
        )
    }

    /// Parses a token issued by [`CookieSession::to_token`].
    ///
    /// Only the exact spelling the engine produces is accepted, so a token has
    /// a single textual form.
    pub fn from_token(token: &str) -> Option<Self> {
        let mut parts = token.split('.');
        if parts.next()? != COOKIE_SESSION_PREFIX {
            return None;
        }
        let engine = parts.next()?;
        let slot = parts.next()?;
        let generation = parts.next()?;
        let nonce = parts.next()?;
        if parts.next().is_some() || engine.len() != 16 || nonce.len() != 32 {
            return None;
        }
        let lowercase_hex = |text: &str| {
            text.bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        };
        if !lowercase_hex(engine) || !lowercase_hex(nonce) {
            return None;
        }
        let session = Self {
            engine: u64::from_str_radix(engine, 16).ok()?,
            slot: slot.parse().ok()?,
            generation: generation.parse().ok()?,
            nonce: u128::from_str_radix(nonce, 16).ok()?,
        };
        // Rejects leading zeros, signs and other spellings `parse` tolerates.
        (session.to_token() == token).then_some(session)
    }
}

impl fmt::Debug for CookieSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CookieSession(<redacted>)")
    }
}

impl Serialize for CookieSession {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_token())
    }
}

impl<'de> Deserialize<'de> for CookieSession {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let token = String::deserialize(deserializer)?;
        Self::from_token(&token)
            .ok_or_else(|| D::Error::custom("cookie session handle is not well formed"))
    }
}

/// HTTP response cache for GET and HEAD.
///
/// Entries live in the engine, bounded by `EngineConfig::max_cache_entries`
/// and `max_cache_bytes`, and are keyed by the request together with its
/// authentication, TLS identity, proxy, and redirect policy. Only 2xx
/// responses without `no-store` are stored.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct CachePolicy {
    /// Uses the cache. Any other method fails with `INVALID_INPUT`; a cookie
    /// session or an engine without cache capacity fails with
    /// `POLICY_VIOLATION`. Default false.
    pub enabled: bool,
    /// How long a stored response is served without contacting the service,
    /// in milliseconds, capped by the response's own `max-age`. Once stale,
    /// or always when zero (the default) or when the response requires
    /// revalidation, the entry is revalidated with a conditional request and
    /// a 304 refreshes it.
    pub fresh_for_ms: u64,
    /// Allows caching requests that carry authentication, an `Authorization`
    /// header, a cookie session, or a TLS client identity; without it they
    /// fail with `POLICY_VIOLATION`. Default false.
    pub allow_authenticated: bool,
}

/// Circuit breaker, one state per request origin and `group`.
///
/// While open, requests fail with `CIRCUIT_OPEN` without network activity.
/// States are bounded by `EngineConfig::max_circuit_origins`.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct CircuitBreakerPolicy {
    /// Uses the breaker. Default false.
    pub enabled: bool,
    /// Name that, with the origin, identifies the state; connections sharing
    /// it share the breaker. Must not be blank (`INVALID_INPUT`). Default
    /// `default`.
    pub group: String,
    /// Consecutive failures that open the circuit; zero fails with
    /// `INVALID_INPUT`. A failure is a status in `failure_statuses` or a
    /// timeout, transport, or DNS error, after retries; any other outcome
    /// resets the count. Default 5.
    pub failure_threshold: u32,
    /// Time the circuit stays open, in milliseconds, before a single probe
    /// request is let through: its success closes the circuit, its failure
    /// reopens it. Default 30 000.
    pub recovery_timeout_ms: u64,
    /// HTTP statuses counted as failures. Default 429, 500, 502, 503, 504.
    pub failure_statuses: Vec<u16>,
}

impl Default for CircuitBreakerPolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            group: "default".to_owned(),
            failure_threshold: 5,
            recovery_timeout_ms: 30_000,
            failure_statuses: vec![429, 500, 502, 503, 504],
        }
    }
}

/// Polling of an asynchronous job started by the initial request.
///
/// When the initial response already reports a success status it is used
/// directly. Otherwise the engine polls a status URL until a success or
/// failure status, then returns the status response, or the response at the
/// result URL when one is configured. Status values are compared
/// case-insensitively; an unknown or `null` status, a failure status, or a
/// poll response without a status fails with `INVALID_RESPONSE`. Follow-up
/// URLs are resolved against the URL of the response they came from.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct PollingConfig {
    /// JSON path, in the initial response, of the status URL. Takes
    /// precedence over `url_template` and `location_header`.
    pub url_path: Option<String>,
    /// Status URL template: `{id}` and `{job_id}` become the job id and
    /// `{base}` the origin of the response. Used when `url_path` is not set;
    /// required by `resume`.
    pub url_template: Option<String>,
    /// JSON path of the job id in the initial response, used when
    /// `id_header` is not set; a `null` id counts as no id. Default `id`.
    pub id_path: String,
    /// Response header holding the job id, instead of `id_path`.
    pub id_header: Option<String>,
    /// Response header holding the status URL, used when neither `url_path`
    /// nor `url_template` is set. Default `location`. With all three unset
    /// the request fails with `INVALID_INPUT`.
    pub location_header: Option<String>,
    /// Method of the status requests. Default GET.
    pub method: HttpMethod,
    /// JSON path of the status in every status response. Default `status`.
    pub status_path: String,
    /// JSON path of the result inside the final response; absent means the
    /// whole response. A path that does not resolve fails with
    /// `INVALID_RESPONSE`.
    pub result_path: Option<String>,
    /// JSON path, in the final status response, of a URL from which the
    /// result is fetched. It may use the same placeholders as
    /// `url_template`. `download` needs it or `result_url_template`: the
    /// file is fetched from that URL.
    pub result_url_path: Option<String>,
    /// Template of the result URL, with the placeholders of `url_template`;
    /// takes precedence over `result_url_path`.
    pub result_url_template: Option<String>,
    /// Method of the result request. Default GET.
    pub result_method: HttpMethod,
    /// Statuses meaning the job is still running. Default `pending`,
    /// `queued`, `running`, `processing`, `in_progress`.
    pub pending_values: Vec<String>,
    /// Statuses meaning the job succeeded. Default `completed`, `complete`,
    /// `succeeded`, `success`, `done`.
    pub success_values: Vec<String>,
    /// Statuses meaning the job failed. Default `failed`, `error`,
    /// `cancelled`, `canceled`.
    pub failure_values: Vec<String>,
    /// Wait before each status request, in milliseconds. Default 1 000.
    pub interval_ms: u64,
    /// Multiplier applied to the wait after each status request; must be
    /// finite and at least 1, otherwise `INVALID_INPUT`. Default 1.0.
    pub interval_backoff: f64,
    /// Ceiling of the wait between status requests, in milliseconds.
    /// Default 30 000.
    pub max_interval_ms: u64,
    /// Total polling time, in milliseconds, bounding both the waits and the
    /// status requests; zero fails with `INVALID_INPUT`. Default `None`: only
    /// `max_attempts` bounds polling.
    pub max_wait_ms: Option<u64>,
    /// Status requests at most; zero fails with `INVALID_INPUT`. Running out
    /// of attempts or of `max_wait_ms` fails with `POLLING_TIMEOUT`.
    /// Default 60.
    pub max_attempts: u32,
    /// Allows status, result, and cancel URLs on another origin; otherwise
    /// they fail with `UNSAFE_ADDRESS`. Credentials, cookies, and the TLS
    /// client identity are never sent there. Default false.
    pub allow_cross_origin: bool,
    /// Resumes polling an existing job instead of sending the initial
    /// request. Default `None`.
    pub resume: Option<PollingResumeConfig>,
    /// Remote cancellation of the job when the execution stops early.
    /// Default `None`: the job is never cancelled remotely.
    pub cancel: Option<PollingCancelConfig>,
}

impl Default for PollingConfig {
    fn default() -> Self {
        Self {
            url_path: None,
            url_template: None,
            id_path: "id".to_owned(),
            id_header: None,
            location_header: Some("location".to_owned()),
            method: HttpMethod::Get,
            status_path: "status".to_owned(),
            result_path: None,
            result_url_path: None,
            result_url_template: None,
            result_method: HttpMethod::Get,
            pending_values: vec![
                "pending".to_owned(),
                "queued".to_owned(),
                "running".to_owned(),
                "processing".to_owned(),
                "in_progress".to_owned(),
            ],
            success_values: vec![
                "completed".to_owned(),
                "complete".to_owned(),
                "succeeded".to_owned(),
                "success".to_owned(),
                "done".to_owned(),
            ],
            failure_values: vec![
                "failed".to_owned(),
                "error".to_owned(),
                "cancelled".to_owned(),
                "canceled".to_owned(),
            ],
            interval_ms: 1_000,
            interval_backoff: 1.0,
            max_interval_ms: 30_000,
            max_wait_ms: None,
            max_attempts: 60,
            allow_cross_origin: false,
            resume: None,
            cancel: None,
        }
    }
}

/// Resumption of a job whose id the caller already holds, for example from
/// an [`AsyncJobRecovery`]. The initial request is not repeated.
///
/// Needs `polling.url_template`; it cannot be combined with pagination or an
/// enabled batch, and `enrich` must have exactly one input record. Otherwise
/// the request fails with `INVALID_INPUT`.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PollingResumeConfig {
    /// The job id: 1 to 512 characters, no control characters, otherwise
    /// `INVALID_INPUT`.
    pub job_id: String,
}

/// Best-effort remote cancellation of a polled job.
///
/// The cancel request is sent once, without retries, when one of the enabled
/// triggers stops the execution; all cancellations of an execution share a
/// five second budget. The outcome is reported in
/// [`AsyncJobRecovery::cancel_accepted`].
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct PollingCancelConfig {
    /// Cancel URL template with the placeholders of
    /// `PollingConfig::url_template`, resolved against the status URL.
    /// Default `None`: the status URL itself.
    pub url_template: Option<String>,
    /// Method of the cancel request. Default DELETE.
    pub method: HttpMethod,
    /// Timeout of the cancel request, in milliseconds; zero fails with
    /// `INVALID_INPUT`. Default 5 000.
    pub timeout_ms: u64,
    /// Cancels when the caller's cancellation token fires. Default true.
    pub on_cancellation: bool,
    /// Cancels when the execution deadline expires. Default true.
    pub on_deadline: bool,
    /// Cancels when polling ends with `POLLING_TIMEOUT`. Default false.
    pub on_poll_timeout: bool,
}

impl Default for PollingCancelConfig {
    fn default() -> Self {
        Self {
            url_template: None,
            method: HttpMethod::Delete,
            timeout_ms: 5_000,
            on_cancellation: true,
            on_deadline: true,
            on_poll_timeout: false,
        }
    }
}

/// Pagination for `generate`, an object tagged by `type` in JSON.
///
/// Pages are requested in sequence and their records concatenated, up to
/// `max_rows`. Every page is subject to the credential scope of the first
/// one: a page on another origin receives no credentials, and neither does
/// any later page.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum PaginationConfig {
    /// `offset`: sends an offset and a limit, advancing the offset by
    /// `page_size`; stops when a page has fewer records than requested.
    Offset {
        /// Parameter carrying the offset. Default `offset`.
        #[serde(default = "default_offset_param")]
        offset_param: String,
        /// Parameter carrying the limit: `page_size`, or fewer for the last
        /// page under `max_rows`. Default `limit`.
        #[serde(default = "default_limit_param")]
        limit_param: String,
        /// Records requested per page; zero fails with `INVALID_INPUT`.
        /// Default 100.
        #[serde(default = "default_page_size")]
        page_size: usize,
        /// Records collected at most; extra records are dropped.
        /// Default 10 000.
        #[serde(default = "default_max_rows")]
        max_rows: usize,
        /// Offset of the first page. Default 0.
        #[serde(default)]
        start: usize,
    },
    /// `page`: sends a page number and a page size, advancing the number by
    /// one; stops when a page has fewer records than requested.
    Page {
        /// Parameter carrying the page number. Default `page`.
        #[serde(default = "default_page_param")]
        page_param: String,
        /// Parameter carrying the page size. Default `page_size`.
        #[serde(default = "default_page_size_param")]
        page_size_param: String,
        /// Records requested per page; zero fails with `INVALID_INPUT`.
        /// Default 100.
        #[serde(default = "default_page_size")]
        page_size: usize,
        /// Records collected at most; extra records are dropped.
        /// Default 10 000.
        #[serde(default = "default_max_rows")]
        max_rows: usize,
        /// Number of the first page. Default 1.
        #[serde(default = "default_start_page")]
        start_page: usize,
    },
    /// `cursor`: sends the cursor read from the previous page; stops when a
    /// page has no cursor or repeats one already seen.
    Cursor {
        /// Parameter carrying the cursor, omitted on the first page.
        /// Default `cursor`.
        #[serde(default = "default_cursor_param")]
        cursor_param: String,
        /// JSON path of the next cursor in each page. Default `next_cursor`.
        #[serde(default = "default_cursor_path")]
        cursor_path: String,
        /// Records collected at most; extra records are dropped.
        /// Default 10 000.
        #[serde(default = "default_max_rows")]
        max_rows: usize,
        /// Pages requested at most. Default 100.
        #[serde(default = "default_max_pages")]
        max_pages: usize,
    },
    /// `link`: follows the next-page URL found in the response body; stops
    /// when there is none or it repeats one already followed. Parameters are
    /// sent only with the first page.
    Link {
        /// JSON path of the next-page URL, resolved against the page URL.
        /// Default `next`.
        #[serde(default = "default_link_path")]
        link_path: String,
        /// Records collected at most; extra records are dropped.
        /// Default 10 000.
        #[serde(default = "default_max_rows")]
        max_rows: usize,
        /// Pages requested at most. Default 100.
        #[serde(default = "default_max_pages")]
        max_pages: usize,
        /// Allows next-page URLs on another origin, which receive no
        /// credentials; otherwise they fail with `UNSAFE_ADDRESS`.
        /// Default false.
        #[serde(default)]
        allow_cross_origin: bool,
    },
    /// `header_link`: follows the RFC 8288 `Link` response header; stops when
    /// there is no matching link or it repeats one already followed.
    /// Parameters are sent only with the first page.
    HeaderLink {
        /// Link relation to follow, compared case-insensitively; blank fails
        /// with `INVALID_INPUT`. Default `next`.
        #[serde(default = "default_next_relation")]
        relation: String,
        /// Records collected at most; extra records are dropped.
        /// Default 10 000.
        #[serde(default = "default_max_rows")]
        max_rows: usize,
        /// Pages requested at most. Default 100.
        #[serde(default = "default_max_pages")]
        max_pages: usize,
        /// Allows next-page URLs on another origin, which receive no
        /// credentials; otherwise they fail with `UNSAFE_ADDRESS`.
        /// Default false.
        #[serde(default)]
        allow_cross_origin: bool,
    },
}

/// Input data of an execution. Every field is optional.
#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ExecutionInput {
    /// Parameters of the execution, overriding `static_parameters`; for
    /// `enrich` they are shared by every record, and a record overrides them
    /// by name.
    pub params: JsonObject,
    /// Records to enrich, one request (or one batch slot) each; reported in
    /// `metrics.input_records`.
    pub records: Vec<JsonObject>,
    /// The local file of a `download` or `upload`; required by those
    /// operations. The runtime binding refuses it for the others.
    pub file: Option<FileTransferInput>,
}

/// The local side of a file transfer: the file and how to check it.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct FileTransferInput {
    /// File path, absolute or relative to `EngineConfig::file_root`, that
    /// must resolve inside the root (`POLICY_VIOLATION` otherwise). For a
    /// download the parent directory must exist. On the runtime boundary it
    /// must be empty and is filled from the artifact reference.
    pub path: String,
    /// Artifact an upload reads, resolved by
    /// [`RuntimeResources`](crate::RuntimeResources); required by a runtime
    /// upload and refused for a runtime download. Its reference is reported
    /// as `artifact_reference`.
    pub artifact_source: Option<ArtifactReference>,
    /// Artifact a download writes, resolved by
    /// [`RuntimeResources`](crate::RuntimeResources); required by a runtime
    /// download and refused for a runtime upload. Its reference is reported
    /// as `artifact_reference`.
    pub artifact_sink: Option<ArtifactReference>,
    /// Lets a download replace an existing file; otherwise an existing target
    /// fails with `FILE_IO`. A directory is never replaced. Default false.
    pub overwrite: bool,
    /// Lets a download retry continue where the previous attempt stopped
    /// with `Range` and `If-Range`, when the response had an `ETag`, instead
    /// of starting again. It applies to the retries of one execution only.
    /// Default false.
    pub resume: bool,
    /// Byte limit for this transfer, capped by
    /// `EngineConfig::max_file_transfer_bytes`; zero fails with
    /// `INVALID_INPUT`, exceeding it with `FILE_TOO_LARGE`.
    pub max_bytes: Option<u64>,
    /// Expected SHA-256 of the file, 64 hexadecimal characters in any case
    /// (`INVALID_INPUT` otherwise). A different digest fails with
    /// `CHECKSUM_MISMATCH`: for a download the file is not published, for an
    /// upload nothing is sent.
    pub expected_sha256: Option<String>,
    /// Upload `Content-Type` of the raw body or of the multipart part;
    /// reported as `media_type`.
    pub content_type: Option<String>,
    /// File name of the multipart part. Default: the source file name.
    pub filename: Option<String>,
    /// Multipart field name of the file part. Default `file`.
    pub field_name: String,
}

impl Default for FileTransferInput {
    fn default() -> Self {
        Self {
            path: String::new(),
            artifact_source: None,
            artifact_sink: None,
            overwrite: false,
            resume: false,
            max_bytes: None,
            expected_sha256: None,
            content_type: None,
            filename: None,
            field_name: "file".to_owned(),
        }
    }
}

/// Options of an execution. Every field is optional.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ExecutionOptions {
    /// For `enrich`, records (or batch chunks) after a failing one are still
    /// processed and the result is `partial`; when false, processing stops at
    /// the first failure. Default true.
    pub continue_on_error: bool,
    /// Reports status, origin, attempts, and selected headers of each
    /// response in `responses`. Default false.
    pub capture_response_metadata: bool,
    /// Response headers to report, matched case-insensitively; `*` selects
    /// all. Sensitive headers are never reported. Default empty.
    pub response_headers: Vec<String>,
    /// Records `enrich` processes at once; zero fails with `INVALID_INPUT`.
    /// Values above 1 apply only with `continue_on_error`; output order stays
    /// input order. Default 1.
    pub enrichment_concurrency: usize,
    /// RFC 3339 instant at which the execution stops with `TIMEOUT`; an
    /// instant already past stops it before any network activity. Anything
    /// else fails with `INVALID_INPUT`.
    pub deadline: Option<String>,
    /// Idempotency key: 1 to 255 visible ASCII bytes (`INVALID_INPUT`
    /// otherwise), sent as described by `connection.idempotency` and enabling
    /// retries of non-idempotent methods. Reusing it with a different request
    /// fails with `IDEMPOTENCY_CONFLICT`. Each page, record, or batch chunk
    /// sends its own key derived from it.
    pub idempotency_key: Option<String>,
}

impl Default for ExecutionOptions {
    fn default() -> Self {
        Self {
            continue_on_error: true,
            capture_response_metadata: false,
            response_headers: Vec::new(),
            enrichment_concurrency: 1,
            deadline: None,
            idempotency_key: None,
        }
    }
}

/// Outcome of an execution: the `plenora-rest-execution-result-v1` contract
/// (for `download` and `upload`, the file transfer result contract).
///
/// Failures are reported here, never as a Rust error.
#[derive(Clone, Debug, Serialize)]
pub struct ExecutionResult {
    /// Always [`SCHEMA_VERSION`].
    pub schema_version: u32,
    /// Overall outcome.
    pub status: ExecutionStatus,
    /// Produced output; [`ExecutionOutput::None`] when the execution failed
    /// as a whole.
    pub output: ExecutionOutput,
    /// Counters of the execution.
    pub metrics: ExecutionMetrics,
    /// Response metadata, filled only with
    /// `options.capture_response_metadata`.
    pub responses: Vec<HttpResponseMetadata>,
    /// Errors, each with its `input_index` when it belongs to one record.
    pub errors: Vec<ExecutionError>,
    /// Asynchronous jobs left unfinished, at most 128; omitted from JSON when
    /// empty.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub recoveries: Vec<AsyncJobRecovery>,
}

/// An asynchronous job the execution left unfinished, so the caller can
/// resume it with [`PollingResumeConfig`] instead of resubmitting it.
#[derive(Clone, Debug, Serialize)]
pub struct AsyncJobRecovery {
    /// Always [`ASYNC_JOB_RECOVERY_CONTRACT`].
    pub contract: String,
    /// The job id, 1 to 512 characters.
    pub job_id: String,
    /// True when a remote cancellation was triggered.
    pub cancel_requested: bool,
    /// Whether the cancel request got a 2xx response; absent when none was
    /// sent or its outcome is unknown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cancel_accepted: Option<bool>,
}

/// Public metadata of one response.
#[derive(Clone, Debug, Serialize)]
pub struct HttpResponseMetadata {
    /// HTTP status of the final response.
    pub status: u16,
    /// Origin (`scheme://host[:port]`) the response came from; path and
    /// query are never reported.
    pub final_url: String,
    /// Attempts made, at least 1 (a cache hit counts as one).
    pub attempts: u32,
    /// Headers selected by `options.response_headers`, names in lower case,
    /// repeated headers joined with `, `.
    pub headers: BTreeMap<String, String>,
}

/// Overall outcome of an execution, `snake_case` in JSON.
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionStatus {
    /// No errors.
    Success,
    /// Some records failed and at least one succeeded.
    Partial,
    /// Nothing succeeded.
    Failed,
}

/// Output of an execution, an object tagged by `type` in JSON.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ExecutionOutput {
    /// No output: the execution failed as a whole.
    None,
    /// The parsed response of `test`.
    Json {
        /// The response, after polling and `result_path` when configured.
        value: Value,
    },
    /// The records of `generate` and `enrich`.
    Records {
        /// Output records; for `enrich`, a failed record is passed through
        /// unchanged.
        records: Vec<JsonObject>,
    },
    /// The outcome of `download` or `upload`.
    File {
        /// Direction of the transfer.
        direction: FileTransferDirection,
        /// The artifact reference of the request, or `sha256:<digest>`
        /// without one; never a local path.
        artifact_reference: String,
        /// Bytes in the published download, or the size of the uploaded
        /// file.
        bytes_transferred: u64,
        /// SHA-256 of the transferred bytes.
        checksum: IntegrityMetadata,
        /// For a download the response `Content-Type`, for an upload
        /// `input.file.content_type`; omitted when absent.
        #[serde(skip_serializing_if = "Option::is_none")]
        media_type: Option<String>,
        /// The parsed upload response; omitted for downloads.
        #[serde(skip_serializing_if = "Option::is_none")]
        response: Option<Value>,
    },
}

/// Opaque reference to an artifact owned by the host.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactReference {
    /// The reference, resolved only by
    /// [`RuntimeResources`](crate::RuntimeResources). On the runtime boundary
    /// it must be 1 to 512 bytes, relative, without `..` segments or a
    /// `file:` scheme, otherwise `INVALID_INPUT`.
    pub reference: String,
}

/// Integrity digest of a transferred file.
#[derive(Clone, Debug, Serialize)]
pub struct IntegrityMetadata {
    /// Digest algorithm; always `sha256`.
    pub algorithm: String,
    /// Digest in lower-case hexadecimal.
    pub value: String,
}

/// Direction of a file transfer, `snake_case` in JSON.
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FileTransferDirection {
    /// From the service to a local file.
    Download,
    /// From a local file to the service.
    Upload,
}

/// Counters of one execution. All counters saturate instead of overflowing.
#[derive(Clone, Debug, Default, Serialize)]
pub struct ExecutionMetrics {
    /// HTTP requests sent over the network, redirects, retries, token, poll,
    /// and result requests included; cache hits excluded.
    pub requests: u64,
    /// Attempts beyond the first, token requests included.
    pub retries: u64,
    /// Requests to OAuth and ArcGIS token endpoints.
    pub auth_requests: u64,
    /// Status and result requests of asynchronous polling.
    pub poll_requests: u64,
    /// Responses served from the HTTP cache, revalidated ones included.
    pub cache_hits: u64,
    /// Cache entries confirmed by a 304 response.
    pub cache_revalidations: u64,
    /// Time spent waiting for the rate limiter, in milliseconds.
    pub rate_limit_wait_ms: u64,
    /// Length of `input.records`.
    pub input_records: usize,
    /// Records (or the single test or file result) that succeeded.
    pub output_records: usize,
    /// Download body bytes received, those of abandoned attempts included.
    pub bytes_downloaded: u64,
    /// Bytes of a completed upload.
    pub bytes_uploaded: u64,
    /// Execution time, in milliseconds; zero when the execution was refused
    /// before it started.
    pub elapsed_ms: u64,
}

/// One error of an execution, in the shape of the `plenora-error-v1`
/// contract.
///
/// It carries no remote text, parameter values, paths, or addresses.
#[derive(Clone, Debug, Serialize)]
pub struct ExecutionError {
    /// Error category.
    pub category: ErrorCategory,
    /// Phase in which the error occurred.
    pub phase: ErrorPhase,
    /// What the failure may have done on the remote side.
    pub remote_effect: RemoteEffect,
    /// Whether and how the operation may be retried.
    pub retry: RetryAdvice,
    /// Stable machine-readable code, such as `INVALID_INPUT`.
    pub code: String,
    /// Static, human-readable message of the code.
    pub message: String,
    /// Index of the input record the error belongs to, omitted when it
    /// belongs to the whole execution.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_index: Option<usize>,
    /// Structured details chosen by the engine, such as `limit_bytes` or
    /// `http_status`.
    pub details: BTreeMap<String, Value>,
}

impl HttpMethod {
    /// The method name as sent on the wire: upper case for the standard
    /// methods, verbatim for [`HttpMethod::Custom`].
    pub fn as_str(&self) -> &str {
        match self {
            Self::Get => "GET",
            Self::Head => "HEAD",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Patch => "PATCH",
            Self::Delete => "DELETE",
            Self::Options => "OPTIONS",
            Self::Custom(value) => value,
        }
    }

    pub(crate) fn is_idempotent(&self) -> bool {
        matches!(
            self,
            Self::Get | Self::Head | Self::Put | Self::Delete | Self::Options
        )
    }

    pub(crate) fn is_custom(&self) -> bool {
        matches!(self, Self::Custom(_))
    }

    fn parse(value: String) -> Result<Self, String> {
        match value.as_str() {
            "GET" => Ok(Self::Get),
            "HEAD" => Ok(Self::Head),
            "POST" => Ok(Self::Post),
            "PUT" => Ok(Self::Put),
            "PATCH" => Ok(Self::Patch),
            "DELETE" => Ok(Self::Delete),
            "OPTIONS" => Ok(Self::Options),
            _ if is_http_token(&value) => Ok(Self::Custom(value)),
            _ => Err("HTTP method must be a non-empty RFC 9110 token".to_owned()),
        }
    }
}

impl Serialize for HttpMethod {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for HttpMethod {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(D::Error::custom)
    }
}

fn is_http_token(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'\x60'
                        | b'|'
                        | b'~'
                )
        })
}

fn default_offset_param() -> String {
    "offset".to_owned()
}
fn default_limit_param() -> String {
    "limit".to_owned()
}
fn default_page_param() -> String {
    "page".to_owned()
}
fn default_page_size_param() -> String {
    "page_size".to_owned()
}
fn default_cursor_param() -> String {
    "cursor".to_owned()
}
fn default_cursor_path() -> String {
    "next_cursor".to_owned()
}
fn default_link_path() -> String {
    "next".to_owned()
}
fn default_next_relation() -> String {
    "next".to_owned()
}
fn default_page_size() -> usize {
    100
}
fn default_max_rows() -> usize {
    10_000
}
fn default_max_pages() -> usize {
    100
}
fn default_start_page() -> usize {
    1
}
fn default_arcgis_client() -> String {
    "requestip".to_owned()
}
fn default_arcgis_expiration() -> u32 {
    60
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::{OutputMapping, ParameterSpec, ResponseTransform};

    #[test]
    fn an_explicit_null_is_kept_apart_from_an_absent_value() {
        let explicit: ParameterSpec =
            serde_json::from_value(json!({"name": "p", "mode": "fixed", "value": null})).unwrap();
        assert_eq!(explicit.value, Some(Value::Null));
        let absent: ParameterSpec =
            serde_json::from_value(json!({"name": "p", "mode": "fixed"})).unwrap();
        assert_eq!(absent.value, None);

        let transform: ResponseTransform = serde_json::from_value(
            json!({"column": "c", "source": "s", "operation": "default_if_null", "value": null}),
        )
        .unwrap();
        assert_eq!(transform.value, Some(Value::Null));

        let mapping: OutputMapping =
            serde_json::from_value(json!({"path": "a", "column": "c", "default": null})).unwrap();
        assert_eq!(mapping.default, Some(Value::Null));
    }

    #[test]
    fn null_means_absent_for_typed_optional_fields() {
        let request: super::RequestConfig =
            serde_json::from_value(json!({"timeout_ms": null})).unwrap();
        assert_eq!(request.timeout_ms, None);
        let response: super::ResponseConfig = serde_json::from_value(json!({
            "records_path": null,
            "error_path": null,
            "success_when": null
        }))
        .unwrap();
        assert_eq!(response.records_path, None);
        assert_eq!(response.error_path, None);
        assert_eq!(response.success_when, None);
    }

    #[test]
    fn serialization_round_trips_absent_and_null() {
        for document in [
            json!({"name": "p", "mode": "fixed", "value": null}),
            json!({"name": "p", "mode": "fixed"}),
        ] {
            let spec: ParameterSpec = serde_json::from_value(document.clone()).unwrap();
            let again: ParameterSpec =
                serde_json::from_value(serde_json::to_value(&spec).unwrap()).unwrap();
            assert_eq!(again.value, spec.value, "{document}");
        }
    }

    #[test]
    fn a_cookie_session_token_has_one_canonical_128_bit_spelling() {
        let session = super::CookieSession {
            engine: 0x0123_4567_89ab_cdef,
            slot: 7,
            generation: 42,
            nonce: u128::MAX - 1,
        };
        let token = session.to_token();
        assert_eq!(
            token,
            "rcs1.0123456789abcdef.7.42.fffffffffffffffffffffffffffffffe"
        );
        assert_eq!(super::CookieSession::from_token(&token), Some(session));
        for malformed in [
            // A 64-bit nonce, the earlier format.
            "rcs1.0123456789abcdef.7.42.fffffffffffffffe",
            // Uppercase, a sign, leading zeros, extra parts.
            "rcs1.0123456789ABCDEF.7.42.fffffffffffffffffffffffffffffffe",
            "rcs1.0123456789abcdef.+7.42.fffffffffffffffffffffffffffffffe",
            "rcs1.0123456789abcdef.07.42.fffffffffffffffffffffffffffffffe",
            "rcs1.0123456789abcdef.7.42.fffffffffffffffffffffffffffffffe.0",
        ] {
            assert_eq!(
                super::CookieSession::from_token(malformed),
                None,
                "{malformed}"
            );
        }
    }
}
