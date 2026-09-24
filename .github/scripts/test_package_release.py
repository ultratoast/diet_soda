"""Build identity and completeness checks; no native binaries or network required."""
import hashlib
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import package_release as release


class PackagingTests(unittest.TestCase):
    def test_tags_must_match_but_snapshots_are_commit_specific(self):
        self.assertEqual(release.archive_name("v0.1.0", "aarch64-apple-darwin"),
                         "diet_soda-v0.1.0-aarch64-apple-darwin.tar.gz")
        self.assertEqual(release.archive_name("v0.1.0", "x86_64-unknown-linux-gnu"),
                         "diet_soda-v0.1.0-x86_64-unknown-linux-gnu.tar.gz")
        self.assertNotIn("x86_64-pc-windows-msvc", release.TARGETS)
        self.assertEqual(release.build_label("0.1.0", tag="v0.1.0"), "v0.1.0")
        self.assertEqual(release.build_label("0.2.0-rc.1", tag="v0.2.0-rc.1"), "v0.2.0-rc.1")
        with self.assertRaises(ValueError):
            release.build_label("0.1.0", tag="v0.2.0")
        self.assertEqual(release.build_label("0.1.0", snapshot="a" * 40), "v0.1.0-main-aaaaaaaaaaaa")
        for invalid in ("main", "abc", "../bad", "x" * 40):
            with self.assertRaises(ValueError):
                release.build_label("0.1.0", snapshot=invalid)

    def test_checksums_require_all_targets_for_each_build(self):
        for label in ("v0.1.0", "v0.1.0-main-aaaaaaaaaaaa"):
            with tempfile.TemporaryDirectory() as temporary, patch.object(release, "DIST", Path(temporary)):
                with self.assertRaises(FileNotFoundError):
                    release.checksums(label)
                self.assertFalse((release.DIST / "SHA256SUMS").exists())
                for target in release.TARGETS:
                    (release.DIST / release.archive_name(label, target)).write_bytes(target.encode())
                release.checksums(label)
                lines = (release.DIST / "SHA256SUMS").read_text().splitlines()
                self.assertEqual(len(lines), len(release.TARGETS))
                for line in lines:
                    digest, name = line.split("  ")
                    self.assertEqual(digest, hashlib.sha256((release.DIST / name).read_bytes()).hexdigest())


if __name__ == "__main__":
    unittest.main()
