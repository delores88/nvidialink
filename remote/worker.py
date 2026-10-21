#!/usr/bin/env python3
"""Fixed in-container supervisor. It never runs outside an admitted Docker job."""
import json
import hashlib
import http.client
import os
from pathlib import Path
import re
import signal
import shutil
import stat
import subprocess
import sys
import threading
import time
import uuid

MAX_LOG = 256 * 1024 * 1024
SKIP = {".git", "target", "node_modules", ".venv", "build", "dist"}
# Must match prepare.LOCKS: each runtime's prepared env is named by its lock's SHA-256.
LOCKS = {"python": "requirements.lock", "cuda": "requirements-cuda.lock"}
CALL_ID = re.compile(r"^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$")
MAX_INPUT = 1024 * 1024
MAX_REPLY = 256 * 1024 * 1024
LOG_MARKER = b"\nsparkdev: output limit reached; further output is drained without recording\n"


def atomic_file(path, data):
    """Publish a complete file; metadata is published after its body."""
    path = Path(path)
    temporary = path.parent / (".pending-" + uuid.uuid4().hex)
    try:
        with open(temporary, "xb") as stream:
            stream.write(data)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, path)
    finally:
        if temporary.exists():
            temporary.unlink()


def read_regular(path, maximum):
    before = os.lstat(path)
    if not stat.S_ISREG(before.st_mode) or before.st_nlink != 1 or before.st_size > maximum:
        raise ValueError("invalid request spool")
    flags = os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_NONBLOCK", 0) | getattr(os, "O_BINARY", 0)
    with os.fdopen(os.open(path, flags), "rb") as stream:
        info = os.fstat(stream.fileno())
        if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1 or info.st_size > maximum or (before.st_dev, before.st_ino) != (info.st_dev, info.st_ino):
            raise ValueError("invalid request spool")
        raw = stream.read(maximum + 1)
    if len(raw) > maximum:
        raise ValueError("oversized request spool")
    return raw


def forward_call(port, path, body, content_type, *, connection=http.client.HTTPConnection, maximum=MAX_REPLY):
    """Fixed loopback only; http.client never interprets HTTP_PROXY or redirects."""
    conn = connection("127.0.0.1", port, timeout=600)
    try:
        deadline = time.monotonic() + 600
        conn.request("POST", path, body=body, headers={"Content-Type": content_type, "Connection": "close"})
        response = conn.getresponse()
        output = bytearray()
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError("model response deadline")
            if conn.sock is not None:
                conn.sock.settimeout(min(600, remaining))
            chunk = response.read(min(65536, maximum + 1 - len(output)))
            if not chunk:
                break
            output.extend(chunk)
            if len(output) > maximum:
                raise ValueError("model reply too large")
        mime = response.getheader("Content-Type", "application/octet-stream")
        if not isinstance(mime, str) or not 1 <= len(mime) <= 128 or any(not 32 <= ord(c) <= 126 for c in mime):
            mime = "application/octet-stream"
        return response.status, mime, bytes(output)
    except (OSError, ValueError, http.client.HTTPException):
        return 502, "text/plain", b"DGXBridge could not obtain a bounded response from the model server.\n"
    finally:
        conn.close()


def server_forwarder(port, stop, requests=Path("/sparkdev-requests"), replies=Path("/sparkdev-replies"), *, connection=http.client.HTTPConnection):
    ready = False
    while not stop.is_set():
        if not ready:
            conn = connection("127.0.0.1", port, timeout=1)
            try:
                conn.request("GET", "/", headers={"Connection": "close"})
                conn.getresponse()  # Any HTTP status proves the local listener is ready.
                atomic_file(replies / "_ready", b"ready\n")
                ready = True
            except (OSError, ValueError, http.client.HTTPException):
                pass
            finally:
                conn.close()
        if ready:
            try:
                names = sorted(entry.name for entry in os.scandir(requests) if entry.name.endswith(".json"))
            except OSError:
                names = []
            for name in names:
                if stop.is_set():
                    return
                call_id = name[:-5]
                if not CALL_ID.fullmatch(call_id) or os.path.lexists(replies / name):
                    continue
                started = replies / (call_id + ".started")
                try:
                    # An interrupted supervisor cannot replay an uncertain HTTP request.
                    fd = os.open(started, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
                    os.close(fd)
                except FileExistsError:
                    status, mime, output = 502, "text/plain", b"The previous model call was interrupted; it was not replayed.\n"
                else:
                    try:
                        meta = json.loads(read_regular(requests / name, 4096))
                        body = read_regular(requests / (call_id + ".body"), MAX_INPUT)
                        path, mime = meta["path"], meta["content_type"]
                        if meta.get("id") != call_id or not isinstance(path, str) or not re.fullmatch(r"/[!-~]{0,1023}", path):
                            raise ValueError("invalid path")
                        if not isinstance(mime, str) or not 1 <= len(mime) <= 128 or any(not 32 <= ord(c) <= 126 for c in mime):
                            raise ValueError("invalid content type")
                        status, mime, output = forward_call(port, path, body, mime, connection=connection)
                    except (OSError, ValueError, KeyError, TypeError):
                        status, mime, output = 502, "text/plain", b"The model call request could not be read safely.\n"
                try:
                    atomic_file(replies / (call_id + ".body"), output)
                    atomic_file(replies / name, json.dumps({"status": status, "content_type": mime, "size": len(output)}, separators=(",", ":")).encode())
                except OSError:
                    # Disk exhaustion must not cause repeat execution. The .started
                    # guard remains and the scheduler applies its spool/disk stop.
                    pass
        stop.wait(0.1 if ready else 1)


def lifetime_stop(process, stop, lifetime, *, killpg=os.killpg if hasattr(os, "killpg") else None):
    if not stop.wait(lifetime + 60):
        try:
            killpg(process.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass


def drain_output(source, log, maximum=MAX_LOG):
    """Keep consuming the pipe after the bounded log fills, without killing work."""
    kept, capped = 0, False
    room = max(0, maximum - len(LOG_MARKER))
    while True:
        block = os.read(source.fileno(), 65536)
        if not block:
            break
        if capped:
            continue
        take = min(len(block), room - kept)
        if take:
            log.write(block[:take])
            kept += take
        if take < len(block):
            log.write(LOG_MARKER[:maximum - kept])
            capped = True


def env_paths(runtime, root, envs):
    """Prepared site directories for this runtime's lockfiles, in prepare.py's sorted lock order.

    Never directory-listing order: which env wins an import must not depend on the filesystem.
    """
    name = LOCKS.get(runtime)
    if name is None:
        return []
    locks = []
    for directory, names, filenames in os.walk(root, followlinks=False):
        names[:] = [n for n in names if n not in SKIP]
        if name in filenames:
            locks.append(Path(directory) / name)
    paths = []
    for lock in sorted(locks):
        cache = str(Path(envs) / hashlib.sha256(lock.read_bytes()).hexdigest())
        if os.path.isdir(cache) and cache not in paths:
            paths.append(cache)
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
    # pip --user entrypoints are available to direct argv launches as well as shells.
    userbase = os.environ.get("PYTHONUSERBASE")
    if userbase:
        os.environ["PATH"] = str(Path(userbase) / "bin") + os.pathsep + os.environ.get("PATH", "")
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
        stop = threading.Event()
        if os.environ.get("SPARKDEV_JOB_KIND") == "service":
            port = int(os.environ["PORT"])
            lifetime = min(8 * 3600, max(1, int(os.environ.get("SPARKDEV_SERVER_LIFETIME", 8 * 3600))))
            threading.Thread(target=server_forwarder, args=(port, stop), daemon=True).start()
            threading.Thread(target=lifetime_stop, args=(process, stop, lifetime), daemon=True).start()
        try:
            drain_output(process.stdout, log)
            code = process.wait()
        finally:
            stop.set()
        os.fsync(log.fileno())
        return code if code >= 0 else 128 - code


if __name__ == "__main__":
    sys.exit(main())
