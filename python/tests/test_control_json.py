import unittest

from plenora_rest._engine import (
    NativePlenoraError, PlenoraError, _load_object, _public_error,
)


class ControlJsonTests(unittest.TestCase):
    def test_discovery_and_result_reject_duplicates_recursively(self):
        for name in ("capability document", "execution result"):
            for raw in ('{"version":0,"version":1}',
                        '{"extra":[{"secret":0,"secr\\u0065t":1}]}'):
                with self.subTest(name=name, raw=raw):
                    with self.assertRaises(PlenoraError) as caught:
                        _load_object(raw, name)
                    self.assertEqual(caught.exception.code, "INVALID_RESULT")
                    self.assertEqual(caught.exception.payload["message"],
                                     "control document contains duplicate JSON members")
            self.assertEqual(_load_object('{"a":[{"x":0},{"x":1}]}', name),
                             {"a": [{"x": 0}, {"x": 1}]})

    def test_error_envelope_rejects_duplicates(self):
        for raw in ('{"code":"secret","code":"last"}',
                    '{"details":{"secret":0,"secret":1}}'):
            error = _public_error(NativePlenoraError(raw))
            self.assertEqual(error.code, "RUNTIME_ERROR")
            self.assertEqual(error.payload["message"],
                             "control document contains duplicate JSON members")
        self.assertEqual(_public_error(NativePlenoraError('{"code":"CLEAN"}')).code,
                         "CLEAN")
