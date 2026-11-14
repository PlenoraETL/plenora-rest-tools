"""Parity against the synchronous SDK and deterministic worker lifecycle tests."""
import asyncio
from contextvars import ContextVar
import inspect
from pathlib import Path
import tempfile
import threading
import unittest
from unittest.mock import patch
from http.server import ThreadingHTTPServer

from plenora_rest import AsyncEngine, CancellationToken, Engine, PlenoraError
from test_smoke import JsonHandler


class AsyncParityTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.server = ThreadingHTTPServer(("127.0.0.1", 0), JsonHandler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.connection = {"url": f"http://127.0.0.1:{self.server.server_port}/profile", "method": "GET"}
        self.files = tempfile.TemporaryDirectory()
        self.addCleanup(self.files.cleanup)
        config = {"allow_private_networks": True, "allow_cookie_store": True,
                  "allow_file_transfers": True, "file_root": self.files.name}
        self.sync = Engine(config)
        self.client = AsyncEngine(config)

    async def asyncTearDown(self):
        await self.client.close()
        await asyncio.to_thread(self.sync.close)
        await asyncio.to_thread(self.server.shutdown)
        self.server.server_close()
        self.thread.join()

    async def compare(self, name, *args, **kwargs):
        expected = await asyncio.to_thread(getattr(self.sync, name), *args, **kwargs)
        actual = await getattr(self.client, name)(*args, **kwargs)
        # Durations measure wall time, so compare the stable result surfaces.
        for key in ("status", "output", "errors", "responses"):
            self.assertEqual(actual[key], expected[key], (name, key))
        self.assertEqual(
            {key: value for key, value in actual["metrics"].items() if key != "elapsed_ms"},
            {key: value for key, value in expected["metrics"].items() if key != "elapsed_ms"},
        )
        return actual

    async def test_every_operation_matches_sync(self):
        with tempfile.TemporaryDirectory(dir=self.files.name) as directory:
            path = Path(directory) / "payload"
            for name in ("test", "generate"):
                result = await self.compare(name, self.connection, idempotency_key="parity-" + name)
                self.assertEqual(result["status"], "success")
            result = await self.compare("enrich", self.connection, [{"id": 2}, {"id": 1}], concurrency=2)
            self.assertEqual(result["status"], "success")
            download = {**self.connection, "url": self.connection["url"].replace("profile", "artifact")}
            result = await self.compare("download", download, path, overwrite=True)
            self.assertEqual(result["status"], "success")
            self.assertEqual(path.read_bytes(), b"streamed-download" * 4096)
            upload = {**self.connection, "url": self.connection["url"].replace("profile", "upload"), "method": "PUT"}
            result = await self.compare("upload", upload, path, content_type="application/octet-stream")
            self.assertEqual(result["status"], "success")
            for name, extra in (("test", ()), ("generate", ()), ("enrich", ([],)),
                                ("download", (path,)), ("upload", (path,))):
                token = CancellationToken()
                token.cancel()
                await self.compare(name, self.connection, *extra, cancellation=token)

    async def test_discovery_cookie_sessions_and_lifecycle(self):
        self.assertEqual(await self.client.capabilities(), self.sync.capabilities())
        handle = await self.client.open_cookie_session()
        JsonHandler.cookie_headers = []
        connection = {**self.connection, "url": self.connection["url"].replace("profile", "session"),
                      "cookies": {"session": handle}}
        await self.client.test(connection)
        await self.client.test(connection)
        self.assertEqual(JsonHandler.cookie_headers, [None, "sid=python"])
        await self.client.close_cookie_session(handle)
        with self.assertRaises(PlenoraError):
            await self.client.close_cookie_session(handle)
        async with self.client as entered:
            self.assertIs(entered, self.client)
            self.assertFalse(self.client.is_closed)
        self.assertTrue(self.client.is_closed)
        with self.assertRaises(PlenoraError) as caught:
            async with self.client:
                self.fail("closed context was entered")
        self.assertEqual(caught.exception.code, "ENGINE_CLOSED")

    async def test_errors_match_for_every_operation(self):
        bad = {**self.connection, "method": "BAD METHOD"}
        for name, args in (("test", (bad,)), ("generate", (bad,)), ("enrich", (bad, [])),
                           ("download", (bad, "unused")), ("upload", (bad, "unused"))):
            with self.subTest(name=name):
                with self.assertRaises(PlenoraError) as sync_error:
                    getattr(self.sync, name)(*args)
                with self.assertRaises(PlenoraError) as async_error:
                    await getattr(self.client, name)(*args)
                self.assertEqual(async_error.exception.payload, sync_error.exception.payload)

    async def test_native_cancellation_and_close_during_inflight_io(self):
        JsonHandler.arrivals = threading.Semaphore(0)
        JsonHandler.hold.clear()
        token = CancellationToken()
        connection = {**self.connection, "url": self.connection["url"].replace("profile", "hold")}
        task = asyncio.create_task(self.client.test(connection, cancellation=token))
        try:
            self.assertTrue(await asyncio.to_thread(JsonHandler.arrivals.acquire, True, 5))
            heartbeat = asyncio.Event()
            asyncio.get_running_loop().call_soon(heartbeat.set)
            await asyncio.wait_for(heartbeat.wait(), 2)
            await self.client.close()
            self.assertTrue(self.client.is_closed)
            self.assertFalse(task.done())
            task.cancel()
            with self.assertRaises(asyncio.CancelledError):
                await asyncio.wait_for(task, 5)
            self.assertTrue(token.is_cancelled)
        finally:
            JsonHandler.hold.set()
            if not task.done():
                task.cancel()
                try:
                    await task
                except asyncio.CancelledError:
                    pass

    async def test_surface_and_signatures_derive_from_sync(self):
        for name, method in vars(Engine).items():
            if not name.startswith("_") and callable(method):
                asynchronous = getattr(AsyncEngine, name)
                self.assertTrue(inspect.iscoroutinefunction(asynchronous))
                self.assertEqual(inspect.signature(asynchronous), inspect.signature(method))


class WorkerLifecycleTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.loop_errors = []
        asyncio.get_running_loop().set_exception_handler(
            lambda loop, context: self.loop_errors.append(context))

    async def asyncTearDown(self):
        await asyncio.sleep(0)
        self.assertEqual(self.loop_errors, [])

    async def test_cancellation_reaps_worker_and_preserves_context(self):
        entered, release, stopped = threading.Event(), threading.Event(), threading.Event()
        context = ContextVar("probe", default="missing")
        context.set("present")
        observed = []
        def operation(self, connection, *, cancellation=None):
            observed.append((context.get(), threading.get_ident(), cancellation))
            entered.set()
            try:
                release.wait(5)
            finally:
                stopped.set()
        client = AsyncEngine()
        try:
            with patch.object(Engine, "test", operation):
                task = asyncio.create_task(client.test({}))
                self.assertTrue(await asyncio.to_thread(entered.wait, 5))
                task.cancel()
                await asyncio.sleep(0)
                task.cancel()
                await asyncio.sleep(0)
                self.assertFalse(task.done())
                self.assertTrue(observed[0][2].is_cancelled)
                release.set()
                with self.assertRaises(asyncio.CancelledError):
                    await task
                self.assertTrue(stopped.is_set())
                self.assertEqual(observed[0][0], "present")
                self.assertNotEqual(observed[0][1], threading.get_ident())
        finally:
            release.set()
            await client.close()

    async def test_lazy_construction_runs_off_loop_and_failure_propagates(self):
        seen = []
        def fail(*args):
            seen.append(threading.get_ident())
            raise ValueError("invalid configuration")
        with patch("plenora_rest._async_engine.Engine", side_effect=fail):
            client = AsyncEngine()
            self.assertEqual(seen, [])
        with patch.object(Engine, "__init__", fail):
            with self.assertRaisesRegex(ValueError, "invalid configuration"):
                await client.capabilities()
        self.assertNotEqual(seen[0], threading.get_ident())

    async def test_cancelled_context_entry_closes_constructed_engine(self):
        entered, release = threading.Event(), threading.Event()
        original = Engine.__init__
        instances = []
        def slow_init(engine, config=None):
            entered.set()
            release.wait(5)
            original(engine, config)
            instances.append(engine)
        client = AsyncEngine()
        with patch.object(Engine, "__init__", slow_init):
            task = asyncio.create_task(client.__aenter__())
            self.assertTrue(await asyncio.to_thread(entered.wait, 5))
            task.cancel()
            await asyncio.sleep(0)
            task.cancel()
            release.set()
            with self.assertRaises(asyncio.CancelledError):
                await task
        self.assertEqual(len(instances), 1)
        self.assertTrue(instances[0].is_closed)
        self.assertTrue(client.is_closed)

    async def test_worker_error_after_cancellation_is_reaped_without_logging(self):
        entered, release = threading.Event(), threading.Event()
        def fail(engine, connection, *, cancellation=None):
            entered.set()
            release.wait(5)
            raise ValueError("worker failed")
        client = AsyncEngine()
        try:
            with patch.object(Engine, "test", fail):
                task = asyncio.create_task(client.test({}))
                self.assertTrue(await asyncio.to_thread(entered.wait, 5))
                task.cancel()
                await asyncio.sleep(0)
                release.set()
                with self.assertRaises(asyncio.CancelledError):
                    await task
        finally:
            release.set()
            await client.close()
