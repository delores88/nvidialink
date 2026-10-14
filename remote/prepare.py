#!/usr/bin/env python3
"""Fixed lockfile-enforcing dependency commands inside an isolated Docker job."""
import hashlib
import os
from pathlib import Path
import shutil
import subprocess
import sys


def files(root, name):
    result = []
    for directory, names, filenames in os.walk(root, followlinks=False):
        names[:] = [n for n in names if n not in {".git", "target", "node_modules", ".venv", "build", "dist"}]
        if name in filenames:
            path = Path(directory) / name
            if path.is_symlink():
                raise ValueError("symlink lockfile refused")
            result.append(path)
    if not result or len(result) > 32:
        raise ValueError("one to thirty-two lockfiles required")
    return sorted(result)


def commands(runtime, root):
    if runtime == "rust":
        return [(lock.parent, ["cargo", "fetch", "--locked", "--manifest-path", str(lock.parent / "Cargo.toml")], lock)
                for lock in files(root, "Cargo.lock") if (lock.parent / "Cargo.toml").is_file()]
    if runtime == "node":
        return [(lock.parent, ["npm", "ci", "--ignore-scripts", "--no-audit", "--no-fund"], lock)
                for lock in files(root, "package-lock.json") if (lock.parent / "package.json").is_file()]
    if runtime == "python":
        return [(lock.parent, ["python3", "-m", "pip", "install", "--require-hashes", "--only-binary=:all:",
                              "--target", "/pip-cache/envs/" + hashlib.sha256(lock.read_bytes()).hexdigest(), "-r", str(lock)], lock)
                for lock in files(root, "requirements.lock")]
    raise ValueError("runtime has no dependency preparation")


def main():
    if len(sys.argv) != 2:
        return 64
    try:
        actions = commands(sys.argv[1], Path("/work"))
        if not actions:
            raise ValueError("no matching manifest")
    except ValueError:
        print("sparkdev: dependency preparation requires supported, hash-locked manifests", flush=True)
        return 65
    for cwd, argv, lock in actions:
        original = hashlib.sha256(lock.read_bytes()).digest()
        print("sparkdev: preparing " + lock.relative_to("/work").as_posix(), flush=True)
        result = subprocess.run(argv, cwd=cwd, stdin=subprocess.DEVNULL, check=False)
        if hashlib.sha256(lock.read_bytes()).digest() != original:
            print("sparkdev: lockfile changed; preparation refused", flush=True)
            return 65
        if result.returncode:
            return result.returncode
        if sys.argv[1] == "node":
            tree = Path("/npm-cache/trees") / hashlib.sha256(lock.read_bytes()).hexdigest()
            tree.parent.mkdir(parents=True, exist_ok=True)
            if tree.is_symlink():
                tree.unlink()
            elif tree.exists():
                shutil.rmtree(tree)
            shutil.copytree(cwd / "node_modules", tree, symlinks=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
