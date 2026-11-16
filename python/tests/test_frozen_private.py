"""Shared admission corpus on both real SDK entry points; requires the native wheel."""
import json
import os
from pathlib import Path
import unittest
import uuid
import subprocess
import tempfile
import threading
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from plenora_rest import AsyncEngine, Engine, PlenoraError

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "scripts"))
from frozen_compare import assert_observation


def cases():
    path = Path(__file__).resolve().parents[2] / "tests/frozen/0.4.0/private-inputs.json"
    sentinel = "private_sdk_" + uuid.uuid4().hex
    corpus = json.loads(path.read_text(encoding="utf-8").replace("__CANARY__", sentinel))
    return sentinel, corpus["cases"]


class FrozenPrivateTests(unittest.IsolatedAsyncioTestCase):
    async def test_remote_bodies_headers_on_cli_sync_and_async(self):
        root = Path(__file__).resolve().parents[2]
        binary = Path(os.environ.get("PLENORA_TEST_CLI",
            str(root / "target/debug" / ("plenora-rest.exe" if os.name == "nt" else "plenora-rest")))).resolve()
        self.assertTrue(binary.is_file(), "CLI fixture is missing; provide PLENORA_TEST_CLI")
        corpus = json.loads((root / "tests/frozen/0.4.0/remote-private.json").read_text())
        seen = []
        class Handler(BaseHTTPRequestHandler):
            def version_string(self):
                return "frozen-http/1"
            def date_time_string(self, timestamp=None):
                return "Thu, 01 Jan 1970 00:00:00 GMT"
            def do_GET(self):
                index, status = map(int, self.path.strip("/").split("/"))
                seen.append((index, status, self.headers.get("Authorization")))
                body = corpus["bodies"][index]["body"].encode()
                self.send_response(status)
                self.send_header("Content-Length", str(len(body)))
                for header in corpus["headers"]:
                    self.send_header(header, "PRIVATE_HEADER_SENTINEL")
                self.end_headers()
                self.wfile.write(body)
            def log_message(self, *args):
                pass
        server = ThreadingHTTPServer(("127.0.0.1",0),Handler)
        thread = threading.Thread(target=server.serve_forever,daemon=True)
        thread.start()
        config = {"allow_private_networks":True}
        options = {"capture_response_metadata":True,"response_headers":["*"]}
        try:
            with tempfile.TemporaryDirectory(prefix="private-surfaces-") as directory:
                config_path = Path(directory) / "config.json"
                config_path.write_text(json.dumps(config))
                with Engine(config) as sync:
                    async with AsyncEngine(config) as asynchronous:
                        for index, case in enumerate(corpus["bodies"]):
                            for status in (200,400):
                                connection = {"url":f"http://127.0.0.1:{server.server_port}/{index}/{status}",
                                    "auth":{"type":"bearer","token":"PRIVATE_CREDENTIAL_SENTINEL"},
                                    "response":{"format":case["format"],"delimiter":","}}
                                for field in ("error_path","success_when"):
                                    if field in case:
                                        connection["response"][field] = case[field]
                                outcomes = [sync.test(connection,execution_options=options),
                                            await asynchronous.test(connection,execution_options=options)]
                                cli = subprocess.run([str(binary),"test","--input","-","--config",str(config_path),"--format","json"],
                                    input=json.dumps({"schema_version":1,"operation":"test","connection":connection,"options":options}).encode(),
                                    capture_output=True,timeout=10)
                                document = json.loads(cli.stdout)
                                self.assertEqual(cli.stderr,b"")
                                self.assertNotIn("PRIVATE_",json.dumps(outcomes+[document]))
                                success = case["id"] == "headers" and status == 200
                                self.assertEqual(cli.returncode == 0,success)
                                for result in outcomes:
                                    self.assertEqual(result["status"],"success" if success else "failed")
                                    if not success:
                                        self.assertEqual(result["errors"][0]["code"],"HTTP_STATUS" if status == 400 else case["code"])
                                    self.assertIsInstance(result["metrics"].pop("elapsed_ms"),int)
                                self.assertEqual(*outcomes)
                                self.assertEqual(seen[-3:],[(index,status,"Bearer PRIVATE_CREDENTIAL_SENTINEL")]*3)
                                if success:
                                    self.assertIsInstance(document["result"]["metrics"].pop("elapsed_ms"),int)
                                for result in outcomes + ([document["result"]] if success else []):
                                    for metadata in result["responses"]:
                                        self.assertEqual(metadata["final_url"],f"http://127.0.0.1:{server.server_port}")
                                        metadata["final_url"] = "<loopback-origin>"
                                identity = f"remote-surfaces-{case['id']}-{status}"
                                observed = {"sync":outcomes[0],"async":outcomes[1],"cli":document,"exit":cli.returncode}
                                if capture := os.environ.get("SEMPL_CAPTURE_DIR") or os.environ.get("SEMPL_CHECK_CAPTURE_DIR"):
                                    destination = Path(capture).resolve()
                                    self.assertTrue(destination.is_relative_to((root / "target").resolve()))
                                    with (destination / f"{identity}.json").open("x",encoding="utf-8") as stream:
                                        json.dump(observed,stream,indent=2)
                                if not os.environ.get("SEMPL_CAPTURE_DIR"):
                                    expected = json.loads((root / "tests/frozen/0.4.0/observed.json").read_text())
                                    assert_observation(identity, observed, expected["cases"][identity])
        finally:
            server.shutdown()
            server.server_close()
            thread.join()

    async def test_sync_and_async_admission_do_not_expose_private_values(self):
        sentinel, corpus = cases()
        with Engine() as sync:
            async with AsyncEngine() as asynchronous:
                for case in corpus:
                    with self.subTest(case=case["id"]):
                        outcomes = []
                        rendered_errors = []
                        for client in (sync, asynchronous):
                            try:
                                if client is sync:
                                    result = client.test(case["connection"])
                                else:
                                    result = await client.test(case["connection"])
                                self.assertEqual(result["status"], "failed")
                                self.assertEqual(result["metrics"]["requests"], 0)
                                self.assertIsInstance(result["metrics"].pop("elapsed_ms"), int)
                                outcome = {"result": result}
                            except PlenoraError as error:
                                outcome = {"exception": str(error), "payload": error.payload}
                                rendered_errors.append(repr(error))
                            outcomes.append(outcome)
                        for surface, outcome in zip(("sync", "async"), outcomes):
                            with self.subTest(surface=surface):
                                self.assertNotIn(sentinel, json.dumps(outcome))
                        for rendered in rendered_errors:
                            self.assertNotIn(sentinel, rendered)
                        self.assertEqual(*outcomes)
                        identity = "private-sdk-" + case["id"]
                        root = Path(__file__).resolve().parents[2]
                        if directory := os.environ.get("SEMPL_CAPTURE_DIR") or os.environ.get("SEMPL_CHECK_CAPTURE_DIR"):
                            destination = Path(directory).resolve()
                            self.assertTrue(destination.is_relative_to((root / "target").resolve()))
                            with (destination / f"{identity}.json").open("x", encoding="utf-8") as stream:
                                json.dump(outcomes[0], stream, indent=2)
                        if not os.environ.get("SEMPL_CAPTURE_DIR"):
                            expected = json.loads((root / "tests/frozen/0.4.0/observed.json").read_text(encoding="utf-8"))
                            self.assertEqual(outcomes[0], expected["cases"][identity])
