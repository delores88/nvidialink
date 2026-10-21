"""Automatic job results, frozen outside every workload mount.

The RPC/scheduler paths only inspect saved metadata and start a bounded number
of collectors. Hashing and copying happen in a separate, restartable process.
Like model_cache, helpers are supplied by the running controller, not imported.
"""
from __future__ import annotations

import base64
import contextlib
import hashlib
import json
import os
from pathlib import Path
import re
import stat
import subprocess
import sys
import time
import uuid

CHUNK = 1024 * 1024
FILE_BYTES = 64 * 1024 ** 3
TOTAL_BYTES = FILE_BYTES
MAX_FILES = 10000
MANIFEST_BYTES = 3 * CHUNK
WALK_MAX = 200000
MAX_DEPTH = 128
UNACKNOWLEDGED_SECONDS = 7 * 86400
ACKNOWLEDGED_SECONDS = 86400
RETRY_SECONDS = 60
HEARTBEAT_SECONDS = 10
EXCLUDED = {".venv", "venv", "env", ".cache", ".pytest_cache", ".mypy_cache", ".ruff_cache",
            ".tox", ".nox", ".gradle", ".idea", ".sparkdev", ".sparkdev-output"}
WINDOWS_DEVICE = re.compile(r"^(?:con|prn|aux|nul|com[1-9]|lpt[1-9])(?:\..*)?$", re.I)


class OutputFailure(Exception):
    def __init__(self, code, message):
        self.code, self.message = code, message


def fail(code, message):
    raise OutputFailure(code, message)


def identity(info):
    return info.st_dev, info.st_ino, info.st_size, info.st_mtime_ns, info.st_ctime_ns


class OutputManager:
    def __init__(self, controller, api, spawn=None):
        self.c, self.api = controller, api
        self.root = controller.root / "results"
        self.root.mkdir(mode=0o700, exist_ok=True)
        info = self.root.lstat()
        if not stat.S_ISDIR(info.st_mode) or (os.name == "posix" and info.st_uid != os.getuid()):
            api.reject("unsafe_state", "Result storage must be an owner-controlled directory")
        if os.name == "posix":
            os.chmod(self.root, 0o700)
        self.spawn = spawn or self._spawn
        self.c.db.executescript("""
          CREATE TABLE IF NOT EXISTS job_outputs(
            job TEXT PRIMARY KEY, state TEXT NOT NULL, manifest TEXT, manifest_sha256 TEXT,
            total_bytes INTEGER NOT NULL DEFAULT 0, files INTEGER NOT NULL DEFAULT 0,
            updated REAL NOT NULL, expires REAL, error TEXT, dispatch REAL NOT NULL DEFAULT 0);
          CREATE TABLE IF NOT EXISTS output_receivers(
            job TEXT PRIMARY KEY, source_job TEXT NOT NULL, acknowledged REAL);
          CREATE INDEX IF NOT EXISTS output_receivers_source ON output_receivers(source_job);
        """)

    def _spawn(self, job):
        script = str(Path(self.api.__file__).resolve())
        # Isolated Python with exactly the installed script directory enabled.
        launcher = "import pathlib,runpy,sys;p=sys.argv.pop(1);sys.argv[0]=p;sys.path.insert(0,str(pathlib.Path(p).parent));runpy.run_path(p,run_name='__main__')"
        subprocess.Popen([sys.executable, "-I", "-c", launcher, script, "collect-outputs", "--state", str(self.c.root), "--job", job],
                         stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                         start_new_session=True, close_fds=True, env=dict(self.api.TOOL_ENV), cwd=str(self.c.root))

    @staticmethod
    def quota():
        return {"file_bytes": FILE_BYTES, "total_bytes": TOTAL_BYTES, "max_files": MAX_FILES, "manifest_bytes": MANIFEST_BYTES}

    def register(self, job, enabled=True, source=None):
        """Called inside run's transaction, before acknowledging admission."""
        now = time.time()
        if source:
            self.c.db.execute("INSERT INTO output_receivers VALUES(?,?,NULL)", (job, source))
            self.c.db.execute("UPDATE job_outputs SET expires=MAX(expires,?) WHERE job=?", (now + UNACKNOWLEDGED_SECONDS, source))
        else:
            self.c.db.execute("INSERT INTO job_outputs(job,state,updated) VALUES(?,?,?)", (job, "pending" if enabled else "disabled", now))
            self.c.db.execute("INSERT INTO output_receivers VALUES(?,?,NULL)", (job, job))

    def cache_ready(self, job):
        return bool(self.c.db.execute("SELECT 1 FROM job_outputs WHERE job=? AND state='ready' AND expires>?", (job, time.time())).fetchone())

    def metadata(self, row, manifest=False):
        source = row["cached_from"] or row["id"]
        columns = "state,manifest_sha256,total_bytes,files,expires,error" + (",manifest" if manifest else "")
        saved = self.c.db.execute("SELECT " + columns + " FROM job_outputs WHERE job=?", (source,)).fetchone()
        ack = self.c.db.execute("SELECT acknowledged FROM output_receivers WHERE job=?", (row["id"],)).fetchone()
        state = saved["state"] if saved else "disabled"
        if saved and state == "ready" and saved["expires"] <= time.time():
            state = "expired"
        result = {"job": row["id"], "source_job": source, "state": state,
                  "manifest_sha256": saved["manifest_sha256"] if saved else None,
                  "total_bytes": saved["total_bytes"] if saved else 0, "files": saved["files"] if saved else 0,
                  "quota": self.quota(), "retention": {"unacknowledged_seconds": UNACKNOWLEDGED_SECONDS,
                  "acknowledged_seconds": ACKNOWLEDGED_SECONDS, "expires_at": saved["expires"] if saved else None,
                  "acknowledged_at": ack[0] if ack else None}, "error": json.loads(saved["error"]) if saved and saved["error"] else None}
        if state == "expired":
            result["error"] = {"code": "outputs_expired", "message": "The retained Spark result files have expired. Run the job again to recreate them."}
        if manifest:
            result["manifest"] = json.loads(saved["manifest"]) if saved and state == "ready" else None
        return result

    def outputs(self, request):
        self.api.fields(request, ("v", "op", "job"))
        row = self.c.get_job(self.api.identifier(request["job"]))
        self.step(row["cached_from"] or row["id"])
        return self.metadata(row, manifest=True)

    def output_chunk(self, request):
        self.api.fields(request, ("v", "op", "job", "scope", "path", "index"))
        row = self.c.get_job(self.api.identifier(request["job"]))
        if request["scope"] not in ("work", "output") or not isinstance(request["path"], str):
            self.api.reject("invalid_request", "Invalid result file selector")
        index = self.api.integer(request["index"], 0, FILE_BYTES // CHUNK)
        source = row["cached_from"] or row["id"]
        with self.rpc_lock(source) as locked:
            if not locked:
                self.api.reject("outputs_busy", "Result collection is in progress. Retry this chunk shortly.")
            saved = self.metadata(row, manifest=True)
            if saved["state"] != "ready":
                self.api.reject("outputs_unavailable", "Result files are not ready or their retention period has expired")
            entries = saved["manifest"]["entries"]
            found = next(((i, e) for i, e in enumerate(entries) if e["scope"] == request["scope"] and e["path"] == request["path"]), None)
            if not found:
                self.api.reject("output_not_found", "The result manifest does not contain that file")
            number, entry = found
            if index >= len(entry["chunks"]):
                self.api.reject("invalid_offset", "Chunk index is outside this result file")
            try:
                with self.open_dir(self.root) as parent, self.open_dir(source, parent) as snapshot:
                    fd = os.open(str(number), os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=snapshot)
                    with os.fdopen(fd, "rb") as stream:
                        info = os.fstat(stream.fileno())
                        if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1 or info.st_size != entry["bytes"]:
                            fail("outputs_corrupt", "The saved result file failed its integrity check")
                        stream.seek(index * CHUNK)
                        data = stream.read(CHUNK)
                        if hashlib.sha256(data).hexdigest() != entry["chunks"][index]:
                            fail("outputs_corrupt", "The saved result chunk failed its integrity check")
            except OutputFailure as error:
                self.api.reject(error.code, error.message)
            except OSError:
                self.api.reject("outputs_corrupt", "The saved result file cannot be read safely")
        return {"job": row["id"], "source_job": source, "scope": request["scope"], "path": request["path"], "index": index,
                "bytes": len(data), "data": base64.b64encode(data).decode("ascii"), "sha256": entry["chunks"][index]}

    def outputs_ack(self, request):
        self.api.fields(request, ("v", "op", "job", "manifest_sha256"))
        digest = self.api.digest(request["manifest_sha256"])
        row = self.c.get_job(self.api.identifier(request["job"]))
        source = row["cached_from"] or row["id"]
        with self.rpc_lock(source) as locked:
            if not locked:
                self.api.reject("outputs_busy", "Result collection is in progress. Retry acknowledgment shortly.")
            with self.c.transaction():
                saved = self.metadata(row)
                if digest != saved["manifest_sha256"]:
                    self.api.reject("hash_mismatch", "Acknowledge the exact complete result manifest after verifying every file")
                now = saved["retention"]["acknowledged_at"]
                if saved["state"] != "ready" and now is None:
                    self.api.reject("outputs_unavailable", "Result files are not ready or their retention period has expired")
                if now is None:
                    now = time.time()
                    self.c.db.execute("UPDATE output_receivers SET acknowledged=? WHERE job=?", (now, row["id"]))
                    if not self.c.db.execute("SELECT 1 FROM output_receivers WHERE source_job=? AND acknowledged IS NULL", (source,)).fetchone():
                        self.c.db.execute("UPDATE job_outputs SET expires=? WHERE job=?", (now + ACKNOWLEDGED_SECONDS, source))
                    self.c.audit("outputs_acknowledged", row["id"], row["client"], manifest_sha256=digest)
        return {"job": row["id"], "manifest_sha256": digest, "acknowledged": True, "acknowledged_at": now}

    @contextlib.contextmanager
    def rpc_lock(self, job):
        try:
            with self.lock(job, shared=True) as locked:
                yield locked
        except OutputFailure as error:
            self.api.reject(error.code, error.message)
        except OSError:
            self.api.reject("outputs_unsafe", "Result storage cannot be opened safely")

    @contextlib.contextmanager
    def open_dir(self, path, parent=None):
        # Spark's controller runs on Linux; do not silently weaken dirfd fencing.
        if os.name != "posix":
            fail("outputs_platform", "Spark result collection requires Linux")
        fd = os.open(path, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=parent)
        try:
            yield fd
        finally:
            os.close(fd)

    @contextlib.contextmanager
    def lock(self, job, shared=False):
        import fcntl
        self.api.identifier(job)
        with self.open_dir(self.root) as parent:
            fd = os.open(job + ".lock", os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW | os.O_NONBLOCK, 0o600, dir_fd=parent)
            try:
                info = os.fstat(fd)
                if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1 or info.st_uid != os.getuid():
                    fail("outputs_unsafe", "Result collection lock is not a protected regular file")
                try:
                    fcntl.flock(fd, (fcntl.LOCK_SH if shared else fcntl.LOCK_EX) | fcntl.LOCK_NB)
                except BlockingIOError:
                    yield False
                else:
                    yield True
            finally:
                os.close(fd)

    def step(self, job=None):
        """Bounded dispatch only. No directory walks, file hashes or Docker calls."""
        now = time.time()
        with self.c.transaction():
            # At most one live collector. A stale heartbeat permits restart;
            # the OS lock still fences a merely slow original collector.
            if self.c.db.execute("SELECT 1 FROM job_outputs WHERE state='collecting' AND updated>?", (now - RETRY_SECONDS,)).fetchone():
                return
            query = """SELECT o.job,o.state FROM job_outputs o JOIN jobs j ON j.id=o.job
                WHERE j.state IN ('succeeded','failed','cancelled') AND
                ((o.state IN ('pending','collecting','error') AND o.dispatch<?)
                  OR (o.state='ready' AND o.expires<=?))"""
            args = [now - RETRY_SECONDS, now]
            if job is not None:
                query += " AND o.job=?"
                args.append(job)
            row = self.c.db.execute(query + " ORDER BY o.updated LIMIT 1", args).fetchone()
            if not row:
                return
            self.c.db.execute("UPDATE job_outputs SET dispatch=?,updated=?,state=? WHERE job=?",
                              (now, now, "collecting" if row[1] != "ready" else "ready", row[0]))
        self.spawn(row[0])

    def _excluded(self, scope, parts):
        if scope == "output" and parts == ("stdout.log",):
            return True
        folded = [p.casefold() for p in parts]
        if any(".spark-original-" in p or ".spark-return-" in p for p in folded):
            return True
        if any(p in self.api.DENY_DIRS or p in EXCLUDED for p in folded):
            return True
        return (Path(parts[-1]).suffix.casefold() in self.api.DENY_SUFFIX or
                (self.api.DENY_NAMES.fullmatch(parts[-1]) and folded[-1] not in {".env.example", ".env.sample", ".env.template", ".env.defaults"}))

    def _path(self, parts):
        path = "/".join(parts)
        if len(parts) > MAX_DEPTH or len(path.encode("utf-8")) > 2048 or any(
                not p or p in (".", "..") or p[-1:] in (".", " ") or WINDOWS_DEVICE.fullmatch(p) or
                any(ord(c) < 32 or c in '\\:<>"|?*' for c in p) for p in parts):
            fail("outputs_unsafe", "A result filename is not a safe portable relative path")
        return path

    def _files(self, base_fd, scope, originals):
        """Iterative walk with no-follow directory descriptors and bounded depth."""
        stack = [(os.dup(base_fd), (), None)]
        seen = 0
        try:
            while stack:
                fd, parts, iterator = stack[-1]
                if iterator is None:
                    iterator = os.scandir(fd)
                    stack[-1] = fd, parts, iterator
                try:
                    item = next(iterator)
                except StopIteration:
                    iterator.close()
                    os.close(fd)
                    stack.pop()
                    continue
                seen += 1
                if seen > WALK_MAX:
                    fail("outputs_quota", "The result tree has too many entries to inspect safely. Exclude generated caches or reduce the number of files.")
                child = parts + (item.name,)
                if self._excluded(scope, child):
                    continue
                path = self._path(child)
                info = os.stat(item.name, dir_fd=fd, follow_symlinks=False)
                if stat.S_ISDIR(info.st_mode):
                    next_fd = os.open(item.name, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=fd)
                    if (os.fstat(next_fd).st_dev, os.fstat(next_fd).st_ino) != (info.st_dev, info.st_ino):
                        os.close(next_fd)
                        fail("outputs_changed", "A result directory changed while it was collected. Retry after the job has stopped.")
                    stack.append((next_fd, child, None))
                elif stat.S_ISLNK(info.st_mode):
                    original = originals.get(path) if scope == "work" else None
                    if original and original["size"] > self.api.LARGE_FILE and os.readlink(item.name, dir_fd=fd) == "/sparkdev-source/" + path:
                        continue  # Controller-created, immutable large input, never dereferenced.
                    fail("outputs_unsafe", "Result files contain a symbolic link. Replace it with an ordinary file before returning results.")
                elif not stat.S_ISREG(info.st_mode) or info.st_nlink != 1:
                    fail("outputs_unsafe", "Result files contain a hard link or a special file. Only ordinary files can be returned.")
                else:
                    yield fd, item.name, path, info
        finally:
            for fd, _, iterator in stack:
                if iterator is not None:
                    iterator.close()
                os.close(fd)

    def _copy(self, fd, name, info, destination, remaining, heartbeat, original=None):
        # Compare existing uploaded files before staging them: unchanged inputs
        # consume neither the result quota nor another full disk copy.
        if original and original["size"] == info.st_size:
            result = self._copy(fd, name, info, None, FILE_BYTES, heartbeat)
            if result[1] == original["sha256"]:
                return result
        if info.st_size > FILE_BYTES:
            fail("outputs_quota", "A result file exceeds the 64 GiB per-file limit. Split it into smaller files.")
        if destination is not None and self.c.free_bytes() - info.st_size < self.api.DISK_STOP_FLOOR:
            fail("outputs_disk_full", "Spark storage is too full to safely freeze result files. Free space and retry.")
        digest, chunks, tail, size = hashlib.sha256(), [], b"", 0
        source_fd = os.open(name, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=fd)
        with os.fdopen(source_fd, "rb") as source:
            opened = os.fstat(source.fileno())
            if not stat.S_ISREG(opened.st_mode) or opened.st_nlink != 1 or identity(opened) != identity(info):
                fail("outputs_changed", "A result file changed while it was collected. Retry after the job has stopped.")
            with destination.open("xb") if destination is not None else contextlib.nullcontext() as target:
                while True:
                    raw = source.read(CHUNK)
                    if not raw:
                        break
                    size += len(raw)
                    if size > FILE_BYTES or size > remaining:
                        fail("outputs_quota", "Result files exceed the 64 GiB per-job limit. Remove unnecessary outputs or split this job.")
                    if self.api.SECRET.search(tail + raw):
                        fail("outputs_secret", "A result file contains credential material and was not returned. Remove the credential before trying again.")
                    tail = raw[-65536:]
                    digest.update(raw)
                    chunks.append(hashlib.sha256(raw).hexdigest())
                    if target is not None:
                        target.write(raw)
                    heartbeat()
                    if target is not None and len(chunks) % 64 == 0 and self.c.free_bytes() < self.api.DISK_STOP_FLOOR:
                        fail("outputs_disk_full", "Spark storage is too full to safely freeze result files. Free space and retry.")
                if target is not None:
                    target.flush()
                    os.fsync(target.fileno())
            if identity(os.fstat(source.fileno())) != identity(opened) or size != info.st_size:
                fail("outputs_changed", "A result file changed while it was collected. Retry after the job has stopped.")
        if destination is not None:
            os.chmod(destination, 0o400)
        return size, digest.hexdigest(), chunks

    def collect(self, job):
        """Detached collector entry point; a retry never publishes partial files."""
        self.api.identifier(job)
        temporary = None
        try:
            with self.lock(job) as locked:
                if not locked:
                    return
                row = self.c.get_job(job)
                saved = self.metadata(row)
                if saved["state"] == "expired":
                    # Recheck with an immediate transaction: a cache-hit admission
                    # may have extended retention while we acquired the lock.
                    with self.c.transaction():
                        if self.metadata(row)["state"] != "expired":
                            return
                        self.c.db.execute("UPDATE job_outputs SET state='expired',manifest=NULL WHERE job=?", (job,))
                    self.api.remove_tree(self.root / job)
                    return
                if row["state"] not in self.api.TERMINAL or saved["state"] in ("disabled", "ready"):
                    return
                info = self.c.docker.inspect(self.c.container_name(job))
                if info is not None:
                    self.c.container_matches(info, row)
                    if info.get("State", {}).get("Status") not in ("exited", "dead"):
                        fail("outputs_running", "The workload container has not stopped. Results will be collected after it stops.")
                if self.c.free_bytes() < self.api.DISK_STOP_FLOOR:
                    fail("outputs_disk_full", "Spark storage is too full to safely freeze result files. Free space and retry.")
                now = time.time()
                self.c.db.execute("UPDATE job_outputs SET state='collecting',updated=?,error=NULL WHERE job=?", (now, job))
                last = [now]
                def heartbeat():
                    at = time.time()
                    if at - last[0] >= HEARTBEAT_SECONDS:
                        self.c.db.execute("UPDATE job_outputs SET updated=? WHERE job=?", (at, job))
                        last[0] = at
                originals = {e["path"]: e for e in self.c.manifest_entries(row["manifest"])}
                # Crash leftovers are private, unmounted trees. This job's lock
                # is held while reclaiming them; each new attempt has its own name.
                for leftover in self.root.glob(job + ".pending-*"):
                    self.api.remove_tree(leftover)
                temporary = self.root / (job + ".pending-" + uuid.uuid4().hex)
                temporary.mkdir(mode=0o700)
                entries, total, paths = [], 0, {}
                manifest_bytes = len(self.api.canonical({"v": 1, "entries": []}))
                with self.open_dir(self.c.root / "jobs") as jobs, self.open_dir(job, jobs) as job_fd:
                    for scope, directory in (("work", "work"), ("output", "output")):
                        try:
                            base_fd = os.open(directory, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=job_fd)
                        except FileNotFoundError:
                            continue  # A cancelled queued job may never have launched.
                        try:
                            for parent, name, path, info in self._files(base_fd, scope, originals):
                                destination = temporary / str(len(entries))
                                original = originals.get(path) if scope == "work" else None
                                size, digest, chunks = self._copy(parent, name, info, destination, TOTAL_BYTES - total, heartbeat, original)
                                heartbeat()
                                if original and digest == original["sha256"]:
                                    destination.unlink(missing_ok=True)
                                    continue
                                pieces = path.split("/")
                                for depth in range(1, len(pieces) + 1):
                                    prefix = "/".join(pieces[:depth])
                                    key = scope, prefix.casefold()
                                    if key in paths and paths[key] != prefix:
                                        fail("outputs_unsafe", "Result filenames collide on the laptop's case-insensitive filesystem")
                                    paths[key] = prefix
                                entry = {"scope": scope, "path": path, "bytes": size, "sha256": digest, "chunks": chunks,
                                         "original_sha256": original["sha256"] if original else None}
                                manifest_bytes += len(self.api.canonical(entry)) + int(bool(entries))
                                entries.append(entry)
                                total += size
                                if len(entries) > MAX_FILES or manifest_bytes > MANIFEST_BYTES:
                                    fail("outputs_quota", "The result manifest exceeds its file or chunk limit. Split this job into smaller result sets.")
                        finally:
                            os.close(base_fd)
                manifest = {"v": 1, "entries": entries}
                raw = self.api.canonical(manifest)
                manifest_digest = hashlib.sha256(raw).hexdigest()
                self.api.atomic_write(temporary / "manifest.json", raw, 0o400)
                with self.open_dir(temporary) as snapshot_fd:
                    os.fsync(snapshot_fd)
                # The private staging directory is never mounted in a container.
                self.api.remove_tree(self.root / job)
                os.replace(temporary, self.root / job)
                temporary = None
                with self.open_dir(self.root) as results_fd:
                    os.fsync(results_fd)
                with self.c.transaction():
                    self.c.db.execute("UPDATE job_outputs SET state='ready',manifest=?,manifest_sha256=?,total_bytes=?,files=?,updated=?,expires=?,error=NULL WHERE job=?",
                                      (raw.decode("utf-8"), manifest_digest, total, len(entries), time.time(), time.time() + UNACKNOWLEDGED_SECONDS, job))
                    self.c.audit("outputs_ready", job, row["client"], files=len(entries), bytes=total, manifest_sha256=manifest_digest)
        except (OutputFailure, self.api.Rejected, OSError, ValueError) as error:
            code = error.code if isinstance(error, (OutputFailure, self.api.Rejected)) else "outputs_io"
            message = error.message if isinstance(error, OutputFailure) else "Spark result files could not be frozen safely. The workload exit code is unchanged; result collection will retry."
            self.c.db.execute("UPDATE job_outputs SET state='error',updated=?,error=? WHERE job=? AND state NOT IN ('ready','expired','disabled')",
                              (time.time(), self.api.canonical({"code": code, "message": message}).decode(), job))
        finally:
            if temporary is not None:
                self.api.remove_tree(temporary)
