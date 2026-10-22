"""Local filesystem regression tests; never invoke sparkdev or submit jobs."""
import importlib.util
import os
from pathlib import Path
import tempfile
import unittest


spec = importlib.util.spec_from_file_location("verify_returns", Path(__file__).with_name("verify_returns.py"))
verifier = importlib.util.module_from_spec(spec)
spec.loader.exec_module(verifier)


class ReturnLocationTests(unittest.TestCase):
    def setUp(self):
        # Keep Windows fixtures outside redirected AppData so loopback UNC and
        # drive-letter spelling address the same physical, test-owned files.
        self.temporary = tempfile.TemporaryDirectory(prefix="return-location-", dir=Path(__file__).parent)
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name).absolute()
        self.original = self.root / "original.txt"
        self.original.write_bytes(b"same content is not the same file")

    def test_distinct_same_content_siblings_remain_distinct(self):
        conflict = self.root / "original.spark-job.txt"
        conflict.write_bytes(self.original.read_bytes())
        self.assertTrue(verifier.same_location(conflict.parent, self.original.parent, directory=True))
        self.assertFalse(verifier.same_location(conflict, self.original))
        elsewhere = self.root / "elsewhere"
        elsewhere.mkdir()
        self.assertFalse(verifier.same_location(elsewhere, self.root, directory=True))

    def test_missing_and_wrong_type_do_not_match(self):
        with self.assertRaises(OSError):
            verifier.same_location(self.original, self.root / "missing")
        with self.assertRaises(RuntimeError):
            verifier.same_location(self.root, self.root)
        with self.assertRaises(RuntimeError):
            verifier.same_location(self.original, self.original, directory=True)

    def test_symlink_alias_is_rejected_even_when_it_targets_original(self):
        alias = self.root / "alias.txt"
        try:
            alias.symlink_to(self.original)
        except OSError as error:
            self.skipTest("Symlink creation unavailable: " + type(error).__name__)
        with self.assertRaises(RuntimeError):
            verifier.same_location(alias, self.original)
        with self.assertRaises(RuntimeError):
            verifier.file_hash(alias)

    @unittest.skipUnless(os.name == "nt", "Windows path spelling regression")
    def test_drive_letter_and_verbatim_paths_identify_same_file(self):
        verbatim = Path("\\\\?\\" + str(self.original))
        self.assertTrue(verifier.same_location(self.original, verbatim))
        self.assertTrue(verifier.same_location(self.root, verbatim.parent, directory=True))

    @unittest.skipUnless(os.name == "nt", "Windows loopback UNC regression")
    def test_loopback_unc_and_physical_paths_identify_same_file(self):
        drive = self.original.drive
        if len(drive) != 2 or drive[1] != ":":
            self.skipTest("Fixture is not on a drive-letter volume")
        unc = Path("\\\\localhost\\" + drive[0] + "$" + str(self.original)[2:])
        try:
            unc.stat()
        except OSError as error:
            self.skipTest("Loopback administrative share unavailable: " + type(error).__name__)
        self.assertTrue(verifier.same_location(self.original, unc))
        self.assertTrue(verifier.same_location(self.root, unc.parent, directory=True))


if __name__ == "__main__":
    unittest.main()
