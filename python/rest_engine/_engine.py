from __future__ import annotations

import json
import os
from collections.abc import Mapping, Sequence
from typing import Any, Optional, Union, cast

from ._native import NativeEngine, NativeRestEngineError, SCHEMA_VERSION
from .types import ExecutionResult


class RestEngineError(RuntimeError):
    """A contract or runtime error raised before an execution result exists."""

    def __init__(self, payload: Mapping[str, Any]) -> None:
        self.payload = dict(payload)
        self.code = str(self.payload.get("code", "runtime_error"))
        self.retriable = bool(self.payload.get("retriable", False))
        super().__init__(str(self.payload.get("message", "REST engine failed")))


class Engine:
    """Persistent black-box REST engine backed by Rust."""

    def __init__(self, config: Optional[Mapping[str, Any]] = None) -> None:
        config_json = None if config is None else _dump_object(config, "config")
        try:
            self._native = NativeEngine(config_json)
        except NativeRestEngineError as error:
            raise _public_error(error) from None

    def execute(self, request: Mapping[str, Any]) -> ExecutionResult:
        """Execute one versioned request and return its structured result."""
        try:
            raw_result = self._native.execute(_dump_object(request, "request"))
        except NativeRestEngineError as error:
            raise _public_error(error) from None
        try:
            result = json.loads(raw_result)
        except (TypeError, ValueError) as error:
            raise RestEngineError(
                {"code": "invalid_result", "message": str(error)}
            ) from None
        if not isinstance(result, dict):
            raise RestEngineError(
                {"code": "invalid_result", "message": "engine returned a non-object"}
            )
        return cast(ExecutionResult, result)

    def test(
        self,
        connection: Mapping[str, Any],
        *,
        params: Optional[Mapping[str, Any]] = None,
    ) -> ExecutionResult:
        return self.execute(
            _request("test", connection, params=params)
        )

    def generate(
        self,
        connection: Mapping[str, Any],
        *,
        params: Optional[Mapping[str, Any]] = None,
    ) -> ExecutionResult:
        return self.execute(
            _request("generate", connection, params=params)
        )

    def enrich(
        self,
        connection: Mapping[str, Any],
        records: Sequence[Mapping[str, Any]],
        *,
        params: Optional[Mapping[str, Any]] = None,
        continue_on_error: bool = True,
    ) -> ExecutionResult:
        request = _request("enrich", connection, params=params)
        request["input"]["records"] = [dict(record) for record in records]
        request["options"] = {"continue_on_error": continue_on_error}
        return self.execute(request)

    def download(
        self,
        connection: Mapping[str, Any],
        destination: Union[str, os.PathLike[str]],
        *,
        params: Optional[Mapping[str, Any]] = None,
        overwrite: bool = False,
        max_bytes: Optional[int] = None,
        expected_sha256: Optional[str] = None,
    ) -> ExecutionResult:
        """Stream an HTTP response into a staged local file."""
        request = _request("download", connection, params=params)
        request["input"]["file"] = _file_input(
            destination,
            overwrite=overwrite,
            max_bytes=max_bytes,
            expected_sha256=expected_sha256,
        )
        return self.execute(request)

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
    ) -> ExecutionResult:
        """Stream a local file as a raw body or multipart part."""
        prepared_connection = dict(connection)
        request_config = dict(prepared_connection.get("request") or {})
        configured_body_type = request_config.get("body_type")
        if field_name is not None:
            request_config["body_type"] = "multipart"
        elif configured_body_type is None:
            request_config["body_type"] = "raw"
        prepared_connection["request"] = request_config

        request = _request("upload", prepared_connection, params=params)
        file_input = _file_input(
            source,
            overwrite=False,
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
        return self.execute(request)


def _request(
    operation: str,
    connection: Mapping[str, Any],
    *,
    params: Optional[Mapping[str, Any]],
) -> dict[str, Any]:
    return {
        "schema_version": SCHEMA_VERSION,
        "operation": operation,
        "connection": dict(connection),
        "input": {"params": dict(params or {}), "records": []},
    }


def _dump_object(value: Mapping[str, Any], name: str) -> str:
    if not isinstance(value, Mapping):
        raise TypeError(f"{name} must be a mapping")
    return json.dumps(dict(value), ensure_ascii=False, separators=(",", ":"))


def _file_input(
    path: Union[str, os.PathLike[str]],
    *,
    overwrite: bool,
    max_bytes: Optional[int],
    expected_sha256: Optional[str],
) -> dict[str, Any]:
    value: dict[str, Any] = {
        "path": os.fspath(path),
        "overwrite": overwrite,
    }
    if max_bytes is not None:
        value["max_bytes"] = max_bytes
    if expected_sha256 is not None:
        value["expected_sha256"] = expected_sha256
    return value


def _public_error(error: NativeRestEngineError) -> RestEngineError:
    try:
        payload = json.loads(str(error))
    except (TypeError, ValueError):
        payload = {"code": "runtime_error", "message": str(error)}
    if not isinstance(payload, dict):
        payload = {"code": "runtime_error", "message": str(error)}
    return RestEngineError(payload)
