#!/usr/bin/env python3
"""Fixed in-container supervisor. It never runs outside an admitted Docker job."""
import json
import hashlib
import os
from pathlib import Path
import signal
import shutil
import subprocess
import sys

MAX_LOG = 256 * 1024 * 1024
SKIP = {".git", "target", "node_modules", ".venv", "build", "dist"}
# Must match prepare.LOCKS: each runtime's prepared env is named by its lock's SHA-256.
LOCKS = {"python": "requirements.lock", "cuda": "requirements-cuda.lock"}


def env_paths(runtime, root, envs):
    """Prepared site directories for this runtime's lockfiles, in walk order."""
    name = LOCKS.get(runtime)
    paths = []
    if name is None:
        return paths
    for directory, names, filenames in os.walk(root, followlinks=False):
        names[:] = [n for n in names if n not in SKIP]
        if name in filenames:
            lock = Path(directory) / name
            cache = Path(envs) / hashlib.sha256(lock.read_bytes()).hexdigest()
            if cache.is_dir():
                paths.append(str(cache))
    return paths


def python_path(prepared, inherited):
    """Prepared envs first, then any PYTHONPATH the image or job already sets."""
    return os.pathsep.join(prepared + ([inherited] if inherited else []))


def main():
    os.nice(10)
    with open("/sparkdev-argv.json", "r", encoding="utf-8") as stream:
        argv = json.load(stream)
    # Restore packages from the matching immutable lockfile, using caches which
    # the controller namespaces by both worktree and runtime image identity.
    if "/sparkdev-prepare.py" not in argv:
        runtime = os.environ.get("SPARKDEV_RUNTIME_NAME")
        if runtime == "node":
            for directory, names, filenames in os.walk("/work", followlinks=False):
                names[:] = [n for n in names if n not in SKIP]
                if "package-lock.json" in filenames:
                    lock = Path(directory) / "package-lock.json"
                    cache = Path("/npm-cache/trees") / hashlib.sha256(lock.read_bytes()).hexdigest()
                    if cache.is_dir() and not (Path(directory) / "node_modules").exists():
                        shutil.copytree(cache, Path(directory) / "node_modules", symlinks=True)
        prepared = env_paths(runtime, "/work", "/pip-cache/envs")
        if prepared:
            os.environ["PYTHONPATH"] = python_path(prepared, os.environ.get("PYTHONPATH"))
    flags = os.O_WRONLY | os.O_CREAT | os.O_TRUNC | getattr(os, "O_NOFOLLOW", 0)
    fd = os.open("/sparkdev-output/stdout.log", flags, 0o600)
    process = None

    def cancelled(_signum, _frame):
        if process:
            try:
                os.killpg(process.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass

    signal.signal(signal.SIGTERM, cancelled)
    signal.signal(signal.SIGINT, cancelled)
    with os.fdopen(fd, "wb", buffering=0) as log:
        try:
            process = subprocess.Popen(argv, cwd="/work", stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                       stdin=subprocess.DEVNULL, start_new_session=True)
        except (OSError, ValueError):
            log.write(b"sparkdev: command could not be launched\n")
            return 127
        total = 0
        while True:
            block = os.read(process.stdout.fileno(), 65536)
            if not block:
                break
            if total + len(block) > MAX_LOG:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait()
                return 125
            log.write(block)
            total += len(block)
        code = process.wait()
        os.fsync(log.fileno())
        return code if code >= 0 else 128 - code


if __name__ == "__main__":
    sys.exit(main())
