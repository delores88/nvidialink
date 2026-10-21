"""Build inside the pinned Spark Rust runtime, with a verified auditable tool.

The build-only archive is supplied explicitly by the release operator. No
network downloads or shell interpolation happen here. The scheduler retains
the outputs in the project's target cache for hash-checked retrieval.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tarfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('archive', type=Path)
    args = parser.parse_args()
    archive_hash = 'bcb2d46002a8dd8e94d0d669cb39d38c9651db2ae5d1f3f1f1bab516ee1e212c'
    tool_hash = 'f1fe64c099129892a42c11c863a4e7c1fac94d61e16d16b7382690cf8a8c6f72'
    assert hashlib.sha256(args.archive.read_bytes()).hexdigest() == archive_hash
    with tarfile.open(args.archive) as archive:
        entries = [entry for entry in archive.getmembers()
                   if entry.isfile() and Path(entry.name).name == 'cargo-auditable']
        assert len(entries) == 1
        raw = archive.extractfile(entries[0]).read()
    assert hashlib.sha256(raw).hexdigest() == tool_hash
    # Some installed runtimes intentionally mount /tmp with noexec. The
    # scheduler's private materialized work tree is the executable build area.
    tools = Path('/work/build-tools')
    tools.mkdir(mode=0o700, exist_ok=True)
    tool = tools / 'cargo-auditable'
    tool.write_bytes(raw)
    tool.chmod(0o700)
    os.environ['PATH'] = str(tools) + ':' + os.environ['PATH']
    print(json.dumps({'tool': 'cargo-auditable', 'archive_sha256': archive_hash,
                      'executable_sha256': tool_hash}), flush=True)
    subprocess.run(['cargo', 'auditable', 'build', '--workspace', '--release',
                    '--locked', '--offline'], check=True)
    subprocess.run(['/target/release/sparkdev', '--version'], check=True)
    for name in ('sparkdev', 'sparkdev-link'):
        binary = Path('/target/release') / name
        raw = binary.read_bytes()
        assert raw[:6] == b'\x7fELF\x02\x01'
        print(json.dumps({'artifact': name, 'bytes': len(raw),
                          'sha256': hashlib.sha256(raw).hexdigest()}), flush=True)


if __name__ == '__main__':
    main()
