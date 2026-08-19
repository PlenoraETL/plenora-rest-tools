import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from rest_engine import Engine, RestEngineError


class JsonHandler(BaseHTTPRequestHandler):
    def do_GET(self) -> None:
        body = b'{"profile":{"name":"Ada"}}'
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
        self.assertEqual(result["responses"], [])
        self.assertEqual(
            [record["id"] for record in enriched["output"]["records"]],
            [1, 2],
        )
        self.assertEqual(enriched["metrics"]["requests"], 2)

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


if __name__ == "__main__":
    unittest.main()
