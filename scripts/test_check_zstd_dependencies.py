"""Offline regression guard for the zstd advisory fixes in both lockfiles."""

from pathlib import Path
import tomllib
import unittest


ROOT = Path(__file__).resolve().parents[1]


class ZstdDependencyTests(unittest.TestCase):
    def test_both_graphs_exclude_vulnerable_zstd_releases(self):
        # RUSTSEC-2026-0391: zstd; 0395 through 0406: zstd-safe;
        # 0410: the bundled native library. Keep every resolved copy safe.
        minimums = {
            "zstd": (0, 14, 3),
            "zstd-safe": (8, 2, 0),
            "zstd-sys": (2, 1, 2),
        }
        for lockfile in ("Cargo.lock", "fuzz/Cargo.lock"):
            packages = tomllib.loads((ROOT / lockfile).read_text(encoding="utf-8"))["package"]
            for name, minimum in minimums.items():
                with self.subTest(lockfile=lockfile, package=name):
                    versions = [p["version"] for p in packages if p["name"] == name]
                    self.assertTrue(versions, "required compression dependency is missing")
                    for version in versions:
                        release = version.split("+", 1)[0]
                        self.assertNotIn("-", release, "prerelease dependency is not qualified")
                        self.assertGreaterEqual(
                            tuple(map(int, release.split("."))), minimum,
                            "vulnerable compression dependency is locked",
                        )


if __name__ == "__main__":
    unittest.main()
