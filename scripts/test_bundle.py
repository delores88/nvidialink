"""Packaging contract tests; no real Spark, build, or service changes."""
import hashlib
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location('prepare_bundle', Path(__file__).with_name('prepare-bundle.py'))
bundle = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bundle)


class BundleTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name) / 'source'
        self.output = Path(self.temporary.name) / 'output'
        binary = self.root / 'dist/linux-arm64/sparkdev-link'
        binary.parent.mkdir(parents=True)
        binary.write_bytes(b'\x7fELF\x00fixture')
        for name, path in bundle.sources(self.root).items():
            path.parent.mkdir(parents=True, exist_ok=True)
            if not path.exists():
                path.write_bytes(('fixture:' + name).encode())

    def test_manifest_keeps_payloads_raw_exact_and_contains_no_encoded_data(self):
        receipt = bundle.prepare(self.root, self.output)
        manifest = json.loads((self.output / 'sparkdev-bundle.json').read_bytes())
        self.assertEqual(receipt['v'], 2)
        self.assertEqual(len(manifest['files']), 13)
        self.assertLess(receipt['bytes'], 64 * 1024)
        for entry in manifest['files']:
            self.assertEqual(set(entry), {'path', 'size', 'sha256'})
            raw = (self.output / 'sparkdev-remote' / entry['path']).read_bytes()
            self.assertEqual(raw, bundle.sources(self.root)[entry['path']].read_bytes())
            self.assertEqual(entry['size'], len(raw))
            self.assertEqual(entry['sha256'], hashlib.sha256(raw).hexdigest())

    def test_repeated_build_is_deterministic_and_stale_file_is_rejected_before_writes(self):
        first = bundle.prepare(self.root, self.output)
        self.assertEqual(first, bundle.prepare(self.root, self.output))
        manifest = (self.output / 'sparkdev-bundle.json').read_bytes()
        (self.output / 'sparkdev-remote/unexpected.exe').write_bytes(b'unexpected')
        with self.assertRaisesRegex(ValueError, 'stale bundle entry'):
            bundle.prepare(self.root, self.output)
        self.assertEqual(manifest, (self.output / 'sparkdev-bundle.json').read_bytes())

    def test_binary_required_for_release_and_plaintext_size_is_bounded(self):
        (self.root / 'dist/linux-arm64/sparkdev-link').unlink()
        with self.assertRaisesRegex(ValueError, 'requires'):
            bundle.prepare(self.root, self.output)
        self.assertFalse(self.output.exists())
        (self.root / 'remote/controller.py').write_bytes(b'x' * (1024 * 1024 + 1))
        with self.assertRaisesRegex(ValueError, 'bounded'):
            bundle.prepare(self.root, self.output, controller_only=True)
        self.assertFalse(self.output.exists())

    def test_symlink_source_is_rejected_when_platform_allows_creation(self):
        source = self.root / 'remote/controller.py'
        outside = self.root / 'other.py'
        outside.write_bytes(source.read_bytes())
        source.unlink()
        try:
            source.symlink_to(outside)
        except OSError as error:
            self.skipTest('Symlink creation unavailable: ' + type(error).__name__)
        with self.assertRaisesRegex(ValueError, 'symlinks'):
            bundle.prepare(self.root, self.output)


if __name__ == '__main__':
    unittest.main()
