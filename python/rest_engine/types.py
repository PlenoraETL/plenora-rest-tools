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


class ConnectionConfig(TypedDict, total=False):
    url: str
    method: str
    headers: Dict[str, str]
    auth: JsonObject
    parameters: List[ParameterSpec]
    static_parameters: JsonObject
    request: JsonObject
    response: JsonObject
    retry: JsonObject
    pagination: Optional[JsonObject]
    polling: Optional[JsonObject]
    batch: Optional[JsonObject]
    success_statuses: List[int]
    requests_per_second: Optional[float]
    tls: JsonObject
    proxy: Optional[JsonObject]


class ExecutionError(TypedDict, total=False):
    code: str
    message: str
    retriable: bool
    input_index: int
    http_status: int
    body_preview: str


class ExecutionMetrics(TypedDict):
    requests: int
    retries: int
    auth_requests: int
    poll_requests: int
    rate_limit_wait_ms: int
    input_records: int
    output_records: int
    bytes_downloaded: int
    bytes_uploaded: int
    elapsed_ms: int


class FileTransferInput(TypedDict, total=False):
    path: str
    overwrite: bool
    max_bytes: Optional[int]
    expected_sha256: Optional[str]
    content_type: Optional[str]
    filename: Optional[str]
    field_name: str


class FileOutput(TypedDict, total=False):
    type: Literal["file"]
    direction: Literal["download", "upload"]
    path: str
    bytes_transferred: int
    sha256: str
    response: Any


class ExecutionOptions(TypedDict, total=False):
    continue_on_error: bool
    capture_response_metadata: bool
    response_headers: List[str]


class HttpResponseMetadata(TypedDict):
    status: int
    final_url: str
    attempts: int
    headers: Dict[str, str]


class ExecutionResult(TypedDict):
    schema_version: int
    status: Literal["success", "partial", "failed"]
    output: JsonObject
    metrics: ExecutionMetrics
    responses: List[HttpResponseMetadata]
    errors: List[ExecutionError]
