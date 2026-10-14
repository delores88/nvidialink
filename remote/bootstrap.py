#!/usr/bin/env python3
"""Pinned-SSH bootstrap: read a hash-checked, closed-file bundle from stdin.

Only ~/sparkdev and its two user systemd unit files are touched. The calling app
must authenticate SSH before sending this program or a bundle. No sudo, package
manager or runtime download is implicit unless build_runtime is explicitly true.
"""
import base64
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time

ALLOWED = {"controller.py", "worker.py", "proxy.py", "prepare.py", "install-runtime.py", "install-cuda.py", "bootstrap.py",
           "runtime/Dockerfile", "runtime/Dockerfile.cuda", "runtime/requirements.txt", "sparkdev-controller.service", "sparkdev-link.service", "bin/sparkdev-link"}
REQUIRED = {"controller.py", "worker.py", "proxy.py", "prepare.py", "install-runtime.py",
            "runtime/Dockerfile", "runtime/requirements.txt", "sparkdev-controller.service"}


def write(path, raw, mode=0o600):
    if path.is_symlink():
        raise ValueError("Refusing a symlink destination")
    for parent in path.parents:
        if parent.is_symlink():
            raise ValueError("Refusing a symlink directory")
    path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    fd, temporary = tempfile.mkstemp(prefix=".install-", dir=path.parent)
    try:
        with os.fdopen(fd, "wb") as stream:
            stream.write(raw)
            stream.flush()
            os.fsync(stream.fileno())
        os.chmod(temporary, mode)
        os.replace(temporary, path)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


def install(bundle):
    if not isinstance(bundle, dict) or set(bundle) - {"v", "files", "build_runtime", "build_cuda", "relay_mode"} or bundle.get("v") != 1:
        raise ValueError("Invalid install envelope")
    files = bundle.get("files")
    if not isinstance(files, dict) or not REQUIRED <= files.keys() or files.keys() - ALLOWED:
        raise ValueError("Bundle file set is invalid")
    if type(bundle.get("build_runtime", False)) is not bool:
        raise ValueError("build_runtime must be boolean")
    if type(bundle.get("build_cuda", False)) is not bool:
        raise ValueError("build_cuda must be boolean")
    if "relay_mode" in bundle and bundle["relay_mode"] not in ("disabled", "n0_public"):
        raise ValueError("relay_mode must be disabled or n0_public")
    if bundle.get("build_cuda") and not {"install-cuda.py", "runtime/Dockerfile.cuda"} <= files.keys():
        raise ValueError("CUDA setup files are missing")
    decoded, hashes = {}, {}
    for name, item in files.items():
        if not isinstance(item, dict) or set(item) != {"data", "sha256"}:
            raise ValueError("Invalid file envelope")
        raw = base64.b64decode(item["data"], validate=True)
        if len(raw) > (128 * 1024**2 if name == "bin/sparkdev-link" else 1024**2):
            raise ValueError("Install file exceeds bounds")
        actual = hashlib.sha256(raw).hexdigest()
        if actual != item["sha256"]:
            raise ValueError("Install file digest mismatch")
        hashes[name], decoded[name] = actual, raw
    root = Path.home() / "sparkdev"
    if root.is_symlink():
        raise ValueError("Install root is a symlink")
    root.mkdir(exist_ok=True, mode=0o700)
    os.chmod(root, 0o700)
    # All content is validated before the first write. Preserve overwritten source
    # in a private backup directory so an interrupted update can be diagnosed.
    revision = hashlib.sha256(json.dumps(hashes, sort_keys=True).encode()).hexdigest()
    backup = root / "install-backups" / str(time.time_ns())
    for name, raw in decoded.items():
        target = root.joinpath(*name.split("/"))
        if target.exists() and not target.is_symlink() and target.read_bytes() != raw:
            write(backup.joinpath(*name.split("/")), target.read_bytes())
        write(target, raw, 0o700 if name == "bin/sparkdev-link" else 0o600)
    units = ["sparkdev-controller.service"]
    if "sparkdev-link.service" in decoded and "bin/sparkdev-link" in decoded:
        relay = root / "link/relay.json"
        if "relay_mode" in bundle or not relay.exists():
            write(relay, json.dumps({"mode": bundle.get("relay_mode", "disabled")}).encode())
        units.append("sparkdev-link.service")
    for name in units:
        write(Path.home() / ".config/systemd/user" / name, decoded[name])
    subprocess.run(["systemctl", "--user", "daemon-reload"], check=True, capture_output=True, timeout=20)
    for name in units:
        subprocess.run(["systemctl", "--user", "enable", name], check=True, capture_output=True, timeout=20)
        subprocess.run(["systemctl", "--user", "restart", name], check=True, capture_output=True, timeout=20)
    runtime_state = "not_started"
    if bundle.get("build_runtime"):
        active = subprocess.run(["systemctl", "--user", "is-active", "sparkdev-runtime-build.service"], capture_output=True, timeout=10)
        if active.returncode == 0:
            runtime_state = "building"
        elif (root / "runtimes.json").exists():
            runtime_state = "provisioned"
        else:
            subprocess.run(["systemd-run", "--user", "--unit", "sparkdev-runtime-build", "--collect", "--property=Nice=10",
                            "/usr/bin/python3", str(root / "install-runtime.py")], check=True, capture_output=True, timeout=20)
            runtime_state = "building"
    cuda_state = "not_started"
    if bundle.get("build_cuda"):
        active = subprocess.run(["systemctl", "--user", "is-active", "sparkdev-cuda-build.service"], capture_output=True, timeout=10)
        if active.returncode == 0:
            cuda_state = "building"
        else:
            subprocess.run(["systemd-run", "--user", "--unit", "sparkdev-cuda-build", "--collect", "--property=Nice=10",
                            "/usr/bin/python3", str(root / "install-cuda.py")], check=True, capture_output=True, timeout=20)
            cuda_state = "building"
    receipt = {"ok": True, "revision": revision, "file_hashes": hashes, "services": units, "runtime_state": runtime_state, "cuda_state": cuda_state, "installed_at": time.time()}
    write(root / "install-receipt.json", json.dumps(receipt, sort_keys=True).encode())
    return receipt


def main():
    os.umask(0o077)
    try:
        raw = sys.stdin.buffer.read(192 * 1024**2 + 1)
        if len(raw) > 192 * 1024**2:
            raise ValueError("Bundle too large")
        result = install(json.loads(raw))
    except (ValueError, OSError, subprocess.SubprocessError, TypeError, KeyError):
        result = {"ok": False, "error": {"code": "bootstrap_failed", "message": "Install could not complete; inspect the isolated sparkdev directory and user services"}}
    print(json.dumps(result, sort_keys=True))
    return 0 if result["ok"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
