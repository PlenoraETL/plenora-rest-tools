from __future__ import annotations

import json
from collections.abc import Mapping, Sequence
from typing import Any, Optional, cast

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
        concurrency: int = 1,
    ) -> ExecutionResult:
        request = _request("enrich", connection, params=params)
        request["input"]["records"] = [dict(record) for record in records]
        request["options"] = {
            "continue_on_error": continue_on_error,
            "enrichment_concurrency": concurrency,
        }
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


def _public_error(error: NativeRestEngineError) -> RestEngineError:
    try:
        payload = json.loads(str(error))
    except (TypeError, ValueError):
        payload = {"code": "runtime_error", "message": str(error)}
    if not isinstance(payload, dict):
        payload = {"code": "runtime_error", "message": str(error)}
    return RestEngineError(payload)
