"""Generate a release-specific Marketplace manifest; never upload or submit it.

Requires live GitHub identities, matching installer/media bytes, and the actual
Marketplace runtime contract. The input template deliberately cannot pass.
"""
import argparse
import hashlib
import io
import json
from pathlib import Path
import re
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
                        'requirements', 'artwork', 'screenshots', 'screenshotsReviewed'], 'Release input')
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
    require(isinstance(r['minimumOs'], str) and re.fullmatch(r'\d+(\.\d+){0,3}', r['minimumOs']), 'Supply the tested numeric Windows version')
    require(type(r['memoryGb']) in {int, float} and 0 < r['memoryGb'] <= 4096, 'Supply the tested client memory requirement')
    require(type(r['diskMb']) in {int, float} and 0 < r['diskMb'] <= 4000000, 'Supply the measured positive client disk requirement')
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
    require(asset['browser_download_url'] == expected, 'Installer download URL is not the canonical release URL')
    return {'repositoryId': repo['id'], 'releaseId': release['id'], 'draft': release['draft'], 'prerelease': prerelease,
            'sourceCommit': ref['sha'], 'assetId': asset['id'], 'filename': asset['name'], 'bytes': asset['size'], 'sha256': digest}


def runtime_downloads(commit):
    downloads = []
    cpu = source(commit, 'runtimes/Dockerfile').decode().replace('\r\n', '\n')
    match = re.search(r'^FROM rust:[^\s]+@sha256:([0-9a-f]{64})$', cpu, re.M)
    require(match is not None, 'CPU OCI base pin is missing')
    downloads.append({'url': 'https://registry-1.docker.io/v2/library/rust/manifests/sha256:' + match[1],
                      'sha256': match[1], 'purpose': 'CPU base OCI manifest; Docker verifies its digest and layer digests. Setup installs Debian repository packages and locked wheels, then pins the final local image ID.'})
    cuda = source(commit, 'runtimes/Dockerfile.cuda').decode().replace('\r\n', '\n')
    match = re.search(r'^FROM nvcr.io/nvidia/pytorch@sha256:([0-9a-f]{64})$', cuda, re.M)
    require(match is not None, 'Optional CUDA OCI base pin is missing')
    downloads.append({'url': 'https://nvcr.io/v2/nvidia/pytorch/manifests/sha256:' + match[1], 'sha256': match[1],
                      'purpose': 'Optional owner-requested CUDA base OCI manifest; Docker checks its digest and layer digests. No model weights are bundled or downloaded by the bridge.'})
    wheels = source(commit, 'runtimes/requirements.txt').decode().replace('\r\n', '\n')
    for name, version, digest in re.findall(r'^([A-Za-z0-9_-]+)==([^\s]+) --hash=sha256:([0-9a-f]{64})$', wheels, re.M):
        with urllib.request.urlopen(f'https://pypi.org/pypi/{name}/{version}/json', timeout=30) as response:
            metadata = json.load(response)
        files = [item for item in metadata['urls'] if item['digests']['sha256'] == digest]
        require(len(files) == 1 and urlsplit(files[0]['url']).hostname == 'files.pythonhosted.org', 'Pinned runtime wheel cannot be resolved')
        downloads.append({'url': files[0]['url'], 'sha256': digest, 'purpose': f'Hash-locked {name} {version} wheel installed only inside the Spark CPU runtime during setup.'})
    require(len(downloads) == 4, 'Expected two OCI manifests and two hash-locked runtime wheels')
    return downloads


def manifest(value, downloads):
    commit = value['sourceCommit']
    repo_url = 'https://github.com/' + REPOSITORY
    return {
        'schemaVersion': 1, 'appId': 'dgxbridge', 'publisherId': value['publisherId'],
        'name': 'DGXBridge', 'slug': 'dgxbridge',
        'repository': {'id': REPOSITORY_ID, 'url': repo_url},
        'release': {'id': value['releaseId'], 'tag': value['tag'], 'version': value['version'], 'channel': 'beta' if '-' in value['version'] else 'stable', 'sourceCommit': commit},
        'summary': 'Run agent-directed builds and tests on your own NVIDIA DGX Spark, with a Windows desktop job board and CLI.',
        'description': 'DGXBridge connects a Windows laptop to an owner-controlled NVIDIA DGX Spark. It provides a shared job board, resumable project transfer, durable job IDs, retained output, cancellation, per-project caches and a stdio MCP interface for coding agents. Windows work can run as native owner processes; Linux work runs in restricted Spark containers. Native laptop execution is not sandboxed.\n\nFirst setup requires an existing authenticated SSH configuration with a verified host key, Windows OpenSSH and preinstalled Microsoft Edge WebView2. Spark requires ARM64 Linux, Python 3, a working user Docker setup, systemd user services and sufficient disk/memory headroom. Setup installs independent bridge services. Phone QR approval is a separate owner action on the same LAN; optional encrypted public n0 relays can then provide remote connectivity. No owner VM is configured or assumed.\n\nRuntime setup downloads a digest-pinned OCI base, installs Debian repository packages and hash-locked Python wheels, then locks jobs to the resulting local image ID. Debian package versions are resolved at setup rather than pinned individually in this release. Optional CUDA provisioning is separate. Dependency preparation downloads project packages through a fixed domain allowlist; normal Spark jobs have no network. This release has no automatic data cleanup and does not bundle an AI model or call model APIs. Consult the release receipts for the exact tested workflows and remaining phone/hotspot/native acceptance limits.',
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
            {'name': 'Public n0 iroh relays', 'required': False, 'purpose': 'Owner-enabled encrypted remote device connectivity after pairing; no owner VM is included.', 'costDisclosure': 'Optional third-party network infrastructure has its own availability and terms; relay operators can observe connection metadata.'},
            {'name': 'Claude Code, Codex or another CLI client', 'required': False, 'purpose': 'Calls the common CLI or stdio MCP interface; the bridge itself does not invoke a model API.', 'costDisclosure': 'Agent accounts, subscriptions and any model API charges remain separate from this free utility.'},
            {'name': 'Container and package registries', 'required': True, 'purpose': 'Initial runtime provisioning and explicitly requested dependency preparation.', 'costDisclosure': 'Downloads use owner bandwidth and disk. Existing prepared jobs can use LAN SSH without an internet route, but the full product requires a reachable Spark.'}
        ],
        'privacy': {
            'dataLeavesDevice': 'Selected project files and job arguments are sent to the owner-selected Spark. Program output returns to the laptop. Dependency registries receive requested package information; optional relays see connection metadata while payloads remain encrypted. No project data is sent to a model API by this app.',
            'retention': 'Source snapshots, caches, audit events, logs and receipts persist in owner application/Spark state until removed by the owner. Pending offline cancellations are retained. Uninstall does not remove pairing identity, Spark services or retained job data. No automatic retention cleanup is included.',
            'telemetry': 'No application telemetry or embedded model calls. SSH key contents stay with OpenSSH. Windows device keys use user-scoped DPAPI; QR tokens and approval codes are transient.',
            'policyUrl': repo_url + '/blob/' + commit + '/docs/PRIVACY.md'},
        'permissions': {
            'filesystem': ['Read the selected project tree and write application configuration, logs, receipts and pending cancellations.', 'Native laptop jobs execute with the owner account and can access files that account permits; they are not a sandbox.', 'Write only independent bridge state, source snapshots and project caches on the selected Spark; install its user service units.'],
            'credentials': ['Use the owner-selected OpenSSH key/config and pinned known-host entry without copying private key contents.', 'Generate and retain a separate paired-device identity protected with Windows DPAPI; hold approval secrets only during pairing.'],
            'domains': ['deloresai.com', 'registry-1.docker.io', 'auth.docker.io', 'nvcr.io', 'deb.debian.org', 'security.debian.org', 'pypi.org', 'files.pythonhosted.org', 'crates.io', 'static.crates.io', 'index.crates.io', 'registry.npmjs.org', 'release-assets.githubusercontent.com', 'objects.githubusercontent.com', 'github-releases.githubusercontent.com', 'use1-1.relay.n0.iroh.link', 'usw1-1.relay.n0.iroh.link', 'euc1-1.relay.n0.iroh.link', 'aps1-1.relay.n0.iroh.link'],
            'elevation': False, 'backgroundServices': True},
        'models': [], 'executableDownloads': downloads,
        'updater': {'mode': 'coordinated', 'coordination': 'The drawer updater discovers accepted stable Windows artifacts through DeloresAI Marketplace. It requires explicit browser approval with S256 proof, verifies acquisition and automated-review signatures, checks exact artifact identity, SHA-256 and size, and revalidates immediately before interactive current-user NSIS setup. No silent installation or GitHub fallback. Existing bridge configuration, pairing and job state are preserved. Owner-triggered SSH setup separately updates independent Spark services.'},
        'rollback': {'dataCompatible': False, 'backupRequired': True, 'notes': 'This preview does not promise downgrade compatibility. Back up bridge-owned configuration, pairing state and job data before replacing an installed version. Uninstall preserves those records and remote services; never remove another application\'s data.'},
        'artwork': {'iconUrl': value['artwork']['icon']['url'], 'thumbnailUrl': value['artwork']['thumbnail']['url']},
        'screenshots': [{'url': row['url'], 'alt': row['alt'].strip()} for row in value['screenshots']],
        'artifacts': [{'os': 'windows', 'architectures': ['x64'], 'format': 'nsis', 'assetId': value['assetId'], 'filename': value['installer']['filename'], 'installationAdapter': 'windows-nsis', 'preferred': True,
                       'requirements': {**value['requirements'], 'gpu': None, 'runtimes': ['Microsoft Edge WebView2 Runtime (preinstalled)', 'Windows OpenSSH Client (preinstalled)'], 'distributions': []}}]
    }


def runtime_validate(document, checkout):
    contracts = checkout / 'packages/contracts/src'
    require((contracts / 'index.ts').is_file() and (contracts / 'artwork.ts').is_file(), 'Marketplace contract checkout is required')
    loader = checkout / 'node_modules/tsx/dist/loader.mjs'
    require(loader.is_file(), 'The existing Marketplace checkout needs its installed tsx dependency; this script does not install it')
    code = f"import {{readFileSync}} from 'node:fs';import {{manifestSchema}} from {json.dumps((contracts/'index.ts').as_uri())};import {{publicationArtworkIssues}} from {json.dumps((contracts/'artwork.ts').as_uri())};const p=manifestSchema.safeParse(JSON.parse(readFileSync(process.argv[1],'utf8')));const issues=p.success?publicationArtworkIssues(p.data):p.error.issues;console.log(JSON.stringify({{ok:p.success&&!issues.length,issues}}));process.exit(p.success&&!issues.length?0:1);"
    (ROOT / '.local').mkdir(exist_ok=True)
    with tempfile.NamedTemporaryFile(mode='w', suffix='.json', prefix='manifest-validation-', dir=ROOT / '.local', delete=False, encoding='utf-8') as file:
        temporary = Path(file.name)
        json.dump(document, file)
    try:
        result = subprocess.run(['node', '--import', loader.as_uri(), '--input-type=module', '--eval', code, str(temporary)], capture_output=True, timeout=30)
        require(result.returncode == 0, 'Marketplace runtime validation failed: ' + result.stdout.decode(errors='replace')[:6000])
    finally:
        temporary.unlink(missing_ok=True)
    revision = subprocess.check_output(['git', '-C', str(checkout), 'rev-parse', 'HEAD'], timeout=10).decode().strip()
    return {'contractCommit': revision, 'manifestSchema': True, 'publicationArtworkIssues': [],
            'contractSha256': {name: hashlib.sha256((contracts / name).read_bytes()).hexdigest() for name in ['index.ts', 'artwork.ts']}}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--input', required=True, type=Path)
    parser.add_argument('--marketplace-checkout', required=True, type=Path)
    parser.add_argument('--output', required=True, type=Path)
    parser.add_argument('--require-published', action='store_true')
    args = parser.parse_args()
    value = json.loads(args.input.read_text(encoding='utf-8'))
    validate_input(value)
    input_directory = args.input.resolve().parent
    identity = verify_release(value, input_directory, args.require_published)
    media = {name: verify_media(value['artwork'][name], name, input_directory) for name in ['icon', 'thumbnail']}
    media['screenshots'] = [verify_media({'url': item['url'], 'localPath': item['localPath']}, 'screenshot', input_directory) for item in value['screenshots']]
    document = manifest(value, runtime_downloads(value['sourceCommit']))
    checked = runtime_validate(document, args.marketplace_checkout.resolve())
    raw = (json.dumps(document, indent=2, ensure_ascii=False) + '\n').encode('utf-8')
    require(len(raw) <= 128000, 'Manifest exceeds the Marketplace size limit')
    require(args.output.name == 'deloresai.manifest.json', 'Output filename must be deloresai.manifest.json')
    require(not args.output.exists() or args.output.read_bytes() == raw, 'Refusing to overwrite a different release manifest; use a fresh output directory')
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_bytes(raw)
    receipt = {'manifestSha256': hashlib.sha256(raw).hexdigest(), 'release': identity, 'media': media, 'validation': checked,
               'publisherIdentity': 'Supplied by the release operator; account ownership is checked by the Marketplace submission workflow.',
               'submitted': False, 'admissionApproved': False}
    args.output.with_name('manifest-validation.json').write_text(json.dumps(receipt, indent=2) + '\n', encoding='utf-8')
    print(json.dumps({'ok': True, 'manifestSha256': receipt['manifestSha256'], 'draftRelease': identity['draft'], 'submitted': False, 'admissionApproved': False}))


if __name__ == '__main__':
    try:
        main()
    except (ValueError, OSError, KeyError, TypeError, subprocess.SubprocessError) as error:
        raise SystemExit('Manifest preparation failed: ' + str(error))
