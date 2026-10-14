#!/usr/bin/env python3
"""Build one fixed owner-authorized CPU runtime, then pin its immutable ID."""
import hashlib
import json
import os
from pathlib import Path
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
                                     "--entrypoint", "/bin/sh", image_id, "-c", "rustc --version && node --version && python3 --version && python3 -c 'import numpy, PIL; print(numpy.__version__, PIL.__version__)'"])
    provenance = "sparkdev CPU Dockerfile; base " + BASE
    registry = {"v": 1, "runtimes": {name: {"image": image_id, "provenance": provenance} for name in ("rust", "node", "python")}}
    atomic(root / "runtimes.json", registry)
    receipt.update(state="succeeded", image=image_id, probe=probe.decode(), probe_sha256=hashlib.sha256(probe).hexdigest(), finished_at=time.time(), exit_code=0)
    atomic(status, receipt)
    print(json.dumps(receipt))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
