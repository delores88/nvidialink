#!/usr/bin/env python3
"""Pinned-SSH bootstrap: read a hash-checked, closed-file bundle from stdin.

Only ~/sparkdev and its two user systemd unit files are touched. The calling app
must authenticate SSH before sending this program or a bundle. No sudo, package
manager or runtime download is implicit unless build_runtime is explicitly true.
"""
import base64
import contextlib
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import subprocess
import sqlite3
import stat
import sys
import tempfile
import time
import uuid

ALLOWED = {"outputs.py", "system_packages.py", "media_probe.py", "runtime/Dockerfile.amd64", "runtime/requirements-amd64.txt", "runtime/Dockerfile.cuda.amd64", "runtime/Dockerfile.blender", "runtime/blender/prune_deps.py", "runtime/blender/patch_deps_cuda13.py", "runtime/blender/patch_deps_toolchain.py", "runtime/blender/patch_deps_ispc.py", "runtime/blender/patch_deps_oidn_cudart.py", "runtime/blender/patch_deps_numpy.py", "runtime/blender/10_nvidia.json", "controller.py", "worker.py", "proxy.py", "prepare.py", "model_cache.py", "model_server.py", "install-runtime.py", "install-cuda.py", "bootstrap.py",
           "runtime/blender/locked_sources.py", "runtime/blender/inputs.lock.json",
           "runtime/Dockerfile", "runtime/Dockerfile.cuda", "runtime/requirements.txt", "sparkdev-controller.service", "sparkdev-link.service", "bin/sparkdev-link"}
REQUIRED = {"outputs.py", "system_packages.py", "media_probe.py", "controller.py", "worker.py", "proxy.py", "prepare.py", "model_cache.py", "model_server.py", "bootstrap.py", "install-runtime.py",
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


RUNTIME_SOURCES = ("runtime/Dockerfile", "runtime/requirements.txt")
CUDA_SOURCES = ("runtime/Dockerfile.cuda", "media_probe.py", "runtime/Dockerfile.blender", "runtime/blender/prune_deps.py", "runtime/blender/patch_deps_cuda13.py", "runtime/blender/patch_deps_toolchain.py", "runtime/blender/patch_deps_ispc.py", "runtime/blender/patch_deps_oidn_cudart.py", "runtime/blender/patch_deps_numpy.py", "runtime/blender/10_nvidia.json", "runtime/blender/locked_sources.py", "runtime/blender/inputs.lock.json")
AMD64_RUNTIME_SOURCES = ("runtime/Dockerfile.amd64", "runtime/requirements-amd64.txt")
AMD64_CUDA_SOURCES = ("runtime/Dockerfile.cuda.amd64", "media_probe.py")


def runtime_architecture(machine=None):
    """Map the executing Linux host, never a caller-provided image, to a closed set."""
    value = platform.machine() if machine is None else machine
    if not isinstance(value, str):
        raise ValueError("Unsupported runtime architecture")
    value = {"aarch64": "arm64", "arm64": "arm64", "x86_64": "amd64", "amd64": "amd64"}.get(value.lower())
    if value is None:
        raise ValueError("Unsupported runtime architecture")
    return value


def runtime_sources(cuda=False, architecture=None):
    architecture = runtime_architecture(architecture)
    if architecture == "arm64":
        return CUDA_SOURCES if cuda else RUNTIME_SOURCES
    return AMD64_CUDA_SOURCES if cuda else AMD64_RUNTIME_SOURCES


class RuntimeRefreshError(ValueError):
    def __init__(self, code, message):
        super().__init__(message)
        self.code = code


def runtime_attempt_path(root, attempt_id):
    if not isinstance(attempt_id, str) or not re.fullmatch(r"[0-9a-f]{32}", attempt_id):
        raise ValueError("Invalid runtime refresh attempt")
    return root / "runtime-build-attempts" / (attempt_id + ".json")


def write_runtime_attempt(root, attempt_id, receipt):
    # The distinct attempt path also permits a bounded lock timeout to be
    # reported without overwriting the receipt of the builder holding the lock.
    write(runtime_attempt_path(root, attempt_id), json.dumps({**receipt, "attempt_id": attempt_id}, sort_keys=True).encode())


def runtime_source_digest(files, sources=RUNTIME_SOURCES):
    return hashlib.sha256(json.dumps({name: hashlib.sha256(files[name]).hexdigest() for name in sources}, sort_keys=True, separators=(",", ":")).encode()).hexdigest()


def bounded_json(path):
    if path.is_symlink():
        raise ValueError("Refusing a symlink receipt")
    with path.open("rb") as stream:
        raw = stream.read(65537)
    if len(raw) > 65536:
        raise ValueError("Receipt is too large")
    return json.loads(raw)


def runtime_current(root, source_digest, cuda=False):
    architecture = runtime_architecture()
    try:
        receipt = bounded_json(root / ("runtime-cuda-build.json" if cuda else "runtime-build.json"))
        registry = bounded_json(root / "runtimes.json")
        image = receipt.get("image")
        current = (receipt.get("state") == "succeeded" and receipt.get("source_digest") == source_digest
                and isinstance(image, str) and re.fullmatch(r"sha256:[0-9a-f]{64}", image)
                and registry.get("v") == 1
                and all(registry.get("runtimes", {}).get(name, {}).get("image") == image for name in (("cuda",) if cuda else ("rust", "node", "python"))))
    except (OSError, ValueError, TypeError, AttributeError):
        return False
    if not current:
        return False
    # A receipt is not proof that Docker still has the image. Inspect the exact
    # immutable ID, never a tag. Only an explicit missing-image reply permits a
    # rebuild; unavailable or inconsistent daemon responses fail closed.
    try:
        result = subprocess.run(["docker", "image", "inspect", "--format", "{{.Id}} {{.Architecture}}", image],
                                capture_output=True, timeout=10, check=False)
    except (OSError, subprocess.SubprocessError):
        raise RuntimeRefreshError("runtime_image_check_unavailable", "Could not verify the pinned runtime image") from None
    if result.returncode:
        error = result.stderr.decode("utf-8", errors="replace").strip()
        if re.fullmatch(r"(?:Error response from daemon: )?No such image: " + re.escape(image), error):
            return False
        raise RuntimeRefreshError("runtime_image_check_unavailable", "Could not verify the pinned runtime image")
    if result.stdout.strip() != (image + " " + architecture).encode():
        raise RuntimeRefreshError("runtime_image_identity_mismatch", "The pinned runtime image identity could not be verified")
    return True


@contextlib.contextmanager
def runtime_lock(root, wait=0):
    """Same lock the scheduler holds shared during submissions/admission."""
    import fcntl
    descriptor = os.open(root / "runtime-refresh.lock", os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o600)
    with os.fdopen(descriptor, "r+") as stream:
        info = os.fstat(stream.fileno())
        if info.st_uid != os.getuid() or not stat.S_ISREG(info.st_mode):
            raise ValueError("Runtime lock has a different owner")
        deadline = time.monotonic() + wait
        while True:
            try:
                fcntl.flock(stream, fcntl.LOCK_EX | fcntl.LOCK_NB)
                break
            except BlockingIOError:
                if time.monotonic() >= deadline:
                    raise RuntimeRefreshError("runtime_refresh_lock_busy", "A scheduler admission or runtime refresh is active; retry later") from None
                time.sleep(.1)
        yield


def ensure_runtime_idle(root):
    path = root / "state" / "state.sqlite3"
    if path.exists():
        if path.is_symlink():
            raise ValueError("Runtime state must not be a symlink")
        # Read-only: a failed check must never initialize a second identity or database.
        with contextlib.closing(sqlite3.connect(path.resolve().as_uri() + "?mode=ro", uri=True, timeout=5)) as db:
            if db.execute("SELECT 1 FROM jobs WHERE state IN ('queued','starting','running') LIMIT 1").fetchone():
                raise RuntimeRefreshError("runtime_work_active", "Finish or cancel DGXBridge jobs and model servers before refreshing its runtime")
    result = subprocess.run(["docker", "ps", "-q", "--filter", "name=sparkdev-job-"], capture_output=True, check=False, timeout=10)
    if result.returncode or result.stdout.strip():
        raise RuntimeRefreshError("runtime_idle_check_failed", "Could not establish that DGXBridge jobs and model servers are idle")


def install(bundle):
    root = Path.home() / "sparkdev"
    if root.is_symlink():
        raise ValueError("Install root is a symlink")
    root.mkdir(exist_ok=True, mode=0o700)
    with runtime_lock(root):
        return install_locked(bundle)


def install_locked(bundle):
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
    architecture = runtime_architecture()
    cpu_sources = runtime_sources(architecture=architecture)
    if not set(cpu_sources) <= decoded.keys():
        raise ValueError("Runtime setup files for this architecture are missing")
    source_digest = runtime_source_digest(decoded, cpu_sources)
    refresh_runtime = bundle.get("build_runtime") and not runtime_current(root, source_digest)
    cuda_requested = bundle.get("build_cuda", False)
    if bundle.get("build_runtime") and (root / "runtimes.json").exists():
        # Refresh every already-provisioned worker image. This does not opt an
        # owner into a new CUDA runtime merely because its recipe is bundled.
        saved_registry = bounded_json(root / "runtimes.json")
        cuda_requested = cuda_requested or "cuda" in saved_registry.get("runtimes", {})
    if cuda_requested and not {"install-cuda.py", "runtime/Dockerfile.cuda"} <= decoded.keys():
        raise ValueError("Refreshing the provisioned CUDA runtime requires its setup files")
    cuda_sources = runtime_sources(cuda=True, architecture=architecture)
    if cuda_requested and not set(cuda_sources) <= decoded.keys():
        raise ValueError("CUDA setup files for this architecture are missing")
    cuda_digest = runtime_source_digest(decoded, cuda_sources) if cuda_requested else None
    refresh_cuda = cuda_digest is not None and not runtime_current(root, cuda_digest, cuda=True)
    if refresh_runtime or refresh_cuda:
        # The caller holds the refresh lock, excluding new scheduler admissions.
        ensure_runtime_idle(root)
        for unit in ("sparkdev-runtime-build.service", "sparkdev-cuda-build.service"):
            active = subprocess.run(["systemctl", "--user", "is-active", unit], capture_output=True, timeout=10)
            if active.returncode == 0:
                raise ValueError("Runtime refresh is already starting; retry setup after it finishes")
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
    runtime_attempt_id = None
    cuda_attempt_id = uuid.uuid4().hex if refresh_cuda else None
    if cuda_attempt_id:
        write_runtime_attempt(root, cuda_attempt_id, {"state": "scheduled", "kind": "cuda", "source_digest": cuda_digest,
            "requested_at": time.time(), "message": "Waiting for the CPU runtime refresh and a fresh idle check" if refresh_runtime else "Waiting for the runtime refresh lock and a fresh idle check"})
    if bundle.get("build_runtime"):
        if not refresh_runtime:
            runtime_state = "provisioned"
        else:
            runtime_attempt_id = uuid.uuid4().hex
            pending = {"state": "scheduled", "source_digest": source_digest, "requested_at": time.time(),
                       "message": "Waiting for the runtime refresh lock and a fresh idle check; no build has started yet"}
            write_runtime_attempt(root, runtime_attempt_id, pending)
            try:
                command = ["systemd-run", "--user", "--unit", "sparkdev-runtime-build", "--collect", "--property=Nice=10",
                                "/usr/bin/python3", str(root / "install-runtime.py"), "--source-digest", source_digest,
                                "--attempt-id", runtime_attempt_id]
                if refresh_cuda:
                    command += ["--cuda-source-digest", cuda_digest, "--cuda-attempt-id", cuda_attempt_id]
                subprocess.run(command, check=True, capture_output=True, timeout=20)
            except (OSError, subprocess.SubprocessError):
                write_runtime_attempt(root, runtime_attempt_id, {**pending, "state": "not_started", "error": "runtime_launch_unavailable", "finished_at": time.time()})
                if cuda_attempt_id:
                    write_runtime_attempt(root, cuda_attempt_id, {"state": "not_started", "kind": "cuda", "source_digest": cuda_digest,
                        "error": "runtime_launch_unavailable", "finished_at": time.time()})
                raise
            runtime_state = "scheduled"
    cuda_state = "not_started"
    if cuda_requested:
        cuda_state = "provisioned" if not refresh_cuda else "scheduled"
        if refresh_cuda and not refresh_runtime:
            try:
                subprocess.run(["systemd-run", "--user", "--unit", "sparkdev-cuda-build", "--collect", "--property=Nice=10",
                                "/usr/bin/python3", str(root / "install-cuda.py"), "--source-digest", cuda_digest,
                                "--attempt-id", cuda_attempt_id], check=True, capture_output=True, timeout=20)
            except (OSError, subprocess.SubprocessError):
                write_runtime_attempt(root, cuda_attempt_id, {"state": "not_started", "kind": "cuda", "source_digest": cuda_digest,
                    "error": "runtime_launch_unavailable", "finished_at": time.time()})
                raise
    receipt = {"ok": True, "revision": revision, "file_hashes": hashes, "services": units, "architecture": architecture, "runtime_state": runtime_state,
               "runtime_source_digest": source_digest, "cuda_state": cuda_state, "installed_at": time.time()}
    if runtime_attempt_id:
        receipt.update(runtime_attempt_id=runtime_attempt_id,
                       runtime_build_receipt=runtime_attempt_path(root, runtime_attempt_id).relative_to(root).as_posix(),
                       runtime_message="The runtime refresh was scheduled; its attempt receipt reports whether it started, finished, or was refused")
    if cuda_digest:
        receipt["cuda_source_digest"] = cuda_digest
    if cuda_attempt_id:
        receipt.update(cuda_attempt_id=cuda_attempt_id, cuda_build_receipt=runtime_attempt_path(root, cuda_attempt_id).relative_to(root).as_posix())
    write(root / "install-receipt.json", json.dumps(receipt, sort_keys=True).encode())
    return receipt


def main():
    os.umask(0o077)
    try:
        raw = sys.stdin.buffer.read(192 * 1024**2 + 1)
        if len(raw) > 192 * 1024**2:
            raise ValueError("Bundle too large")
        result = install(json.loads(raw))
    except (ValueError, OSError, subprocess.SubprocessError, TypeError, KeyError, sqlite3.Error):
        result = {"ok": False, "error": {"code": "bootstrap_failed", "message": "Install could not complete; inspect the isolated sparkdev directory and user services"}}
    print(json.dumps(result, sort_keys=True))
    return 0 if result["ok"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
