"""Generate a release-specific Marketplace manifest; never upload or submit it.

Requires live GitHub identities, matching installer/media bytes, and the actual
Marketplace runtime contract. The input template deliberately cannot pass.
"""
import argparse
import hashlib
import io
import json
import lzma
from pathlib import Path
import re
import struct
import subprocess
import tempfile
from urllib.parse import quote, urlsplit
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
REPOSITORY = 'delores88/nvidialink'
REPOSITORY_ID = 1418188832
MEDIA_HOSTS = {'raw.githubusercontent.com', 'user-images.githubusercontent.com', 'github.com',
               'objects.githubusercontent.com', 'release-assets.githubusercontent.com', 'deloresai.com'}
HEX40 = re.compile(r'[0-9a-f]{40}')
MAX_IMAGE = 8 * 1024**2


def require(condition, message):
    if not condition:
        raise ValueError(message)


def object_keys(value, keys, label):
    require(isinstance(value, dict) and set(value) == set(keys), label + ' fields are incomplete or unknown')


def positive_id(value, label):
    require(type(value) is int and 0 < value <= 9007199254740991, label + ' must be a real positive numeric GitHub ID')


def semver(value):
    require(isinstance(value, str) and re.fullmatch(r'(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-[0-9A-Za-z.-]+)?', value),
            'Canonical stable or prerelease semver without build metadata is required')
    for part in value.split('-', 1)[1].split('.') if '-' in value else []:
        require(part and not (part.isdigit() and len(part) > 1 and part[0] == '0'), 'Invalid prerelease identifier')


def release_names(version, tag, filename):
    semver(version)
    require(tag == 'v' + version, 'The updater requires the canonical v-prefixed release tag')
    require(filename == f'DGXBridge_{version}_x64-setup.exe', 'The updater requires the canonical DGXBridge Windows installer filename')


def github(endpoint):
    result = subprocess.run(['gh', 'api', endpoint], capture_output=True, timeout=60)
    require(result.returncode == 0, 'GitHub read failed; verify gh authentication and the supplied repository/release IDs')
    return json.loads(result.stdout)


def source(commit, path):
    result = subprocess.run(['git', '-C', str(ROOT), 'show', f'{commit}:{path}'], capture_output=True, timeout=20)
    require(result.returncode == 0, 'The exact release source commit/file is unavailable locally: ' + path)
    return result.stdout


def media_url(value, redirect=False):
    require(isinstance(value, str) and len(value) <= 2048, 'Media URL is required')
    parsed = urlsplit(value)
    require(parsed.scheme == 'https' and parsed.hostname in MEDIA_HOSTS and not parsed.username
            and not parsed.password and parsed.port is None and not parsed.fragment, 'Unapproved media URL')
    if not redirect:
        require(not parsed.query, 'Use permanent media URLs without signed query parameters')
    if parsed.hostname == 'deloresai.com':
        require(re.fullmatch(r'/media/[A-Za-z0-9/_-]+\.(png|jpg|jpeg|webp)', parsed.path), 'Unapproved Marketplace media path')
    if parsed.hostname == 'raw.githubusercontent.com':
        parts = parsed.path.strip('/').split('/')
        require(len(parts) >= 4 and HEX40.fullmatch(parts[2]), 'Raw GitHub artwork must be pinned to a full commit')
    return parsed


class ApprovedRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, request, response, code, message, headers, newurl):
        media_url(newurl, redirect=True)
        return super().redirect_request(request, response, code, message, headers, newurl)


def verify_media(item, label, input_directory):
    from PIL import Image
    object_keys(item, ['url', 'localPath'], label)
    media_url(item['url'])
    require(isinstance(item['localPath'], str) and item['localPath'], label + ' requires a real local raster file')
    path = (input_directory / item['localPath']).resolve()
    raw = path.read_bytes()
    require(0 < len(raw) <= MAX_IMAGE, label + ' exceeds the 8 MiB image limit')
    with Image.open(io.BytesIO(raw)) as picture:
        require(picture.format in {'PNG', 'JPEG', 'WEBP'}, label + ' must be PNG, JPEG, or WebP')
        width, height = picture.size
        require(0 < width <= 4096 and 0 < height <= 4096 and width * height * 4 <= 64 * 1024**2,
                label + ' exceeds the decoded image limits')
        picture.verify()
    request = urllib.request.Request(item['url'], headers={'User-Agent': 'DGXBridge-release-check'})
    # Anonymous HTTP only: gh credentials are never passed to image requests.
    with urllib.request.build_opener(ApprovedRedirect()).open(request, timeout=30) as response:
        remote = response.read(MAX_IMAGE + 1)
    require(hashlib.sha256(remote).digest() == hashlib.sha256(raw).digest(), label + ' public URL does not serve the reviewed local bytes')
    return {'url': item['url'], 'sha256': hashlib.sha256(raw).hexdigest(), 'bytes': len(raw), 'width': width, 'height': height}


def validate_input(value):
    object_keys(value, ['publisherId', 'releaseId', 'assetId', 'sourceCommit', 'tag', 'version', 'installer',
                        'requirements', 'testedHost', 'artwork', 'screenshots', 'screenshotsReviewed'], 'Release input')
    publisher = value['publisherId']
    require(isinstance(publisher, str) and re.fullmatch(r'[A-Za-z0-9._:-]{1,128}', publisher)
            and not re.match(r'(?i)(replace|placeholder|example|test-only|todo)', publisher),
            'Supply the actual publisher ID from the signed-in publisher workspace')
    positive_id(value['releaseId'], 'releaseId')
    positive_id(value['assetId'], 'assetId')
    require(isinstance(value['sourceCommit'], str) and HEX40.fullmatch(value['sourceCommit']), 'sourceCommit must be full lowercase SHA-1')
    semver(value['version'])
    object_keys(value['installer'], ['filename', 'localPath'], 'installer')
    require(isinstance(value['installer']['filename'], str)
            and re.fullmatch(r'[^/\\\x00-\x1f]+\.exe', value['installer']['filename']), 'Supply the exact uploaded NSIS EXE filename')
    release_names(value['version'], value['tag'], value['installer']['filename'])
    require(isinstance(value['installer']['localPath'], str) and value['installer']['localPath'], 'Local installer is required')
    object_keys(value['requirements'], ['minimumOs', 'memoryGb', 'diskMb'], 'requirements')
    r = value['requirements']
    # Marketplace enforces these as install floors. "0" and 0 are its "Not specified" values; never copy the
    # test machine into them. The observed host is recorded separately as testedHost.
    require(isinstance(r['minimumOs'], str) and re.fullmatch(r'\d+(\.\d+){0,3}', r['minimumOs']), 'Supply a numeric minimum Windows version, or "0" when not established')
    require(type(r['memoryGb']) in {int, float} and 0 <= r['memoryGb'] <= 4096, 'Supply client memoryGb from 0 to 4096; 0 means not specified')
    require(type(r['diskMb']) in {int, float} and 0 < r['diskMb'] <= 4000000, 'Supply the measured positive client disk requirement')
    object_keys(value['testedHost'], ['os', 'memoryGb'], 'testedHost')
    require(isinstance(value['testedHost']['os'], str) and re.fullmatch(r'\d+(\.\d+){0,3}', value['testedHost']['os']), 'Record the observed test host Windows version')
    require(type(value['testedHost']['memoryGb']) in {int, float} and 0 < value['testedHost']['memoryGb'] <= 4096, 'Record the observed test host memory')
    object_keys(value['artwork'], ['icon', 'thumbnail'], 'artwork')
    require(isinstance(value['screenshots'], list) and 1 <= len(value['screenshots']) <= 10, 'One to ten actual application screenshots are required')
    require(value['screenshotsReviewed'] is True, 'Review screenshots for actual installed app content and removal of private data/tokens before generation')
    for shot in value['screenshots']:
        object_keys(shot, ['url', 'localPath', 'alt'], 'screenshot')
        require(isinstance(shot['alt'], str) and 0 < len(shot['alt'].strip()) <= 240, 'Screenshot alt text is required and limited to 240 characters')


def verify_release(value, input_directory, require_published):
    repo = github('repos/' + REPOSITORY)
    require(repo['id'] == REPOSITORY_ID and repo.get('private') is False and (repo.get('license') or {}).get('spdx_id') == 'MIT',
            'The live repository must retain its expected identity, public visibility and MIT license')
    release = github(f'repos/{REPOSITORY}/releases/{value["releaseId"]}')
    prerelease = '-' in value['version']
    require(release['id'] == value['releaseId'] and release['tag_name'] == value['tag'] and release.get('prerelease') is prerelease,
            'GitHub release ID, tag or prerelease status does not match the version')
    require(not require_published or not release.get('draft'), 'Publish the complete release before admission validation')
    ref = github(f'repos/{REPOSITORY}/git/ref/tags/{quote(value["tag"], safe="")}')['object']
    for _ in range(5):
        if ref['type'] == 'commit':
            break
        require(ref['type'] == 'tag', 'Unsupported release tag target')
        ref = github(f'repos/{REPOSITORY}/git/tags/{ref["sha"]}')['object']
    require(ref['type'] == 'commit' and ref['sha'] == value['sourceCommit'], 'The release tag does not resolve to sourceCommit')
    ui = json.loads(source(value['sourceCommit'], 'src-tauri/tauri.conf.json'))
    package = json.loads(source(value['sourceCommit'], 'package.json'))
    require(ui['version'] == package['version'] == value['version'], 'Product versions at sourceCommit do not match the release')
    require(ui['bundle']['windows']['webviewInstallMode']['type'] == 'skip', 'Manifest assumes preinstalled WebView2 and no installer bootstrap download')
    require(b'MIT License' in source(value['sourceCommit'], 'LICENSE')[:300], 'The tagged first-party source is not MIT')
    matches = [asset for asset in release['assets'] if asset['id'] == value['assetId']]
    require(len(matches) == 1, 'The installer asset ID does not belong to the release')
    asset = matches[0]
    require(asset['name'] == value['installer']['filename'] and asset.get('state') == 'uploaded', 'The uploaded installer name/state differs')
    installer = (input_directory / value['installer']['localPath']).resolve()
    with installer.open('rb') as stream:
        digest = hashlib.file_digest(stream, 'sha256').hexdigest()
    require(0 < installer.stat().st_size <= 2000000000 and asset['size'] == installer.stat().st_size, 'Installer byte size differs from GitHub')
    require(asset.get('digest') == 'sha256:' + digest, 'GitHub installer SHA-256 metadata differs or is unavailable')
    expected = f'https://github.com/{REPOSITORY}/releases/download/{quote(value["tag"], safe="")}/{quote(asset["name"], safe="")}'
    # GitHub serves draft assets from an untagged-* path until publication. The manifest names the installer by
    # asset ID and filename, so it can be validated and attached to the complete draft before Marketplace intake
    # can observe the release; --require-published then confirms the canonical URL after publication.
    draft_url = re.fullmatch(rf'https://github\.com/{re.escape(REPOSITORY)}/releases/download/untagged-[0-9a-f]+/{re.escape(quote(asset["name"], safe=""))}',
                             asset['browser_download_url'])
    require(asset['browser_download_url'] == expected or (release.get('draft') is True and not require_published and draft_url is not None),
            'Installer download URL is not the canonical release URL')
    return {'repositoryId': repo['id'], 'releaseId': release['id'], 'draft': release['draft'], 'prerelease': prerelease,
            'sourceCommit': ref['sha'], 'assetId': asset['id'], 'filename': asset['name'], 'bytes': asset['size'], 'sha256': digest}


def cuda_wheel_downloads(cuda, architecture='aarch64', required=False):
    # Only the explicitly supported, hash-enforced recipe is understood here.
    # Reject new ADD forms instead of silently omitting executable downloads.
    instructions = re.sub(r'\\\n[ \t]*', ' ', cuda).splitlines()
    additions = [line.strip() for line in instructions if re.match(r'^\s*ADD(?:\s|$)', line, re.I)]
    if not additions:
        require(not required, 'AMD64 CUDA recipe is missing its hash-locked Pillow ADD')
        return []  # Historical releases used only the immutable CUDA base.
    require(len(additions) == 1, 'Unsupported CUDA ADD recipe: expected one hash-locked Pillow wheel')
    match = re.fullmatch(r'ADD (https://files\.pythonhosted\.org/packages/[A-Za-z0-9/._-]+\.whl) /opt/sparkdev/wheels/', additions[0])
    require(match is not None, 'Unsupported CUDA ADD recipe')
    url = match[1]
    filename = urlsplit(url).path.rsplit('/', 1)[-1]
    wheel = re.fullmatch(r'pillow-([0-9]+\.[0-9]+\.[0-9]+)-cp312-cp312-(manylinux_[a-z0-9_.]+)\.whl', filename)
    require(architecture in {'aarch64', 'x86_64'}, 'Unsupported CUDA wheel architecture')
    require(wheel is not None and all(re.fullmatch(r'manylinux_[0-9]+_[0-9]+_' + architecture, tag)
                                    for tag in wheel[2].split('.')), 'Unsupported CUDA Pillow wheel')
    recipes = [re.fullmatch(r"RUN printf '%s\\n' 'pillow==([^\s']+) --hash=sha256:([0-9a-f]{64})' > /opt/sparkdev/pillow-requirements\.txt\s+&& /usr/bin/python3 -m pip install (.+)", line.strip())
               for line in instructions]
    recipes = [recipe for recipe in recipes if recipe is not None]
    expected_flags = '--no-cache-dir --disable-pip-version-check --no-index --find-links=/opt/sparkdev/wheels --require-hashes --only-binary=:all: --no-deps --force-reinstall --break-system-packages -r /opt/sparkdev/pillow-requirements.txt'
    require(len(recipes) == 1 and recipes[0][1] == wheel[1] and recipes[0][3].split() == expected_flags.split(),
            'CUDA ADD wheel lacks the supported hash-enforced installation recipe')
    digest = recipes[0][2]
    with urllib.request.urlopen(f'https://pypi.org/pypi/pillow/{wheel[1]}/json', timeout=30) as response:
        metadata = json.load(response)
    files = [item for item in metadata['urls'] if item.get('url') == url and item.get('filename') == filename
             and item.get('digests', {}).get('sha256') == digest and item.get('packagetype') == 'bdist_wheel'
             and not item.get('yanked', False)]
    require(len(files) == 1, 'Pinned CUDA wheel URL and SHA-256 cannot be verified against PyPI')
    scope = 'ARM64 CPython 3.12 wheel installed only inside the optional owner-requested Spark CUDA runtime' if architecture == 'aarch64' else 'AMD64 CPython 3.12 wheel installed only inside the optional owner-requested AMD64 CUDA runtime'
    return [{'url': url, 'sha256': digest,
             'purpose': f'Hash-locked Pillow {wheel[1]} {scope} during setup.'}]


def docker_stages(recipe, variables=()):
    """Understand only pinned OCI bases, named prior stages, and fixed ARG defaults."""
    defaults, stages, result = {}, set(), []
    for line in re.sub(r'\\\n[ \t]*', ' ', recipe).splitlines():
        line = line.strip()
        if re.match(r'ARG\s', line, re.I):
            match = re.fullmatch(r'ARG ([A-Z][A-Z0-9_]*)(?:=([^\s]+))?', line)
            if match and match[2]:
                require(match[1] not in defaults, 'Duplicate Docker base ARG')
                defaults[match[1]] = match[2]
        if not re.match(r'FROM(?:\s|$)', line, re.I):
            continue
        match = re.fullmatch(r'FROM ([^\s]+)(?: AS ([a-z][a-z0-9_-]*))?', line)
        require(match is not None, 'Unsupported Docker FROM recipe')
        base, stage = match[1], match[2]
        variable = re.fullmatch(r'\$\{([A-Z][A-Z0-9_]*)\}', base)
        if variable:
            if variable[1] in variables:
                require(variable[1] not in defaults, 'Dynamic Blender image must come from the digest-checking installer')
                base = variable[1]
            else:
                require(variable[1] in defaults, 'Docker FROM variable lacks a pinned default')
                base = defaults[variable[1]]
        require(base in variables or base in stages or re.fullmatch(r'[a-z0-9./_-]+(?::[A-Za-z0-9._-]+)?@sha256:[0-9a-f]{64}', base),
                'Docker FROM image is not pinned')
        result.append(base)
        if stage:
            require(stage not in stages, 'Duplicate Docker stage')
            stages.add(stage)
    require(result, 'Docker recipe has no FROM')
    return result


def blender_sources(commit):
    recipe_raw = source(commit, 'runtimes/Dockerfile.blender')
    recipe = recipe_raw.decode().replace('\r\n', '\n')
    bases = docker_stages(recipe)
    require(len(bases) == 2 and bases[0].startswith('nvidia/cuda:') and bases[1].startswith('ubuntu:'),
            'Unsupported Blender fallback base recipe')
    require(not re.search(r'^\s*ADD(?:\s|$)', recipe, re.M | re.I), 'Unsupported Blender ADD download')
    provenance = json.loads(source(commit, 'runtimes/blender/PROVENANCE.json'))
    pins, lock_evidence = {}, {}
    if 'COPY locked_sources.py inputs.lock.json /build-scripts/' in recipe:
        require('python3 /build-scripts/locked_sources.py sources /src/blender /opt/optix-dev' in recipe
                and 'python3 /build-scripts/locked_sources.py dependencies' in recipe
                and 'git clone ' not in recipe and 'PACKAGE_USE_UPSTREAM_SOURCES=ON' not in recipe,
                'Blender recipe bypasses the locked input helper')
        raw_lock = source(commit, 'runtimes/blender/inputs.lock.json')
        lock = json.loads(raw_lock)
        require(lock.get('v') == 1 and len(lock.get('sources', [])) == 2 and len(lock.get('dependencies', [])) == 66
                and len(lock.get('python_wheels', [])) == 9 and len(lock.get('lfs_assets', [])) == 198,
                'Unexpected Blender locked input inventory')
        for group in ['sources', 'dependencies', 'python_wheels', 'lfs_assets']:
            for row in lock[group]:
                parsed = urlsplit(row['url'])
                require(parsed.scheme == 'https' and parsed.hostname in {'codeload.github.com', 'projects.blender.org', 'files.pythonhosted.org'}
                        and not parsed.username and not parsed.password and not parsed.port and not parsed.query and not parsed.fragment
                        and re.fullmatch(r'[0-9a-f]{64}', row['sha256']) and type(row['bytes']) is int and row['bytes'] > 0,
                        'Blender download lacks a supported HTTPS SHA256/size pin')
                if group in ('dependencies', 'lfs_assets'):
                    repository = 'lib-source' if group == 'dependencies' else 'blender'
                    require(row['url'] == f'https://projects.blender.org/blender/{repository}.git/info/lfs/objects/' + row['sha256'],
                            'Blender mirror input is not content-addressed')
        for row in lock['sources']:
            name = row['name']
            require(name in ('blender', 'optix') and HEX40.fullmatch(row['commit']) and provenance.get(name + '_commit') == row['commit'],
                    'Blender archive provenance and recipe differ')
            pins[name] = {'url': row['url'], 'commit': row['commit'], 'sha256': row['sha256'], 'bytes': row['bytes']}
        require(set(pins) == {'blender', 'optix'}, 'Duplicate Blender source identity')
        lock_evidence = {'inputsLockSha256': hashlib.sha256(raw_lock).hexdigest(),
                         'inputHelperSha256': hashlib.sha256(source(commit, 'runtimes/blender/locked_sources.py')).hexdigest(),
                         'executableInputs': lock['sources'] + lock['dependencies'] + lock['python_wheels'],
                         'runtimeDataPaths': len(lock['lfs_assets']), 'runtimeDataObjects': len({r['sha256'] for r in lock['lfs_assets']}),
                         'runtimeDataInventory': f'https://github.com/{REPOSITORY}/blob/{commit}/runtimes/blender/inputs.lock.json',
                         'runtimeDataClassification': lock['lfs_classification'],
                         'runtimeDataEvidence': lock['runtime_data']}
    else:
        for name, url, directory in [('BLENDER', 'https://projects.blender.org/blender/blender.git', 'blender'),
                                     ('OPTIX', 'https://github.com/NVIDIA/optix-dev.git', '/opt/optix-dev')]:
            matches = re.findall(r'^ARG ' + name + r'_COMMIT=([0-9a-f]{40})$', recipe, re.M)
            require(len(matches) == 1 and provenance.get(name.lower() + '_commit') == matches[0], 'Blender source provenance and recipe differ')
            require(url in recipe and f'test "$(git -C {directory} rev-parse HEAD)" = "${{{name}_COMMIT}}"' in recipe,
                    'Blender source checkout lacks exact commit verification')
            pins[name.lower()] = {'url': url, 'commit': matches[0]}
    installer = source(commit, 'remote/install-cuda.py').decode()
    existing = re.findall(r'^BLENDER_EXISTING = "(sha256:[0-9a-f]{64})"$', installer, re.M)
    require(len(existing) == 1 and '"BLENDER_IMAGE=" + blender' in installer and 'image["Id"] != identity' in installer,
            'Dynamic Blender stage lacks the supported immutable image selection')
    return {'bases': bases, 'sources': pins, 'recipeSha256': hashlib.sha256(recipe_raw).hexdigest(), **lock_evidence,
            'existingLocalImage': existing[0],
            'downloadDeclarationComplete': False,
            'declarationGaps': (['The source-bound non-code data inventory has payload and Blender inspection evidence; final Marketplace draft classification/admission remains separate.',
                                 'Setup apt packages use moving signed repositories; resolved package inventory/hashes and license coverage require physical setup evidence and draft review.'] if lock_evidence else
                                ['Blender/OptiX Git source, LFS data and transitive archives need exact downloadable SHA256 inventories; setup apt resolution needs physical evidence.']),
            'sourceIntegrity': ('All source archives, Python wheels and runtime LFS data are SHA256/size-locked; dependencies use only content-addressed HTTPS Blender mirror URLs. No upstream/HTTP fallback. Apt packages still resolve at setup.' if lock_evidence else
                                'Git source commits are SHA-1 identities, not SHA-256 archive hashes. Blender dependency archives and LFS objects use upstream pinned hashes. Signed apt repository packages are resolved at setup.'),
            'scope': 'Optional Spark setup only; not bundled Windows executables. Existing local image reuse makes no network download. Otherwise build the standalone recipe.'}


def amd64_runtime_downloads(commit, evidence=None):
    paths = ['runtimes/Dockerfile.amd64', 'runtimes/Dockerfile.cuda.amd64', 'runtimes/requirements-amd64.txt']
    raw = {path: source(commit, path) for path in paths}
    cpu, cuda, lock = [raw[path].decode().replace('\r\n', '\n') for path in paths]
    cpu_bases, cuda_bases = docker_stages(cpu), docker_stages(cuda)
    require(len(cpu_bases) == 1 and cpu_bases[0].startswith('rust:'), 'Unsupported AMD64 CPU OCI base recipe')
    require(len(cuda_bases) == 1 and cuda_bases[0].startswith('nvcr.io/nvidia/pytorch@'), 'Unsupported AMD64 CUDA OCI base recipe')
    require(re.search(r'^COPY requirements-amd64\.txt /opt/sparkdev/requirements\.txt$', cpu, re.M), 'AMD64 CPU recipe must use its separate wheel lock')
    require(not re.search(r'^\s*ADD(?:\s|$)', cpu, re.I | re.M), 'Unsupported AMD64 CPU ADD recipe')
    require(not re.search(r'^\s*COPY\s+--from=', cuda, re.I | re.M), 'AMD64 CUDA must not copy an undeclared runtime stage')
    downloads = []
    for base, registry, scope in [(cpu_bases[0], 'https://registry-1.docker.io/v2/library/rust/', 'AMD64 CPU'),
                                  (cuda_bases[0], 'https://nvcr.io/v2/nvidia/pytorch/', 'optional owner-requested AMD64 CUDA')]:
        digest = base.rsplit('sha256:', 1)[1]
        downloads.append({'url': registry + 'manifests/sha256:' + digest, 'sha256': digest,
                          'purpose': f'Independent {scope} base OCI manifest; Docker verifies its digest and layer digests. Setup resolves signed repository packages, then pins the final local image ID.'})
    lines = [line.strip() for line in lock.splitlines() if line.strip() and not line.lstrip().startswith('#')]
    wheels = [re.fullmatch(r'(numpy|pillow)==([^\s]+) --hash=sha256:([0-9a-f]{64})', line) for line in lines]
    require(len(wheels) == 2 and all(wheels) and {wheel[1] for wheel in wheels} == {'numpy', 'pillow'}, 'Unsupported AMD64 CPU wheel lock')
    for name, version, digest in [wheel.groups() for wheel in wheels]:
        with urllib.request.urlopen(f'https://pypi.org/pypi/{name}/{version}/json', timeout=30) as response:
            metadata = json.load(response)
        files = [item for item in metadata['urls'] if item.get('digests', {}).get('sha256') == digest]
        require(len(files) == 1, 'Pinned AMD64 CPU wheel cannot be resolved')
        item = files[0]
        url, filename = item.get('url', ''), item.get('filename', '')
        wheel = re.fullmatch(re.escape(name + '-' + version) + r'-cp313-cp313-(manylinux_[a-z0-9_.]+)\.whl', filename)
        parsed = urlsplit(url)
        require(parsed.scheme == 'https' and parsed.hostname == 'files.pythonhosted.org' and not parsed.query and not parsed.fragment
                and not parsed.username and not parsed.password and parsed.port is None and parsed.path.rsplit('/', 1)[-1] == filename
                and item.get('packagetype') == 'bdist_wheel' and not item.get('yanked', False)
                and wheel is not None and all(re.fullmatch(r'manylinux_[0-9]+_[0-9]+_x86_64', tag) for tag in wheel[1].split('.')),
                'Pinned AMD64 CPU wheel has an unsupported URL, type or architecture')
        downloads.append({'url': url, 'sha256': digest,
                          'purpose': f'Hash-locked {name} {version} AMD64 CPython 3.13 wheel installed only inside the independent AMD64 CPU runtime during setup.'})
    downloads.extend(cuda_wheel_downloads(cuda, architecture='x86_64', required=True))
    require(len(downloads) == 5, 'Expected two independent AMD64 OCI manifests and three hash-locked AMD64 wheels')
    if evidence is not None:
        evidence['amd64'] = {'cpuBase': cpu_bases[0], 'cudaBase': cuda_bases[0],
                             'sourceSha256': {path: hashlib.sha256(raw[path]).hexdigest() for path in paths},
                             'blenderProvisioned': False}
    return downloads


def runtime_downloads(commit, evidence=None, release_version='0.0.0'):
    downloads = []
    cpu = source(commit, 'runtimes/Dockerfile').decode().replace('\r\n', '\n')
    cpu_bases = docker_stages(cpu)
    require(len(cpu_bases) == 1 and cpu_bases[0].startswith('rust:'), 'Unsupported CPU OCI base recipe')
    digest = cpu_bases[0].rsplit('sha256:', 1)[1]
    downloads.append({'url': 'https://registry-1.docker.io/v2/library/rust/manifests/sha256:' + digest,
                      'sha256': digest, 'purpose': 'CPU base OCI manifest; Docker verifies its digest and layer digests. Setup installs Debian repository packages and locked wheels, then pins the final local image ID.'})
    cuda = source(commit, 'runtimes/Dockerfile.cuda').decode().replace('\r\n', '\n')
    cuda_bases = docker_stages(cuda, ('BLENDER_IMAGE',))
    blender = len(cuda_bases) == 2 and cuda_bases[0] == 'BLENDER_IMAGE'
    require((len(cuda_bases) == 1 or blender) and cuda_bases[-1].startswith('nvcr.io/nvidia/pytorch@'), 'Unsupported CUDA OCI base recipe')
    digest = cuda_bases[-1].rsplit('sha256:', 1)[1]
    downloads.append({'url': 'https://nvcr.io/v2/nvidia/pytorch/manifests/sha256:' + digest, 'sha256': digest,
                      'purpose': 'Optional owner-requested CUDA base OCI manifest; Docker checks its digest and layer digests. No model weights are bundled in this runtime declaration. Owner-directed job code may download models into the shared cache.'})
    wheels = source(commit, 'runtimes/requirements.txt').decode().replace('\r\n', '\n')
    for name, version, digest in re.findall(r'^([A-Za-z0-9_-]+)==([^\s]+) --hash=sha256:([0-9a-f]{64})$', wheels, re.M):
        with urllib.request.urlopen(f'https://pypi.org/pypi/{name}/{version}/json', timeout=30) as response:
            metadata = json.load(response)
        files = [item for item in metadata['urls'] if item['digests']['sha256'] == digest]
        require(len(files) == 1 and urlsplit(files[0]['url']).hostname == 'files.pythonhosted.org', 'Pinned runtime wheel cannot be resolved')
        downloads.append({'url': files[0]['url'], 'sha256': digest, 'purpose': f'Hash-locked {name} {version} wheel installed only inside the Spark CPU runtime during setup.'})
    require(len(downloads) == 4, 'Expected two OCI manifests and two hash-locked CPU runtime wheels')
    downloads.extend(cuda_wheel_downloads(cuda))
    if blender:
        details = blender_sources(commit)
        for base, repository in zip(details['bases'], ['nvidia/cuda', 'library/ubuntu']):
            digest = base.rsplit('sha256:', 1)[1]
            downloads.append({'url': f'https://registry-1.docker.io/v2/{repository}/manifests/sha256:{digest}', 'sha256': digest,
                              'purpose': 'Optional standalone Blender source-build OCI base. Docker verifies manifest/layer digests; Blender/OptiX source commits and dependency hashes are disclosed separately. Signed apt package versions resolve during setup.'})
        for row in details.get('executableInputs', []):
            downloads.append({'url': row['url'], 'sha256': row['sha256'],
                              'purpose': f"Optional standalone Blender build input: {row['filename']}. SHA256 and size are checked before use; no HTTP or upstream fallback. Source and wheel licenses remain separate from DGXBridge's MIT license."})
        require(len(downloads) <= 128, 'Runtime inputs exceed the Marketplace declaration limit')
        if evidence is not None:
            evidence['blender'] = details
    if tuple(map(int, release_version.split('-', 1)[0].split('.'))) >= (0, 6, 0):
        downloads.extend(amd64_runtime_downloads(commit, evidence))
    require(len(downloads) <= 128, 'Runtime inputs exceed the Marketplace declaration limit')
    return downloads


WINDOWS_PAYLOAD = {'gui': ('dgxbridge.exe', 'dgxbridge-windows-gui.sbom.json'),
                   'cli': ('sparkdev.exe', 'sparkdev-windows-cli.sbom.json'),
                   'link': ('sparkdev-link.exe', 'sparkdev-windows-link.sbom.json'),
                   'return': ('sparkdev-return.exe', 'sparkdev-windows-return.sbom.json')}


def nsis_records(data):
    """Read bounded solid-LZMA NSIS records; never run or unpack an installer."""
    offsets = [n for n in range(0, len(data) - 28, 512) if data[n + 4:n + 20] == b'\xef\xbe\xad\xdeNullsoftInst']
    require(len(offsets) == 1, 'Expected one NSIS archive header')
    offset = offsets[0]
    flags, header_size, total = struct.unpack_from('<I16xII', data, offset)
    require(flags == 0 and 33 <= total <= len(data) - offset, 'Unsupported or truncated NSIS archive')
    stream = data[offset + 28:offset + total]
    decoder = lzma.LZMADecompressor(lzma.FORMAT_ALONE, memlimit=128 * 1024**2)
    try:
        output = decoder.decompress(stream[:5] + b'\xff' * 8 + stream[5:], max_length=512 * 1024**2 + 1)
    except lzma.LZMAError as error:
        raise ValueError('Invalid NSIS solid-LZMA stream') from error
    require(len(output) <= 512 * 1024**2 and len(output) >= header_size + 4, 'NSIS data exceeds limits or lacks its header')
    records, position = {}, 0
    while position + 4 <= len(output):
        length = struct.unpack_from('<I', output, position)[0]
        require(length < 0x80000000 and position + 4 + length <= len(output), 'Unsupported or truncated NSIS file record')
        if position == 0:
            require(length == header_size, 'NSIS header record length differs')
        else:
            raw = output[position + 4:position + 4 + length]
            records[hashlib.sha256(raw).hexdigest()] = length
        position += 4 + length
    require(position == len(output), 'NSIS record stream has trailing partial bytes')
    return records


def windows_payload_evidence(installer, evidence_file):
    require(evidence_file is not None, '0.5+ requires --windows-evidence with four exact binary/Syft SBOM pairs')
    evidence_file = Path(evidence_file).resolve()
    evidence = json.loads(evidence_file.read_text(encoding='utf-8'))
    object_keys(evidence, WINDOWS_PAYLOAD, 'Windows evidence')
    records = nsis_records(installer.read_bytes())
    checked = {}
    for name, (filename, sbom_filename) in WINDOWS_PAYLOAD.items():
        item = evidence[name]
        object_keys(item, ['binary', 'sbom'], 'Windows ' + name + ' evidence')
        binary = (evidence_file.parent / item['binary']).resolve()
        sbom_file = (evidence_file.parent / item['sbom']).resolve()
        require(binary.name == filename and sbom_file.name == sbom_filename, 'Use canonical Windows binary and artifact SBOM names')
        require(0 < binary.stat().st_size <= 128 * 1024**2, 'Windows binary exceeds verification limit')
        raw = binary.read_bytes()
        digest = hashlib.sha256(raw).hexdigest()
        require(raw.startswith(b'MZ') and records.get(digest) == len(raw), 'NSIS does not contain the exact ' + filename)
        if name == 'gui':
            require(raw.count(b'__TAURI_BUNDLE_TYPE_VAR_NSS') == 1 and b'__TAURI_BUNDLE_TYPE_VAR_UNK' not in raw,
                    'GUI evidence must describe the shipped NSS-marked executable')
        sbom_raw = sbom_file.read_bytes()
        sbom = json.loads(sbom_raw)
        origin = sbom.get('source', {})
        digests = origin.get('metadata', {}).get('digests', [])
        require(sbom.get('descriptor', {}).get('name') == 'syft' and origin.get('type') == 'file'
                and origin.get('name') == filename and origin.get('version') == 'sha256:' + digest
                and {'algorithm': 'sha256', 'value': digest} in digests,
                'Artifact SBOM does not identify the exact ' + filename)
        packages = [p for p in sbom.get('artifacts', []) if p.get('foundBy') == 'cargo-auditable-binary-cataloger' and p.get('type') == 'rust-crate']
        require(packages, 'Artifact SBOM lacks Cargo-auditable components: ' + filename)
        checked[name] = {'filename': filename, 'sha256': digest, 'bytes': len(raw), 'sbom': sbom_filename,
                         'sbomSha256': hashlib.sha256(sbom_raw).hexdigest(), 'auditableComponents': len(packages), 'insideInstaller': True}
    return {'installerSha256': hashlib.sha256(installer.read_bytes()).hexdigest(), 'method': 'Read-only NSIS solid-LZMA file records', 'binaries': checked}


def manifest(value, downloads, runtime_evidence=None):
    commit = value['sourceCommit']
    repo_url = 'https://github.com/' + REPOSITORY
    document = {
        'schemaVersion': 1, 'appId': 'dgxbridge', 'publisherId': value['publisherId'],
        'name': 'DGXBridge', 'slug': 'dgxbridge',
        'repository': {'id': REPOSITORY_ID, 'url': repo_url},
        'release': {'id': value['releaseId'], 'tag': value['tag'], 'version': value['version'], 'channel': 'beta' if '-' in value['version'] else 'stable', 'sourceCommit': commit},
        'summary': 'Run agent-directed builds and tests on your own NVIDIA DGX Spark, with a Windows desktop job board and CLI.',
        'description': 'DGXBridge connects a Windows laptop to an owner-controlled NVIDIA DGX Spark. It provides a shared job board, resumable project transfer, durable job IDs, retained output, cancellation, per-project build caches, shared model downloads, persistent model servers and a stdio MCP interface for coding agents. Windows work can run as native owner processes; Linux work runs in restricted Spark containers. Native laptop execution is not sandboxed.\n\nFirst setup requires an existing authenticated SSH configuration with a verified host key, Windows OpenSSH and preinstalled Microsoft Edge WebView2. Spark requires ARM64 Linux, Python 3, a working user Docker setup, systemd user services and sufficient disk/memory headroom. Setup installs independent bridge services. Phone QR approval is a separate owner action on the same LAN; optional encrypted public n0 relays can then provide remote connectivity. No owner VM is configured or assumed.\n\nRuntime setup downloads a digest-pinned OCI base, installs Debian repository packages and hash-locked Python wheels, then locks jobs to the resulting local image ID. Debian package versions are resolved at setup rather than pinned individually in this release. Optional CUDA provisioning is separate. Dependency preparation downloads locked project packages through a fixed domain allowlist. Normal Spark jobs have public HTTP/HTTPS internet access on by default through an isolated proxy that blocks home/private networks, the Spark host and cloud metadata; the owner can turn it off. Direct sockets and unproxied external DNS are not supported. CONNECT payloads are opaque and may carry other protocols to allowed public destinations. Jobs can send data to public destinations chosen by their code. Model downloads share one owner-managed writable cache across projects and agents, with idle cleanup and deferred owner clearing. Model servers have no published ports; authenticated calls use durable bounded files and replies expire. The bridge bundles no AI model or provider API credential. Consult the release receipts for the exact tested workflows and remaining phone/hotspot/native acceptance limits.\n\nWindows OpenSSH Client must be installed separately for Spark connections. Minimum Windows version and memory are not specified when their compatibility fields use the Marketplace unknown values; this does not assert compatibility with every system. The release receipts record the observed test host separately.',
        'category': 'automation', 'sourceVisibility': 'public', 'license': 'MIT',
        'featureNotices': [
            {'name': 'Codex Windows sandbox', 'status': 'beta', 'description': 'Actual Claude Code and Codex MCP jobs ran concurrently on the physical Spark and both succeeded. The separate Codex restricted Windows shell check failed on the tested laptop with missing writable-root capability SIDs. Restricted shell execution is not accepted by this beta receipt; no global security setting was changed.'},
            {'name': 'Remote and disconnect acceptance', 'status': 'experimental', 'description': 'LAN SSH and phone-approved pairing were exercised. Owner-VM or phone-hotspot execution, physical network-drop tests and laptop sleep during a run remain unverified. Controller restart during a Spark job was tested separately.'},
            {'name': 'Laptop scheduler and runtime provisioning', 'status': 'beta', 'description': 'New managed laptop jobs require a reachable Spark scheduler. Native laptop commands use owner permissions and have no result cache. Spark runtime bases and wheels are pinned; Debian packages are resolved during setup before the resulting image ID is locked. Runtime provisioning is outside the Marketplace installer/source scan.'}
        ],
        'distributionAuthorization': {'freeUse': True, 'marketplaceDistribution': True},
        'free': {'price': 0, 'trial': False, 'paidUnlocks': False, 'mandatoryDeveloperCredits': False},
        'intelligence': {'capabilities': [
            'Submit agent-directed Linux builds and tests to an owner-controlled NVIDIA DGX Spark.',
            'Coordinate native Windows and isolated Spark jobs through a CLI, desktop job board and stdio MCP.',
            'Retain job IDs, exit codes, logs and per-project caches for reconnecting coding agents.'
        ], 'functionalTest': 'After installing the client and independent Spark services through pinned SSH, use a throwaway Rust project with Cargo.lock. Run sparkdev --client codex --json prepare PROJECT --runtime rust, poll its returned job ID to success, then run sparkdev --client codex --json run PROJECT --where spark --runtime rust --detach --request-id marketplace-check -- cargo test --locked. Poll sparkdev --json wait JOB --timeout 20 and sparkdev --json logs JOB --since 0. Verify the expected test output and exit_code 0, and confirm the same job appears on the desktop board. Repeat a request with the same request ID to check that a second job is not created. Phone approval and hotspot operation require separate owner acceptance.'},
        'execution': 'hybrid', 'offline': False,
        'integrations': [
            {'name': 'Owner-controlled NVIDIA DGX Spark', 'required': True, 'purpose': 'Runs Linux jobs and the shared scheduler using Docker and separate user services.', 'costDisclosure': 'Hardware, electricity, storage and network access are supplied by the owner; the bridge does not provide hardware or hosted compute.'},
            {'name': 'OpenSSH and Microsoft Edge WebView2', 'required': True, 'purpose': 'Pinned SSH bootstraps the Spark; preinstalled WebView2 displays the Windows app.', 'costDisclosure': 'Install prerequisites separately. This installer does not download WebView2 or request administrator elevation.'},
            {'name': 'ipify public IPv4 observation', 'required': False, 'purpose': 'Each public-internet helper checks api.ipify.org at startup and every 30 seconds to block its observed public egress address. Online access fails closed when the check is unavailable or stale; offline jobs do not require it.', 'costDisclosure': 'A third-party service with its own availability and terms sees the helper public IP and ordinary connection metadata. The request sends no project contents or credentials.'},
            {'name': 'Public n0 iroh relays', 'required': False, 'purpose': 'Owner-enabled encrypted remote device connectivity after pairing; no owner VM is included.', 'costDisclosure': 'Optional third-party network infrastructure has its own availability and terms; relay operators can observe connection metadata.'},
            {'name': 'Claude Code, Codex or another CLI client', 'required': False, 'purpose': 'Calls the common CLI or stdio MCP interface; the bridge itself does not invoke a model API.', 'costDisclosure': 'Agent accounts, subscriptions and any model API charges remain separate from this free utility.'},
            {'name': 'Container and package registries', 'required': True, 'purpose': 'Initial runtime provisioning and locked dependency preparation use fixed hosts. Job-directed public HTTP/HTTPS requests can upload and download data at other destinations.', 'costDisclosure': 'Transfers use owner bandwidth and disk. Existing prepared jobs can use LAN SSH without an internet route, but the full product requires a reachable Spark.'}
        ],
        'privacy': {
            'dataLeavesDevice': 'Selected project files, job arguments and model-server call bodies are sent to the owner-selected Spark; output and model replies return to the laptop. Spark job internet is enabled by default and the owner can disable it. Beyond the fixed provisioning and relay hostnames listed in permissions.domains, job code may send data to arbitrary public HTTP/HTTPS destinations it chooses. Those destinations cannot be exhaustively listed in advance; permissions.domains is not the job proxy allowlist. The proxy blocks private addresses, observed Spark host/on-link networks and cloud metadata. Each public helper checks its observed public IPv4 using fixed credential-free HTTPS requests to api.ipify.org at startup and every 30 seconds, and also blocks that IP. Observations expire after 60 seconds; failed or stale checks cut public internet. The service sees the helper public IP and connection metadata, not project contents or job arguments. Additional router public aliases and addresses used by other outbound routes are not discovered. These controls do not prevent a job from uploading data to public services. Optional relays see connection metadata while device payloads remain encrypted. No provider API credential is bundled.',
            'retention': 'Source snapshots, project build caches, audit events, logs, laptop-saved replies and receipts persist until removed by the owner. Shared downloads are cleaned only when idle, with recent-use protection and deferred owner clearing. Spark model-call payloads/replies become eligible for cleanup at the earlier of creation plus one hour or completed delivery plus ten minutes. Call identity metadata becomes eligible 24 hours after creation. Deletion requires successful controller cleanup; delays or errors can extend retention. Pending cancellations persist. Uninstall preserves pairing, Spark services and retained data.',
            'telemetry': 'No application telemetry or embedded model calls. SSH key contents stay with OpenSSH. Windows device keys use user-scoped DPAPI; QR tokens and approval codes are transient.',
            'policyUrl': repo_url + '/blob/' + commit + '/docs/PRIVACY.md'},
        'permissions': {
            'filesystem': ['Read the selected project tree and write application configuration, logs, receipts and pending cancellations.', 'Native laptop jobs execute with the owner account and can access files that account permits; they are not a sandbox.', 'Write bridge state, source snapshots, per-project build caches, shared model/download caches and model-call spools on the selected Spark; install its user service units.', 'Shared downloads form one writable owner trust domain across projects and agents.'],
            'credentials': ['Use the owner-selected OpenSSH key/config and pinned known-host entry without copying private key contents.', 'Generate and retain a separate paired-device identity protected with Windows DPAPI; hold approval secrets only during pairing.'],
            'domains': ['deloresai.com', 'api.ipify.org', 'registry-1.docker.io', 'auth.docker.io', 'nvcr.io', 'deb.debian.org', 'security.debian.org', 'pypi.org', 'files.pythonhosted.org', 'crates.io', 'static.crates.io', 'index.crates.io', 'registry.npmjs.org', 'release-assets.githubusercontent.com', 'objects.githubusercontent.com', 'github-releases.githubusercontent.com', 'use1-1.relay.n0.iroh.link', 'usw1-1.relay.n0.iroh.link', 'euc1-1.relay.n0.iroh.link', 'aps1-1.relay.n0.iroh.link'],
            'elevation': False, 'backgroundServices': True},
        'models': [], 'executableDownloads': downloads,
        'updater': {'mode': 'coordinated', 'coordination': 'The drawer updater discovers accepted stable Windows artifacts through DeloresAI Marketplace. It requires explicit browser approval with S256 proof, verifies acquisition and automated-review signatures, checks exact artifact identity, SHA-256 and size, and revalidates immediately before interactive current-user NSIS setup. No silent installation or GitHub fallback. Existing bridge configuration, pairing and job state are preserved. Owner-triggered SSH setup separately updates independent Spark services.'},
        'rollback': {'dataCompatible': False, 'backupRequired': True, 'notes': 'This preview does not promise downgrade compatibility. Back up bridge-owned configuration, pairing state and job data before replacing an installed version. Uninstall preserves those records and remote services; never remove another application\'s data.'},
        'artwork': {'iconUrl': value['artwork']['icon']['url'], 'thumbnailUrl': value['artwork']['thumbnail']['url']},
        'screenshots': [{'url': row['url'], 'alt': row['alt'].strip()} for row in value['screenshots']],
        'artifacts': [{'os': 'windows', 'architectures': ['x64'], 'format': 'nsis', 'assetId': value['assetId'], 'filename': value['installer']['filename'], 'installationAdapter': 'windows-nsis', 'preferred': True,
                       'requirements': {**value['requirements'], 'gpu': None, 'runtimes': ['Microsoft Edge WebView2'], 'distributions': []}}]
    }
    if tuple(map(int, value['version'].split('-', 1)[0].split('.'))) >= (0, 5, 0):
        document['description'] += '\n\nCompleted Spark job outputs and eligible project changes return automatically to the originating laptop by default. A separate per-user sparkdev-return.exe background worker resumes durable delivery checkpoints without keeping the desktop window open. Setup registers the worker to start at Windows sign-in. Files from /work return to their original project paths; /sparkdev-output files land in the per-job results folder under local bridge state. Returned bytes are hash-verified; concurrent local edits are preserved as conflicts, and paths, secrets and size limits are checked. Delivery failures remain visible and retryable. Delivery completion is separate from workload success; consult release receipts for physical interruption tests. During upgrades, setup stops and restarts the result worker while preserving configuration, pairing, jobs and pending returns. Uninstall removes its sign-in registration and preserves retained state.\n\nProjects can prepare Debian/Ubuntu system packages in a separate derived Spark image using a signed repository index and a hash-locked package list. Preparation uses independent temporary resources; ordinary jobs remain nonroot and cannot alter the Spark host. Repository package versions resolved at setup and prepared project packages are outside the Windows installer scan. Optional CUDA setup includes FFmpeg and Blender tools. NVENC/NVDEC capability is reported from actual cached encode/decode probes tied to the active image, not from encoder names. Blender CUDA/OptiX/CPU render and OIDN acceptance must be read from the separate physical receipts; their presence in a recipe is not proof of GPU operation.'
        document['intelligence']['capabilities'].append('Automatically return verified Spark outputs and eligible project changes, preserving concurrent local edits and resumable delivery state.')
        document['permissions']['filesystem'] += [
            'Write verified returned outputs and eligible project changes to the original local project; preserve conflicting local edits and retain delivery checkpoints.',
            'Install a per-user background result worker and Windows sign-in entry; updates preserve pairing, job state and pending returns.',
            'Prepare hash-locked Debian/Ubuntu packages in separate Spark images using signed repository metadata; do not install them on the host.']
        document['permissions']['domains'] += ['ports.ubuntu.com', 'archive.ubuntu.com', 'security.ubuntu.com', 'projects.blender.org', 'github.com', 'codeload.github.com']
        document['privacy']['retention'] += ' Automatic-return manifests, verified chunks, conflict copies and delivery records persist until removed by the owner; updates preserve pending delivery.'
        document['featureNotices'].append({'name': 'Automatic file return and GPU tools', 'status': 'beta', 'description': 'The local background worker retries unfinished delivery after reconnect or restart. Workload exit status and file delivery status are separate. GPU video is reported only from cached actual probes bound to the active image; Blender render and GPU denoising acceptance require physical receipts. No phone-hotspot, sleep or hardware success is inferred from source tests.'})
    if tuple(map(int, value['version'].split('-', 1)[0].split('.'))) >= (0, 5, 1):
        document['description'] += '\n\nEligible large LAN transfers use SCP upload packs and SFTP reget for resumable automatic returns through the same pinned OpenSSH host and key checks. Returned files retain hash, path, secret and local-conflict checks; return copies honor disk-reserve and worker time budgets. Paired routes retain RPC transfers. The large-snapshot builder reports bounded health diagnostics. These implementation disclosures do not establish throughput, interruption recovery, installed behavior or network-path acceptance; consult the separate release receipts.'
        document['privacy']['retention'] = document['privacy']['retention'].replace(
            'Source snapshots,', 'Completed source snapshots, content-addressed chunks,', 1)
        document['privacy']['retention'] += ' Queued snapshot records and partial trees are eligible for pruning once at least one hour old when a newer pending record exists for the same project, or once at least 24 hours old. Records locked for active materialization are skipped. This pruning preserves completed snapshots and content-addressed chunks. Spark incoming upload packs and partial packs are ephemeral: opened packs are removed after validation is attempted; unclaimed packs become eligible for cleanup after one hour since last modification. Cleanup delays or errors can extend retention. Materializer health is a bounded record of the latest builder PID, start/finish times, exit code and error kind; builder stdout and stderr are discarded.'
        document['permissions']['filesystem'] += [
            'Write temporary upload packs and bounded materializer health on the Spark; the builder discards stdout and stderr.',
            'Prune stale queued Spark snapshots and their partial trees under age and lock checks; preserve completed snapshots and content-addressed chunks.']
    if tuple(map(int, value['version'].split('-', 1)[0].split('.'))) >= (0, 6, 0):
        document['summary'] = 'Run agent-directed work on your own Linux or Windows/WSL computers and NVIDIA DGX Spark, with one Windows job board and CLI.'
        _, setup = document['description'].split('\n\n', 1)
        setup = setup.replace(
            'Spark requires ARM64 Linux, Python 3, a working user Docker setup, systemd user services and sufficient disk/memory headroom.',
            'Each remote controller requires supported ARM64 or AMD64 Linux, Python 3, working Docker, systemd user services and sufficient disk/memory headroom. Windows SSH targets need an existing WSL2 distribution and Linux user.')
        setup = setup.replace(
            'Phone QR approval is a separate owner action on the same LAN; optional encrypted public n0 relays can then provide remote connectivity.',
            'Existing Spark phone approval is a separate owner action on the same LAN; optional encrypted public n0 relays can then provide Spark connectivity. Additional computers use independently pinned SSH, with LAN SSH tested and no new-node relay pairing. Their off-LAN availability is unverified.')
        for old, new in [('Normal Spark jobs', 'Normal remote jobs'), ('the Spark host', 'the selected Linux host'),
                         ('Spark connections', 'remote connections'), ('Completed Spark job', 'Completed remote job'),
                         ('derived Spark image', 'derived remote image'), ('Spark incoming', 'Remote incoming')]:
            setup = setup.replace(old, new)
        setup = setup.replace('Optional CUDA setup includes FFmpeg and Blender tools.',
                              'Optional CUDA setup includes FFmpeg; only the ARM64 recipe provisions Blender. AMD64 Blender is not provisioned.')
        document['description'] = ('DGXBridge connects a Windows laptop to owner-controlled Linux compute computers, including NVIDIA DGX Spark and Windows computers with an existing WSL2 Linux environment. It provides a shared job board, resumable project transfer, immutable request and job IDs, automatic file return, cancellation, per-project caches, shared downloads, model servers and stdio MCP for coding agents. Windows work can run as native owner processes; remote Linux work runs in restricted containers on its selected computer. Native laptop execution is not sandboxed.\n\n' + setup +
            '\n\nNamed computers retain separate connection profiles, controller state, runtimes, policies and return workers. Saving or inspecting a computer does not install software; explicit setup uses the packaged hash-checked bundle. Spark is optional for named remote Linux jobs. Managed native Windows laptop jobs still require the original reachable Spark scheduler; a named controller does not replace that admission dependency. Existing Spark pairing and state are preserved. Each controller enforces its own admission and resource policy.\n\nAutomatic placement considers authenticated runtime and available budget observations, including client quotas and explicit memory requirements. Linux system RAM, discrete GPU VRAM and Windows installed RAM are separate observations, not reservations. Docker system-memory caps do not enforce GPU VRAM. Separate jobs may run concurrently on separate computers; each job uses one computer. Distributed model training and cross-host GPU memory pooling are not implemented. The fleet journal retains request/source/target identities before submission and preserves uncertain submissions and worktree claims until verified completion and automatic delivery. Local responses above 40 MiB report incomplete observation, not an offline computer; saved receipts are retained. Future logon/reboot recovery, hotspot and sleep acceptance remain separate from source tests and LAN job receipts.')
        document['intelligence']['capabilities'] = [
            'Run Linux jobs on owner-selected ARM64 or AMD64 computers, including Windows/WSL targets, through independently pinned SSH.',
            'Select a compatible computer from authenticated runtime and resource observations; retain immutable request and device identity across retries.',
            'Run independent jobs concurrently across computers and return verified files automatically, preserving newer local edits.',
            'Preserve the existing Spark/laptop job board, Spark pairing and policies; native Windows jobs retain their original Spark admission dependency.']
        document['intelligence']['functionalTest'] = ('Install the Windows client. Register an independently verified SSH profile named test-node for an ARM64/AMD64 Linux controller, or an existing Windows/WSL2 environment. Explicitly bootstrap that profile using the installed validated bundle and wait for its CPU runtime and scheduler to become ready. With a throwaway project and locked dependencies, run sparkdev --client codex --json --device test-node prepare PROJECT --runtime rust and poll its qualified job ID. Then run sparkdev --client codex --json --device test-node run PROJECT --where auto --runtime rust --detach --request-id marketplace-check -- cargo test --locked. Retain test-node:JOB_ID; poll wait JOB_ID --timeout 20 and consume logs JOB_ID --since OFFSET through their returned next offset. Require terminal workload exit_code 0 and completed automatic file return. Verify local output hashes and the qualified job on the desktop board. Retry the same original source/arguments/request ID and require the same job; moving that ID to another device must fail. Spark pairing, GPU tools, multiple-computer overlap, installed startup and hotspot operation each require separate acceptance.')
        document['featureNotices'][0]['description'] = 'Historical release evidence: Claude Code and Codex MCP jobs ran concurrently on the physical Spark. The prior restricted Windows shell check failed with missing writable-root capability SIDs; no global security setting changed. No new 0.6 Claude Code or restricted-shell acceptance is claimed.'
        document['integrations'][0]['required'] = False
        document['integrations'][0]['purpose'] = 'Optional ARM64 compute computer and existing phone-paired route. Its reachable scheduler remains required for managed native Windows laptop jobs.'
        document['integrations'].insert(1, {
            'name': 'Owner-selected Linux or Windows/WSL compute computer', 'required': True,
            'purpose': 'At least one remote compute controller is required for remote jobs; Spark can supply it. Additional ARM64/AMD64 computers use pinned SSH, Docker and independent user services; Windows targets need WSL2.',
            'costDisclosure': 'The owner supplies hardware, storage, memory, network, Docker and any Windows/WSL prerequisites. This utility supplies no hosted compute. No GPU or system memory is pooled between computers.'})
        for integration in document['integrations']:
            if integration['name'] == 'OpenSSH and Microsoft Edge WebView2':
                integration['purpose'] = 'Pinned SSH connects to each selected remote computer; preinstalled WebView2 displays the Windows app.'
            if integration['name'] == 'Container and package registries':
                integration['costDisclosure'] = 'Transfers use owner bandwidth and disk. Prepared remote jobs can use LAN SSH without public internet; each needs its selected controller. Native Windows jobs retain the original Spark admission dependency.'
        document['privacy']['dataLeavesDevice'] = document['privacy']['dataLeavesDevice'].replace('owner-selected Spark', 'owner-selected compute computer').replace('Spark job internet', 'Remote job internet').replace('observed Spark host/on-link', 'observed remote host/on-link')
        document['privacy']['retention'] = ('Completed source snapshots, content-addressed chunks, project caches, audit events, logs, laptop replies, automatic-return manifests/chunks, conflict copies and receipts persist until owner removal. Updates preserve pairing and pending delivery. Shared downloads are cleaned only when idle, with recent-use protection and deferred owner clearing. Remote model-call payloads/replies become eligible for cleanup at the earlier of creation plus one hour or completed delivery plus ten minutes; call identity metadata becomes eligible 24 hours after creation. Actual deletion requires successful controller housekeeping; delays or errors can extend retention. Pending cancellations persist. Uninstall preserves pairing, remote services and retained data. Queued snapshot records and partial trees become eligible for pruning after one hour when a newer pending record exists for that project, or after 24 hours; active locked records are skipped and completed snapshots/chunks are preserved. Incoming upload packs are removed after validation is attempted; unclaimed packs become eligible after one hour since modification. Materializer health retains only the latest bounded PID/timing/exit/error-kind record; its stdout/stderr are discarded. Named profiles, fleet request/source/SSH identity references, worktree claims and per-computer delivery checkpoints persist in owner-local state. Uncertain submissions and pending delivery retain their claims.')
        document['permissions']['filesystem'] = [entry.replace('selected Spark', 'selected remote computer').replace('Spark images', 'remote images').replace('on the Spark', 'on the selected computer').replace('Spark snapshots', 'remote snapshots') for entry in document['permissions']['filesystem']]
        document['permissions']['filesystem'].append('Store independent computer profiles and immutable fleet request/target identities, claims and per-profile automatic-return checkpoints in owner-local state.')
        document['updater']['coordination'] = document['updater']['coordination'].replace('independent Spark services', 'independent services on the explicitly selected computer')
        document['featureNotices'].append({
            'name': 'Multiple compute computers', 'status': 'beta',
            'description': 'Named Linux and Windows/WSL controllers have independent state and pinned SSH. Separate jobs can overlap across computers; each job uses one computer. LAN CPU, RTX 4090 CUDA and file-return evidence is separate from installed startup and off-LAN acceptance. AMD64 Blender, distributed training and cross-host GPU memory pooling are not implemented.'})
    blender = (runtime_evidence or {}).get('blender')
    if blender:
        pins = blender['sources']
        document['description'] += ('\n\nThe standalone Blender fallback builds upstream Blender at ' + pins['blender']['commit'] +
            ' and NVIDIA OptiX headers at ' + pins['optix']['commit'] + '. These are verified Git commit identities, not archive SHA-256 values. ' +
            ('Its OCI base digests, two source archives, 66 dependency archives and nine Python wheels are declared with SHA256. The fallback uses only content-addressed HTTPS mirror objects, verifies all hashes and sizes including cache hits, and installs those Python wheels offline. A separate source-bound inventory covers 198 paths backed by 187 data objects: geometry icons, images, a bounded single-stream font and 18 Blender assets inspected with script autoexecution disabled. Nine embedded README Text blocks are prose; no drivers or OSL script nodes were observed. Built-in scene/node behavior remains available. Inventory SHA256: ' + blender['inputsLockSha256'] + '. Inventory: ' + blender['runtimeDataInventory'] + '. ' if blender.get('inputsLockSha256') else
             'Its two OCI base digests are in executableDownloads. Blender dependency archives and Git LFS data are checked against upstream hashes; source archives may use the Blender mirror or their upstream URLs. ') +
            'The source build can take substantial time and disk. If the exact already-present owner image ' + blender['existingLocalImage'] +
            ' is available, setup may reuse it read-only; no PhaseForge installation is required. The source pins, image-selection policy and remaining transitive download-declaration gaps are recorded in the manifest validation receipt; direct declarations do not claim a complete fallback build inventory. '
            'Blender and its dependencies have their own upstream licenses; the MIT declaration applies to DGXBridge first-party code. The Windows Cargo notice collection does not represent a complete optional Spark runtime license inventory.')
    release_disclosures(document, value['version'])
    return document


def release_disclosures(document, version):
    """Keep historical manifests stable while disclosing 0.6.2 owner enrollment."""
    if tuple(map(int, version.split('-', 1)[0].split('.'))) < (0, 6, 2):
        return
    document['name'] = 'RTXBridge'
    document['privacy']['telemetry'] = ('No application telemetry or embedded model calls. SSH private keys remain in owner-local OpenSSH files protected by filesystem permissions. The Windows paired-device identity uses user-scoped DPAPI; generated SSH key files are not DPAPI-encrypted by the bridge. QR tokens and approval codes are transient.')
    document['permissions'].setdefault('credentials', []).extend([
        'For optional owner enrollment, pass the password via local stdin and the SSH helper environment; never save it in configuration, argv or logs.',
        'Retain generated per-computer OpenSSH private keys with owner-local file permissions, public keys, confirmed host pins and SSH profiles until removed.'
    ])
    document['description'] = document['description'].replace('DGXBridge connects', 'RTXBridge connects', 1)
    document['description'] += ('\n\nRTXBridge was named DGXBridge. The application identity, Marketplace slug, state folders, CLI paths and canonical DGXBridge_VERSION_x64-setup.exe release asset name remain compatible with existing installations. Setup upgrades a legacy installation in place and replaces only its owned legacy registration and shortcuts. The Devices view uses a bundled, locally rendered three.js network map; it does not fetch a remote map service.\n\nOptional password enrollment is an explicit owner-only action. The owner first compares the scanned ED25519 fingerprint with an independently obtained identity for the selected computer. A fresh scan must match that confirmation before any password-bearing SSH connection begins. The password reaches OpenSSH through local standard input and the helper child-process environment; it is not written to configuration, command arguments or bridge logs. Enrollment adds a dedicated public SSH key to that selected account, preserves existing authorized keys, verifies key-only login and saves the independently confirmed pin. Agents cannot invoke enrollment through MCP; existing pinned manual SSH setup remains available.')
    document['privacy']['retention'] += (' Enrollment retains generated per-computer SSH key pairs, confirmed known_hosts pins and SSH profiles in owner-local state until explicitly removed. The password is used in local process memory and the SSH helper environment during sign-in, not stored as a saved bridge credential.')
    document['permissions']['filesystem'].append('On owner enrollment, retain a dedicated SSH key pair and host pin locally; add only its public key to the selected remote account, preserve existing keys and save the verified profile.')
    document['featureNotices'].append({'name': 'RTXBridge Devices and owner enrollment', 'status': 'beta', 'description': 'The unified Devices map, independently confirmed password-to-key enrollment and legacy-name installer migration have separate source, isolated enrollment, installed and network receipts. A fresh password sign-in is not inferred from a preexisting SSH-key job.'})


def runtime_validate(document, checkout):
    contracts = checkout / 'packages/contracts/src'
    require((contracts / 'index.ts').is_file() and (contracts / 'artwork.ts').is_file(), 'Marketplace contract checkout is required')
    loader = checkout / 'node_modules/tsx/dist/loader.mjs'
    require(loader.is_file(), 'The existing Marketplace checkout needs its installed tsx dependency; this script does not install it')
    policy = checkout / 'packages/policy/src/evidence.ts'
    require(policy.is_file(), 'Marketplace parsed canonical digest implementation is required')
    code = f"import {{readFileSync}} from 'node:fs';import {{manifestSchema}} from {json.dumps((contracts/'index.ts').as_uri())};import {{publicationArtworkIssues}} from {json.dumps((contracts/'artwork.ts').as_uri())};import {{digestJson}} from {json.dumps(policy.as_uri())};const p=manifestSchema.safeParse(JSON.parse(readFileSync(process.argv[1],'utf8')));const issues=p.success?publicationArtworkIssues(p.data):p.error.issues;console.log(JSON.stringify({{ok:p.success&&!issues.length,issues,parsedCanonicalSha256:p.success?digestJson(p.data):null}}));process.exit(p.success&&!issues.length?0:1);"
    (ROOT / '.local').mkdir(exist_ok=True)
    with tempfile.NamedTemporaryFile(mode='w', suffix='.json', prefix='manifest-validation-', dir=ROOT / '.local', delete=False, encoding='utf-8') as file:
        temporary = Path(file.name)
        json.dump(document, file)
    try:
        result = subprocess.run(['node', '--import', loader.as_uri(), '--input-type=module', '--eval', code, str(temporary)], capture_output=True, timeout=30)
        require(result.returncode == 0, 'Marketplace runtime validation failed: ' + result.stdout.decode(errors='replace')[:6000])
        validation = json.loads(result.stdout)
        require(re.fullmatch(r'[0-9a-f]{64}', validation.get('parsedCanonicalSha256', '')), 'Marketplace canonical digest is missing')
    finally:
        temporary.unlink(missing_ok=True)
    revision = subprocess.check_output(['git', '-C', str(checkout), 'rev-parse', 'HEAD'], timeout=10).decode().strip()
    return {'contractCommit': revision, 'manifestSchema': True, 'publicationArtworkIssues': [],
            'parsedCanonicalSha256': validation['parsedCanonicalSha256'],
            'contractSha256': {**{name: hashlib.sha256((contracts / name).read_bytes()).hexdigest() for name in ['index.ts', 'artwork.ts']},
                               'packages/policy/src/evidence.ts': hashlib.sha256(policy.read_bytes()).hexdigest()}}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--input', required=True, type=Path)
    parser.add_argument('--marketplace-checkout', required=True, type=Path)
    parser.add_argument('--output', required=True, type=Path)
    parser.add_argument('--require-published', action='store_true')
    parser.add_argument('--windows-evidence', type=Path, help='JSON mapping gui/cli/link/return to canonical binary and Syft SBOM paths; required for 0.5+')
    args = parser.parse_args()
    value = json.loads(args.input.read_text(encoding='utf-8'))
    validate_input(value)
    input_directory = args.input.resolve().parent
    identity = verify_release(value, input_directory, args.require_published)
    windows = None
    if tuple(map(int, value['version'].split('-', 1)[0].split('.'))) >= (0, 5, 0) or args.windows_evidence:
        windows = windows_payload_evidence((input_directory / value['installer']['localPath']).resolve(), args.windows_evidence)
        require(windows['installerSha256'] == identity['sha256'], 'Installer changed during payload verification')
    media = {name: verify_media(value['artwork'][name], name, input_directory) for name in ['icon', 'thumbnail']}
    media['screenshots'] = [verify_media({'url': item['url'], 'localPath': item['localPath']}, 'screenshot', input_directory) for item in value['screenshots']]
    runtime_evidence = {}
    document = manifest(value, runtime_downloads(value['sourceCommit'], runtime_evidence, value['version']), runtime_evidence)
    checked = runtime_validate(document, args.marketplace_checkout.resolve())
    raw = (json.dumps(document, indent=2, ensure_ascii=False) + '\n').encode('utf-8')
    require(len(raw) <= 128000, 'Manifest exceeds the Marketplace size limit')
    require(args.output.name == 'deloresai.manifest.json', 'Output filename must be deloresai.manifest.json')
    require(not args.output.exists() or args.output.read_bytes() == raw, 'Refusing to overwrite a different release manifest; use a fresh output directory')
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_bytes(raw)
    receipt = {'manifestSha256': hashlib.sha256(raw).hexdigest(), 'release': identity, 'media': media, 'validation': checked, 'testedHost': value['testedHost'],
               'windowsPayload': windows, 'runtimeSources': runtime_evidence,
               'publisherIdentity': 'Supplied by the release operator; account ownership is checked by the Marketplace submission workflow.',
               'submitted': False, 'admissionApproved': False}
    args.output.with_name('manifest-validation.json').write_text(json.dumps(receipt, indent=2) + '\n', encoding='utf-8')
    print(json.dumps({'ok': True, 'manifestSha256': receipt['manifestSha256'], 'draftRelease': identity['draft'], 'submitted': False, 'admissionApproved': False}))


if __name__ == '__main__':
    try:
        main()
    except (ValueError, OSError, KeyError, TypeError, subprocess.SubprocessError) as error:
        raise SystemExit('Manifest preparation failed: ' + str(error))
