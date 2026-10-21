"""Release identity must remain usable by the strict in-app updater."""
import hashlib
import importlib.util
import json
from pathlib import Path
import tempfile
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


def release_input(**changes):
    value = {'publisherId': 'publisher-1', 'releaseId': 1, 'assetId': 2, 'sourceCommit': 'a' * 40, 'tag': 'v0.3.1', 'version': '0.3.1',
             'installer': {'filename': 'DGXBridge_0.3.1_x64-setup.exe', 'localPath': 'setup.exe'},
             'requirements': {'minimumOs': '0', 'memoryGb': 0, 'diskMb': 58}, 'testedHost': {'os': '10.0.26200', 'memoryGb': 32},
             'artwork': {'icon': {'url': 'i'}, 'thumbnail': {'url': 't'}},
             'screenshots': [{'url': 's', 'localPath': 's.png', 'alt': 'Installed DGXBridge'}], 'screenshotsReviewed': True}
    value.update(changes)
    return value


class Compatibility(unittest.TestCase):
    def test_unknown_minimums_are_accepted(self):
        manifest.validate_input(release_input())

    def test_invalid_minimums_and_missing_tested_host_fail(self):
        for changes in ({'requirements': {'minimumOs': '0', 'memoryGb': -1, 'diskMb': 58}},
                        {'requirements': {'minimumOs': 'latest', 'memoryGb': 0, 'diskMb': 58}},
                        {'requirements': {'minimumOs': '0', 'memoryGb': 0, 'diskMb': 0}},
                        {'testedHost': {'os': '10.0.26200', 'memoryGb': 0}}):
            with self.assertRaises(ValueError):
                manifest.validate_input(release_input(**changes))
        value = release_input()
        del value['testedHost']
        with self.assertRaises(ValueError):
            manifest.validate_input(value)

    def test_tested_host_never_becomes_a_requirement(self):
        r = manifest.manifest(release_input(), [])['artifacts'][0]['requirements']
        self.assertEqual((r['minimumOs'], r['memoryGb'], r['diskMb']), ('0', 0, 58))
        self.assertEqual(r['runtimes'], ['Microsoft Edge WebView2'])
        self.assertNotIn('testedHost', r)


class DraftValidation(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.root = Path(self.directory.name)
        (self.root / 'setup.exe').write_bytes(b'installer bytes')
        self.saved = manifest.github, manifest.source
        manifest.source = lambda commit, path: {
            'src-tauri/tauri.conf.json': json.dumps({'version': '0.3.1', 'bundle': {'windows': {'webviewInstallMode': {'type': 'skip'}}}}),
            'package.json': json.dumps({'version': '0.3.1'})}.get(path, 'MIT License').encode()

    def tearDown(self):
        manifest.github, manifest.source = self.saved
        self.directory.cleanup()

    def verify(self, draft, url, require_published):
        digest = hashlib.sha256(b'installer bytes').hexdigest()
        asset = {'id': 2, 'name': 'DGXBridge_0.3.1_x64-setup.exe', 'state': 'uploaded', 'size': 15, 'digest': 'sha256:' + digest, 'browser_download_url': url}
        responses = {'repos/delores88/nvidialink': {'id': manifest.REPOSITORY_ID, 'private': False, 'license': {'spdx_id': 'MIT'}},
                     'repos/delores88/nvidialink/releases/1': {'id': 1, 'tag_name': 'v0.3.1', 'prerelease': False, 'draft': draft, 'assets': [asset]},
                     'repos/delores88/nvidialink/git/ref/tags/v0.3.1': {'object': {'type': 'commit', 'sha': 'a' * 40}}}
        manifest.github = responses.__getitem__
        return manifest.verify_release(release_input(), self.root, require_published)

    def test_complete_draft_validates_before_publication(self):
        draft = 'https://github.com/delores88/nvidialink/releases/download/untagged-0b76751e759b004bb14e/DGXBridge_0.3.1_x64-setup.exe'
        self.assertTrue(self.verify(True, draft, False)['draft'])
        with self.assertRaises(ValueError):
            self.verify(True, draft, True)
        with self.assertRaises(ValueError):
            self.verify(False, draft, False)

    def test_published_release_requires_canonical_url(self):
        canonical = 'https://github.com/delores88/nvidialink/releases/download/v0.3.1/DGXBridge_0.3.1_x64-setup.exe'
        self.assertFalse(self.verify(False, canonical, True)['draft'])
        with self.assertRaises(ValueError):
            self.verify(True, 'https://example.com/DGXBridge_0.3.1_x64-setup.exe', False)


if __name__ == '__main__':
    unittest.main()
