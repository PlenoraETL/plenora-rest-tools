"""Awaitable facade over the synchronous engine, with cooperative cancellation."""
from __future__ import annotations

import asyncio
from contextvars import copy_context
from functools import wraps
import inspect
import threading
from typing import Any

from ._engine import CancellationToken, Engine


class AsyncEngine:
    """Persistent Engine whose construction and calls run in executor workers.

    Cancellation signals the operation token and waits for its worker, including
    repeated cancellation. Completed remote effects are not rolled back.
    Use ``async with`` or await ``close()`` to release the engine.
    """

    def __init__(self, config=None) -> None:
        self._config = config
        self._engine: Engine | None = None
        self._init_lock = threading.Lock()
        self._closed = False

    @property
    def is_closed(self) -> bool:
        return self._closed

    async def _invoke(self, name: str, *args: Any, **kwargs: Any) -> Any:
        method = getattr(Engine, name)
        bound = inspect.signature(method).bind(None, *args, **kwargs)
        token = None
        if "cancellation" in inspect.signature(method).parameters:
            token = bound.arguments.get("cancellation")
            if token is None:
                token = CancellationToken()
                bound.arguments["cancellation"] = token
        cancelled = threading.Event()

        def invoke():
            with self._init_lock:
                if self._engine is None:
                    self._engine = Engine(self._config)
            if cancelled.is_set() and name != "close":
                raise asyncio.CancelledError
            result = method(self._engine, *bound.args[1:], **bound.kwargs)
            if name == "close":
                self._closed = True
            return result

        def outcome():
            # Python 3.14 shield logs exceptions from a detached inner future.
            # Transport them as outcomes and raise only at the public await:
            # cancellation can then drain without unsolicited diagnostics.
            try:
                return True, invoke()
            except BaseException as error:
                return False, error

        worker = asyncio.get_running_loop().run_in_executor(None, copy_context().run, outcome)
        try:
            succeeded, value = await asyncio.shield(worker)
            if succeeded:
                return value
            raise value
        except asyncio.CancelledError:
            cancelled.set()
            if token is not None:
                token.cancel()
            while not worker.done():
                try:
                    await asyncio.shield(worker)
                except asyncio.CancelledError:
                    continue
                except Exception:
                    break
            if not worker.cancelled():
                worker.exception()
            raise

    async def __aenter__(self) -> AsyncEngine:
        try:
            await self._invoke("__enter__")
        except BaseException:
            # A failed enter has no automatic __aexit__; close any engine the
            # drained construction worker produced, even during cancellation.
            if self._engine is not None:
                await self._invoke("close")
            raise
        return self

    async def __aexit__(self, exception_type, exception, traceback) -> None:
        await self._invoke("close")


def _async_method(name):
    @wraps(getattr(Engine, name))
    async def call(self: AsyncEngine, *args: Any, **kwargs: Any) -> Any:
        return await self._invoke(name, *args, **kwargs)
    return call


# Derive the callable surface from Engine so new operations cannot be omitted.
for _name, _method in vars(Engine).items():
    if not _name.startswith("_") and callable(_method):
        setattr(AsyncEngine, _name, _async_method(_name))
