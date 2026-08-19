import hashlib
import pathlib
import tempfile
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from rest_engine import Engine, RestEngineError


class JsonHandler(BaseHTTPRequestHandler):
    uploaded = b""

    def do_GET(self) -> None:
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

    def do_POST(self) -> None:
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
            engine = Engine({"allow_private_networks": True})
            result = engine.test(
                {
                    "url": f"http://127.0.0.1:{server.server_port}/profile",
                    "method": "GET",
                    "auth": {"type": "none"},
                }
            )
        finally:
            server.shutdown()
            server.server_close()
            thread.join()

        self.assertEqual(result["status"], "success")
        self.assertEqual(result["output"]["value"]["profile"]["name"], "Ada")
        self.assertEqual(result["responses"], [])

    def test_invalid_contract_raises_the_public_error(self) -> None:
        engine = Engine()
        with self.assertRaises(RestEngineError) as context:
            engine.test(
                {
                    "url": "https://api.example.test/resource",
                    "method": "BAD METHOD",
                }
            )
        self.assertEqual(context.exception.code, "invalid_input")

    def test_streaming_download_and_upload_convenience_methods(self) -> None:
        server = ThreadingHTTPServer(("127.0.0.1", 0), JsonHandler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
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
                    downloaded["output"]["sha256"],
                    hashlib.sha256(destination.read_bytes()).hexdigest(),
                )
                self.assertEqual(async_downloaded["status"], "success")
                self.assertEqual(async_downloaded["metrics"]["requests"], 3)
                self.assertEqual(async_downloaded["metrics"]["poll_requests"], 2)
                self.assertEqual(
                    async_downloaded["output"]["sha256"],
                    hashlib.sha256(async_destination.read_bytes()).hexdigest(),
                )
                self.assertEqual(uploaded["status"], "success")
                self.assertEqual(uploaded["output"]["response"]["uploaded"], True)
                self.assertEqual(JsonHandler.uploaded, source_bytes)
        finally:
            server.shutdown()
            server.server_close()
            thread.join()


if __name__ == "__main__":
    unittest.main()
