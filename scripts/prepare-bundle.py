"""Package inspectable remote files; encode the SSH envelope only in memory."""
import argparse
import hashlib
import json
from pathlib import Path
import stat

ROOT = Path(__file__).resolve().parents[1]
MAX_MANIFEST = 64 * 1024
MAX_TOTAL = 192 * 1024**2


def plain(path):
    for entry in (path, *path.parents):
        info = entry.lstat()
        if stat.S_ISLNK(info.st_mode) or getattr(info, 'st_file_attributes', 0) & 0x400:
            raise ValueError('Bundle paths must not contain symlinks or reparse points')


def sources(root, controller_only=False):
    files = {name: root / 'remote' / name for name in [
        'controller.py', 'worker.py', 'prepare.py', 'proxy.py', 'model_cache.py', 'model_server.py', 'install-runtime.py',
        'bootstrap.py', 'sparkdev-controller.service', 'install-cuda.py']}
    files.update({'runtime/' + name: root / 'runtimes' / name for name in [
        'Dockerfile', 'requirements.txt', 'Dockerfile.cuda']})
    binary = root / 'dist/linux-arm64/sparkdev-link'
    if not binary.is_file() and not controller_only:
        raise ValueError('A release bundle requires dist/linux-arm64/sparkdev-link')
    if binary.is_file():
        files.update({'bin/sparkdev-link': binary,
                      'sparkdev-link.service': root / 'crates/sparkdev-link/deploy/sparkdev-link.service'})
    return files


def prepare(root, output, controller_only=False):
    root, output = Path(root).absolute(), Path(output).absolute()
    files = sources(root, controller_only)
    payloads, entries, total = {}, [], 0
    for name, path in sorted(files.items()):
        plain(path)
        limit = (128 if name == 'bin/sparkdev-link' else 1) * 1024**2
        if not path.is_file() or path.stat().st_size > limit:
            raise ValueError('Bundle source is not a bounded regular file: ' + name)
        with path.open('rb') as stream:
            raw = stream.read(limit + 1)
        if len(raw) > limit:
            raise ValueError('Bundle source exceeds its size limit: ' + name)
        total += len(raw)
        payloads[name] = raw
        entries.append({'path': name, 'size': len(raw), 'sha256': hashlib.sha256(raw).hexdigest()})
    if len(entries) > 15 or total > MAX_TOTAL:
        raise ValueError('Bundle exceeds aggregate limits')
    manifest = {'v': 2, 'build_runtime': True, 'files': entries}
    raw_manifest = (json.dumps(manifest, indent=2) + '\n').encode('utf-8')
    if len(raw_manifest) > MAX_MANIFEST:
        raise ValueError('Bundle manifest is too large')
    # Reject stale unrelated entries before replacing anything in this build.
    existing = output
    while not existing.exists() and not existing.is_symlink():
        existing = existing.parent
    plain(existing)
    raw_dir = output / 'sparkdev-remote'
    expected = set(payloads)
    expected_dirs = {str(Path(name).parent).replace('\\', '/') for name in expected} - {'.'}
    if raw_dir.exists() or raw_dir.is_symlink():
        plain(raw_dir)
        if not raw_dir.is_dir():
            raise ValueError('Remote payload root must be a directory')
        for path in raw_dir.rglob('*'):
            plain(path)
            relative = path.relative_to(raw_dir).as_posix()
            if path.is_dir() and relative in expected_dirs:
                continue
            if not path.is_file() or relative not in expected:
                raise ValueError('Refusing unexpected stale bundle entry: ' + relative)
    manifest_path = output / 'sparkdev-bundle.json'
    if manifest_path.exists() or manifest_path.is_symlink():
        plain(manifest_path)
        if not manifest_path.is_file():
            raise ValueError('Manifest destination must be a regular file')
    raw_dir.mkdir(parents=True, exist_ok=True)
    for name, raw in payloads.items():
        path = raw_dir / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(raw)
    manifest_path.write_bytes(raw_manifest)
    return {'v': 2, 'files': len(entries), 'link_binary_included': 'bin/sparkdev-link' in files,
            'bytes': len(raw_manifest), 'payload_bytes': total,
            'sha256': hashlib.sha256(raw_manifest).hexdigest()}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--controller-only', action='store_true',
                        help='Explicit development bundle permitting an absent pairing binary')
    parser.add_argument('--output-dir', type=Path, default=ROOT / 'dist')
    args = parser.parse_args()
    print(json.dumps(prepare(ROOT, args.output_dir, args.controller_only)))


if __name__ == '__main__':
    main()
