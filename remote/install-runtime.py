#!/usr/bin/env python3
"""Build one fixed owner-authorized CPU runtime, then pin its immutable ID."""
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import time

BASE = "rust:1.98.1-slim-trixie@sha256:441452f8033b93f69dce3a3ff5c821c411f0a580723e6343932fc34a1b03ba5d"


def atomic(path, value):
    temporary = path.with_suffix(".pending")
    with temporary.open("w") as stream:
        json.dump(value, stream, sort_keys=True)
        stream.flush()
        os.fsync(stream.fileno())
    temporary.chmod(0o600)
    temporary.replace(path)


def merge_registry(previous, updates):
    if set(previous) != {"v", "runtimes"} or previous["v"] != 1 or not isinstance(previous["runtimes"], dict):
        raise ValueError("Invalid existing runtime registry")
    entries = {**previous["runtimes"], **updates}
    for name, entry in entries.items():
        if name not in ("rust", "node", "python", "cuda") or not isinstance(entry, dict):
            raise ValueError("Unknown runtime registration")
        if set(entry) - {"image", "provenance", "gpu"} or not re.fullmatch(r"sha256:[0-9a-f]{64}", entry.get("image", "")):
            raise ValueError("Runtime image must be an immutable digest")
        if not isinstance(entry.get("provenance"), str) or not entry["provenance"] or type(entry.get("gpu", False)) is not bool or entry.get("gpu", False) != (name == "cuda"):
            raise ValueError("Invalid runtime provenance or GPU policy")
    return {"v": 1, "runtimes": entries}


def register_runtimes(root, updates):
    import fcntl
    # CPU and optional CUDA installers may complete together; preserve each
    # other's registrations under one owner-only lock and atomic replacement.
    descriptor = os.open(root / "runtimes.lock", os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o600)
    with os.fdopen(descriptor, "r+") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        path = root / "runtimes.json"
        if path.is_symlink() or (path.exists() and (path.stat().st_uid != os.getuid() or path.stat().st_mode & 0o022)):
            raise ValueError("Runtime registry ownership or permissions are unsafe")
        previous = json.loads(path.read_text()) if path.exists() else {"v": 1, "runtimes": {}}
        atomic(path, merge_registry(previous, updates))


def main():
    os.umask(0o077)
    root = Path.home() / "sparkdev"
    receipt = {"started_at": time.time(), "state": "building", "base": BASE, "tag": "sparkdev/cpu:1.98.1-arm64"}
    status = root / "runtime-build.json"
    atomic(status, receipt)
    env = os.environ.copy()
    env["DOCKER_BUILDKIT"] = "0"  # Classic builder honors CPU/memory controls.
    os.nice(10)
    with (root / "runtime-build.log").open("wb") as log:
        code = subprocess.run(["docker", "build", "--cpu-period", "100000", "--cpu-quota", "200000", "--memory", "2g",
                               "--memory-swap", "2g", "--tag", receipt["tag"], str(root / "runtime")],
                              stdout=log, stderr=subprocess.STDOUT, env=env, check=False).returncode
    if code:
        receipt.update(state="failed", exit_code=code, finished_at=time.time())
        atomic(status, receipt)
        return code
    image = json.loads(subprocess.check_output(["docker", "image", "inspect", receipt["tag"]]))[0]
    image_id = image["Id"]
    if image.get("Architecture") != "arm64" or not image_id.startswith("sha256:") or len(image_id) != 71:
        raise ValueError("Unexpected image identity")
    # Probe only the new sparkdev image, with the same isolation limits as a job.
    probe = subprocess.check_output(["docker", "run", "--rm", "--pull", "never", "--network", "none", "--read-only",
                                     "--cap-drop", "ALL", "--security-opt", "no-new-privileges", "--user", "1000:1000",
                                     "--cpus", "1", "--memory", "512m", "--memory-swap", "512m", "--pids-limit", "512",
                                     "--entrypoint", "/bin/sh", image_id, "-c", "rustc --version && git --version && node --version && python3 --version && python3 -c 'import numpy, PIL; print(numpy.__version__, PIL.__version__)'"])
    provenance = "sparkdev CPU Dockerfile; base " + BASE
    register_runtimes(root, {name: {"image": image_id, "provenance": provenance} for name in ("rust", "node", "python")})
    receipt.update(state="succeeded", image=image_id, probe=probe.decode(), probe_sha256=hashlib.sha256(probe).hexdigest(), finished_at=time.time(), exit_code=0)
    atomic(status, receipt)
    print(json.dumps(receipt))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
