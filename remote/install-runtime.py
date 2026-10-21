#!/usr/bin/env python3
"""Build one fixed owner-authorized CPU runtime, then pin its immutable ID."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import sqlite3
import subprocess
import sys
import time
import uuid

from bootstrap import RUNTIME_SOURCES, bounded_json, ensure_runtime_idle, runtime_attempt_path, runtime_current, runtime_lock, runtime_source_digest, write, write_runtime_attempt

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


def runtime_inputs(root, expected=None, sources=RUNTIME_SOURCES):
    files = {}
    for name in sources:
        path = root / name
        if any(part.is_symlink() for part in (path, *path.parents)):
            raise ValueError("Runtime input path is a symlink")
        with path.open("rb") as stream:
            raw = stream.read(1024 ** 2 + 1)
        if len(raw) > 1024 ** 2:
            raise ValueError("Runtime input exceeds bounds")
        files[name] = raw
    digest = runtime_source_digest(files, sources)
    if expected is not None and digest != expected:
        raise ValueError("Runtime sources changed after setup; retry the current setup")
    context = root / "runtime-build-inputs" / digest
    if any(part.is_symlink() for part in (context, *context.parents)):
        raise ValueError("Runtime snapshot path is a symlink")
    for name, raw in files.items():
        path = context / Path(name).name
        if path.exists() and (path.is_symlink() or path.read_bytes() != raw):
            raise ValueError("Immutable runtime input snapshot differs")
        if not path.exists():
            write(path, raw, 0o400)
    return digest, context


def publish_runtime_status(root, receipt, attempt_id=None, cuda=False):
    # Only call while holding the exclusive refresh lock. Attempt receipts are
    # independently addressable so a failed handoff cannot conceal its outcome.
    if attempt_id:
        receipt = {**receipt, "attempt_id": attempt_id}
    atomic(root / ("runtime-cuda-build.json" if cuda else "runtime-build.json"), receipt)
    if attempt_id:
        write_runtime_attempt(root, attempt_id, receipt)


def build_runtime(root, expected=None, attempt_id=None):
    # The exclusive runtime lock is held for this whole function. The scheduler
    # uses its shared counterpart, so no new job can appear after this check.
    ensure_runtime_idle(root)
    source_digest, context = runtime_inputs(root, expected)
    if runtime_current(root, source_digest):
        if attempt_id:
            write_runtime_attempt(root, attempt_id, {**bounded_json(root / "runtime-build.json"), "reused": True})
        return 0
    receipt = {"started_at": time.time(), "state": "building", "base": BASE, "tag": "sparkdev/cpu:1.98.1-arm64",
               "source_digest": source_digest}
    publish_runtime_status(root, receipt, attempt_id)
    env = os.environ.copy()
    env["DOCKER_BUILDKIT"] = "0"  # Classic builder honors CPU/memory controls.
    try:
        with (root / "runtime-build.log").open("wb") as log:
            code = subprocess.run(["docker", "build", "--cpu-period", "100000", "--cpu-quota", "200000", "--memory", "2g",
                                   "--memory-swap", "2g", "--tag", receipt["tag"], str(context)],
                                  stdout=log, stderr=subprocess.STDOUT, env=env, check=False).returncode
        if code:
            receipt.update(state="failed", exit_code=code, finished_at=time.time())
            publish_runtime_status(root, receipt, attempt_id)
            return code
        image = json.loads(subprocess.check_output(["docker", "image", "inspect", receipt["tag"]]))[0]
        image_id = image["Id"]
        if image.get("Architecture") != "arm64" or not re.fullmatch(r"sha256:[0-9a-f]{64}", image_id):
            raise ValueError("Unexpected image identity")
        # Verify user installs are available in the actual pinned runtime, not
        # merely that the Dockerfile contains the requested venv option.
        probe = subprocess.check_output(["docker", "run", "--rm", "--pull", "never", "--network", "none", "--read-only",
                                         "--cap-drop", "ALL", "--security-opt", "no-new-privileges", "--user", "1000:1000",
                                         "--cpus", "1", "--memory", "512m", "--memory-swap", "512m", "--pids-limit", "512",
                                         "--entrypoint", "/bin/sh", image_id, "-c", "rustc --version && git --version && node --version && python3 --version && python3 -c 'import io, site, numpy, PIL; from PIL import Image; assert site.ENABLE_USER_SITE; assert PIL.__version__ == \"13.0.0\"; b=io.BytesIO(); Image.new(\"RGB\",(2,2),(1,2,3)).save(b,format=\"PNG\"); b.seek(0); im=Image.open(b); im.load(); assert im.resize((1,1)).getpixel((0,0)) == (1,2,3); print(numpy.__version__, PIL.__version__, \"PNG decode/resize verified\")'"])
        provenance = "sparkdev CPU Dockerfile; source " + source_digest + "; base " + BASE
        register_runtimes(root, {name: {"image": image_id, "provenance": provenance} for name in ("rust", "node", "python")})
        receipt.update(state="succeeded", image=image_id, probe=probe.decode(), probe_sha256=hashlib.sha256(probe).hexdigest(), finished_at=time.time(), exit_code=0)
        publish_runtime_status(root, receipt, attempt_id)
        print(json.dumps(receipt))
    except (OSError, ValueError, TypeError, KeyError, subprocess.SubprocessError):
        receipt.update(state="failed", exit_code=1, finished_at=time.time(), error="runtime_verification_failed")
        publish_runtime_status(root, receipt, attempt_id)
        return 1
    return 0


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source-digest")
    parser.add_argument("--attempt-id")
    parser.add_argument("--cuda-source-digest")
    parser.add_argument("--cuda-attempt-id")
    args = parser.parse_args(argv)
    if args.source_digest is not None and not re.fullmatch(r"[0-9a-f]{64}", args.source_digest):
        parser.error("source digest must be a SHA-256")
    if args.attempt_id is not None and not re.fullmatch(r"[0-9a-f]{32}", args.attempt_id):
        parser.error("attempt id must be a 32-character lowercase hexadecimal ID")
    if bool(args.cuda_source_digest) != bool(args.cuda_attempt_id) or (args.cuda_source_digest and
            (not re.fullmatch(r"[0-9a-f]{64}", args.cuda_source_digest) or not re.fullmatch(r"[0-9a-f]{32}", args.cuda_attempt_id))):
        parser.error("CUDA refresh requires a valid source digest and attempt id")
    os.umask(0o077)
    root = Path.home() / "sparkdev"
    os.nice(10)
    attempt_id = args.attempt_id or uuid.uuid4().hex
    acquired = False
    try:
        # systemd can start this child before bootstrap has returned and released
        # its lock. Wait boundedly for that handoff, never launch a second builder.
        with runtime_lock(root, wait=30):
            acquired = True
            try:
                code = build_runtime(root, args.source_digest, attempt_id)
                if args.cuda_source_digest:
                    if code:
                        write_runtime_attempt(root, args.cuda_attempt_id, {"state": "not_started", "kind": "cuda",
                            "source_digest": args.cuda_source_digest, "error": "cpu_runtime_refresh_failed", "finished_at": time.time()})
                    else:
                        import runpy
                        cuda = runpy.run_path(str(root / "install-cuda.py"))
                        code = cuda["refresh_locked"](root, args.cuda_source_digest, args.cuda_attempt_id)
                return code
            except (OSError, ValueError, sqlite3.Error, subprocess.SubprocessError) as error:
                receipt = {"state": "not_started", "error": getattr(error, "code", "runtime_refresh_unavailable"),
                           "source_digest": args.source_digest, "finished_at": time.time(),
                           "message": "Runtime refresh did not start; the existing runtime registry is preserved"}
                try:
                    recorded = bounded_json(root / "runtime-build.json")
                    if recorded.get("attempt_id") == attempt_id and recorded.get("state") in ("building", "succeeded", "failed"):
                        receipt = {**recorded, "error": "runtime_status_publish_failed", "message": "Runtime work started, but publishing its latest status failed"}
                except (OSError, ValueError, TypeError, AttributeError):
                    pass
                publish_runtime_status(root, receipt, attempt_id)
                if args.cuda_source_digest:
                    write_runtime_attempt(root, args.cuda_attempt_id, {"state": "not_started", "kind": "cuda",
                        "source_digest": args.cuda_source_digest, "error": "cpu_runtime_refresh_unavailable", "finished_at": time.time()})
                print(json.dumps({**receipt, "attempt_id": attempt_id}))
                return 1
    except (OSError, ValueError, sqlite3.Error, subprocess.SubprocessError) as error:
        receipt = {"state": "unknown" if acquired else "not_started", "error": getattr(error, "code", "runtime_refresh_unavailable"),
                   "source_digest": args.source_digest, "finished_at": time.time(),
                   "message": "Runtime refresh did not acquire its lock or could not publish its status; inspect the isolated runtime receipts"}
        # A timeout must never overwrite another builder's current receipt.
        # A filesystem failure may prevent persistence; stdout remains honest.
        try:
            write_runtime_attempt(root, attempt_id, receipt)
            if args.cuda_source_digest:
                write_runtime_attempt(root, args.cuda_attempt_id, {"state": "not_started", "kind": "cuda",
                    "source_digest": args.cuda_source_digest, "error": "cpu_runtime_refresh_unavailable", "finished_at": time.time()})
        except (OSError, ValueError):
            receipt["receipt_saved"] = False
        print(json.dumps({**receipt, "attempt_id": attempt_id,
                          "runtime_build_receipt": runtime_attempt_path(root, attempt_id).relative_to(root).as_posix()}))
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
