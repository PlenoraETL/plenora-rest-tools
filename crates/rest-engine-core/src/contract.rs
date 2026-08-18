use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};
use serde_json::{Map, Value};

pub const SCHEMA_VERSION: u32 = 1;
pub type JsonObject = Map<String, Value>;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct EngineConfig {
    pub connect_timeout_ms: u64,
    pub request_timeout_ms: u64,
    pub max_request_bytes: usize,
    pub max_response_bytes: usize,
    pub max_pooled_origins: usize,
    pub pool_max_idle_per_host: usize,
    pub pool_idle_timeout_ms: u64,
    pub max_concurrent_requests: usize,
    pub requests_per_second: Option<u32>,
    pub allow_private_networks: bool,
    pub allow_insecure_tls: bool,
    pub allow_proxies: bool,
    pub automatic_decompression: bool,
    pub allowed_custom_methods: Vec<String>,
    pub user_agent: String,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            connect_timeout_ms: 5_000,
            request_timeout_ms: 30_000,
            max_request_bytes: 32 * 1024 * 1024,
            max_response_bytes: 32 * 1024 * 1024,
            max_pooled_origins: 128,
            pool_max_idle_per_host: 50,
            pool_idle_timeout_ms: 90_000,
            max_concurrent_requests: 64,
            requests_per_second: None,
            allow_private_networks: false,
            allow_insecure_tls: false,
            allow_proxies: false,
            automatic_decompression: true,
            allowed_custom_methods: Vec::new(),
            user_agent: format!("rest-engine/{}", env!("CARGO_PKG_VERSION")),
        }
    }
}

#[derive(Clone, Deserialize, Serialize)]
pub struct ExecutionRequest {
    pub schema_version: u32,
    pub operation: ExecutionOperation,
    pub connection: ConnectionConfig,
    #[serde(default)]
    pub input: ExecutionInput,
    #[serde(default)]
    pub options: ExecutionOptions,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionOperation {
    Test,
    Generate,
    Enrich,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ConnectionConfig {
    pub url: String,
    pub method: HttpMethod,
    pub headers: BTreeMap<String, String>,
    pub auth: AuthConfig,
    pub parameters: Vec<ParameterSpec>,
    pub static_parameters: JsonObject,
    pub request: RequestConfig,
    pub response: ResponseConfig,
    pub retry: RetryPolicy,
    pub pagination: Option<PaginationConfig>,
    pub polling: Option<PollingConfig>,
    pub batch: Option<BatchConfig>,
    pub success_statuses: Vec<u16>,
    pub requests_per_second: Option<f64>,
    pub tls: TlsConfig,
    pub proxy: Option<ProxyConfig>,
}

impl Default for ConnectionConfig {
    fn default() -> Self {
        Self {
            url: String::new(),
            method: HttpMethod::Get,
            headers: BTreeMap::new(),
            auth: AuthConfig::None,
            parameters: Vec::new(),
            static_parameters: JsonObject::new(),
            request: RequestConfig::default(),
            response: ResponseConfig::default(),
            retry: RetryPolicy::default(),
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

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct BatchConfig {
    pub enabled: bool,
    pub max_size: usize,
    pub input_key: String,
    pub input_format: BatchInputFormat,
    pub output_path: String,
    pub endpoint_override: Option<String>,
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

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BatchInputFormat {
    #[default]
    Array,
    FlatArray,
    Object,
}

#[derive(Clone, Debug, Deserialize, Hash, Serialize)]
#[serde(default)]
pub struct TlsConfig {
    pub verify: bool,
    pub ca_bundle_pem: Option<String>,
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

#[derive(Clone, Debug, Deserialize, Hash, Serialize)]
pub struct ProxyConfig {
    pub url: String,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum HttpMethod {
    #[default]
    Get,
    Head,
    Post,
    Put,
    Patch,
    Delete,
    Options,
    Custom(String),
}

#[derive(Clone, Default, Deserialize, Hash, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AuthConfig {
    #[default]
    None,
    Bearer {
        token: String,
    },
    ApiKey {
        key_name: String,
        key_value: String,
        #[serde(default)]
        location: ApiKeyLocation,
    },
    #[serde(rename = "basic_auth", alias = "basic")]
    Basic {
        username: String,
        password: String,
    },
    #[serde(rename = "oauth2_client_credentials")]
    OAuth2ClientCredentials {
        token_url: String,
        client_id: String,
        client_secret: String,
        #[serde(default)]
        scope: Option<String>,
        #[serde(default)]
        audience: Option<String>,
        #[serde(default)]
        extra_params: BTreeMap<String, String>,
        #[serde(default)]
        client_auth: OAuthClientAuth,
    },
    OAuth2Password {
        token_url: String,
        username: String,
        password: String,
        #[serde(default)]
        client_id: Option<String>,
        #[serde(default)]
        client_secret: Option<String>,
        #[serde(default)]
        scope: Option<String>,
        #[serde(default)]
        extra_params: BTreeMap<String, String>,
    },
    ArcgisToken {
        token_url: String,
        username: String,
        password: String,
        #[serde(default = "default_arcgis_client")]
        client: String,
        #[serde(default)]
        referer: Option<String>,
        #[serde(default)]
        ip: Option<String>,
        #[serde(default = "default_arcgis_expiration")]
        expiration: u32,
    },
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, Hash, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OAuthClientAuth {
    #[default]
    Basic,
    Body,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Hash, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ApiKeyLocation {
    #[default]
    Header,
    Query,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ParameterSpec {
    pub name: String,
    #[serde(default)]
    pub mode: ParameterMode,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub value: Option<Value>,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub location: ParameterLocation,
    #[serde(default)]
    pub query_serialization: Option<QuerySerialization>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ParameterMode {
    #[default]
    Mapped,
    Fixed,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ParameterLocation {
    #[default]
    Auto,
    Path,
    Query,
    Header,
    Body,
    Cookie,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct QuerySerialization {
    pub style: QueryStyle,
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

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum QueryStyle {
    #[default]
    Form,
    #[serde(alias = "spaceDelimited")]
    SpaceDelimited,
    #[serde(alias = "pipeDelimited")]
    PipeDelimited,
    #[serde(alias = "deepObject")]
    DeepObject,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct RequestConfig {
    pub body_type: BodyType,
    pub raw_body: Option<String>,
    pub timeout_ms: Option<u64>,
    pub allow_redirects: bool,
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

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BodyType {
    #[default]
    Json,
    #[serde(alias = "form-urlencoded")]
    FormUrlencoded,
    Multipart,
    Raw,
    None,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct ResponseConfig {
    pub format: ResponseFormat,
    pub delimiter: String,
    pub records_path: Option<String>,
    pub output_mapping: Vec<OutputMapping>,
    pub iterate_on: Vec<IterationSpec>,
    pub transforms: Vec<ResponseTransform>,
    pub success_when: Option<Value>,
    pub error_path: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ResponseFormat {
    #[default]
    Json,
    Csv,
    Xml,
    Ndjson,
    Text,
    Binary,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct IterationSpec {
    #[serde(default)]
    pub path: String,
    #[serde(rename = "as")]
    pub alias: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ResponseTransform {
    pub column: String,
    pub source: String,
    pub operation: String,
    #[serde(default)]
    pub value: Option<Value>,
    #[serde(default)]
    pub condition: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct OutputMapping {
    pub path: String,
    pub column: String,
    #[serde(default)]
    pub default: Option<Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub backoff_base_ms: u64,
    pub backoff_factor: f64,
    pub max_backoff_ms: u64,
    pub retry_on_status: Vec<u16>,
    pub respect_retry_after: bool,
    pub max_retry_after_ms: u64,
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

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct PollingConfig {
    pub url_path: Option<String>,
    pub url_template: Option<String>,
    pub id_path: String,
    pub id_header: Option<String>,
    pub location_header: Option<String>,
    pub method: HttpMethod,
    pub status_path: String,
    pub result_path: Option<String>,
    pub result_url_path: Option<String>,
    pub result_url_template: Option<String>,
    pub result_method: HttpMethod,
    pub pending_values: Vec<String>,
    pub success_values: Vec<String>,
    pub failure_values: Vec<String>,
    pub interval_ms: u64,
    pub interval_backoff: f64,
    pub max_interval_ms: u64,
    pub max_wait_ms: Option<u64>,
    pub max_attempts: u32,
    pub allow_cross_origin: bool,
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
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PaginationConfig {
    Offset {
        #[serde(default = "default_offset_param")]
        offset_param: String,
        #[serde(default = "default_limit_param")]
        limit_param: String,
        #[serde(default = "default_page_size")]
        page_size: usize,
        #[serde(default = "default_max_rows")]
        max_rows: usize,
        #[serde(default)]
        start: usize,
    },
    Page {
        #[serde(default = "default_page_param")]
        page_param: String,
        #[serde(default = "default_page_size_param")]
        page_size_param: String,
        #[serde(default = "default_page_size")]
        page_size: usize,
        #[serde(default = "default_max_rows")]
        max_rows: usize,
        #[serde(default = "default_start_page")]
        start_page: usize,
    },
    Cursor {
        #[serde(default = "default_cursor_param")]
        cursor_param: String,
        #[serde(default = "default_cursor_path")]
        cursor_path: String,
        #[serde(default = "default_max_rows")]
        max_rows: usize,
        #[serde(default = "default_max_pages")]
        max_pages: usize,
    },
    Link {
        #[serde(default = "default_link_path")]
        link_path: String,
        #[serde(default = "default_max_rows")]
        max_rows: usize,
        #[serde(default = "default_max_pages")]
        max_pages: usize,
        #[serde(default)]
        allow_cross_origin: bool,
    },
    HeaderLink {
        #[serde(default = "default_next_relation")]
        relation: String,
        #[serde(default = "default_max_rows")]
        max_rows: usize,
        #[serde(default = "default_max_pages")]
        max_pages: usize,
        #[serde(default)]
        allow_cross_origin: bool,
    },
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct ExecutionInput {
    pub params: JsonObject,
    pub records: Vec<JsonObject>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct ExecutionOptions {
    pub continue_on_error: bool,
    pub capture_response_metadata: bool,
    pub response_headers: Vec<String>,
}

impl Default for ExecutionOptions {
    fn default() -> Self {
        Self {
            continue_on_error: true,
            capture_response_metadata: false,
            response_headers: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct ExecutionResult {
    pub schema_version: u32,
    pub status: ExecutionStatus,
    pub output: ExecutionOutput,
    pub metrics: ExecutionMetrics,
    pub responses: Vec<HttpResponseMetadata>,
    pub errors: Vec<ExecutionError>,
}

#[derive(Clone, Debug, Serialize)]
pub struct HttpResponseMetadata {
    pub status: u16,
    pub final_url: String,
    pub attempts: u32,
    pub headers: BTreeMap<String, String>,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionStatus {
    Success,
    Partial,
    Failed,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ExecutionOutput {
    None,
    Json { value: Value },
    Records { records: Vec<JsonObject> },
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ExecutionMetrics {
    pub requests: u64,
    pub retries: u64,
    pub auth_requests: u64,
    pub poll_requests: u64,
    pub rate_limit_wait_ms: u64,
    pub input_records: usize,
    pub output_records: usize,
    pub elapsed_ms: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct ExecutionError {
    pub code: String,
    pub message: String,
    pub retriable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_index: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body_preview: Option<String>,
}

impl HttpMethod {
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
