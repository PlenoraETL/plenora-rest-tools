from __future__ import annotations

import json
import os
from collections.abc import Mapping, Sequence
from importlib.metadata import PackageNotFoundError, version as distribution_version
from typing import Any, Optional, Union, cast

from ._native import (
    NativeCancellationToken,
    NativeEngine,
    NativePlenoraError,
    SCHEMA_VERSION,
    __version__,
)
from .types import CapabilityDocument, ExecutionResult


class PlenoraError(RuntimeError):
    """Stable root exception for failures crossing the Python boundary."""

    def __init__(self, payload: Mapping[str, Any]) -> None:
        self.payload = dict(payload)
        self.category = str(self.payload.get("category", "internal"))
        self.phase = str(self.payload.get("phase", "cleanup"))
        self.remote_effect = str(self.payload.get("remote_effect", "none"))
        retry = self.payload.get("retry")
        self.retry = dict(retry) if isinstance(retry, Mapping) else {"kind": "never"}
        self.code = str(self.payload.get("code", "RUNTIME_ERROR"))
        details = self.payload.get("details")
        self.details = dict(details) if isinstance(details, Mapping) else {}
        super().__init__(str(self.payload.get("message", "REST engine failed")))


class CancellationToken:
    """Thread-safe cooperative cancellation handle."""

    def __init__(self) -> None:
        self._native = NativeCancellationToken()

    def cancel(self) -> None:
        self._native.cancel()

    @property
    def is_cancelled(self) -> bool:
        return bool(self._native.is_cancelled())


class Engine:
    """Persistent black-box REST engine backed by Rust."""

    def __init__(self, config: Optional[Mapping[str, Any]] = None) -> None:
        config_json = None if config is None else _dump_object(config, "config")
        try:
            self._native = NativeEngine(config_json)
        except NativePlenoraError as error:
            raise _public_error(error) from None

    def capabilities(self) -> CapabilityDocument:
        try:
            raw_document = self._native.capabilities()
        except NativePlenoraError as error:
            raise _public_error(error) from None
        document = _load_object(raw_document, "capability document")
        return cast(CapabilityDocument, document)

    def close(self) -> None:
        self._native.close()

    @property
    def is_closed(self) -> bool:
        return bool(self._native.is_closed())

    def __enter__(self) -> "Engine":
        if self.is_closed:
            raise PlenoraError(_closed_payload())
        return self

    def __exit__(
        self,
        exception_type: object,
        exception: object,
        traceback: object,
    ) -> None:
        self.close()

    def _execute(
        self,
        request: Mapping[str, Any],
        cancellation: Optional[CancellationToken],
    ) -> ExecutionResult:
        native_cancellation = None if cancellation is None else cancellation._native
        try:
            raw_result = self._native.execute(
                _dump_object(request, "request"),
                native_cancellation,
            )
        except NativePlenoraError as error:
            raise _public_error(error) from None
        return cast(ExecutionResult, _load_object(raw_result, "execution result"))

    def test(
        self,
        connection: Mapping[str, Any],
        *,
        params: Optional[Mapping[str, Any]] = None,
        execution_options: Optional[Mapping[str, Any]] = None,
        deadline: Optional[str] = None,
        cancellation: Optional[CancellationToken] = None,
    ) -> ExecutionResult:
        return self._execute(
            _request(
                "test",
                connection,
                params=params,
                execution_options=execution_options,
                deadline=deadline,
            ),
            cancellation,
        )

    def generate(
        self,
        connection: Mapping[str, Any],
        *,
        params: Optional[Mapping[str, Any]] = None,
        execution_options: Optional[Mapping[str, Any]] = None,
        deadline: Optional[str] = None,
        cancellation: Optional[CancellationToken] = None,
    ) -> ExecutionResult:
        return self._execute(
            _request(
                "generate",
                connection,
                params=params,
                execution_options=execution_options,
                deadline=deadline,
            ),
            cancellation,
        )

    def enrich(
        self,
        connection: Mapping[str, Any],
        records: Sequence[Mapping[str, Any]],
        *,
        params: Optional[Mapping[str, Any]] = None,
        continue_on_error: bool = True,
        concurrency: int = 1,
        execution_options: Optional[Mapping[str, Any]] = None,
        deadline: Optional[str] = None,
        cancellation: Optional[CancellationToken] = None,
    ) -> ExecutionResult:
        request = _request(
            "enrich",
            connection,
            params=params,
            execution_options=execution_options,
            deadline=deadline,
        )
        request["input"]["records"] = [dict(record) for record in records]
        request["options"].update(
            {
                "continue_on_error": continue_on_error,
                "enrichment_concurrency": concurrency,
            }
        )
        return self._execute(request, cancellation)

    def download(
        self,
        connection: Mapping[str, Any],
        destination: Union[str, os.PathLike[str]],
        *,
        params: Optional[Mapping[str, Any]] = None,
        overwrite: bool = False,
        resume: bool = False,
        max_bytes: Optional[int] = None,
        expected_sha256: Optional[str] = None,
        execution_options: Optional[Mapping[str, Any]] = None,
        deadline: Optional[str] = None,
        cancellation: Optional[CancellationToken] = None,
    ) -> ExecutionResult:
        request = _request(
            "download",
            connection,
            params=params,
            execution_options=execution_options,
            deadline=deadline,
        )
        request["input"]["file"] = _file_input(
            destination,
            overwrite=overwrite,
            resume=resume,
            max_bytes=max_bytes,
            expected_sha256=expected_sha256,
        )
        return self._execute(request, cancellation)

    def upload(
        self,
        connection: Mapping[str, Any],
        source: Union[str, os.PathLike[str]],
        *,
        params: Optional[Mapping[str, Any]] = None,
        content_type: Optional[str] = None,
        filename: Optional[str] = None,
        field_name: Optional[str] = None,
        max_bytes: Optional[int] = None,
        expected_sha256: Optional[str] = None,
        execution_options: Optional[Mapping[str, Any]] = None,
        deadline: Optional[str] = None,
        cancellation: Optional[CancellationToken] = None,
    ) -> ExecutionResult:
        prepared_connection = dict(connection)
        request_config = dict(prepared_connection.get("request") or {})
        configured_body_type = request_config.get("body_type")
        if field_name is not None:
            request_config["body_type"] = "multipart"
        elif configured_body_type is None:
            request_config["body_type"] = "raw"
        prepared_connection["request"] = request_config

        request = _request(
            "upload",
            prepared_connection,
            params=params,
            execution_options=execution_options,
            deadline=deadline,
        )
        file_input = _file_input(
            source,
            overwrite=False,
            resume=False,
            max_bytes=max_bytes,
            expected_sha256=expected_sha256,
        )
        if content_type is not None:
            file_input["content_type"] = content_type
        if filename is not None:
            file_input["filename"] = filename
        if field_name is not None:
            file_input["field_name"] = field_name
        request["input"]["file"] = file_input
        return self._execute(request, cancellation)


def version() -> str:
    try:
        return distribution_version("plenora-rest")
    except PackageNotFoundError:
        return str(__version__)


def _request(
    operation: str,
    connection: Mapping[str, Any],
    *,
    params: Optional[Mapping[str, Any]],
    execution_options: Optional[Mapping[str, Any]],
    deadline: Optional[str],
) -> dict[str, Any]:
    options = dict(execution_options or {})
    if deadline is not None:
        options["deadline"] = deadline
    return {
        "schema_version": SCHEMA_VERSION,
        "operation": operation,
        "connection": dict(connection),
        "input": {"params": dict(params or {}), "records": []},
        "options": options,
    }


def _dump_object(value: Mapping[str, Any], name: str) -> str:
    if not isinstance(value, Mapping):
        raise TypeError(f"{name} must be a mapping")
    return json.dumps(dict(value), ensure_ascii=False, separators=(",", ":"))


def _load_object(value: str, name: str) -> dict[str, Any]:
    try:
        document = json.loads(value)
    except (TypeError, ValueError):
        raise PlenoraError(_invalid_result_payload(name)) from None
    if not isinstance(document, dict):
        raise PlenoraError(_invalid_result_payload(name))
    return document


def _file_input(
    path: Union[str, os.PathLike[str]],
    *,
    overwrite: bool,
    resume: bool,
    max_bytes: Optional[int],
    expected_sha256: Optional[str],
) -> dict[str, Any]:
    value: dict[str, Any] = {
        "path": os.fspath(path),
        "overwrite": overwrite,
        "resume": resume,
    }
    if max_bytes is not None:
        value["max_bytes"] = max_bytes
    if expected_sha256 is not None:
        value["expected_sha256"] = expected_sha256
    return value


def _public_error(error: NativePlenoraError) -> PlenoraError:
    try:
        payload = json.loads(str(error))
    except (TypeError, ValueError):
        payload = _internal_payload()
    if not isinstance(payload, dict):
        payload = _internal_payload()
    return PlenoraError(payload)


def _internal_payload() -> dict[str, Any]:
    return {
        "category": "internal",
        "phase": "cleanup",
        "remote_effect": "none",
        "retry": {"kind": "never"},
        "code": "RUNTIME_ERROR",
        "message": "REST engine failed internally",
        "details": {},
    }


def _closed_payload() -> dict[str, Any]:
    return {
        "category": "execution",
        "phase": "validate",
        "remote_effect": "none",
        "retry": {"kind": "never"},
        "code": "ENGINE_CLOSED",
        "message": "REST engine is closed",
        "details": {},
    }


def _invalid_result_payload(name: str) -> dict[str, Any]:
    payload = _internal_payload()
    payload["code"] = "INVALID_RESULT"
    payload["message"] = f"Native {name} is invalid"
    return payload
