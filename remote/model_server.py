"""Durable, owner-only model service calls. The controller supplies its API explicitly.

Container-owned replies are untrusted. Nothing here opens a listener or imports
controller (which also runs as __main__); authentication stays with its transport.
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
import time
import uuid

NAME = re.compile(r"^[a-z][a-z0-9-]{0,23}$")
CALL_ID = re.compile(r"^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$")
HTTP_PATH = re.compile(r"^/[!-~]{0,1023}$")
INPUT_LIMIT = 1024 * 1024
REPLY_LIMIT = 256 * 1024 * 1024
INLINE_LIMIT = 2 * 1024 * 1024
SPOOL_BYTES = 2 * 1024 * 1024 * 1024
SPOOL_ENTRIES = 10000
LIFETIME = 8 * 3600
STARTUP = 30 * 60
WAKE_REASONS = {"idle", "lifetime", "internet_off"}
NORMAL_STOPS = {"idle", "lifetime", "replaced", "stopped", "disk_full", "internet_off"}
ACTIVE = ("queued", "starting", "running")


def is_service(row):
    return "kind" in row.keys() and row["kind"] == "service"


def status_fields(row):
    if not is_service(row):
        return {"kind": "job"}
    state = ("ready" if row["ready_at"] is not None else
             "waiting for memory" if row["state"] == "queued" else "loading")
    return {"kind": "service", "name": row["name"], "port": row["port"],
            "idle_seconds": row["idle_seconds"], "ready_at": row["ready_at"],
            "last_activity": row["last_activity"], "stop_reason": row["stop_reason"],
            "service_state": state if row["state"] in ACTIVE else row["state"]}


class ServiceManager:
    def __init__(self, controller, api, *, now=None, sleep=None):
        self.c, self.a = controller, api
        self.db = controller.db
        self.now, self.sleep = now or time.time, sleep or time.sleep
        self.last_housekeeping = None
        self.last_space_check = {}
        self.space_full = set()
        self.migrate()

    def migrate(self):
        columns = {r[1] for r in self.db.execute("PRAGMA table_info(jobs)")}
        for name, sql in (("kind", "TEXT NOT NULL DEFAULT 'job'"), ("name", "TEXT"),
                          ("port", "INTEGER"), ("idle_seconds", "INTEGER"),
                          ("ready_at", "REAL"), ("last_activity", "REAL"),
                          ("stop_reason", "TEXT")):
            if name not in columns:
                self.db.execute(f"ALTER TABLE jobs ADD COLUMN {name} {sql}")
        self.db.executescript("""
          CREATE UNIQUE INDEX IF NOT EXISTS active_service_name ON jobs(name)
            WHERE kind='service' AND state IN ('queued','starting','running') AND cancel=0;
          CREATE TABLE IF NOT EXISTS calls(
            id TEXT PRIMARY KEY, job TEXT NOT NULL, name TEXT NOT NULL, client TEXT NOT NULL,
            body_sha TEXT NOT NULL, path TEXT NOT NULL, input_type TEXT NOT NULL,
            state TEXT NOT NULL, created REAL NOT NULL, done_at REAL, delivered_at REAL,
            status INTEGER, content_type TEXT, size INTEGER, sha256 TEXT,
            retargeted INTEGER NOT NULL DEFAULT 0, error TEXT);
          CREATE INDEX IF NOT EXISTS service_calls_job ON calls(job,state);
          CREATE TABLE IF NOT EXISTS service_requests(
            request_id TEXT PRIMARY KEY, job TEXT NOT NULL, request TEXT NOT NULL);
        """)

    def name(self, value):
        if not isinstance(value, str) or not NAME.fullmatch(value) or value.startswith("laptop"):
            self.a.reject("invalid_request", "Server name must use 1-24 lowercase letters, digits or hyphens and cannot begin with laptop")
        return value

    def call_id(self, value):
        if not isinstance(value, str) or not CALL_ID.fullmatch(value):
            self.a.reject("invalid_request", "Call request ID must use 1-64 letters, digits, underscores or hyphens")
        return value

    def content_type(self, value):
        if not isinstance(value, str) or not 1 <= len(value) <= 128 or any(not 32 <= ord(c) <= 126 for c in value):
            self.a.reject("invalid_request", "Content type must be printable ASCII up to 128 characters")
        return value

    def resolve(self, name, live=False):
        self.name(name)
        sql = "SELECT * FROM jobs WHERE kind='service' AND name=?"
        if live:
            sql += " AND state IN ('queued','starting','running') AND cancel=0"
        return self.db.execute(sql + " ORDER BY (state IN ('queued','starting','running') AND cancel=0) DESC,created DESC,id DESC LIMIT 1", (name,)).fetchone()

    def spool(self, job):
        return self.c.root / "jobs" / self.a.identifier(job) / "spool"

    def make_spool(self, job):
        spool = self.spool(job)
        spool.mkdir(mode=0o700, exist_ok=True)
        for name in ("requests", "replies", "collected"):
            path = spool / name
            if path.is_symlink():
                self.a.reject("unsafe_state", "Server spool is not a directory")
            path.mkdir(mode=0o700, exist_ok=True)
        return spool

    def serve(self, request):
        self.a.fields(request, ("v", "op", "project", "manifest_id", "argv", "client", "request_id", "name"),
                      ("runtime", "port", "memory_gb", "idle_minutes", "replace"))
        project, manifest = self.a.digest(request["project"]), self.a.digest(request["manifest_id"])
        client, request_id = self.a.client_name(request["client"]), self.a.identifier(request["request_id"])
        name = self.name(request["name"])
        args = request["argv"]
        if not isinstance(args, list) or not 1 <= len(args) <= 1024 or any(not isinstance(a, str) or "\0" in a for a in args) or not args[0] or sum(len(a.encode()) for a in args) > 65536:
            self.a.reject("invalid_argv", "Command must be a bounded non-empty array of strings")
        if self.a.SECRET.search(self.a.canonical(args)):
            self.a.reject("secret_detected", "Command contains credential material")
        if type(request.get("replace", False)) is not bool:
            self.a.reject("invalid_request", "replace must be boolean")
        registry = self.c.registry()
        runtime_name = request.get("runtime", "cuda" if "cuda" in registry else "python")
        if runtime_name not in registry:
            self.a.reject("runtime_unavailable", "Requested runtime has not been provisioned")
        limits = self.c.resource_policy()["limits"]
        if limits["cpu_total"] < 2:
            self.a.reject("resource_policy_exceeded", "A model server needs two CPU cores within the owner total budget")
        memory = self.a.integer(request.get("memory_gb", 16), 1, min(48, limits["memory_gb_total"]))
        port = self.a.integer(request.get("port", 8000), 1, 65535)
        idle = self.a.integer(request.get("idle_minutes", 30), 5, 240) * 60
        normalized = {**request, "runtime": runtime_name, "memory_gb": memory, "port": port,
                      "idle_minutes": idle // 60, "replace": request.get("replace", False)}
        encoded = self.a.canonical(normalized).decode()
        with self.c.transaction():
            alias = self.db.execute("SELECT * FROM service_requests WHERE request_id=?", (request_id,)).fetchone()
            if alias:
                if alias["request"] != encoded:
                    self.a.reject("idempotency_conflict", "Request ID was already used for a different request")
                return self.c.job_status(self.c.get_job(alias["job"]))
            old = self.db.execute("SELECT * FROM jobs WHERE request_id=?", (request_id,)).fetchone()
            if old:
                if old["request"] != encoded:
                    self.a.reject("idempotency_conflict", "Request ID was already used for a different request")
                return self.c.job_status(old)
            if not self.db.execute("SELECT 1 FROM projects WHERE project=? AND manifest=?", (project, manifest)).fetchone():
                self.a.reject("manifest_not_found", "Sync this manifest before submitting the server")
            if self.db.execute("SELECT COUNT(*) FROM jobs").fetchone()[0] >= self.a.MAX_JOBS:
                self.a.reject("capacity", "Retained job capacity reached; owner must archive receipts")
            active = self.resolve(name, live=True)
            if active:
                # New request IDs for the same immutable definition do not start duplicates.
                previous = json.loads(active["request"])
                shape = lambda r: {k: v for k, v in r.items() if k not in {"request_id", "replace"}}
                if shape(previous) == shape(normalized):
                    self.db.execute("INSERT INTO service_requests VALUES(?,?,?)", (request_id, active["id"], encoded))
                    return self.c.job_status(active)
                if not normalized["replace"] and (active["client"] != client or active["project"] != project):
                    self.a.reject("server_name_in_use", f"{active['client']} is using {name} for a different server. Pick another name or add --replace.")
            count = self.db.execute("SELECT COUNT(*) FROM jobs WHERE kind='service' AND state IN ('queued','starting','running') AND cancel=0").fetchone()[0]
            if count - bool(active) >= 2:
                self.a.reject("too_many_servers", "At most two model servers can be active")
            if active:
                self.db.execute("UPDATE jobs SET cancel=1,stop_reason='replaced' WHERE id=?", (active["id"],))
                if active["state"] == "queued":
                    self.db.execute("UPDATE jobs SET state='cancelled',finished=?,exit_code=130 WHERE id=?", (self.now(), active["id"]))
                self.c.audit("service_replaced", active["id"], client)
            job, now = uuid.uuid4().hex, self.now()
            root = self.c.root / "jobs" / job
            root.mkdir(mode=0o700)
            self.a.atomic_write(root / "worker.py", Path(__file__).with_name("worker.py").read_bytes(), 0o444)
            self.make_spool(job)
            self.db.execute("""INSERT INTO jobs(id,request_id,request,project,manifest,runtime,image,client,state,created,
                cache_key,cpu,memory,scratch_mb,headroom_gb,kind,name,port,idle_seconds,last_activity)
                VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)""",
                (job, request_id, encoded, project, manifest, runtime_name, registry[runtime_name]["image"], client,
                 "queued", now, self.a.sha(job.encode()), 2, memory, 2048, limits["headroom_gb"], "service", name, port, idle, now))
            self.db.execute("INSERT INTO service_requests VALUES(?,?,?)", (request_id, job, encoded))
            # Use exactly the same network snapshot and helper staging as run().
            network = self.c.job_network()
            self.c.stage_network(root, network)
            self.db.execute("UPDATE jobs SET egress=?,proxy_image=? WHERE id=?", (network["egress"], network["proxy_image"], job))
            self.c.audit("service_queued", job, client, name=name, memory_gb=memory)
        return self.c.job_status(self.c.get_job(job))

    def create_args(self, row, args):
        if not is_service(row):
            return args
        spool = self.make_spool(row["id"])
        for i, value in enumerate(args):
            if i and args[i-1] == "--mount" and any(f",dst={dest}" in value for dest in ("/target", "/cargo-home", "/npm-cache", "/pip-cache")) and not value.endswith(",readonly"):
                args[i] += ",readonly"
        args += ["--mount", f"type=bind,src={spool / 'requests'},dst=/sparkdev-requests,readonly",
                 "--mount", f"type=bind,src={spool / 'replies'},dst=/sparkdev-replies"]
        for value in ("SPARKDEV_JOB_KIND=service", f"PORT={row['port']}", f"SPARKDEV_SERVER_LIFETIME={LIFETIME}",
                      "NO_PROXY=localhost,127.0.0.1,::1", "no_proxy=localhost,127.0.0.1,::1"):
            args += ["--env", value]
        return args

    def cancel_name(self, request):
        self.a.fields(request, ("v", "op", "name"))
        with self.c.transaction():
            row = self.resolve(request["name"], live=True)
            if row is None:
                self.a.reject("server_not_found", "No live server has this name")
            self.db.execute("UPDATE jobs SET cancel=1,stop_reason='stopped' WHERE id=?", (row["id"],))
            if row["state"] == "queued":
                self.db.execute("UPDATE jobs SET state='cancelled',finished=?,exit_code=130 WHERE id=?", (self.now(), row["id"]))
            self.c.audit("service_stop_requested", row["id"], row["client"])
        return self.c.job_status(self.c.get_job(row["id"]))

    def waiting(self, job):
        return self.db.execute("SELECT COUNT(*) FROM calls WHERE job=? AND state='waiting'", (job,)).fetchone()[0]

    def reserved_for_headroom(self, active):
        return sum(self.a.reserved_memory(r) for r in active if not (is_service(r) and r["ready_at"] is not None))

    def admit(self, active, limits):
        """Called within the scheduler transaction, before per-client batch admission."""
        used_cpu = sum(self.a.reserved_cpu(r) for r in active)
        used_memory = sum(self.a.reserved_memory(r) for r in active)
        reserved = self.reserved_for_headroom(active)
        for row in self.db.execute("SELECT * FROM jobs WHERE kind='service' AND state='queued' AND cancel=0 ORDER BY created,id").fetchall():
            if (used_cpu + self.a.reserved_cpu(row) <= limits["cpu_total"] and
                used_memory + self.a.reserved_memory(row) <= limits["memory_gb_total"] and
                self.c.memory_probe() - reserved - self.a.reserved_memory(row) >= limits["headroom_gb"] and
                self.c.disk_admits(row)):
                self.db.execute("UPDATE jobs SET state='starting' WHERE id=?", (row["id"],))
                return True
        return False

    def _open(self, path, maximum):
        # lstat closes Windows' missing O_NOFOLLOW gap; compare the opened inode too.
        before = os.lstat(path)
        if not stat.S_ISREG(before.st_mode) or before.st_nlink != 1 or before.st_size > maximum:
            self.a.reject("unsafe_reply", "Server reply is not a bounded regular file")
        flags = os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_NONBLOCK", 0) | getattr(os, "O_BINARY", 0)
        fd = os.open(path, flags)
        after = os.fstat(fd)
        if not stat.S_ISREG(after.st_mode) or after.st_nlink != 1 or after.st_size > maximum or (before.st_dev, before.st_ino) != (after.st_dev, after.st_ino):
            os.close(fd)
            self.a.reject("unsafe_reply", "Server reply changed while opening")
        return fd

    def _ready(self, row):
        if row["ready_at"] is not None:
            return
        try:
            info = os.lstat(self.spool(row["id"]) / "replies" / "_ready")
        except FileNotFoundError:
            return
        if stat.S_ISREG(info.st_mode) and info.st_nlink == 1 and info.st_size <= 4096:
            self.db.execute("UPDATE jobs SET ready_at=?,last_activity=? WHERE id=? AND ready_at IS NULL", (self.now(), self.now(), row["id"]))

    def _space_exceeded(self, job):
        now = self.now()
        if now - self.last_space_check.get(job, -1e30) < 10:
            return job in self.space_full
        self.last_space_check[job] = now
        size = count = 0
        for sub in ("replies", "collected"):
            try:
                with os.scandir(self.spool(job) / sub) as entries:
                    for entry in entries:
                        info = entry.stat(follow_symlinks=False)
                        count += 1
                        size += info.st_size
                        if count > SPOOL_ENTRIES or size > SPOOL_BYTES or stat.S_ISDIR(info.st_mode):
                            self.space_full.add(job)
                            return True
            except FileNotFoundError:
                pass
        self.space_full.discard(job)
        return False

    def reconcile(self, row, info):
        """Run after container identity verification; return True when stopping handled it."""
        if not is_service(row):
            return False
        spool_error = False
        try:
            self._ready(row)
        except (OSError, ValueError, self.a.Rejected):
            # The workload can chmod its writable mount. An unreadable spool
            # must not prevent cancellation, the watchdog or other jobs running.
            spool_error = True
        row = self.c.get_job(row["id"])
        now, started = self.now(), row["started"]
        reason = row["stop_reason"] if row["cancel"] else None
        if row["cancel"] and not reason:
            reason = "stopped"
        elif started is not None and now - started > LIFETIME:
            reason = "lifetime"
        elif started is not None and row["ready_at"] is None and now - started > STARTUP:
            reason = "never_ready"
        elif row["ready_at"] is not None and not self.waiting(row["id"]) and now - max(row["ready_at"], row["last_activity"] or 0) > row["idle_seconds"]:
            reason = "idle"
        if not reason:
            try:
                if spool_error or self._space_exceeded(row["id"]):
                    reason = "reply_space_full"
            except (OSError, ValueError, self.a.Rejected):
                reason = "reply_space_full"
        if not reason and self.c.free_bytes() < self.a.DISK_RESERVE:
            newest = self.db.execute("SELECT id FROM jobs WHERE kind='service' AND state IN ('starting','running') ORDER BY started DESC,created DESC LIMIT 1").fetchone()
            if newest and newest[0] == row["id"]:
                reason = "disk_full"
        if not reason:
            return False
        if info["State"]["Status"] in ("running", "restarting", "paused"):
            self.c.docker.call(["stop", "--time", "10", self.c.container_name(row["id"])], timeout=20)
        self.db.execute("UPDATE jobs SET stop_reason=?,cancel=1 WHERE id=?", (reason, row["id"]))
        good = reason in NORMAL_STOPS
        self.c.finish(row, "cancelled" if good else "failed", 130 if good else 137, None if good else reason)
        return True

    def _write_request(self, row, raw, publish=True):
        spool = self.make_spool(row["job"]) / "requests"
        body, meta = spool / (row["id"] + ".body"), spool / (row["id"] + ".json")
        # Files become visible complete, body-first. This directory is controller
        # owned and mounted read-only. Submission holds BEGIN IMMEDIATE, so another
        # RPC cannot publish a conflicting request between this check and rename.
        files = [(body, raw)]
        if publish:
            files.append((meta, self.a.canonical({"id": row["id"], "path": row["path"], "content_type": row["input_type"]})))
        for path, data in files:
            if os.path.lexists(path):
                with os.fdopen(self._open(path, max(INPUT_LIMIT, 4096)), "rb") as stream:
                    if stream.read(len(data) + 1) != data:
                        self.a.reject("unsafe_state", "Existing call spool differs from its durable record")
                continue
            temporary = spool / (".pending-" + uuid.uuid4().hex)
            try:
                fd = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_BINARY", 0), 0o400)
                with os.fdopen(fd, "wb") as stream:
                    stream.write(data)
                    stream.flush()
                    os.fsync(stream.fileno())
                os.rename(temporary, path)
            finally:
                if temporary.exists():
                    if os.name == "nt":
                        os.chmod(temporary, 0o600)
                    temporary.unlink()

    def _publish_request(self, call_id):
        # The calls row (and a retargeted job identity) must be committed before
        # the worker can see executable metadata. This also repairs a controller
        # crash after commit but before metadata publication on the next poll.
        with self.c.transaction():
            row = self.db.execute("SELECT * FROM calls WHERE id=?", (call_id,)).fetchone()
            if not row or row["state"] != "waiting":
                return
            spool = self.spool(row["job"]) / "requests"
            if os.path.lexists(spool / (call_id + ".json")):
                return
            with os.fdopen(self._open(spool / (call_id + ".body"), INPUT_LIMIT), "rb") as stream:
                raw = stream.read(INPUT_LIMIT + 1)
            if self.a.sha(raw) != row["body_sha"]:
                self.a.reject("unsafe_state", "Call request body differs from its durable record")
            self._write_request(row, raw)

    def _wake(self, name, previous=None):
        live = self.resolve(name, live=True)
        if live:
            return live
        old = previous or self.resolve(name)
        if old is None:
            self.a.reject("server_not_found", "Start this server with sparkdev serve before calling it")
        if old["stop_reason"] not in WAKE_REASONS or old["finished"] is None or self.now() - old["finished"] > 86400:
            self.a.reject("server_stopped", f"Ben stopped {name}. Start it again with sparkdev serve.")
        snapshot = self.c.snapshots / old["manifest"]
        if snapshot.is_symlink() or not snapshot.is_dir():
            self.a.reject("manifest_not_found", "Server snapshot expired. Sync and start it again with sparkdev serve.")
        request = json.loads(old["request"])
        request["request_id"] = "wake-" + self.a.sha(old["id"].encode())[:40]
        request["replace"] = False
        with self.c.runtime_gate():
            result = self.serve(request)
        return self.c.get_job(result["job"])

    def _collect(self, row):
        # Serialize publication of the controller-owned copy with its durable
        # digest. A second collector may not replace it after the first commits.
        with self.c.transaction():
            row = self.db.execute("SELECT * FROM calls WHERE id=?", (row["id"],)).fetchone()
            return self._collect_locked(row)

    def _collect_locked(self, row):
        if row["state"] != "waiting":
            return
        spool = self.spool(row["job"])
        meta, body = spool / "replies" / (row["id"] + ".json"), spool / "replies" / (row["id"] + ".body")
        try:
            with os.fdopen(self._open(meta, 4096), "rb") as stream:
                payload = stream.read(4097)
            if len(payload) > 4096:
                self.a.reject("unsafe_reply", "Server reply metadata is too large")
            data = json.loads(payload)
            if not isinstance(data, dict):
                self.a.reject("unsafe_reply", "Server reply metadata must be an object")
            status = self.a.integer(data.get("status"), 100, 599)
            content_type = self.content_type(data.get("content_type", "application/octet-stream"))
            fd = self._open(body, REPLY_LIMIT)
        except FileNotFoundError:
            return
        # Copy into a directory that is never mounted in the container. Reply chunks
        # subsequently use this immutable controller-owned copy, not mutable workload bytes.
        target = spool / "collected" / (row["id"] + ".body")
        temporary = target.with_suffix(".pending-" + uuid.uuid4().hex)
        try:
            hasher, size = hashlib.sha256(), 0
            with os.fdopen(fd, "rb") as source, open(temporary, "xb") as output:
                before = os.fstat(source.fileno())
                self.c.require_disk(before.st_size)
                while True:
                    block = source.read(min(1024 * 1024, REPLY_LIMIT + 1 - size))
                    if not block:
                        break
                    size += len(block)
                    if size > REPLY_LIMIT:
                        self.a.reject("unsafe_reply", "Server reply exceeded its size limit")
                    hasher.update(block)
                    output.write(block)
                after = os.fstat(source.fileno())
                if (before.st_size, before.st_mtime_ns, before.st_nlink) != (after.st_size, after.st_mtime_ns, after.st_nlink) or size != before.st_size:
                    self.a.reject("unsafe_reply", "Server reply changed during collection")
                if "size" in data and (type(data["size"]) is not int or data["size"] != size):
                    self.a.reject("unsafe_reply", "Server reply metadata size does not match its body")
                output.flush()
                os.fsync(output.fileno())
            os.chmod(temporary, 0o400)
            if os.name == "nt" and target.exists():
                os.chmod(target, 0o600)
            os.replace(temporary, target)
            now = self.now()
            self.db.execute("UPDATE calls SET state='done',done_at=?,status=?,content_type=?,size=?,sha256=? WHERE id=? AND state='waiting'",
                            (now, status, content_type, size, hasher.hexdigest(), row["id"]))
            self.db.execute("UPDATE jobs SET last_activity=? WHERE id=?", (now, row["job"]))
        finally:
            with contextlib.suppress(FileNotFoundError):
                temporary.unlink()

    def _refresh(self, call_id):
        row = self.db.execute("SELECT * FROM calls WHERE id=?", (call_id,)).fetchone()
        if row is None:
            self.a.reject("call_not_found", "Call request ID does not exist")
        if row["state"] == "waiting":
            try:
                self._publish_request(call_id)
                self._collect(row)
            except (self.a.Rejected, OSError, ValueError, TypeError):
                self.db.execute("UPDATE calls SET state='failed',done_at=?,error='unsafe_reply' WHERE id=? AND state='waiting'", (self.now(), call_id))
            row = self.db.execute("SELECT * FROM calls WHERE id=?", (call_id,)).fetchone()
        if row["state"] == "waiting":
            job = self.c.get_job(row["job"])
            if job["state"] in ACTIVE and job["stop_reason"] == "lifetime" and job["cancel"]:
                return row  # The scheduler must finish stopping before a wake can start.
            if job["state"] not in ACTIVE or job["cancel"]:
                if job["stop_reason"] == "lifetime" and not row["retargeted"] and self.now() - row["created"] < 3600:
                    try:
                        replacement = self._wake(row["name"], job)
                        # Never retarget to unrelated code that claimed the name meanwhile.
                        original = json.loads(job["request"])
                        current = json.loads(replacement["request"])
                        signature = lambda d: {k: v for k, v in d.items() if k not in {"request_id", "replace"}}
                        if signature(original) != signature(current):
                            self.a.reject("server_stopped", "A different server now owns this name")
                        path = self.spool(row["job"]) / "requests" / (row["id"] + ".body")
                        with os.fdopen(self._open(path, INPUT_LIMIT), "rb") as stream:
                            raw = stream.read(INPUT_LIMIT + 1)
                        if self.a.sha(raw) != row["body_sha"]:
                            self.a.reject("unsafe_state", "Call request body differs from its durable record")
                        moved = dict(row)
                        moved["job"] = replacement["id"]
                        with self.c.transaction():
                            current_row = self.db.execute("SELECT * FROM calls WHERE id=?", (call_id,)).fetchone()
                            if current_row["state"] == "waiting" and not current_row["retargeted"]:
                                self._write_request(moved, raw, publish=False)
                                self.db.execute("UPDATE calls SET job=?,retargeted=1 WHERE id=?", (replacement["id"], call_id))
                                self.db.execute("UPDATE jobs SET last_activity=? WHERE id=?", (self.now(), replacement["id"]))
                        self._publish_request(call_id)
                    except (self.a.Rejected, OSError, ValueError) as error:
                        if isinstance(error, self.a.Rejected) and error.code == "runtime_refresh_in_progress":
                            # The owner can refresh runtimes once the previous
                            # service stops. Keep the durable call waiting until
                            # that temporary gate clears; no request is replayed.
                            return self.db.execute("SELECT * FROM calls WHERE id=?", (call_id,)).fetchone()
                        self.db.execute("UPDATE calls SET state='stopped',done_at=?,error='server_stopped' WHERE id=?", (self.now(), call_id))
                else:
                    self.db.execute("UPDATE calls SET state='stopped',done_at=?,error='server_stopped' WHERE id=?", (self.now(), call_id))
        return self.db.execute("SELECT * FROM calls WHERE id=?", (call_id,)).fetchone()

    def call(self, request):
        self.a.fields(request, ("v", "op", "server", "client", "request_id"), ("body", "content_type", "path", "timeout"))
        name, client, request_id = self.name(request["server"]), self.a.client_name(request["client"]), self.call_id(request["request_id"])
        timeout = self.a.integer(request.get("timeout", 0), 0, 60)
        path, content_type = request.get("path", "/"), self.content_type(request.get("content_type", "application/json"))
        if not isinstance(path, str) or not HTTP_PATH.fullmatch(path):
            self.a.reject("invalid_request", "HTTP path must begin with / and use at most 1024 visible ASCII characters")
        raw = request.get("body", "")
        if not isinstance(raw, str) or len(raw) > ((INPUT_LIMIT + 2) // 3) * 4:
            self.a.reject("size_limit", "Call input is limited to 1 MiB")
        try:
            body = base64.b64decode(raw, validate=True)
        except (ValueError, TypeError):
            self.a.reject("invalid_request", "Call body must be base64")
        if len(body) > INPUT_LIMIT:
            self.a.reject("size_limit", "Call input is limited to 1 MiB")
        if self.a.SECRET.search(body):
            self.a.reject("secret_detected", "Call body contains credential material")
        digest = self.a.sha(body)
        old = self.db.execute("SELECT * FROM calls WHERE id=?", (request_id,)).fetchone()
        if old and (old["name"], old["client"], old["body_sha"], old["path"], old["input_type"]) != (name, client, digest, path, content_type):
            self.a.reject("idempotency_conflict", "Call ID was already used for a different request")
        if not old:
            server = self.resolve(name, live=True) or self._wake(name)
            with self.c.transaction():
                # An independent RPC can submit the same ID while wake is in flight.
                old = self.db.execute("SELECT * FROM calls WHERE id=?", (request_id,)).fetchone()
                if old:
                    if (old["name"], old["client"], old["body_sha"], old["path"], old["input_type"]) != (name, client, digest, path, content_type):
                        self.a.reject("idempotency_conflict", "Call ID was already used for a different request")
                else:
                    if self.waiting(server["id"]) >= 32:
                        self.a.reject("server_busy", "This server already has 32 waiting calls")
                    self.db.execute("INSERT INTO calls(id,job,name,client,body_sha,path,input_type,state,created) VALUES(?,?,?,?,?,?,?,'waiting',?)",
                                    (request_id, server["id"], name, client, digest, path, content_type, self.now()))
                    row = self.db.execute("SELECT * FROM calls WHERE id=?", (request_id,)).fetchone()
                    self._write_request(row, body, publish=False)
                    self.db.execute("UPDATE jobs SET last_activity=? WHERE id=?", (self.now(), server["id"]))
                    self.c.audit("service_call", server["id"], client, request_id=request_id)
        deadline = time.monotonic() + timeout
        while True:
            row = self._refresh(request_id)
            if row["state"] != "waiting" or time.monotonic() >= deadline:
                return self._response(row, 0, INLINE_LIMIT)
            self.sleep(min(0.1, max(0, deadline - time.monotonic())))

    def _response(self, row, since, limit):
        state, data = row["state"], b""
        if state == "waiting":
            job = self.c.get_job(row["job"])
            state = "loading" if job["ready_at"] is None else "waiting"
        if row["state"] == "done":
            size = row["size"]
            if since > size:
                self.a.reject("invalid_offset", "Reply offset is past the response")
            path = self.spool(row["job"]) / "collected" / (row["id"] + ".body")
            try:
                with os.fdopen(self._open(path, REPLY_LIMIT), "rb") as stream:
                    if os.fstat(stream.fileno()).st_size != size:
                        self.a.reject("unsafe_reply", "Stored response size changed")
                    stream.seek(since)
                    data = stream.read(min(limit, size - since))
            except FileNotFoundError:
                self.db.execute("UPDATE calls SET state='expired' WHERE id=?", (row["id"],))
                state = "expired"
            if state == "done" and since + len(data) == size:
                self.db.execute("UPDATE calls SET delivered_at=COALESCE(delivered_at,?) WHERE id=?", (self.now(), row["id"]))
        elif since:
            self.a.reject("invalid_offset", "A response is not yet available")
        messages = {"loading": "The model server is loading or waiting for memory.", "waiting": "The model server is processing this call.",
                    "done": "The model server replied.", "failed": "The server response failed validation.",
                    "stopped": "The model server stopped. Start it again with sparkdev serve.", "expired": "The retained response expired; this request ID will not run again."}
        return {"request_id": row["id"], "server": row["name"], "job": row["job"], "state": state,
                "status": row["status"], "content_type": row["content_type"], "size": row["size"], "sha256": row["sha256"],
                "data": base64.b64encode(data).decode(), "since": since, "next_offset": since + len(data), "message": messages[state]}

    def reply(self, request):
        self.a.fields(request, ("v", "op", "request_id", "since"), ("limit",))
        row = self._refresh(self.call_id(request["request_id"]))
        return self._response(row, self.a.integer(request["since"], 0, REPLY_LIMIT), self.a.integer(request.get("limit", INLINE_LIMIT), 1, INLINE_LIMIT))

    def _unlink_call(self, row):
        spool = self.spool(row["job"])
        for sub, suffixes in (("requests", (".body", ".json")), ("replies", (".body", ".json", ".started")), ("collected", (".body",))):
            for suffix in suffixes:
                path = spool / sub / (row["id"] + suffix)
                with contextlib.suppress(FileNotFoundError, IsADirectoryError):
                    if os.name == "nt" and path.is_file() and not path.is_symlink():
                        os.chmod(path, 0o600)
                    path.unlink()

    def housekeeping(self):
        now = self.now()
        if self.last_housekeeping is not None and now - self.last_housekeeping < 10:
            return
        self.last_housekeeping = now
        for row in self.db.execute("SELECT * FROM calls WHERE created<? OR (delivered_at IS NOT NULL AND delivered_at<?)", (now-3600, now-600)).fetchall():
            self._unlink_call(row)
            self.db.execute("UPDATE calls SET state='expired' WHERE id=?", (row["id"],))
        self.db.execute("DELETE FROM calls WHERE created<?", (now-86400,))
        for job in self.db.execute("SELECT id,state,finished FROM jobs WHERE kind='service'").fetchall():
            spool = self.spool(job["id"])
            if job["state"] not in ACTIVE and job["finished"] is not None and now-job["finished"] > 3600:
                # Retention already unlinked exact call contents above. Remove
                # the directory only once Docker confirms its container is gone.
                if spool.exists() and self.c.docker.inspect(self.c.container_name(job["id"])) is None:
                    self.a.remove_tree(spool)
                continue
            try:
                with os.scandir(spool / "replies") as entries:
                    for entry in entries:
                        if entry.name == "_ready":
                            continue
                        if entry.name.startswith(".pending-"):
                            if entry.stat(follow_symlinks=False).st_mtime < now-3600:
                                with contextlib.suppress(FileNotFoundError, IsADirectoryError):
                                    Path(entry.path).unlink()
                            continue
                        call_id, dot, suffix = entry.name.rpartition(".")
                        if not dot or suffix not in {"body", "json", "started"} or not CALL_ID.fullmatch(call_id):
                            continue
                        if not self.db.execute("SELECT 1 FROM calls WHERE id=? AND job=? AND state<>'expired'", (call_id, job["id"])).fetchone():
                            with contextlib.suppress(FileNotFoundError, IsADirectoryError):
                                Path(entry.path).unlink()
            except FileNotFoundError:
                pass
