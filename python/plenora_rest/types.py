from typing import Any, Dict, List, Literal, Optional, TypedDict


JsonObject = Dict[str, Any]


class EngineConfig(TypedDict, total=False):
    connect_timeout_ms: int
    request_timeout_ms: int
    max_request_bytes: int
    max_response_bytes: int
    max_file_transfer_bytes: int
    max_pooled_origins: int
    pool_max_idle_per_host: int
    pool_idle_timeout_ms: int
    max_concurrent_requests: int
    requests_per_second: Optional[int]
    allow_private_networks: bool
    allow_insecure_tls: bool
    allow_proxies: bool
    allow_file_transfers: bool
    file_root: Optional[str]
    automatic_decompression: bool
    allowed_custom_methods: List[str]
    allow_cookie_store: bool
    max_cache_entries: int
    max_cache_bytes: int
    max_circuit_origins: int
    max_idempotency_keys: int
    user_agent: str


class QuerySerialization(TypedDict, total=False):
    style: Literal["form", "space_delimited", "pipe_delimited", "deep_object"]
    explode: bool


class ParameterSpec(TypedDict, total=False):
    name: str
    mode: Literal["mapped", "fixed"]
    source: Optional[str]
    value: Any
    required: bool
    location: Literal["auto", "path", "query", "header", "body", "cookie"]
    query_serialization: QuerySerialization


class CookiePolicy(TypedDict, total=False):
    enabled: bool
    jar_id: str


class CachePolicy(TypedDict, total=False):
    enabled: bool
    fresh_for_ms: int
    allow_authenticated: bool


class CircuitBreakerPolicy(TypedDict, total=False):
    enabled: bool
    group: str
    failure_threshold: int
    recovery_timeout_ms: int
    failure_statuses: List[int]


class IdempotencyConfig(TypedDict, total=False):
    name: str
    location: Literal["header", "query", "body"]


class ConnectionConfig(TypedDict, total=False):
    url: str
    method: str
    headers: Dict[str, str]
    auth: JsonObject
    credential_ref: Optional[str]
    parameters: List[ParameterSpec]
    static_parameters: JsonObject
    request: JsonObject
    response: JsonObject
    retry: JsonObject
    cookies: CookiePolicy
    cache: CachePolicy
    circuit_breaker: CircuitBreakerPolicy
    idempotency: IdempotencyConfig
    pagination: Optional[JsonObject]
    polling: Optional[JsonObject]
    batch: Optional[JsonObject]
    success_statuses: List[int]
    requests_per_second: Optional[float]
    tls: JsonObject
    proxy: Optional[JsonObject]


class RetryAdvice(TypedDict):
    kind: Literal[
        "never",
        "quarantine",
        "safe",
        "requires_idempotency_key",
        "requires_recovery",
        "after",
    ]


class ExecutionError(TypedDict, total=False):
    category: str
    phase: str
    remote_effect: str
    retry: RetryAdvice
    code: str
    message: str
    input_index: int
    details: JsonObject


class ExecutionMetrics(TypedDict):
    requests: int
    retries: int
    auth_requests: int
    poll_requests: int
    cache_hits: int
    cache_revalidations: int
    rate_limit_wait_ms: int
    input_records: int
    output_records: int
    bytes_downloaded: int
    bytes_uploaded: int
    elapsed_ms: int


class ArtifactReference(TypedDict):
    reference: str


class FileTransferInput(TypedDict, total=False):
    path: str
    artifact_source: ArtifactReference
    artifact_sink: ArtifactReference
    overwrite: bool
    resume: bool
    max_bytes: Optional[int]
    expected_sha256: Optional[str]
    content_type: Optional[str]
    filename: Optional[str]
    field_name: str


class IntegrityMetadata(TypedDict):
    algorithm: Literal["sha256"]
    value: str


class FileOutput(TypedDict, total=False):
    type: Literal["file"]
    direction: Literal["download", "upload"]
    artifact_reference: str
    bytes_transferred: int
    checksum: IntegrityMetadata
    media_type: Optional[str]
    response: Any


class ExecutionOptions(TypedDict, total=False):
    continue_on_error: bool
    capture_response_metadata: bool
    response_headers: List[str]
    enrichment_concurrency: int
    deadline: Optional[str]
    idempotency_key: Optional[str]


class HttpResponseMetadata(TypedDict):
    status: int
    final_url: str
    attempts: int
    headers: Dict[str, str]


class _AsyncJobRecoveryRequired(TypedDict):
    contract: Literal["plenora-rest-async-job-recovery-v1"]
    job_id: str
    cancel_requested: bool


class AsyncJobRecovery(_AsyncJobRecoveryRequired, total=False):
    cancel_accepted: bool


class _ExecutionResultRequired(TypedDict):
    schema_version: int
    status: Literal["success", "partial", "failed"]
    output: JsonObject
    metrics: ExecutionMetrics
    responses: List[HttpResponseMetadata]
    errors: List[ExecutionError]


class ExecutionResult(_ExecutionResultRequired, total=False):
    recoveries: List[AsyncJobRecovery]


class CapabilityInterface(TypedDict):
    kind: Literal["rust", "python_sdk", "runtime"]
    contract: str
    version: int
    artifact: str


class CapabilityPayload(TypedDict):
    contract: str
    content_types: List[str]


class CapabilityControls(TypedDict):
    cancellation: bool
    deadline: bool
    idempotency_key: bool


class OperationCapability(TypedDict):
    id: str
    version: int
    status: Literal["available"]
    surfaces: List[Literal["rust", "python_sdk", "runtime"]]
    input: CapabilityPayload
    output: CapabilityPayload
    side_effect: Literal["remote"]
    controls: CapabilityControls
    attributes: JsonObject


class CapabilityDocument(TypedDict):
    schema_version: Literal[2]
    component: Literal["plenora-rest-tools"]
    component_version: str
    interfaces: List[CapabilityInterface]
    operations: List[OperationCapability]
