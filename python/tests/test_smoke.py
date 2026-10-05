import hashlib
import importlib.metadata
import pathlib
import socket
import tempfile
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any, List, Optional, Tuple

from plenora_rest import CancellationToken, Engine, PlenoraError, version


class JsonHandler(BaseHTTPRequestHandler):
    uploaded = b""
    resumable_body = b"resumable-download" * 4096
    resume_requests: List[Tuple[Optional[str], Optional[str]]] = []
    idempotency_headers: List[Optional[str]] = []
    post_requests = 0
    # Handshake used by the concurrency test: `arrivals` counts requests that
    # reached the server, `hold` releases their responses.
    arrivals = threading.Semaphore(0)
    hold = threading.Event()

    def do_GET(self) -> None:
        type(self).idempotency_headers.append(self.headers.get("Idempotency-Key"))
        if self.path == "/resumable":
            self._resumable_download()
            return
        # `startswith`, because enrich appends the record fields as query
        # parameters: the path arrives as `/hold?row=0`, not as a bare `/hold`.
        if self.path.startswith("/hold"):
            # Announces that the request has reached the server and then blocks,
            # so a test can be certain the native call is genuinely in flight
            # before it does anything else.
            type(self).arrivals.release()
            type(self).hold.wait(timeout=60)
        if self.path == "/artifact" or self.path.startswith("/artifacts/"):
            body = b"streamed-download" * 4096
            content_type = "application/octet-stream"
        elif self.path.startswith("/jobs/"):
            body = b'{"status":"completed"}'
            content_type = "application/json"
        else:
            body = b'{"profile":{"name":"Ada"}}'
            content_type = "application/json"
        self.send_response(200)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _resumable_download(self) -> None:
        range_header = self.headers.get("Range")
        if_range = self.headers.get("If-Range")
        type(self).resume_requests.append((range_header, if_range))
        body = type(self).resumable_body
        if range_header is None:
            split = 4096
            self.send_response(200)
            self.send_header("ETag", "\"python-v1\"")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body[:split])
            self.wfile.flush()
            self.connection.shutdown(socket.SHUT_WR)
            self.close_connection = True
            return

        offset = int(range_header.removeprefix("bytes=").removesuffix("-"))
        remaining = body[offset:]
        self.send_response(206)
        self.send_header("ETag", "\"python-v1\"")
        self.send_header(
            "Content-Range",
            f"bytes {offset}-{len(body) - 1}/{len(body)}",
        )
        self.send_header("Content-Length", str(len(remaining)))
        self.end_headers()
        self.wfile.write(remaining)

    def do_POST(self) -> None:
        type(self).post_requests += 1
        self.send_response(202)
        self.send_header("X-Job-Id", "export/1")
        self.send_header("Content-Length", "0")
        self.end_headers()

    def do_PUT(self) -> None:
        length = int(self.headers["Content-Length"])
        type(self).uploaded = self.rfile.read(length)
        body = b'{"uploaded":true}'
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, format: str, *args: object) -> None:
        pass


class PythonSdkSmokeTest(unittest.TestCase):
    def test_native_engine_through_public_sdk(self) -> None:
        server = ThreadingHTTPServer(("127.0.0.1", 0), JsonHandler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            JsonHandler.idempotency_headers = []
            engine = Engine({"allow_private_networks": True})
            result = engine.test(
                {
                    "url": f"http://127.0.0.1:{server.server_port}/profile",
                    "method": "GET",
                    "auth": {"type": "none"},
                },
                idempotency_key="python-test-42",
            )
            enriched = engine.enrich(
                {
                    "url": f"http://127.0.0.1:{server.server_port}/profile",
                    "method": "GET",
                    "auth": {"type": "none"},
                },
                [{"id": 1}, {"id": 2}],
                concurrency=2,
            )
        finally:
            server.shutdown()
            server.server_close()
            thread.join()

        self.assertEqual(result["status"], "success")
        self.assertEqual(result["output"]["value"]["profile"]["name"], "Ada")
        self.assertEqual(JsonHandler.idempotency_headers[0], "python-test-42")
        self.assertEqual(result["responses"], [])
        self.assertEqual(
            [record["id"] for record in enriched["output"]["records"]],
            [1, 2],
        )
        self.assertEqual(enriched["metrics"]["requests"], 2)

    def test_invalid_contract_raises_the_public_error(self) -> None:
        engine = Engine()
        with self.assertRaises(PlenoraError) as context:
            engine.test(
                {
                    "url": "https://api.example.test/resource",
                    "method": "BAD METHOD",
                }
            )
        self.assertEqual(context.exception.code, "INVALID_INPUT")
        self.assertEqual(context.exception.category, "invalid_configuration")
        self.assertEqual(context.exception.remote_effect, "none")

    def test_capabilities_version_lifecycle_and_cancellation(self) -> None:
        engine = Engine()
        capabilities = engine.capabilities()
        self.assertEqual(capabilities["schema_version"], 2)
        self.assertEqual(capabilities["component"], "plenora-rest-tools")
        self.assertEqual(
            [operation["id"] for operation in capabilities["operations"]],
            [
                "rest.test",
                "rest.generate",
                "rest.enrich",
                "rest.download",
                "rest.upload",
            ],
        )
        self.assertTrue(
            all(
                operation["controls"]["idempotency_key"]
                for operation in capabilities["operations"]
            )
        )
        self.assertEqual(version(), importlib.metadata.version("plenora-rest"))

        token = CancellationToken()
        token.cancel()
        result = engine.test(
            {"url": "https://api.example.test/resource", "method": "GET"},
            cancellation=token,
        )
        self.assertEqual(result["errors"][0]["category"], "cancelled")
        self.assertEqual(result["errors"][0]["remote_effect"], "unknown")
        self.assertEqual(result["errors"][0]["retry"]["kind"], "quarantine")

        engine.close()
        engine.close()
        with self.assertRaises(PlenoraError) as context:
            engine.test(
                {"url": "https://api.example.test/resource", "method": "GET"}
            )
        self.assertEqual(context.exception.code, "ENGINE_CLOSED")

    def test_streaming_download_and_upload_convenience_methods(self) -> None:
        server = ThreadingHTTPServer(("127.0.0.1", 0), JsonHandler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            JsonHandler.post_requests = 0
            with tempfile.TemporaryDirectory() as directory:
                root = pathlib.Path(directory)
                source = root / "source.bin"
                source_bytes = b"streamed-upload" * 4096
                source.write_bytes(source_bytes)
                destination = root / "download.bin"
                engine = Engine(
                    {
                        "allow_private_networks": True,
                        "allow_file_transfers": True,
                        "file_root": directory,
                        "max_request_bytes": 64,
                        "max_response_bytes": 64,
                        "max_file_transfer_bytes": 1024 * 1024,
                    }
                )
                downloaded = engine.download(
                    {
                        "url": (
                            f"http://127.0.0.1:{server.server_port}/artifact"
                        ),
                        "method": "GET",
                    },
                    destination,
                )
                async_destination = root / "async-download.bin"
                async_downloaded = engine.download(
                    {
                        "url": (
                            f"http://127.0.0.1:{server.server_port}/exports"
                        ),
                        "method": "POST",
                        "polling": {
                            "url_template": "{base}/jobs/{job_id}",
                            "id_header": "X-Job-Id",
                            "location_header": None,
                            "status_path": "status",
                            "result_url_template": (
                                "{base}/artifacts/{job_id}"
                            ),
                            "interval_ms": 0,
                            "max_attempts": 2,
                        },
                    },
                    async_destination,
                )
                resumed_async_destination = root / "resumed-async-download.bin"
                resumed_async_download = engine.download(
                    {
                        "url": (
                            f"http://127.0.0.1:{server.server_port}/exports"
                        ),
                        "method": "POST",
                        "polling": {
                            "url_template": "{base}/jobs/{job_id}",
                            "status_path": "status",
                            "result_url_template": (
                                "{base}/artifacts/{job_id}"
                            ),
                            "interval_ms": 0,
                            "max_attempts": 1,
                            "resume": {"job_id": "existing-2"},
                        },
                    },
                    resumed_async_destination,
                )
                JsonHandler.resume_requests = []
                resumed_destination = root / "resumed-download.bin"
                resumed = engine.download(
                    {
                        "url": (
                            f"http://127.0.0.1:{server.server_port}/resumable"
                        ),
                        "method": "GET",
                        "retry": {
                            "max_attempts": 2,
                            "backoff_base_ms": 0,
                        },
                    },
                    resumed_destination,
                    resume=True,
                )
                uploaded = engine.upload(
                    {
                        "url": f"http://127.0.0.1:{server.server_port}/upload",
                        "method": "PUT",
                    },
                    source,
                    content_type="application/octet-stream",
                )

                self.assertEqual(downloaded["status"], "success")
                self.assertEqual(
                    downloaded["output"]["checksum"]["value"],
                    hashlib.sha256(destination.read_bytes()).hexdigest(),
                )
                self.assertEqual(async_downloaded["status"], "success")
                self.assertEqual(async_downloaded["metrics"]["requests"], 3)
                self.assertEqual(async_downloaded["metrics"]["poll_requests"], 2)
                self.assertEqual(resumed_async_download["status"], "success")
                self.assertEqual(resumed_async_download["metrics"]["requests"], 2)
                self.assertEqual(
                    resumed_async_download["metrics"]["poll_requests"], 2
                )
                self.assertEqual(JsonHandler.post_requests, 1)
                self.assertEqual(
                    async_downloaded["output"]["checksum"]["value"],
                    hashlib.sha256(async_destination.read_bytes()).hexdigest(),
                )
                self.assertEqual(resumed["status"], "success")
                self.assertEqual(resumed["metrics"]["requests"], 2)
                self.assertEqual(
                    resumed_destination.read_bytes(),
                    JsonHandler.resumable_body,
                )
                self.assertEqual(
                    JsonHandler.resume_requests,
                    [(None, None), ("bytes=4096-", "\"python-v1\"")],
                )
                self.assertEqual(uploaded["status"], "success")
                self.assertEqual(uploaded["output"]["response"]["uploaded"], True)
                self.assertEqual(JsonHandler.uploaded, source_bytes)
                self.assertNotIn("path", downloaded["output"])
                self.assertNotIn(str(destination), str(downloaded))
        finally:
            server.shutdown()
            server.server_close()
            thread.join()

    def test_engine_is_usable_from_several_python_threads(self) -> None:
        """The binding releases the GIL around every native call, so the same
        Engine must tolerate concurrent use, concurrent cancellation, and a
        close() racing against in-flight work."""
        server = ThreadingHTTPServer(("127.0.0.1", 0), JsonHandler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            engine = Engine({"allow_private_networks": True})
            connection = {
                "url": f"http://127.0.0.1:{server.server_port}/",
                "method": "GET",
            }
            results: List[Any] = []
            failures: List[BaseException] = []
            lock = threading.Lock()

            def call(index: int) -> None:
                try:
                    if index % 4 == 3:
                        token = CancellationToken()
                        token.cancel()
                        outcome = engine.test(connection, cancellation=token)
                    else:
                        outcome = engine.enrich(
                            connection,
                            [{"row": index}],
                            concurrency=2,
                        )
                    with lock:
                        results.append((index, outcome))
                except BaseException as error:  # pragma: no cover - failure path
                    with lock:
                        failures.append(error)

            workers = [
                threading.Thread(target=call, args=(index,)) for index in range(16)
            ]
            for worker in workers:
                worker.start()
            for worker in workers:
                worker.join(timeout=60)
            for worker in workers:
                self.assertFalse(worker.is_alive(), "a native call did not return")

            self.assertEqual(failures, [])
            self.assertEqual(len(results), 16)
            for index, outcome in results:
                if index % 4 == 3:
                    self.assertEqual(outcome["status"], "failed", index)
                    self.assertEqual(outcome["errors"][0]["code"], "CANCELLED", index)
                else:
                    self.assertEqual(outcome["status"], "success", index)
                    self.assertEqual(
                        outcome["output"]["records"][0]["row"], index, index
                    )

            # close() must stay well defined while calls are genuinely in
            # flight: the ones already running finish or fail cleanly, and every
            # later call fails closed instead of hanging.
            racing: List[Any] = []
            racing_failures: List[BaseException] = []
            # The server itself confirms the race: every worker blocks inside the
            # handler, so close() only runs once all eight requests are provably
            # in flight. A barrier in the client would not prove that, because
            # the closing thread could win the scheduler right after it.
            JsonHandler.arrivals = threading.Semaphore(0)
            JsonHandler.hold = threading.Event()
            held = {
                "url": f"http://127.0.0.1:{server.server_port}/hold",
                "method": "GET",
            }

            def race(index: int) -> None:
                try:
                    outcome = engine.enrich(held, [{"row": index}])
                    with lock:
                        racing.append(outcome)
                except PlenoraError as error:
                    with lock:
                        racing_failures.append(error)

            in_flight = [
                threading.Thread(target=race, args=(index,)) for index in range(8)
            ]
            for worker in in_flight:
                worker.start()
            try:
                for _ in in_flight:
                    self.assertTrue(
                        JsonHandler.arrivals.acquire(timeout=60),
                        "a request never reached the server",
                    )
                # close() only flips a flag, so calling it right here — with all
                # eight requests provably parked inside the server — is the race
                # itself. Handing it to another thread would make the overlap
                # depend on the scheduler, and the test could pass without ever
                # exercising it.
                engine.close()
            finally:
                # Released even if the wait above fails, so a failing assertion
                # reports the real problem instead of leaving handler threads
                # blocked until their own timeout expires.
                JsonHandler.hold.set()
            for worker in in_flight:
                worker.join(timeout=60)
                self.assertFalse(worker.is_alive(), "close() raced into a hang")

            # All eight were already past validation, so none may fail closed;
            # close() must not disturb work the engine had already accepted.
            self.assertEqual(racing_failures, [])
            self.assertEqual(len(racing), 8)
            for outcome in racing:
                self.assertEqual(outcome["status"], "success")

            self.assertTrue(engine.is_closed)
            with self.assertRaises(PlenoraError) as closed:
                engine.test(connection)
            self.assertEqual(closed.exception.code, "ENGINE_CLOSED")
        finally:
            server.shutdown()
            server.server_close()
            thread.join()


if __name__ == "__main__":
    unittest.main()
