"""T7 facade tests with a controlled worker; these do not qualify the native SDK."""
import asyncio
import importlib.util
from pathlib import Path
import sys
import threading
import types
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]


def facade():
    """Load the real facade with an explicit worker port, without a fake wheel."""
    package = types.ModuleType("frozen_worker_port")
    package.__path__ = []
    port = types.ModuleType("frozen_worker_port._engine")

    class Token:
        def __init__(self):
            self.cancelled = threading.Event()

        def cancel(self):
            self.cancelled.set()

    class Worker:
        def __init__(self, config=None):
            self.closed = False

        def test(self, connection, *, cancellation=None):
            return connection

        def close(self):
            self.closed = True

        def __enter__(self):
            return self

    port.Engine, port.CancellationToken = Worker, Token
    name = "frozen_worker_port._async_engine"
    spec = importlib.util.spec_from_file_location(name, ROOT / "python/plenora_rest/_async_engine.py")
    module = importlib.util.module_from_spec(spec)
    with patch.dict(sys.modules, {package.__name__: package, port.__name__: port, name: module}):
        spec.loader.exec_module(module)
    return module, Worker


class FrozenWorkerTests(unittest.IsolatedAsyncioTestCase):
    async def test_repeated_cancel_drains_late_error_and_concurrent_close(self):
        module, worker = facade()
        entered, release, stopped = threading.Event(), threading.Event(), threading.Event()
        tokens, diagnostics = [], []
        loop = asyncio.get_running_loop()
        original_handler = loop.get_exception_handler()
        loop.set_exception_handler(lambda loop, context: diagnostics.append(context))

        def operation(self, connection, *, cancellation=None):
            tokens.append(cancellation)
            entered.set()
            try:
                if not release.wait(5):
                    raise AssertionError("worker release timed out")
                raise ValueError("late failure sentinel")
            finally:
                stopped.set()

        client = module.AsyncEngine()
        task = None
        try:
            with patch.object(worker, "test", operation):
                task = asyncio.create_task(client.test({}))
                self.assertTrue(await asyncio.to_thread(entered.wait, 5))
                for _ in range(3):
                    task.cancel()
                    await asyncio.sleep(0)
                self.assertFalse(task.done(), "cancelled caller must drain its worker")
                self.assertTrue(tokens[0].cancelled.is_set())
                await asyncio.gather(client.close(), client.close())
                self.assertTrue(client.is_closed)
                self.assertFalse(task.done())
                release.set()
                with self.assertRaises(asyncio.CancelledError):
                    await asyncio.wait_for(task, 5)
                self.assertTrue(stopped.is_set())
                await asyncio.sleep(0)
                self.assertEqual(diagnostics, [])
        finally:
            release.set()
            if task is not None:
                await asyncio.gather(task, return_exceptions=True)
            loop.set_exception_handler(original_handler)

    async def test_failed_construction_can_be_retried_without_publishing_instance(self):
        module, worker = facade()
        original = worker.__init__
        attempts = []

        def construct(self, config=None):
            attempts.append(threading.get_ident())
            if len(attempts) == 1:
                raise ValueError("construction failed")
            original(self, config)

        client = module.AsyncEngine()
        with patch.object(worker, "__init__", construct):
            with self.assertRaisesRegex(ValueError, "construction failed"):
                await client.test({})
            self.assertIsNone(client._engine)
            self.assertEqual(await client.test({"ok": True}), {"ok": True})
            await client.close()
        self.assertEqual(len(attempts), 2)
        self.assertTrue(all(thread != threading.get_ident() for thread in attempts))
        self.assertTrue(client.is_closed)
