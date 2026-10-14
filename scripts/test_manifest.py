"""Release identity must remain usable by the strict in-app updater."""
import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location('prepare_manifest', Path(__file__).with_name('prepare-manifest.py'))
manifest = importlib.util.module_from_spec(spec)
spec.loader.exec_module(manifest)


class ReleaseIdentity(unittest.TestCase):
    def test_stable_and_historical_beta_names(self):
        for version in ('0.2.0', '0.1.0-beta.2'):
            manifest.release_names(version, 'v' + version, f'DGXBridge_{version}_x64-setup.exe')

    def test_no_accepted_but_unusable_release_names(self):
        for tag, filename in [('0.2.0', 'DGXBridge_0.2.0_x64-setup.exe'), ('v0.2.0', 'setup.exe'), ('v0.2.1', 'DGXBridge_0.2.0_x64-setup.exe')]:
            with self.assertRaises(ValueError):
                manifest.release_names('0.2.0', tag, filename)

    def test_noncanonical_versions_fail(self):
        for version in ('01.2.0', '0.2', '0.2.0+other', '0.2.0-beta.01', '0.2.0-'):
            with self.assertRaises(ValueError):
                manifest.semver(version)


if __name__ == '__main__':
    unittest.main()
