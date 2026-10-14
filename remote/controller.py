#!/usr/bin/env python3
"""Owner-only Spark Dev Bridge controller. Transport authentication is external.

SSH invokes `controller.py rpc`; a separate user service invokes `serve`.
Only the scheduler launches containers. Workload argv never becomes a shell string.
"""
from __future__ import annotations

import argparse
import base64
import contextlib
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import sqlite3
import stat
import subprocess
import sys
import tempfile
import time
import uuid

CHUNK = 1024 * 1024
MAX_REQUEST = 32 * CHUNK
MAX_FILES = 100000
MAX_TREE = 20 * 1024**3
MAX_FILE = 4 * 1024**3
MAX_JOBS = 4096
MAX_LOG = 256 * CHUNK
CPUS = 4
MEMORY_GB = 8
HEADROOM_GB = 32
RESOURCE_FIELDS = ("cpu_per_job", "memory_gb_per_job", "cpu_total", "memory_gb_total", "client_jobs", "client_cpu", "headroom_gb")
CLIENTS = ("claude", "codex", "human")
ACTIVE = ("queued", "starting", "running")
TERMINAL = ("succeeded", "failed", "cancelled")
HASH = re.compile(r"^[0-9a-f]{64}$")
IDENTIFIER = re.compile(r"^[A-Za-z0-9][A-Za-z0-9_.-]{0,127}$")
RUNTIME_NAMES = {"rust", "node", "python", "cuda"}
DENY_DIRS = {".git", "target", "node_modules", "dist", "build", "__pycache__",
             ".ssh", ".aws", ".azure", ".kube", ".gnupg", ".docker", "secrets", "credentials"}
DENY_SUFFIX = {".pem", ".key", ".p12", ".pfx", ".jks", ".keystore", ".kdbx", ".ppk", ".ovpn", ".tfvars"}
DENY_NAMES = re.compile(r"^(?:\.env(?:\..*)?|id_(?:rsa|dsa|ecdsa|ed25519)(?:_sk)?(?:\.pub)?|\.netrc|\.npmrc|\.pypirc|\.git-credentials|credentials(?:\..*)?|authorized_keys|known_hosts|nvsync\..*)$", re.I)
SECRET = re.compile(rb"-----BEGIN (?:[A-Z0-9]+ )*PRIVATE KEY(?: BLOCK)?-----|PuTTY-User-Key-File-\d|(?:AKIA|ASIA)[A-Z0-9]{16}|gh[pousr]_[A-Za-z0-9]{30,}|github_pat_[A-Za-z0-9_]{30,}")


class Rejected(Exception):
    def __init__(self, code, message):
        self.code, self.message = code, message


def reject(code, message):
    raise Rejected(code, message)


def canonical(obj):
    return json.dumps(obj, sort_keys=True, separators=(",", ":"), ensure_ascii=False).encode("utf-8")


def sha(data):
    return hashlib.sha256(data).hexdigest()


def digest(value):
    if not isinstance(value, str) or not HASH.fullmatch(value):
        reject("invalid_request", "Expected a lowercase SHA-256 digest")
    return value


def integer(value, minimum, maximum):
    if type(value) is not int or not minimum <= value <= maximum:
        reject("invalid_request", "Integer is outside its allowed bounds")
    return value


def identifier(value):
    if not isinstance(value, str) or not IDENTIFIER.fullmatch(value):
        reject("invalid_request", "Invalid identifier")
    return value


def client_name(value):
    if value not in CLIENTS:
        reject("invalid_request", "Invalid client")
    return value


def fields(obj, required, optional=()):
    if not isinstance(obj, dict) or not set(required) <= obj.keys() or obj.keys() - set(required) - set(optional):
        reject("invalid_request", "Unexpected or missing fields")


def path_name(value):
    if not isinstance(value, str) or len(value.encode("utf-8")) > 2048 or any(ord(c) < 32 for c in value):
        reject("unsafe_path", "Invalid project path")
    parts = value.split("/")
    if not value or "\\" in value or ":" in value or any(p in ("", ".", "..") for p in parts):
        reject("unsafe_path", "Project paths must be normalized relative paths")
    if any(p.casefold() in DENY_DIRS for p in parts) or PurePosixPath(value).suffix.lower() in DENY_SUFFIX:
        reject("secret_or_excluded", "Project contains a refused path")
    if DENY_NAMES.fullmatch(parts[-1]) and parts[-1].lower() not in {".env.example", ".env.sample", ".env.template", ".env.defaults"}:
        reject("secret_or_excluded", "Project contains a credential filename")
    return value


def atomic_write(path, data, mode=0o600):
    path = Path(path)
    fd, temporary = tempfile.mkstemp(prefix=".pending-", dir=path.parent)
    try:
        with os.fdopen(fd, "wb") as stream:
            stream.write(data)
            stream.flush()
            os.fsync(stream.fileno())
        os.chmod(temporary, mode)
        os.replace(temporary, path)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


def safe_open(path, flags=os.O_RDONLY, mode=0o600):
    fd = os.open(path, flags | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_BINARY", 0), mode)
    if not stat.S_ISREG(os.fstat(fd).st_mode):
        os.close(fd)
        reject("unsafe_state", "State file is not regular")
    return fd


def available_gb():
    try:
        for line in Path("/proc/meminfo").read_text().splitlines():
            if line.startswith("MemAvailable:"):
                return int(line.split()[1]) / 1024**2
    except OSError:
        pass
    return 0  # Admission must fail closed on an unsupported host.


def hardware_capacity():
    memory = 0
    try:
        for line in Path("/proc/meminfo").read_text().splitlines():
            if line.startswith("MemTotal:"):
                memory = int(line.split()[1]) // 1024**2
    except (OSError, ValueError):
        pass
    return {"cpu_count": os.cpu_count() or 0, "memory_gb": memory}


def validate_resources(value, hardware):
    fields(value, RESOURCE_FIELDS)
    cpus = integer(hardware["cpu_count"], 1, 65536)
    memory = integer(hardware["memory_gb"], 2, 1048576)
    for name in ("cpu_per_job", "cpu_total", "client_cpu"):
        integer(value[name], 1, cpus)
    for name in ("memory_gb_per_job", "memory_gb_total", "headroom_gb"):
        integer(value[name], 1, memory)
    integer(value["client_jobs"], 1, 64)
    if not value["cpu_per_job"] <= value["client_cpu"] <= value["cpu_total"]:
        reject("invalid_resource_policy", "Per-job CPU must not exceed client CPU, and client CPU must not exceed total CPU")
    if value["memory_gb_per_job"] > value["memory_gb_total"] or value["memory_gb_total"] + value["headroom_gb"] > memory:
        reject("invalid_resource_policy", "Per-job memory must fit total memory; total memory plus headroom must fit detected hardware")
    return dict(value)


class Docker:
    def call(self, args, timeout=30):
        result = subprocess.run(["docker", *args], capture_output=True, timeout=timeout, check=False)
        if result.returncode:
            reject("docker_failed", "Docker operation failed; owner may inspect local service logs")
        return result.stdout

    def inspect(self, name):
        result = subprocess.run(["docker", "container", "inspect", name], capture_output=True, timeout=10, check=False)
        if result.returncode:
            # Only a definite not-found permits a new launch. Daemon failures must never
            # be interpreted as absence, or restart reconciliation could duplicate work.
            if b"No such container" in result.stderr or b"No such object" in result.stderr:
                return None
            reject("docker_unavailable", "Docker state is unavailable")
        return json.loads(result.stdout)[0]

    def network_inspect(self, name):
        result = subprocess.run(["docker", "network", "inspect", name], capture_output=True, timeout=10, check=False)
        if result.returncode:
            if b"not found" in result.stderr or b"No such network" in result.stderr:
                return None
            reject("docker_unavailable", "Docker network state is unavailable")
        return json.loads(result.stdout)[0]


class Controller:
    def __init__(self, root=None, docker=None, memory_probe=None, hardware_probe=None):
        self.root = Path(root or os.environ.get("SPARKDEV_STATE", Path.home() / "sparkdev/state")).absolute()
        self.root.mkdir(parents=True, exist_ok=True, mode=0o700)
        if self.root.is_symlink():
            reject("unsafe_state", "State root must not be a symlink")
        if os.name == "posix":
            if self.root.stat().st_uid != os.getuid():
                reject("unsafe_state", "State root has a different owner")
            os.chmod(self.root, 0o700)
        for name in ("objects", "snapshots", "jobs", "caches", "local"):
            path = self.root / name
            if path.is_symlink():
                reject("unsafe_state", "State directory is a symlink")
            path.mkdir(exist_ok=True, mode=0o700)
        self.db = sqlite3.connect(self.root / "state.sqlite3", timeout=30, isolation_level=None)
        self.db.row_factory = sqlite3.Row
        self.db.execute("PRAGMA journal_mode=WAL")
        self.db.execute("PRAGMA synchronous=FULL")
        self.db.executescript("""
          CREATE TABLE IF NOT EXISTS manifests(id TEXT PRIMARY KEY, manifest TEXT NOT NULL, bytes INTEGER NOT NULL);
          CREATE TABLE IF NOT EXISTS projects(project TEXT NOT NULL, manifest TEXT NOT NULL, PRIMARY KEY(project, manifest));
          CREATE TABLE IF NOT EXISTS jobs(id TEXT PRIMARY KEY, request_id TEXT UNIQUE NOT NULL, request TEXT NOT NULL,
            project TEXT NOT NULL, manifest TEXT NOT NULL, runtime TEXT NOT NULL, image TEXT NOT NULL,
            client TEXT NOT NULL, state TEXT NOT NULL, created REAL NOT NULL, started REAL, finished REAL,
            exit_code INTEGER, cancel INTEGER NOT NULL DEFAULT 0, cache_key TEXT NOT NULL,
            cached_from TEXT, error TEXT, cpu INTEGER NOT NULL DEFAULT 4, memory INTEGER NOT NULL DEFAULT 8);
          CREATE TABLE IF NOT EXISTS leases(id TEXT PRIMARY KEY, request_id TEXT UNIQUE NOT NULL, project TEXT NOT NULL,
            client TEXT NOT NULL, state TEXT NOT NULL, created REAL NOT NULL, expires REAL NOT NULL,
            exit_code INTEGER, log_offset INTEGER NOT NULL DEFAULT 0);
          CREATE TABLE IF NOT EXISTS audit(seq INTEGER PRIMARY KEY AUTOINCREMENT, at REAL NOT NULL, kind TEXT NOT NULL,
            job TEXT, client TEXT, prev TEXT NOT NULL, hash TEXT NOT NULL, data TEXT NOT NULL);
          CREATE TABLE IF NOT EXISTS settings(key TEXT PRIMARY KEY, value TEXT NOT NULL);
        """)
        if "job" not in {r[1] for r in self.db.execute("PRAGMA table_info(leases)")}:
            self.db.execute("ALTER TABLE leases ADD COLUMN job TEXT")
        columns = {r[1] for r in self.db.execute("PRAGMA table_info(jobs)")}
        if "proxy_image" not in columns:
            self.db.execute("ALTER TABLE jobs ADD COLUMN proxy_image TEXT")
        if "proxy_cleaned" not in columns:
            self.db.execute("ALTER TABLE jobs ADD COLUMN proxy_cleaned INTEGER NOT NULL DEFAULT 0")
        if "scratch_mb" not in columns:
            self.db.execute("ALTER TABLE jobs ADD COLUMN scratch_mb INTEGER NOT NULL DEFAULT 512")
        if "headroom_gb" not in columns:
            self.db.execute("ALTER TABLE jobs ADD COLUMN headroom_gb INTEGER NOT NULL DEFAULT 32")
        self.docker = docker or Docker()
        self.memory_probe = memory_probe or available_gb
        self.hardware_probe = hardware_probe or hardware_capacity

    def resource_policy(self):
        hardware = self.hardware_probe()
        cpus = integer(hardware["cpu_count"], 1, 65536)
        memory = integer(hardware["memory_gb"], 2, 1048576)
        stored = self.db.execute("SELECT value FROM settings WHERE key='resource_policy'").fetchone()
        if stored:
            limits = validate_resources(json.loads(stored[0]), hardware)
        else:
            headroom = min(HEADROOM_GB, max(1, memory // 2))
            total_memory = min(64, memory - headroom)
            total_cpu = min(16, cpus)
            limits = validate_resources({"cpu_per_job": min(CPUS, total_cpu), "memory_gb_per_job": min(MEMORY_GB, total_memory),
                                         "cpu_total": total_cpu, "memory_gb_total": total_memory, "client_jobs": 2,
                                         "client_cpu": min(8, total_cpu), "headroom_gb": headroom}, hardware)
        return {"limits": limits, "hardware": hardware, "policy_saved": bool(stored),
                "applies_to": "new_jobs_and_admissions", "running_jobs_unchanged": True,
                "legacy_project_overrides": "clamped_to_per_job_caps" if stored else "preserved"}

    def resources(self, request):
        fields(request, ("v", "op"), ("limits",))
        if "limits" in request:
            limits = validate_resources(request["limits"], self.hardware_probe())
            with self.transaction():
                self.db.execute("INSERT OR REPLACE INTO settings VALUES('resource_policy',?)", (canonical(limits).decode(),))
                self.audit("resource_policy_updated", limits=limits)
        return self.resource_policy()

    @contextlib.contextmanager
    def transaction(self):
        self.db.execute("BEGIN IMMEDIATE")
        try:
            yield
            self.db.execute("COMMIT")
        except BaseException:
            self.db.execute("ROLLBACK")
            raise

    def audit(self, kind, job=None, client=None, **data):
        previous = self.db.execute("SELECT hash FROM audit ORDER BY seq DESC LIMIT 1").fetchone()
        previous = previous[0] if previous else "0" * 64
        at = time.time()
        body = canonical({"at": at, "kind": kind, "job": job, "client": client, "data": data})
        self.db.execute("INSERT INTO audit(at,kind,job,client,prev,hash,data) VALUES(?,?,?,?,?,?,?)",
                        (at, kind, job, client, previous, sha(bytes.fromhex(previous) + body), body.decode()))

    def registry(self):
        path = self.root.parent / "runtimes.json"
        if not path.exists():
            return {}
        if path.is_symlink() or (os.name == "posix" and (path.stat().st_uid != os.getuid() or path.stat().st_mode & 0o022)):
            reject("unsafe_registry", "Runtime registry ownership or permissions are unsafe")
        try:
            value = json.loads(path.read_text())
            fields(value, ("v", "runtimes"))
            if value["v"] != 1 or not isinstance(value["runtimes"], dict):
                raise ValueError()
            for name, entry in value["runtimes"].items():
                fields(entry, ("image", "provenance"), ("gpu",))
                if name not in RUNTIME_NAMES or not isinstance(entry["image"], str) or not re.fullmatch(r"sha256:[0-9a-f]{64}", entry["image"]):
                    raise ValueError()
                if not isinstance(entry["provenance"], str) or not entry["provenance"] or type(entry.get("gpu", False)) is not bool:
                    raise ValueError()
                if entry.get("gpu", False) != (name == "cuda"):
                    raise ValueError()
            return value["runtimes"]
        except (ValueError, TypeError, KeyError):
            reject("invalid_registry", "Runtime registry must contain approved immutable image IDs")

    def object_path(self, value):
        return self.root / "objects" / digest(value)

    def put(self, request):
        fields(request, ("v", "op", "sha256", "data"))
        target = self.object_path(request["sha256"])
        if not isinstance(request["data"], str) or len(request["data"]) > 4 * ((CHUNK + 2) // 3):
            reject("size_limit", "Chunk exceeds one MiB")
        try:
            raw = base64.b64decode(request["data"], validate=True)
        except (ValueError, TypeError):
            reject("invalid_request", "Chunk encoding is invalid")
        if len(raw) > CHUNK or sha(raw) != request["sha256"]:
            reject("hash_mismatch", "Chunk hash or length does not match")
        if SECRET.search(raw):
            reject("secret_detected", "Chunk contains credential material")
        atomic_write(target, raw)
        return {"sha256": request["sha256"], "size": len(raw)}

    def put_many(self, request):
        fields(request, ("v", "op", "chunks"))
        chunks = request["chunks"]
        if not isinstance(chunks, list) or not 1 <= len(chunks) <= 128 or len(canonical(request)) > 3 * CHUNK:
            reject("size_limit", "Upload batch exceeds its count or three-MiB body limit")
        accepted = []
        for chunk in chunks:
            fields(chunk, ("sha256", "data"))
            accepted.append(self.put({"v": 1, "op": "put", **chunk}))
        return {"accepted": accepted, "bytes": sum(c["size"] for c in accepted)}

    def missing(self, request):
        fields(request, ("v", "op", "hashes"))
        values = request["hashes"]
        if not isinstance(values, list) or len(values) > 10000:
            reject("size_limit", "Too many chunk hashes")
        absent = []
        for value in values:
            path = self.object_path(value)
            if not path.exists():
                absent.append(value)
            else:
                with os.fdopen(safe_open(path), "rb") as stream:
                    if sha(stream.read(CHUNK + 1)) != value:
                        absent.append(value)
        return {"missing": absent}

    def sync(self, request):
        fields(request, ("v", "op", "project", "manifest"))
        project = digest(request["project"])
        manifest = request["manifest"]
        if not isinstance(manifest, list) or len(manifest) > MAX_FILES:
            reject("size_limit", "Manifest is too large")
        total, paths, folded = 0, set(), set()
        for entry in manifest:
            fields(entry, ("path", "size", "sha256", "mode", "chunks"))
            name = path_name(entry["path"])
            if name in paths or name.casefold() in folded:
                reject("unsafe_path", "Duplicate or case-colliding project paths")
            paths.add(name)
            folded.add(name.casefold())
            digest(entry["sha256"])
            size = integer(entry["size"], 0, MAX_FILE)
            integer(entry["mode"], 0, 0o777)
            if entry["mode"] not in (0o644, 0o755):
                reject("unsafe_path", "File mode must be 0644 or 0755")
            chunks = entry["chunks"]
            if not isinstance(chunks, list) or len(chunks) > MAX_FILE // CHUNK + 1:
                reject("size_limit", "Too many chunks")
            count = 0
            for chunk in chunks:
                fields(chunk, ("sha256", "size"))
                digest(chunk["sha256"])
                count += integer(chunk["size"], 0, CHUNK)
            if count != size:
                reject("size_mismatch", "Chunk lengths do not match file size")
            total += size
        if total > MAX_TREE:
            reject("size_limit", "Project exceeds twenty GiB")
        for name in paths:
            if any(str(parent) in paths for parent in PurePosixPath(name).parents if str(parent) != "."):
                reject("unsafe_path", "A file conflicts with a parent directory")
        manifest = sorted(manifest, key=lambda e: e["path"])
        encoded = canonical(manifest)
        manifest_id = sha(encoded)
        destination = self.root / "snapshots" / manifest_id
        if not destination.exists():
            temporary = Path(tempfile.mkdtemp(prefix=".pending-", dir=destination.parent))
            try:
                for entry in manifest:
                    out = temporary.joinpath(*entry["path"].split("/"))
                    out.parent.mkdir(parents=True, exist_ok=True)
                    whole, tail = hashlib.sha256(), b""
                    with out.open("xb") as stream:
                        for chunk in entry["chunks"]:
                            try:
                                with os.fdopen(safe_open(self.object_path(chunk["sha256"])), "rb") as source:
                                    raw = source.read(CHUNK + 1)
                            except FileNotFoundError:
                                reject("missing_chunks", "Upload missing chunks before committing the snapshot")
                            if len(raw) != chunk["size"] or sha(raw) != chunk["sha256"]:
                                reject("hash_mismatch", "Stored chunk failed verification")
                            if SECRET.search(tail + raw):
                                reject("secret_detected", "Project contains credential material")
                            tail = raw[-512:]
                            whole.update(raw)
                            stream.write(raw)
                    if whole.hexdigest() != entry["sha256"]:
                        reject("hash_mismatch", "Whole-file hash does not match")
                    out.chmod(0o555 if entry["mode"] == 0o755 else 0o444)
                try:
                    os.rename(temporary, destination)
                except OSError:
                    if not destination.is_dir():
                        raise
            finally:
                if temporary.exists():
                    # This directory was generated by this function, below snapshots;
                    # no untrusted links or archive entries were ever materialized.
                    def onerror(func, path, exc):
                        os.chmod(path, 0o700)
                        func(path)
                    shutil.rmtree(temporary, onerror=onerror)
        with self.transaction():
            self.db.execute("INSERT OR IGNORE INTO manifests VALUES(?,?,?)", (manifest_id, encoded.decode(), total))
            self.db.execute("INSERT OR IGNORE INTO projects VALUES(?,?)", (project, manifest_id))
            self.audit("snapshot_committed", project=project, manifest=manifest_id, bytes=total)
        return {"project": project, "manifest_id": manifest_id, "files": len(manifest), "bytes": total}

    def get_job(self, job):
        identifier(job)
        row = self.db.execute("SELECT * FROM jobs WHERE id=?", (job,)).fetchone()
        if row is None:
            reject("job_not_found", "Job does not exist")
        return row

    @staticmethod
    def job_status(row):
        return {"job": row["id"], "project": row["project"], "manifest_id": row["manifest"],
                "runtime": row["runtime"], "runtime_digest": row["image"], "client": row["client"],
                "where": "spark", "state": row["state"], "exit_code": row["exit_code"],
                "created_at": row["created"], "started_at": row["started"], "finished_at": row["finished"],
                "cancel_requested": bool(row["cancel"]), "cache_hit": row["cached_from"] is not None,
                "cached_from": row["cached_from"], "error": row["error"], "cpu": row["cpu"], "memory_gb": row["memory"], "scratch_mb": row["scratch_mb"], "headroom_gb": row["headroom_gb"]}

    def claim_conflict(self, project):
        remote = self.db.execute("SELECT id FROM jobs WHERE project=? AND state IN ('queued','starting','running')", (project,)).fetchone()
        # A sleeping laptop cannot prove that its workload stopped at lease expiry.
        # Keep stale claims blocking until that client explicitly reports/relinquishes.
        local = self.db.execute("SELECT id FROM leases WHERE project=? AND state='running'", (project,)).fetchone()
        if remote or local:
            reject("worktree_claimed", "This worktree already has an active job on the shared board")

    def run(self, request):
        fields(request, ("v", "op", "project", "manifest_id", "argv", "runtime", "client", "request_id"), ("no_cache",))
        project, manifest_id = digest(request["project"]), digest(request["manifest_id"])
        client, request_id = client_name(request["client"]), identifier(request["request_id"])
        args = request["argv"]
        if not isinstance(args, list) or not 1 <= len(args) <= 1024 or any(not isinstance(a, str) or "\0" in a for a in args) or not args[0] or sum(len(a.encode()) for a in args) > 65536:
            reject("invalid_argv", "Command must be a bounded non-empty array of strings")
        if SECRET.search(canonical(args)):
            reject("secret_detected", "Command contains credential material")
        if type(request.get("no_cache", False)) is not bool:
            reject("invalid_request", "no_cache must be boolean")
        encoded = canonical(request).decode()
        with self.transaction():
            old = self.db.execute("SELECT * FROM jobs WHERE request_id=?", (request_id,)).fetchone()
            if old:
                if old["request"] != encoded:
                    reject("idempotency_conflict", "Request ID was already used for a different request")
                return self.job_status(old)
            registry = self.registry()
            if request["runtime"] not in registry:
                reject("runtime_unavailable", "Requested runtime has not been provisioned")
            runtime = registry[request["runtime"]]
            if request["op"] == "prepare" and "python" not in registry:
                reject("runtime_unavailable", "Preparation also requires the approved Python proxy runtime")
            if not self.db.execute("SELECT 1 FROM projects WHERE project=? AND manifest=?", (project, manifest_id)).fetchone():
                reject("manifest_not_found", "Sync this manifest before submitting the job")
            if self.db.execute("SELECT COUNT(*) FROM jobs").fetchone()[0] >= MAX_JOBS:
                reject("capacity", "Retained job capacity reached; owner must archive receipts")
            worker = Path(__file__).with_name("worker.py").read_bytes()
            owner_policy = self.resource_policy()
            limits = owner_policy["limits"]
            cpu, memory, scratch_mb = limits["cpu_per_job"], limits["memory_gb_per_job"], 512
            profile = self.root.parent / "limits.json"
            if profile.exists() and request["op"] != "prepare":
                if profile.is_symlink() or (os.name == "posix" and (profile.stat().st_uid != os.getuid() or profile.stat().st_mode & 0o022)):
                    reject("unsafe_state", "Owner resource policy is not protected")
                policy = json.loads(profile.read_text())
                fields(policy, ("v", "projects"))
                if policy["v"] != 1 or not isinstance(policy["projects"], dict):
                    reject("invalid_request", "Invalid owner resource policy")
                project_limits = policy["projects"].get(project)
                if project_limits:
                    fields(project_limits, ("cpu", "memory_gb"), ("scratch_mb",))
                    cpu = integer(project_limits["cpu"], 1, owner_policy["hardware"]["cpu_count"])
                    memory = integer(project_limits["memory_gb"], 1, owner_policy["hardware"]["memory_gb"])
                    scratch_mb = integer(project_limits.get("scratch_mb", 512), 128, min(4096, memory * 512))
                    if owner_policy["policy_saved"]:
                        cpu, memory = min(cpu, limits["cpu_per_job"]), min(memory, limits["memory_gb_per_job"])
                        scratch_mb = min(scratch_mb, memory * 512)
            overhead_cpu, overhead_memory = (0.25, 0.125) if request["op"] == "prepare" else (0, 0)
            if cpu + overhead_cpu > limits["client_cpu"] or cpu + overhead_cpu > limits["cpu_total"] or memory + overhead_memory > limits["memory_gb_total"]:
                reject("resource_policy_exceeded", "Job and preparation proxy must fit owner total and client caps; adjust resources before submitting")
            cache_key = sha(canonical({"project": project, "manifest": manifest_id, "argv": args,
                                      "runtime": runtime, "worker": sha(worker), "policy": 1,
                                      "cpu": cpu, "memory": memory, "scratch_mb": scratch_mb}))
            cached = None if request.get("no_cache") else self.db.execute(
                "SELECT id FROM jobs WHERE cache_key=? AND state='succeeded' AND cached_from IS NULL ORDER BY finished DESC LIMIT 1", (cache_key,)).fetchone()
            if not cached:
                self.claim_conflict(project)
            job = uuid.uuid4().hex
            now = time.time()
            job_root = self.root / "jobs" / job
            job_root.mkdir(mode=0o700)
            atomic_write(job_root / "worker.py", worker, 0o444)
            if request["op"] == "prepare":
                for name in ("prepare.py", "proxy.py"):
                    atomic_write(job_root / name, Path(__file__).with_name(name).read_bytes(), 0o444)
            self.db.execute("""INSERT INTO jobs(id,request_id,request,project,manifest,runtime,image,client,state,created,
                            finished,exit_code,cache_key,cached_from) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?)""",
                            (job, request_id, encoded, project, manifest_id, request["runtime"], runtime["image"], client,
                             "succeeded" if cached else "queued", now, now if cached else None, 0 if cached else None,
                             cache_key, cached[0] if cached else None))
            if request["op"] == "prepare":
                self.db.execute("UPDATE jobs SET proxy_image=? WHERE id=?", (registry["python"]["image"], job))
            self.db.execute("UPDATE jobs SET cpu=?,memory=?,scratch_mb=?,headroom_gb=? WHERE id=?", (cpu, memory, scratch_mb, limits["headroom_gb"], job))
            self.audit("job_cached" if cached else "job_queued", job, client, cache_key=cache_key, cached_from=cached[0] if cached else None)
        return self.job_status(self.get_job(job))

    def prepare(self, request):
        fields(request, ("v", "op", "project", "manifest_id", "runtime", "client", "request_id"))
        if request["runtime"] not in ("rust", "node", "python"):
            reject("invalid_request", "This runtime has no dependency preparation")
        return self.run({**request, "argv": ["/usr/bin/python3", "/sparkdev-prepare.py", request["runtime"]], "no_cache": True})

    def logs(self, request):
        fields(request, ("v", "op", "job", "since"), ("limit",))
        offset = integer(request["since"], 0, MAX_LOG)
        limit = integer(request.get("limit", CHUNK), 1, CHUNK)
        job = identifier(request["job"])
        local = self.db.execute("SELECT * FROM leases WHERE id=? OR job=?", (job, job)).fetchone()
        if local:
            path = self.root / "local" / (local["id"] + ".log")
            status = {"job": local["job"] or local["id"], "where": "laptop", "client": local["client"], "state": local["state"], "exit_code": local["exit_code"]}
        else:
            row = self.get_job(job)
            path = self.root / "jobs" / (row["cached_from"] or row["id"]) / "output" / "stdout.log"
            status = self.job_status(row)
        try:
            with os.fdopen(safe_open(path), "rb") as stream:
                size = os.fstat(stream.fileno()).st_size
                if offset > size:
                    reject("invalid_offset", "Log offset is past the available output")
                stream.seek(offset)
                raw = stream.read(limit)
        except FileNotFoundError:
            if offset:
                reject("invalid_offset", "Log offset is past the available output")
            raw = b""
        return {**status, "data": base64.b64encode(raw).decode(), "text": raw.decode("utf-8", "replace"),
                "since": offset, "next_offset": offset + len(raw)}

    def wait(self, request):
        fields(request, ("v", "op", "job"), ("timeout",))
        timeout = integer(request.get("timeout", 0), 0, 60)
        deadline = time.monotonic() + timeout
        while True:
            row = self.get_job(request["job"])
            if row["state"] in TERMINAL or time.monotonic() >= deadline:
                return self.job_status(row)
            time.sleep(min(0.2, max(0, deadline - time.monotonic())))

    def cancel(self, request):
        fields(request, ("v", "op", "job"))
        with self.transaction():
            row = self.get_job(request["job"])
            if row["state"] not in TERMINAL and not row["cancel"]:
                self.db.execute("UPDATE jobs SET cancel=1 WHERE id=?", (row["id"],))
                self.audit("cancel_requested", row["id"], row["client"])
                if row["state"] == "queued":
                    self.db.execute("UPDATE jobs SET state='cancelled',exit_code=130,finished=? WHERE id=?", (time.time(), row["id"]))
        return self.job_status(self.get_job(request["job"]))

    def claim(self, request):
        fields(request, ("v", "op", "project", "client", "request_id"), ("ttl", "job"))
        project, client, request_id = digest(request["project"]), client_name(request["client"]), identifier(request["request_id"])
        ttl = integer(request.get("ttl", 60), 15, 300)
        local_job = identifier(request["job"]) if "job" in request else None
        with self.transaction():
            old = self.db.execute("SELECT * FROM leases WHERE request_id=?", (request_id,)).fetchone()
            if old:
                if old["project"] != project or old["client"] != client or old["job"] != local_job:
                    reject("idempotency_conflict", "Claim ID was already used for another worktree")
                if old["state"] != "running" or old["expires"] <= time.time():
                    reject("lease_expired", "Claim expired; use a new request ID")
                return {"lease": old["id"], "job": old["job"] or old["id"], "expires_at": old["expires"]}
            self.claim_conflict(project)
            lease = uuid.uuid4().hex
            if local_job and self.db.execute("SELECT 1 FROM leases WHERE job=?", (local_job,)).fetchone():
                reject("idempotency_conflict", "Laptop job ID is already registered")
            now = time.time()
            self.db.execute("INSERT INTO leases(id,request_id,project,client,state,created,expires,job) VALUES(?,?,?,?,?,?,?,?)",
                            (lease, request_id, project, client, "running", now, now + ttl, local_job))
            self.audit("laptop_claimed", lease, client, project=project)
        return {"lease": lease, "job": local_job or lease, "expires_at": now + ttl}

    def heartbeat(self, request):
        fields(request, ("v", "op", "lease"), ("ttl", "state", "exit_code", "data", "since", "job"))
        lease = identifier(request["lease"])
        ttl = integer(request.get("ttl", 60), 15, 300)
        state = request.get("state", "running")
        if state not in ("running", *TERMINAL):
            reject("invalid_request", "Invalid laptop job state")
        code = request.get("exit_code")
        if state in TERMINAL:
            integer(code, 0, 255)
            if (state == "succeeded") != (code == 0):
                reject("invalid_request", "Terminal state does not match exit code")
        elif code is not None:
            reject("invalid_request", "Running jobs have no exit code")
        with self.transaction():
            row = self.db.execute("SELECT * FROM leases WHERE id=?", (lease,)).fetchone()
            if not row:
                reject("lease_not_found", "Laptop claim does not exist")
            if "job" in request and identifier(request["job"]) != (row["job"] or lease):
                reject("invalid_request", "Laptop job does not match claim")
            if row["state"] in TERMINAL:
                if state != row["state"] or code != row["exit_code"]:
                    reject("invalid_state", "Laptop claim already completed")
                if "data" not in request:
                    return {"lease": lease, "state": state, "next_offset": row["log_offset"], "expires_at": row["expires"]}
            if row["expires"] <= time.time() and state not in TERMINAL:
                reject("lease_expired", "Laptop claim expired; do not continue the worktree")
            offset = row["log_offset"]
            if "data" in request:
                if not isinstance(request["data"], str) or len(request["data"]) > 4 * ((CHUNK + 2) // 3):
                    reject("size_limit", "Log chunk exceeds one MiB")
                try:
                    raw = base64.b64decode(request["data"], validate=True)
                except ValueError:
                    reject("invalid_request", "Invalid log encoding")
                since = integer(request.get("since"), 0, MAX_LOG)
                if len(raw) > CHUNK or since > offset or since + len(raw) > MAX_LOG:
                    reject("invalid_offset", "Invalid laptop log range")
                path = self.root / "local" / (lease + ".log")
                with os.fdopen(safe_open(path, os.O_RDWR | os.O_CREAT), "r+b") as stream:
                    stream.seek(since)
                    if since < offset:
                        if since + len(raw) > offset or stream.read(len(raw)) != raw:
                            reject("invalid_offset", "Overlapping laptop log data differs")
                    else:
                        stream.write(raw)
                        stream.flush()
                        os.fsync(stream.fileno())
                        offset += len(raw)
            self.db.execute("UPDATE leases SET expires=?,state=?,exit_code=?,log_offset=? WHERE id=?", (time.time() + ttl, state, code, offset, lease))
            if state in TERMINAL and row["state"] not in TERMINAL:
                self.audit("laptop_finished", lease, row["client"], state=state, exit_code=code)
        return {"lease": lease, "state": state, "next_offset": offset, "expires_at": time.time() + ttl}

    def release(self, request):
        fields(request, ("v", "op", "lease"))
        row = self.db.execute("SELECT * FROM leases WHERE id=?", (identifier(request["lease"]),)).fetchone()
        if not row:
            reject("lease_not_found", "Laptop claim does not exist")
        if row["state"] in TERMINAL:
            return {"lease": row["id"], "state": row["state"]}
        return self.heartbeat({"v": 1, "op": "heartbeat", "lease": row["id"], "state": "cancelled", "exit_code": 130})

    def status(self, request):
        fields(request, ("v", "op"), ("all",))
        if "all" in request and type(request["all"]) is not bool:
            reject("invalid_request", "all must be boolean")
        policy = self.resource_policy()
        jobs = [self.job_status(r) for r in self.db.execute("SELECT * FROM jobs ORDER BY created DESC LIMIT 200")]
        for job in jobs:
            if job["state"] == "queued" and policy["policy_saved"] and (job["cpu"] > policy["limits"]["cpu_per_job"] or job["memory_gb"] > policy["limits"]["memory_gb_per_job"]):
                job["admission_blocked"] = "Saved per-job caps are below this existing job's immutable allocation; raise caps or cancel and resubmit"
        for row in self.db.execute("SELECT * FROM leases ORDER BY created DESC LIMIT 200"):
            jobs.append({"job": row["job"] or row["id"], "project": row["project"], "client": row["client"], "where": "laptop",
                         "state": "lease_lost" if row["state"] == "running" and row["expires"] < time.time() else row["state"],
                         "exit_code": row["exit_code"], "created_at": row["created"], "expires_at": row["expires"], "next_offset": row["log_offset"]})
        last = self.db.execute("SELECT value FROM settings WHERE key='scheduler_heartbeat'").fetchone()
        return {"jobs": sorted(jobs, key=lambda j: j["created_at"], reverse=True), "runtimes": list(self.registry()),
                "scheduler": {"last_heartbeat": float(last[0]) if last else None, "healthy": bool(last and time.time() - float(last[0]) < 15)},
                **policy, "dependency_preparation": "isolated_allowlist_proxy"}

    def doctor(self, request):
        fields(request, ("v", "op"))
        result = self.status({"v": 1, "op": "status"})
        result["checks"] = {"sqlite": self.db.execute("PRAGMA quick_check").fetchone()[0] == "ok", "memory_available_gb": self.memory_probe(),
                            "linux": sys.platform.startswith("linux"), "docker": False}
        try:
            self.docker.call(["version", "--format", "{{.Server.Version}}"], timeout=10)
            result["checks"]["docker"] = True
        except (Rejected, OSError, subprocess.TimeoutExpired):
            pass
        return result

    def rpc(self, request):
        try:
            if not isinstance(request, dict) or type(request.get("v")) is not int or request["v"] != 1:
                reject("version_unsupported", "Protocol version 1 is required")
            op = request.get("op")
            if op not in {"put", "put_many", "missing", "sync", "run", "prepare", "logs", "wait", "cancel", "status", "doctor", "claim", "heartbeat", "release", "resources"}:
                reject("unknown_operation", "Unsupported operation")
            return {"ok": True, **getattr(self, op)(request)}
        except Rejected as error:
            return {"ok": False, "error": {"code": error.code, "message": error.message}}
        except (OSError, ValueError, TypeError, KeyError, sqlite3.Error, subprocess.SubprocessError):
            return {"ok": False, "error": {"code": "controller_error", "message": "Controller operation failed; no request contents were logged"}}

    def container_name(self, job):
        return "sparkdev-job-" + identifier(job)

    def container_matches(self, info, row):
        labels = info.get("Config", {}).get("Labels", {}) or {}
        if labels.get("com.delores.sparkdev.job") != row["id"] or info.get("Image") != row["image"]:
            reject("container_conflict", "Existing container does not match its immutable job identity")

    def finish(self, row, state, code, error=None):
        with self.transaction():
            self.db.execute("UPDATE jobs SET state=?,exit_code=?,finished=?,error=? WHERE id=?", (state, code, time.time(), error, row["id"]))
            self.audit("job_finished", row["id"], row["client"], state=state, exit_code=code, error=error)

    def create_args(self, row):
        root = self.root / "jobs" / row["id"]
        root.mkdir(mode=0o700, exist_ok=True)
        work = root / "work"
        if not work.exists():
            shutil.copytree(self.root / "snapshots" / row["manifest"], work)
            for directory, _, filenames in os.walk(work):
                for filename in filenames:
                    path = Path(directory) / filename
                    path.chmod(0o755 if path.stat().st_mode & 0o111 else 0o644)
        output = root / "output"
        output.mkdir(mode=0o700, exist_ok=True)
        request = json.loads(row["request"])
        atomic_write(root / "argv.json", canonical(request["argv"]))
        # Worker is copied into the job record so a later software upgrade cannot
        # silently change a queued job's execution or its cache identity.
        if not (root / "worker.py").exists():
            atomic_write(root / "worker.py", Path(__file__).with_name("worker.py").read_bytes(), 0o444)
        cache = self.root / "caches" / row["project"] / row["image"].split(":", 1)[1]
        cache.mkdir(parents=True, exist_ok=True, mode=0o700)
        uid = os.getuid() if hasattr(os, "getuid") else 1000
        gid = os.getgid() if hasattr(os, "getgid") else 1000
        preparing = json.loads(row["request"])["op"] == "prepare"
        args = ["create", "--name", self.container_name(row["id"]), "--pull", "never", "--network", "sparkdev-fetch-" + row["id"] if preparing else "none", "--read-only",
                "--cap-drop", "ALL", "--security-opt", "no-new-privileges", "--user", f"{uid}:{gid}",
                "--cpus", str(row["cpu"]), "--cpu-shares", "256", "--memory", f"{row['memory']}g", "--memory-swap", f"{row['memory']}g",
                "--pids-limit", "1024", "--init", "--restart", "no", "--log-driver", "none", "--ulimit", f"fsize={MAX_FILE}:{MAX_FILE}",
                "--tmpfs", "/tmp:rw,nosuid,nodev,size=" + str(row["scratch_mb"] * 1024 * 1024), "--workdir", "/work",
                "--label", "com.delores.sparkdev.job=" + row["id"], "--label", "com.delores.sparkdev.client=" + row["client"],
                "--mount", f"type=bind,src={work},dst=/work", "--mount", f"type=bind,src={output},dst=/sparkdev-output",
                "--mount", f"type=bind,src={root / 'argv.json'},dst=/sparkdev-argv.json,readonly",
                "--mount", f"type=bind,src={root / 'worker.py'},dst=/sparkdev-worker.py,readonly"]
        for name, destination in (("target", "/target"), ("cargo", "/cargo-home"), ("npm", "/npm-cache"), ("pip", "/pip-cache")):
            path = cache / name
            if path.is_symlink():
                reject("unsafe_state", "Cache root is a symlink")
            path.mkdir(exist_ok=True)
            args += ["--mount", f"type=bind,src={path},dst={destination}"]
        for value in ("HOME=/tmp", "CARGO_HOME=/cargo-home", "CARGO_TARGET_DIR=/target", "CARGO_NET_OFFLINE=true",
                      "npm_config_cache=/npm-cache", "npm_config_offline=true", "PIP_CACHE_DIR=/pip-cache", "PIP_NO_INDEX=1",
                      "PYTHONDONTWRITEBYTECODE=1", "PYTHONUNBUFFERED=1", "SPARKDEV_RUNTIME_NAME=" + row["runtime"]):
            args += ["--env", value]
        for name in ("RAYON_NUM_THREADS", "OMP_NUM_THREADS", "OPENBLAS_NUM_THREADS", "MKL_NUM_THREADS", "CARGO_BUILD_JOBS"):
            args += ["--env", name + "=" + str(row["cpu"])]
        if row["runtime"] == "cuda":
            args += ["--gpus", "all", "--env", "NVIDIA_DRIVER_CAPABILITIES=compute,utility,graphics"]
        if preparing:
            args += ["--mount", f"type=bind,src={root / 'prepare.py'},dst=/sparkdev-prepare.py,readonly"]
            for name in ("HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy", "ALL_PROXY"):
                args += ["--env", name + "=http://sparkdev-proxy:3128"]
            for value in ("CARGO_NET_OFFLINE=false", "npm_config_offline=false", "PIP_NO_INDEX=0", "NO_PROXY=", "no_proxy="):
                args += ["--env", value]
        args += ["--entrypoint", "/usr/bin/python3", row["image"], "/sparkdev-worker.py"]
        return args

    def ensure_proxy(self, row):
        if not row["proxy_image"]:
            return
        network = "sparkdev-fetch-" + row["id"]
        info = self.docker.network_inspect(network)
        if info is None:
            self.docker.call(["network", "create", "--internal", "--opt", "com.docker.network.bridge.gateway_mode_ipv4=isolated",
                              "--opt", "com.docker.network.bridge.gateway_mode_ipv6=isolated", "--label", "com.delores.sparkdev.job=" + row["id"], network])
        elif not info.get("Internal") or (info.get("Labels") or {}).get("com.delores.sparkdev.job") != row["id"] or (info.get("Options") or {}).get("com.docker.network.bridge.gateway_mode_ipv4") != "isolated":
            reject("network_conflict", "Dependency network does not match its job")
        name = "sparkdev-proxy-" + row["id"]
        info = self.docker.inspect(name)
        if info is None:
            uid = os.getuid() if hasattr(os, "getuid") else 1000
            gid = os.getgid() if hasattr(os, "getgid") else 1000
            self.docker.call(["create", "--name", name, "--pull", "never", "--network", "bridge", "--read-only", "--cap-drop", "ALL",
                              "--security-opt", "no-new-privileges", "--user", f"{uid}:{gid}", "--cpus", "0.25", "--cpu-shares", "128",
                              "--memory", "128m", "--memory-swap", "128m", "--pids-limit", "64", "--log-driver", "none",
                              "--label", "com.delores.sparkdev.job=" + row["id"], "--mount",
                              f"type=bind,src={self.root / 'jobs' / row['id'] / 'proxy.py'},dst=/sparkdev-proxy.py,readonly",
                              "--entrypoint", "/usr/bin/python3", row["proxy_image"], "/sparkdev-proxy.py"])
            info = self.docker.inspect(name)
        if info is None or info.get("Image") != row["proxy_image"] or (info.get("Config", {}).get("Labels") or {}).get("com.delores.sparkdev.job") != row["id"]:
            reject("container_conflict", "Dependency proxy identity does not match its job")
        if network not in info.get("NetworkSettings", {}).get("Networks", {}):
            self.docker.call(["network", "connect", "--alias", "sparkdev-proxy", network, name])
        if info["State"]["Status"] == "created":
            self.docker.call(["start", name])
        elif info["State"]["Status"] != "running":
            reject("proxy_failed", "Dependency proxy stopped; cancel and retry preparation")

    def cleanup_proxy(self, row):
        if not row["proxy_image"] or row["proxy_cleaned"]:
            return
        name, network = "sparkdev-proxy-" + row["id"], "sparkdev-fetch-" + row["id"]
        info = self.docker.inspect(name)
        if info:
            if info.get("Image") != row["proxy_image"] or (info.get("Config", {}).get("Labels") or {}).get("com.delores.sparkdev.job") != row["id"]:
                reject("container_conflict", "Refusing to clean an unrelated proxy")
            self.docker.call(["rm", "--force", name])
        net = self.docker.network_inspect(network)
        if net:
            if (net.get("Labels") or {}).get("com.delores.sparkdev.job") != row["id"]:
                reject("network_conflict", "Refusing to clean an unrelated network")
            job = self.docker.inspect(self.container_name(row["id"]))
            if job and network in job.get("NetworkSettings", {}).get("Networks", {}):
                self.docker.call(["network", "disconnect", network, self.container_name(row["id"])])
            self.docker.call(["network", "rm", network])
        self.db.execute("UPDATE jobs SET proxy_cleaned=1 WHERE id=?", (row["id"],))

    def reconcile(self):
        for row in self.db.execute("SELECT * FROM jobs WHERE state IN ('starting','running')").fetchall():
            name = self.container_name(row["id"])
            info = self.docker.inspect(name)
            if info is None:
                if row["state"] == "running":
                    self.finish(row, "failed", 125, "container_lost")
                    continue
                if row["cancel"]:
                    self.finish(row, "cancelled", 130)
                    continue
                self.ensure_proxy(row)
                self.docker.call(self.create_args(row))
                info = self.docker.inspect(name)
                if info is None:
                    reject("docker_unavailable", "Created container cannot be inspected")
            self.container_matches(info, row)
            state = info["State"]["Status"]
            if row["cancel"] or (row["runtime"] == "cuda" and self.memory_probe() < row["headroom_gb"]):
                if state in ("running", "restarting", "paused"):
                    self.docker.call(["stop", "--time", "5", name], timeout=15)
                self.finish(row, "cancelled" if row["cancel"] else "failed", 130 if row["cancel"] else 137,
                            None if row["cancel"] else "memory_watchdog")
            elif state == "created":
                # A cancellation which arrived while Docker creation was underway wins
                # before start. The next tick observes it and removes the proxy.
                if self.get_job(row["id"])["cancel"]:
                    continue
                self.docker.call(["start", name])
                with self.transaction():
                    self.db.execute("UPDATE jobs SET state='running',started=COALESCE(started,?) WHERE id=?", (time.time(), row["id"]))
                    self.audit("job_started", row["id"], row["client"], image=row["image"], cpu=row["cpu"], memory_gb=row["memory"], scratch_mb=row["scratch_mb"])
            elif state == "running":
                self.db.execute("UPDATE jobs SET state='running',started=COALESCE(started,?) WHERE id=?", (time.time(), row["id"]))
            elif state in ("exited", "dead"):
                code = integer(info["State"]["ExitCode"], 0, 255)
                self.finish(row, "succeeded" if code == 0 else "failed", code, "oom_killed" if info["State"].get("OOMKilled") else None)
            else:
                reject("container_state_unknown", "Container state is not safe for reconciliation")

    def tick(self):
        self.reconcile()
        for row in self.db.execute("SELECT * FROM jobs WHERE state IN ('succeeded','failed','cancelled') AND proxy_image IS NOT NULL AND proxy_cleaned=0").fetchall():
            self.cleanup_proxy(row)
        with self.transaction():
            policy = self.resource_policy()
            limits = policy["limits"]
            self.db.execute("INSERT OR REPLACE INTO settings VALUES('scheduler_heartbeat',?)", (str(time.time()),))
            active = self.db.execute("SELECT * FROM jobs WHERE state IN ('starting','running')").fetchall()
            cpu = lambda r: r["cpu"] + (0.25 if r["proxy_image"] else 0)
            memory = lambda r: r["memory"] + (0.125 if r["proxy_image"] else 0)
            used_cpu, used_mem = sum(cpu(r) for r in active), sum(memory(r) for r in active)
            last = self.db.execute("SELECT value FROM settings WHERE key='last_client'").fetchone()
            start = (CLIENTS.index(last[0]) + 1) % len(CLIENTS) if last else 0
            for client in CLIENTS[start:] + CLIENTS[:start]:
                mine = [r for r in active if r["client"] == client]
                row = self.db.execute("SELECT * FROM jobs WHERE client=? AND state='queued' AND cancel=0 ORDER BY created LIMIT 1", (client,)).fetchone()
                if not row:
                    continue
                if len(mine) >= limits["client_jobs"] or sum(cpu(r) for r in mine) + cpu(row) > limits["client_cpu"]:
                    continue
                if policy["policy_saved"] and (row["cpu"] > limits["cpu_per_job"] or row["memory"] > limits["memory_gb_per_job"]):
                    continue
                if used_cpu + cpu(row) > limits["cpu_total"] or used_mem + memory(row) > limits["memory_gb_total"] or self.memory_probe() - used_mem - memory(row) < limits["headroom_gb"]:
                    continue
                self.db.execute("UPDATE jobs SET state='starting' WHERE id=?", (row["id"],))
                self.db.execute("INSERT OR REPLACE INTO settings VALUES('last_client',?)", (client,))
                # One admission per tick avoids long uninterruptible Docker bursts.
                break
        self.reconcile()

    def serve(self):
        if os.name != "posix":
            reject("unsupported_platform", "The scheduler must run on Linux")
        import fcntl
        with (self.root / "scheduler.lock").open("a") as lock:
            try:
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                reject("already_running", "A scheduler already owns this state directory")
            while True:
                try:
                    self.tick()
                except (Rejected, OSError, ValueError, sqlite3.Error, subprocess.SubprocessError) as error:
                    # Deliberately never log argv, Docker stderr, paths or user input.
                    print("sparkdev scheduler: " + (error.code if isinstance(error, Rejected) else type(error).__name__), file=sys.stderr, flush=True)
                time.sleep(1)


def main():
    os.umask(0o077)
    parser = argparse.ArgumentParser()
    parser.add_argument("mode", choices=("rpc", "serve", "tick"))
    parser.add_argument("--state", type=Path)
    args = parser.parse_args()
    controller = Controller(args.state)
    if args.mode == "rpc":
        try:
            raw = sys.stdin.buffer.read(MAX_REQUEST + 1)
            if len(raw) > MAX_REQUEST:
                reject("size_limit", "Request exceeds the protocol body limit")
            request = json.loads(raw)
            response = controller.rpc(request)
        except (ValueError, Rejected) as error:
            response = {"ok": False, "error": {"code": error.code if isinstance(error, Rejected) else "invalid_json", "message": "Request could not be read"}}
        sys.stdout.buffer.write(canonical(response) + b"\n")
        return 0
    if args.mode == "serve":
        controller.serve()
    else:
        controller.tick()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
