"""Release identity must remain usable by the strict in-app updater."""
import hashlib
import importlib.util
import io
import json
import lzma
from pathlib import Path
import struct
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('prepare_manifest', Path(__file__).with_name('prepare-manifest.py'))
manifest = importlib.util.module_from_spec(spec)
spec.loader.exec_module(manifest)


class ReleaseIdentity(unittest.TestCase):
    def test_stable_and_historical_beta_names(self):
        for version in ('0.2.0', '0.1.0-beta.2', '0.6.2'):
            manifest.release_names(version, 'v' + version, f'DGXBridge_{version}_x64-setup.exe')

    def test_no_accepted_but_unusable_release_names(self):
        for tag, filename in [('0.2.0', 'DGXBridge_0.2.0_x64-setup.exe'), ('v0.2.0', 'setup.exe'), ('v0.2.1', 'DGXBridge_0.2.0_x64-setup.exe')]:
            with self.assertRaises(ValueError):
                manifest.release_names('0.2.0', tag, filename)

    def test_noncanonical_versions_fail(self):
        for version in ('01.2.0', '0.2', '0.2.0+other', '0.2.0-beta.01', '0.2.0-'):
            with self.assertRaises(ValueError):
                manifest.semver(version)

    def test_rename_keeps_installed_updater_asset_name(self):
        with self.assertRaises(ValueError):
            manifest.release_names('0.6.2', 'v0.6.2', 'RTXBridge_0.6.2_x64-setup.exe')


class EnrollmentDisclosures(unittest.TestCase):
    def document(self):
        return {'name': 'DGXBridge', 'appId': 'dgxbridge', 'slug': 'dgxbridge',
                'description': 'DGXBridge connects computers.', 'privacy': {'retention': 'Existing state.'},
                'permissions': {'filesystem': []}, 'featureNotices': []}

    def test_historical_manifest_does_not_gain_new_authorization(self):
        document = self.document()
        before = json.dumps(document, sort_keys=True)
        manifest.release_disclosures(document, '0.6.1')
        self.assertEqual(json.dumps(document, sort_keys=True), before)

    def test_current_release_declares_credential_and_remote_key_side_effects(self):
        document = self.document()
        manifest.release_disclosures(document, '0.6.2')
        self.assertEqual((document['name'], document['appId'], document['slug']), ('RTXBridge', 'dgxbridge', 'dgxbridge'))
        self.assertIn('independently obtained identity', document['description'])
        self.assertIn('before any password-bearing SSH connection', document['description'])
        self.assertIn('child-process environment', document['description'])
        self.assertIn('known_hosts pins', document['privacy']['retention'])
        self.assertTrue(any('selected remote account' in row for row in document['permissions']['filesystem']))


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

    def test_filesystem_disclosures_fit_marketplace_contract(self):
        # Manifest schema v1 caps each entry at 200 characters. The 0.4.0
        # shared-cache disclosure previously exceeded that admission limit.
        for entry in manifest.manifest(release_input(), [])['permissions']['filesystem']:
            self.assertLessEqual(len(entry), 200)

    def test_050_discloses_return_worker_without_gpu_success_claim(self):
        document = manifest.manifest(release_input(version='0.5.0'), [])
        self.assertIn('sparkdev-return.exe', document['description'])
        self.assertIn('concurrent local edits', document['description'])
        self.assertIn('preserving configuration, pairing, jobs and pending returns', document['description'])
        self.assertIn('not proof of GPU operation', document['description'])
        self.assertTrue(document['permissions']['backgroundServices'])
        self.assertLessEqual(len(document['description']), 12000)
        for entry in document['permissions']['filesystem']:
            self.assertLessEqual(len(entry), 200)
        for entry in document['intelligence']['capabilities']:
            self.assertLessEqual(len(entry), 200)

    def test_060_declares_named_computers_without_changing_native_windows_admission(self):
        document = manifest.manifest(release_input(version='0.6.0'), [])
        self.assertIn('Linux compute computers', document['description'])
        self.assertIn('Spark is optional for named remote Linux jobs', document['description'])
        self.assertIn('native Windows laptop jobs still require the original reachable Spark scheduler', document['description'])
        self.assertIn('AMD64 Blender is not provisioned', document['description'])
        self.assertIn('cross-host GPU memory pooling are not implemented', document['description'])
        self.assertIn('no new-node relay pairing', document['description'])
        self.assertIn('observation', document['description'])
        integrations = {row['name']: row for row in document['integrations']}
        self.assertFalse(integrations['Owner-controlled NVIDIA DGX Spark']['required'])
        self.assertTrue(integrations['Owner-selected Linux or Windows/WSL compute computer']['required'])
        self.assertIn('owner-selected compute computer', document['privacy']['dataLeavesDevice'])
        self.assertIn('Uncertain submissions', document['privacy']['retention'])
        self.assertIn('test-node:JOB_ID', document['intelligence']['functionalTest'])
        self.assertEqual(document['artifacts'][0]['requirements']['runtimes'], ['Microsoft Edge WebView2'])
        self.assertLessEqual(len(document['description']), 12000)
        self.assertLessEqual(len(document['privacy']['retention']), 2000)
        self.assertLessEqual(len(document['intelligence']['functionalTest']), 2000)
        for key, bound in [('filesystem', 200), ('credentials', 160)]:
            for entry in document['permissions'][key]:
                self.assertLessEqual(len(entry), bound, entry)
        for entry in document['intelligence']['capabilities']:
            self.assertLessEqual(len(entry), 200, entry)

    def test_historical_051_listing_does_not_advertise_named_computers(self):
        document = manifest.manifest(release_input(version='0.5.1'), [])
        self.assertTrue(document['integrations'][0]['required'])
        self.assertNotIn('Named computers retain', document['description'])


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


class RuntimeDownloads(unittest.TestCase):
    filename = 'pillow-13.0.0-cp312-cp312-manylinux_2_27_aarch64.manylinux_2_28_aarch64.whl'
    url = 'https://files.pythonhosted.org/packages/aa/bb/' + filename
    digest = 'e' * 64

    def recipe(self):
        return (f'ADD {self.url} /opt/sparkdev/wheels/\n'
                "RUN printf '%s\\n' 'pillow==13.0.0 --hash=sha256:" + self.digest + "' > /opt/sparkdev/pillow-requirements.txt \\\n"
                ' && /usr/bin/python3 -m pip install --no-cache-dir --disable-pip-version-check --no-index --find-links=/opt/sparkdev/wheels --require-hashes --only-binary=:all: --no-deps --force-reinstall --break-system-packages -r /opt/sparkdev/pillow-requirements.txt\n')

    def metadata(self, **changes):
        item = {'url': self.url, 'filename': self.filename, 'digests': {'sha256': self.digest},
                'packagetype': 'bdist_wheel', 'yanked': False}
        item.update(changes)
        return io.BytesIO(json.dumps({'urls': [item]}).encode())

    def downloads(self, recipe):
        sources = {'runtimes/Dockerfile': 'FROM rust:1.98.1@sha256:' + 'a' * 64,
                   'runtimes/Dockerfile.cuda': 'FROM nvcr.io/nvidia/pytorch@sha256:' + 'b' * 64 + '\n' + recipe,
                   'runtimes/requirements.txt': 'numpy==2.4.0 --hash=sha256:' + 'c' * 64 + '\npillow==13.0.0 --hash=sha256:' + 'd' * 64}

        def metadata(url, timeout):
            if url.endswith('/numpy/2.4.0/json'):
                return io.BytesIO(json.dumps({'urls': [{'url': 'https://files.pythonhosted.org/cpu-numpy.whl', 'digests': {'sha256': 'c' * 64}}]}).encode())
            return io.BytesIO(json.dumps({'urls': [
                {'url': 'https://files.pythonhosted.org/cpu-pillow.whl', 'digests': {'sha256': 'd' * 64}},
                json.load(self.metadata())['urls'][0]]}).encode())

        with patch.object(manifest, 'source', side_effect=lambda commit, path: sources[path].encode()) as source, \
                patch.object(manifest.urllib.request, 'urlopen', side_effect=metadata):
            result = manifest.runtime_downloads('selected-immutable-commit')
        self.assertEqual([call.args[0] for call in source.call_args_list], ['selected-immutable-commit'] * 3)
        return result

    def test_historical_no_add_retains_four_downloads(self):
        self.assertEqual(len(self.downloads('# No additional wheel in this release.\n')), 4)

    def test_cuda_wheel_adds_exact_fifth_download_from_selected_source(self):
        downloads = self.downloads(self.recipe())
        self.assertEqual(len(downloads), 5)
        self.assertEqual((downloads[-1]['url'], downloads[-1]['sha256']), (self.url, self.digest))
        self.assertIn('optional owner-requested Spark CUDA', downloads[-1]['purpose'])

    def test_unsupported_or_unpinned_add_fails_closed(self):
        for recipe in (f'ADD {self.url} /opt/sparkdev/wheels/\n',
                       self.recipe().replace('--require-hashes ', ''),
                       self.recipe().replace(self.digest, 'unpinned'),
                       self.recipe().replace('ADD ', 'ADD --keep-git-dir '),
                       self.recipe().replace('https://files.pythonhosted.org', 'http://files.pythonhosted.org'),
                       self.recipe() + 'add local-file /tmp/\n'):
            with self.subTest(recipe=recipe), self.assertRaises(ValueError), \
                    patch.object(manifest.urllib.request, 'urlopen') as fetch:
                manifest.cuda_wheel_downloads(recipe)
            fetch.assert_not_called()

    def test_cuda_metadata_requires_matching_url_hash_and_wheel(self):
        for changes in ({'url': self.url + '?changed'}, {'digests': {'sha256': 'f' * 64}},
                        {'filename': 'different.whl'}, {'yanked': True}, {'packagetype': 'sdist'}):
            with self.subTest(changes=changes), patch.object(manifest.urllib.request, 'urlopen', return_value=self.metadata(**changes)), \
                    self.assertRaises(ValueError):
                manifest.cuda_wheel_downloads(self.recipe())


class MultiDeviceRuntimeDownloads(unittest.TestCase):
    def sources(self):
        paths = ['runtimes/Dockerfile.amd64', 'runtimes/Dockerfile.cuda.amd64', 'runtimes/requirements-amd64.txt']
        files = {path: (manifest.ROOT / path).read_bytes() for path in paths}
        arm = RuntimeDownloads()
        files.update({'runtimes/Dockerfile': b'FROM rust:1@sha256:' + b'a' * 64,
                      'runtimes/Dockerfile.cuda': ('FROM nvcr.io/nvidia/pytorch@sha256:' + 'b' * 64 + '\n' + arm.recipe()).encode(),
                      'runtimes/requirements.txt': b'numpy==2.4.0 --hash=sha256:' + b'c' * 64 + b'\npillow==13.0.0 --hash=sha256:' + b'd' * 64})
        return files

    def metadata(self):
        rows = [{'url': 'https://files.pythonhosted.org/cpu-numpy.whl', 'digests': {'sha256': 'c' * 64}},
                {'url': 'https://files.pythonhosted.org/cpu-pillow.whl', 'digests': {'sha256': 'd' * 64}},
                json.load(RuntimeDownloads().metadata())['urls'][0]]
        for name, digest in [('numpy-2.4.6-cp313-cp313-manylinux_2_27_x86_64.manylinux_2_28_x86_64.whl', 'a7830bab239b79cda9c08c2da014761cafb48da6150e1da17ac06283f43b6089'),
                             ('pillow-13.0.0-cp313-cp313-manylinux_2_27_x86_64.manylinux_2_28_x86_64.whl', '5f56499c1a74d2b862beb0c33b8016f99d095858455df9d21583bbbc1c15b582'),
                             ('pillow-13.0.0-cp312-cp312-manylinux_2_27_x86_64.manylinux_2_28_x86_64.whl', '42d63be3af4f9b951b199b0a9d23365d37ab14687e6d77b74940432c8515c4ca')]:
            url = 'https://files.pythonhosted.org/packages/aa/bb/' + name
            if '-cp312-' in name:
                url = 'https://files.pythonhosted.org/packages/e6/99/fdf65cc539e266ab93546c261bdf98411fe165c6e4e13f9dbf597097192b/' + name
            rows.append({'url': url, 'filename': name, 'digests': {'sha256': digest}, 'packagetype': 'bdist_wheel', 'yanked': False})
        return {'urls': rows}

    def collect(self, sources=None, metadata=None, version='0.6.0'):
        files = self.sources() if sources is None else sources
        metadata = self.metadata() if metadata is None else metadata
        evidence = {}
        with patch.object(manifest, 'source', side_effect=lambda commit, path: files[path]) as read, \
                patch.object(manifest.urllib.request, 'urlopen', side_effect=lambda *a, **k: io.BytesIO(json.dumps(metadata).encode())):
            result = manifest.runtime_downloads('exact-release-commit', evidence, version)
        self.assertTrue(all(call.args[0] == 'exact-release-commit' for call in read.call_args_list))
        return result, evidence

    def test_060_adds_five_independent_amd64_downloads_and_preserves_arm_inputs(self):
        historical, old_evidence = self.collect(version='0.5.1')
        downloads, evidence = self.collect()
        self.assertEqual(len(historical), 5)
        self.assertEqual(downloads[:5], historical)
        self.assertEqual(len(downloads), 10)
        self.assertNotIn('amd64', old_evidence)
        self.assertEqual([row['sha256'] for row in downloads[5:]], [
            'fce83543909cd5409924a61ce296bdf5bab0fca37f004c0f6c8ef6f4a37bbc59',
            '4bf906c628d572681a977681765a44a3c1d7e34b39633b7160a84de707608139',
            'a7830bab239b79cda9c08c2da014761cafb48da6150e1da17ac06283f43b6089',
            '5f56499c1a74d2b862beb0c33b8016f99d095858455df9d21583bbbc1c15b582',
            '42d63be3af4f9b951b199b0a9d23365d37ab14687e6d77b74940432c8515c4ca'])
        for row in downloads[5:]:
            self.assertIn('AMD64', row['purpose'])
            self.assertLessEqual(len(row['purpose']), 300)
        self.assertFalse(evidence['amd64']['blenderProvisioned'])
        self.assertEqual(evidence['amd64']['sourceSha256'], {path: hashlib.sha256(raw).hexdigest() for path, raw in self.sources().items() if path.endswith(('amd64', 'amd64.txt'))})

    def test_060_never_silently_omits_missing_unpinned_or_cross_architecture_inputs(self):
        for mutation in ['missing-cpu', 'missing-cuda', 'missing-lock', 'floating-base', 'wrong-lock', 'extra-add', 'extra-stage', 'missing-cuda-wheel', 'wrong-cuda-architecture', 'unhashed-wheel']:
            sources = self.sources()
            cpu, cuda, lock = 'runtimes/Dockerfile.amd64', 'runtimes/Dockerfile.cuda.amd64', 'runtimes/requirements-amd64.txt'
            if mutation.startswith('missing-') and mutation != 'missing-cuda-wheel':
                del sources[{'missing-cpu': cpu, 'missing-cuda': cuda, 'missing-lock': lock}[mutation]]
            elif mutation == 'floating-base':
                sources[cpu] = b'FROM rust:latest\nCOPY requirements-amd64.txt /opt/sparkdev/requirements.txt\n'
            elif mutation == 'wrong-lock':
                sources[cpu] = sources[cpu].replace(b'COPY requirements-amd64.txt', b'COPY requirements.txt')
            elif mutation == 'extra-add':
                sources[cpu] += b'\nADD https://example.com/code /tmp/code\n'
            elif mutation == 'extra-stage':
                sources[cuda] += b'\nCOPY --from=unrecorded /opt/blender /opt/blender\n'
            elif mutation == 'missing-cuda-wheel':
                sources[cuda] = b'\n'.join(line for line in sources[cuda].splitlines() if not line.startswith(b'ADD '))
            elif mutation == 'wrong-cuda-architecture':
                sources[cuda] = sources[cuda].replace(b'x86_64', b'aarch64')
            else:
                sources[lock] = sources[lock].replace(b' --hash=sha256:', b' # unhashed ')
            with self.subTest(mutation=mutation), self.assertRaises((ValueError, KeyError)):
                self.collect(sources=sources)

    def test_amd64_cpu_metadata_must_match_architecture_type_and_live_artifact(self):
        for mutation in ['arm-wheel', 'yanked', 'sdist', 'http', 'wrong-hash']:
            metadata = self.metadata()
            row = metadata['urls'][3]
            if mutation == 'arm-wheel':
                row['url'] = row['url'].replace('x86_64', 'aarch64')
                row['filename'] = row['filename'].replace('x86_64', 'aarch64')
            elif mutation == 'yanked':
                row['yanked'] = True
            elif mutation == 'sdist':
                row['packagetype'] = 'sdist'
            elif mutation == 'http':
                row['url'] = row['url'].replace('https:', 'http:')
            else:
                row['digests']['sha256'] = 'f' * 64
            with self.subTest(mutation=mutation), self.assertRaises(ValueError):
                self.collect(metadata=metadata)


class MultiStageRuntime(unittest.TestCase):
    def test_exact_multistage_from_and_reject_unpinned_variants(self):
        pin = 'nvidia/cuda:13.0@sha256:' + '1' * 64
        self.assertEqual(manifest.docker_stages(f'ARG BASE={pin}\nFROM ${{BASE}} AS builder\nFROM builder AS copied'), [pin, 'builder'])
        self.assertEqual(manifest.docker_stages('ARG BLENDER_IMAGE\nFROM ${BLENDER_IMAGE} AS blender', ('BLENDER_IMAGE',)), ['BLENDER_IMAGE'])
        for recipe in ('FROM ubuntu:latest', 'FROM ${UNKNOWN}', 'FROM --platform=linux/arm64 ubuntu:latest',
                       'ARG BLENDER_IMAGE=ubuntu:latest\nFROM ${BLENDER_IMAGE}', f'FROM {pin}\nFROM surprise:latest'):
            with self.subTest(recipe=recipe), self.assertRaises(ValueError):
                manifest.docker_stages(recipe, ('BLENDER_IMAGE',))

    def sources(self):
        paths = ['runtimes/Dockerfile.blender', 'runtimes/blender/PROVENANCE.json', 'remote/install-cuda.py',
                 'runtimes/blender/locked_sources.py', 'runtimes/blender/inputs.lock.json']
        return {p: (manifest.ROOT / p).read_bytes() for p in paths}

    def test_blender_bases_and_source_pins_come_from_selected_commit(self):
        sources = self.sources()
        with patch.object(manifest, 'source', side_effect=lambda commit, path: sources[path]) as read:
            details = manifest.blender_sources('selected-commit')
        self.assertEqual(len(details['bases']), 2)
        self.assertEqual(details['sources']['blender']['commit'], '8bf95cbd38d1456c94e9d751f3804eea4d5b137b')
        self.assertTrue(details['existingLocalImage'].startswith('sha256:'))
        self.assertEqual(len(details['executableInputs']), 77)
        self.assertEqual(details['runtimeDataObjects'], 187)
        self.assertFalse(details['downloadDeclarationComplete'])
        self.assertTrue(all(call.args[0] == 'selected-commit' for call in read.call_args_list))

    def test_blender_provenance_mismatch_or_new_add_fails_closed(self):
        for change in ('provenance', 'add', 'unverified-image', 'floating-mirror'):
            sources = self.sources()
            if change == 'provenance':
                p = json.loads(sources['runtimes/blender/PROVENANCE.json'])
                p['blender_commit'] = 'a' * 40
                sources['runtimes/blender/PROVENANCE.json'] = json.dumps(p).encode()
            elif change == 'add':
                sources['runtimes/Dockerfile.blender'] += b'\nADD https://example.com/code /tmp/\n'
            elif change == 'unverified-image':
                sources['remote/install-cuda.py'] = b'BLENDER_EXISTING = "ubuntu:latest"\n'
            else:
                lock = json.loads(sources['runtimes/blender/inputs.lock.json'])
                lock['dependencies'][0]['url'] = 'https://projects.blender.org/floating-branch/file'
                sources['runtimes/blender/inputs.lock.json'] = json.dumps(lock).encode()
            with self.subTest(change=change), patch.object(manifest, 'source', side_effect=lambda commit, path: sources[path]), self.assertRaises(ValueError):
                manifest.blender_sources('selected-commit')

    def test_blender_download_declarations_include_both_additional_bases(self):
        sources = self.sources()
        sources.update({'runtimes/Dockerfile': b'FROM rust:1@sha256:' + b'a' * 64,
                        'runtimes/Dockerfile.cuda': b'ARG BLENDER_IMAGE\nFROM ${BLENDER_IMAGE} AS blender\nFROM nvcr.io/nvidia/pytorch@sha256:' + b'b' * 64,
                        'runtimes/requirements.txt': b'numpy==1 --hash=sha256:' + b'c' * 64 + b'\npillow==13 --hash=sha256:' + b'd' * 64})
        metadata = {'urls': [{'url': 'https://files.pythonhosted.org/a.whl', 'digests': {'sha256': 'c' * 64}},
                             {'url': 'https://files.pythonhosted.org/b.whl', 'digests': {'sha256': 'd' * 64}}]}
        evidence = {}
        with patch.object(manifest, 'source', side_effect=lambda commit, path: sources[path]), \
                patch.object(manifest.urllib.request, 'urlopen', side_effect=lambda *a, **k: io.BytesIO(json.dumps(metadata).encode())):
            downloads = manifest.runtime_downloads('selected-commit', evidence)
        self.assertEqual(len(downloads), 83)  # 84 with the separately tested CUDA Pillow wheel.
        self.assertIn('/v2/nvidia/cuda/', downloads[4]['url'])
        self.assertIn('/v2/library/ubuntu/', downloads[5]['url'])
        self.assertTrue(any('/codeload.github.com/blender/' in row['url'] for row in downloads))
        self.assertEqual(sum('/lib-source.git/info/lfs/objects/' in row['url'] for row in downloads), 66)
        self.assertTrue(all(len(row['purpose']) <= 300 for row in downloads))
        self.assertIn('blender', evidence)
        document = manifest.manifest(release_input(version='0.5.0'), downloads, evidence)
        self.assertIn('66 dependency archives and nine Python wheels', document['description'])

    def test_060_keeps_the_full_arm_blender_inventory_alongside_amd64(self):
        sources = self.sources()
        fixture = MultiDeviceRuntimeDownloads()
        sources.update(fixture.sources())
        sources['runtimes/Dockerfile.cuda'] = b'ARG BLENDER_IMAGE\nFROM ${BLENDER_IMAGE} AS blender\nFROM nvcr.io/nvidia/pytorch@sha256:' + b'b' * 64
        evidence = {}
        with patch.object(manifest, 'source', side_effect=lambda commit, path: sources[path]), \
                patch.object(manifest.urllib.request, 'urlopen', side_effect=lambda *a, **k: io.BytesIO(json.dumps(fixture.metadata()).encode())):
            downloads = manifest.runtime_downloads('selected-commit', evidence, '0.6.0')
        self.assertEqual(len(downloads), 88)
        self.assertEqual(sum('/lib-source.git/info/lfs/objects/' in row['url'] for row in downloads), 66)
        self.assertEqual(set(evidence), {'blender', 'amd64'})
        document = manifest.manifest(release_input(version='0.6.0'), downloads, evidence)
        self.assertIn('66 dependency archives and nine Python wheels', document['description'])
        self.assertIn('AMD64 Blender is not provisioned', document['description'])
        self.assertLessEqual(len(document['description']), 12000)
        self.assertLessEqual(len(document['privacy']['retention']), 2000)
        for entry in document['permissions']['filesystem']:
            self.assertLessEqual(len(entry), 200, entry)

    def test_051_real_arm_recipes_never_require_amd64_sources(self):
        sources = self.sources()
        for path in ['runtimes/Dockerfile', 'runtimes/Dockerfile.cuda', 'runtimes/requirements.txt']:
            sources[path] = (manifest.ROOT / path).read_bytes()
        cuda_url = next(line.split()[1] for line in sources['runtimes/Dockerfile.cuda'].decode().splitlines() if line.startswith('ADD '))
        metadata = {'urls': [
            {'url': 'https://files.pythonhosted.org/arm-numpy.whl', 'digests': {'sha256': '72fbe16c6fac95aedf5937fa873445cec2110be35d8a4e9433d7501fd98dae6b'}},
            {'url': 'https://files.pythonhosted.org/arm-pillow.whl', 'digests': {'sha256': '19b66ca0a4d25cc0200b381ce824c7ea090db61427315e73cfb05dbf1b35c369'}},
            {'url': cuda_url, 'filename': cuda_url.rsplit('/', 1)[1], 'packagetype': 'bdist_wheel', 'yanked': False,
             'digests': {'sha256': '8bf295fe459343bc6093c8e5e0df60bfa189e0fcb775a22e2c2488fdeccc6c0d'}}]}
        evidence = {}
        with patch.object(manifest, 'source', side_effect=lambda commit, path: sources[path]) as read, \
                patch.object(manifest.urllib.request, 'urlopen', side_effect=lambda *a, **k: io.BytesIO(json.dumps(metadata).encode())):
            downloads = manifest.runtime_downloads('historical-arm-release', evidence, '0.5.1')
        self.assertEqual(len(downloads), 84)
        self.assertEqual(set(evidence), {'blender'})
        self.assertTrue(all(call.args[0] == 'historical-arm-release' and 'amd64' not in call.args[1] for call in read.call_args_list))


def fake_nsis(files):
    header = b'header'
    records = b''.join(struct.pack('<I', len(raw)) + raw for raw in [header, *files])
    compressed = lzma.compress(records, format=lzma.FORMAT_ALONE)
    stream = compressed[:5] + compressed[13:]
    return struct.pack('<I', 0) + b'\xef\xbe\xad\xdeNullsoftInst' + struct.pack('<II', len(header), 28 + len(stream)) + stream


class WindowsArtifactEvidence(unittest.TestCase):
    def test_reads_records_and_rejects_truncated_installer(self):
        raw = b'MZworker'
        self.assertEqual(manifest.nsis_records(fake_nsis([raw]))[hashlib.sha256(raw).hexdigest()], len(raw))
        with self.assertRaises(ValueError):
            manifest.nsis_records(fake_nsis([raw])[:-2])

    def verify_fixture(self, mutation=None):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            evidence, binaries = {}, []
            for name, (filename, sbom_filename) in manifest.WINDOWS_PAYLOAD.items():
                raw = b'MZ' + filename.encode() + (b'__TAURI_BUNDLE_TYPE_VAR_NSS' if name == 'gui' else b'')
                binaries.append(raw)
                digest = hashlib.sha256(raw).hexdigest()
                sbom = {'descriptor': {'name': 'syft'}, 'source': {'name': filename, 'type': 'file', 'version': 'sha256:' + digest,
                        'metadata': {'digests': [{'algorithm': 'sha256', 'value': digest}]}},
                        'artifacts': [{'foundBy': 'cargo-auditable-binary-cataloger', 'type': 'rust-crate'}]}
                if name == 'return' and mutation == 'wrong-sbom':
                    sbom['source']['version'] = 'sha256:' + 'a' * 64
                if name == 'return' and mutation == 'empty-sbom':
                    sbom['artifacts'] = []
                (root / filename).write_bytes(raw)
                (root / sbom_filename).write_text(json.dumps(sbom))
                evidence[name] = {'binary': filename, 'sbom': sbom_filename}
            if mutation == 'missing-worker':
                binaries.pop()
            if mutation == 'unshipped-gui':
                p = root / 'dgxbridge.exe'
                p.write_bytes(p.read_bytes().replace(b'_NSS', b'_UNK'))
            (root / 'setup.exe').write_bytes(fake_nsis(binaries))
            (root / 'evidence.json').write_text(json.dumps(evidence))
            return manifest.windows_payload_evidence(root / 'setup.exe', root / 'evidence.json')

    def test_four_independent_sboms_bind_actual_installer_bytes(self):
        result = self.verify_fixture()
        self.assertEqual(set(result['binaries']), {'gui', 'cli', 'link', 'return'})
        self.assertEqual(result['binaries']['return']['sbom'], 'sparkdev-windows-return.sbom.json')

    def test_missing_worker_or_wrong_sbom_or_unshipped_gui_rejected(self):
        for mutation in ('missing-worker', 'wrong-sbom', 'empty-sbom', 'unshipped-gui'):
            with self.subTest(mutation=mutation), self.assertRaises(ValueError):
                self.verify_fixture(mutation)

    def test_missing_evidence_never_silently_passes(self):
        with self.assertRaises(ValueError):
            manifest.windows_payload_evidence(Path('not-read'), None)


if __name__ == '__main__':
    unittest.main()
