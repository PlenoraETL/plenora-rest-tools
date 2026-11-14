"""Shared admission corpus on both real SDK entry points; requires the native wheel."""
import json
import os
from pathlib import Path
import unittest
import uuid

from plenora_rest import AsyncEngine, Engine, PlenoraError


def cases():
    path = Path(__file__).resolve().parents[2] / "tests/frozen/0.4.0/private-inputs.json"
    sentinel = "private_sdk_" + uuid.uuid4().hex
    corpus = json.loads(path.read_text(encoding="utf-8").replace("__CANARY__", sentinel))
    return sentinel, corpus["cases"]


class FrozenPrivateTests(unittest.IsolatedAsyncioTestCase):
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
