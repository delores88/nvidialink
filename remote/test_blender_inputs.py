"""Blender input locking tests: no Docker, GPU, compiler or real network calls."""
import copy
import bz2
import hashlib
import importlib.util
import io
import json
from pathlib import Path
import tarfile
import tempfile
import unittest
from unittest.mock import Mock, patch

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("locked_sources", ROOT / "runtimes/blender/locked_sources.py")
locked = importlib.util.module_from_spec(spec)
spec.loader.exec_module(locked)


class BlenderInputs(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.lock = json.loads((ROOT / "runtimes/blender/inputs.lock.json").read_text())

    def test_complete_lock_covers_sources_mirror_wheels_and_all_data(self):
        locked.validate(self.lock)
        self.assertEqual(len(self.lock["dependencies"]), 66)
        self.assertEqual(len({r["sha256"] for r in self.lock["lfs_assets"]}), 187)
        self.assertTrue(all(r["url"].endswith(r["sha256"]) for r in self.lock["dependencies"] + self.lock["lfs_assets"]))
        self.assertEqual({r["name"] for r in self.lock["sources"]}, {"blender", "optix"})
        for mutate in (lambda v: v["dependencies"][0].update(url="http://projects.blender.org/archive"),
                       lambda v: v["dependencies"][0].update(sha256="a" * 32),
                       lambda v: v["lfs_assets"][0].update(path="../outside"),
                       lambda v: v["python_wheels"].pop()):
            candidate = copy.deepcopy(self.lock)
            mutate(candidate)
            with self.assertRaises(ValueError):
                locked.validate(candidate)

    def test_fetch_checks_size_hash_and_preserves_prior_file_on_failure(self):
        data = b"verified source"
        row = {"url": "https://projects.blender.org/locked", "sha256": hashlib.sha256(data).hexdigest(), "bytes": len(data)}
        target = self.root / "source"
        opener = Mock()
        opener.open.return_value = io.BytesIO(data)
        with patch.object(locked.urllib.request, "build_opener", return_value=opener):
            locked.fetch(row, target)
        self.assertEqual(target.read_bytes(), data)
        with patch.object(locked.urllib.request, "build_opener") as network:
            locked.fetch(row, target)
        network.assert_not_called()
        for data2 in (b"wrong bytes", data + b"overflow"):
            target.unlink()
            opener.open.return_value = io.BytesIO(data2)
            with patch.object(locked.urllib.request, "build_opener", return_value=opener), self.assertRaises(ValueError):
                locked.fetch(row, target)
            self.assertFalse(target.exists())
            self.assertFalse(list(self.root.glob(".download-*")))
            target.write_bytes(data)
        target.write_bytes(b"corrupted cache")
        with patch.object(locked.urllib.request, "build_opener") as network, self.assertRaisesRegex(ValueError, "Cached"):
            locked.fetch(row, target)
        network.assert_not_called()
        self.assertEqual(target.read_bytes(), b"corrupted cache")

    def test_geometry_data_format_and_digest_are_both_required(self):
        data = b"VCO\x00\xff\xff\x00\x00" + bytes(18)
        row = {"path": "release/datafiles/icons/example.dat", "bytes": len(data),
               "sha256": hashlib.sha256(data).hexdigest()}
        locked.validate_data(row, data)
        for bad in (b"executable instead" + bytes(18), data + b"bad triangle"):
            candidate = {**row, "bytes": len(bad), "sha256": hashlib.sha256(bad).hexdigest()}
            with self.assertRaisesRegex(ValueError, "geometry"):
                locked.validate_data(candidate, bad)
        with self.assertRaisesRegex(ValueError, "SHA256"):
            locked.validate_data(row, data[:-1] + b"x")

    def test_compressed_font_rejects_wrong_type_trailing_data_and_bomb(self):
        for plain, suffix in ((b"unexpected archive/path", b""),
                              (b"SplineFontDB: 1.0\nFontName: DejaVuSans\nEndSplineFont\n", b"hidden"),
                              (b"x" * 802527, b"")):
            data = bz2.compress(plain) + suffix
            row = {"path": "release/datafiles/DejaVuSans-Lite.sfd.bz2", "bytes": len(data),
                   "sha256": hashlib.sha256(data).hexdigest()}
            with self.assertRaisesRegex(ValueError, "font"):
                locked.validate_data(row, data)

    def test_redirect_to_http_or_unapproved_host_fails(self):
        redirect = locked.Redirect()
        for url in ("http://projects.blender.org/file", "https://example.org/file", "https://user:password@projects.blender.org/file"):
            with self.assertRaises(ValueError):
                redirect.redirect_request(None, None, 302, "", {}, url)

    def test_archive_root_and_parent_traversal_rejected(self):
        for name in ("other/file", "source/../../outside"):
            archive = self.root / "archive.tar.gz"
            with tarfile.open(archive, "w:gz") as out:
                entry = tarfile.TarInfo(name)
                entry.size = 1
                out.addfile(entry, io.BytesIO(b"x"))
            with self.assertRaises(ValueError):
                locked.unpack(archive, "source", self.root / name.split('/')[0])
        self.assertFalse((self.root / "outside").exists())

    def test_missing_inventory_entry_fails_before_network_or_patch(self):
        cmake = self.root / "cmake"
        cmake.mkdir()
        (cmake / "download.cmake").write_text("download_source(UNEXPECTED)\n")
        with patch.object(locked, "fetch") as network, self.assertRaises(ValueError):
            locked.dependencies(self.lock, self.root, self.root / "packages")
        network.assert_not_called()

    def test_dependencies_remove_remote_cmake_and_lock_python_offline(self):
        cmake = self.root / "cmake"
        cmake.mkdir()
        (cmake / "download.cmake").write_text("\n".join("download_source(" + r["name"] + ")" for r in self.lock["dependencies"]) + "\n")
        (cmake / "versions.cmake").write_text("# original source\n")
        (cmake / "python_site_packages.cmake").write_text("  INSTALL_COMMAND ${PYTHON_BINARY} -m pip install --no-cache-dir ${SITE_PACKAGES_EXTRA}\n  untrusted-version\n  --no-binary :all:\n)\n")
        packages = self.root / "packages"
        packages.mkdir()
        with patch.object(locked, "fetch") as fetch:
            locked.dependencies(self.lock, self.root, packages)
        self.assertEqual(fetch.call_count, 75)
        downloader = (cmake / "download.cmake").read_text()
        self.assertNotIn("DOWNLOAD", downloader)
        self.assertNotIn("http", downloader)
        self.assertEqual(downloader.count("file(SHA256"), 66)
        versions = (cmake / "versions.cmake").read_text()
        self.assertEqual(versions.count("_HASH_TYPE SHA256)"), 66)
        site = (cmake / "python_site_packages.cmake").read_text()
        self.assertIn("--no-index --no-deps", site)
        self.assertIn("--require-hashes", site)
        self.assertEqual(len((packages / "python-requirements.txt").read_text().splitlines()), 9)

    def test_lfs_pointer_must_match_source_before_data_download(self):
        source = self.root / "blender"
        source.mkdir()
        row = self.lock["lfs_assets"][0]
        target = source / row["path"]
        target.parent.mkdir(parents=True)
        target.write_text("different pointer")
        with patch.object(locked, "unpack"), patch.object(locked, "fetch") as fetch, self.assertRaises(ValueError):
            locked.sources(self.lock, source, self.root / "optix", self.root / "cache")
        self.assertEqual(fetch.call_count, 2)  # Only the two locked source archives.


if __name__ == "__main__":
    unittest.main()
