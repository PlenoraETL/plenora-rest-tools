"""SDK boundary errors consume the native catalog without sharing mutable state."""
import unittest

from plenora_rest import Engine, PlenoraError
from plenora_rest._engine import _closed_payload, _internal_payload, _public_error
from plenora_rest._native import NativePlenoraError


class ErrorProjectionTests(unittest.TestCase):
    def test_malformed_native_error_has_the_frozen_internal_tuple(self):
        expected = {
            "category": "internal", "phase": "cleanup", "remote_effect": "none",
            "retry": {"kind": "never"}, "code": "RUNTIME_ERROR",
            "message": "REST engine failed internally", "details": {},
        }
        for native in ("not JSON", "[]", "null"):
            self.assertEqual(_public_error(NativePlenoraError(native)).payload, expected)
        duplicate = _public_error(NativePlenoraError('{"code":1,"code":2}')).payload
        self.assertEqual(duplicate, {**expected, "message": "control document contains duplicate JSON members"})

    def test_sdk_and_native_closed_errors_use_the_same_tuple(self):
        engine = Engine()
        engine.close()
        with self.assertRaises(PlenoraError) as raised:
            engine.test({"url": "https://example.com"})
        self.assertEqual(raised.exception.payload, _closed_payload())
        self.assertEqual(_closed_payload(), {
            "category": "execution", "phase": "validate", "remote_effect": "none",
            "retry": {"kind": "never"}, "code": "ENGINE_CLOSED",
            "message": "REST engine is closed", "details": {},
        })
        with self.assertRaises(NativePlenoraError) as native:
            engine._native.open_cookie_session()
        self.assertEqual(_public_error(native.exception).payload, _closed_payload())

    def test_each_payload_owns_its_nested_values(self):
        for factory in (_internal_payload, _closed_payload):
            first = factory()
            first["retry"]["kind"] = "safe"
            first["details"]["private"] = "canary"
            self.assertEqual(factory()["retry"], {"kind": "never"})
            self.assertEqual(factory()["details"], {})
