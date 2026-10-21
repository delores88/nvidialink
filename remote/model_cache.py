"""Shared, owner-managed download storage for the Spark scheduler.

There is deliberately no import of controller: it can run as __main__, and a
second import would create a different Rejected class. Pass its helper namespace
to CacheManager instead. Cleanup is called only by serve while scheduler.lock is
held. The directory is one writable trust domain; it is not a project sandbox.
"""
from __future__ import annotations

from collections import deque
import contextlib
import heapq
import json
import math
import os
from pathlib import Path
import stat
import time
import uuid

GIB = 1024 ** 3
FLOOR = 128 * GIB
KEEP_GB = 200
WALK_MAX = 2_000_000
STEP_SECONDS = 0.25
RECENT_SECONDS = 48 * 60 * 60
MEASURE_SECONDS = 10 * 60
PARENTS = {(), ("huggingface",), ("huggingface", "hub"),
           ("huggingface", "datasets"), ("huggingface", "assets"),
           ("torch",), ("torch", "hub"), ("torch", "hub", "checkpoints"),
           ("xdg",), ("files",)}
REPAIR_ROOTS = ("huggingface", "huggingface/hub", "huggingface/datasets",
                "torch", "torch/hub", "torch/hub/checkpoints", "pip", "uv", "xdg", "files")


def safe_name(value):
    value = str(value)
    if value.startswith("models--"):
        value = value[8:].replace("--", "/")
    return "".join(char for char in value if char.isprintable())[:120]


class WalkLimit(Exception):
    pass


class CacheManager:
    def __init__(self, controller, api):
        self.c, self.api = controller, api
        self.root = Path(controller.root)
        self.errors = 0
        self.wall = time.time
        self.monotonic = time.monotonic
        self._units = {}
        self._inodes = {}
        self._used = 0
        self._walk = None
        self._trash = None
        self._entries = 0
        self._measurement_done = False
        self._measuring = False
        self._measurement_error = False
        self._walk_limited = False
        self._measured_keep = None
        self._epoch_extra = 0
        self._evicting = False
        self._remeasure = None
        self._clear = None
        self._guard = None
        self._oldest = []
        self._sizes = []
        self._pip_times = []
        self._pip_bytes = 0
        self._serial = 0
        # This iterator holds only an already-measured inode map, no directory
        # iterator/fd. Its bookkeeping may resume even after an intervening job.
        self._pending_index = None
        for parts in (("shared",), ("shared", "cache"), ("shared", "prepare"), ("shared", "trash")):
            try:
                self._mkdir(parts)
            except (OSError, ValueError):
                api.reject("unsafe_state", "Shared download directory ownership or type is unsafe")

    def _get(self, key, default=None):
        row = self.c.db.execute("SELECT value FROM settings WHERE key=?", (key,)).fetchone()
        if row is None:
            return default
        try:
            return json.loads(row[0])
        except (ValueError, TypeError):
            return default

    def _set(self, key, value):
        self.c.db.execute("INSERT OR REPLACE INTO settings VALUES(?,?)", (key, json.dumps(value, separators=(",", ":"))))

    @staticmethod
    def _owned(info):
        return os.name != "posix" or info.st_uid == os.getuid()

    @staticmethod
    def _identity(info):
        return info.st_dev, info.st_ino

    @staticmethod
    def _parts(parts):
        if any(not isinstance(p, str) or not p or p in (".", "..") or "/" in p or "\\" in p for p in parts):
            raise ValueError("Unsafe relative directory")
        return tuple(parts)

    def _open_dir(self, name, parent=None, writable=False):
        flags = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW
        try:
            fd = os.open(name, flags, dir_fd=parent)
        except PermissionError:
            # Linux O_PATH pins a 0000 directory without following a replacement
            # symlink. chmod through that pinned descriptor, then verify reopening.
            if not writable or not hasattr(os, "O_PATH"):
                raise
            pinned = os.open(name, os.O_PATH | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=parent)
            try:
                info = os.fstat(pinned)
                if not self._owned(info):
                    raise PermissionError("Directory owner mismatch")
                os.chmod("/proc/self/fd/" + str(pinned), 0o700)
                fd = os.open(name, flags, dir_fd=parent)
                if self._identity(os.fstat(fd)) != self._identity(info):
                    os.close(fd)
                    raise OSError("Directory changed during permission repair")
            finally:
                os.close(pinned)
        info = os.fstat(fd)
        if not stat.S_ISDIR(info.st_mode) or not self._owned(info):
            os.close(fd)
            raise PermissionError("Directory ownership or type mismatch")
        if writable:
            os.fchmod(fd, 0o700)
        return fd

    @contextlib.contextmanager
    def _dir(self, parts, writable=False):
        parts = self._parts(parts)
        if os.name == "posix":
            fd = self._open_dir(str(self.root), writable=writable)
            try:
                for part in parts:
                    child = self._open_dir(part, fd, writable)
                    os.close(fd)
                    fd = child
                yield fd
            finally:
                os.close(fd)
        else:
            # Portable argument/unit tests only. Production scheduler is Linux.
            path = self.root
            for part in (None,) + parts:
                if part is not None:
                    path /= part
                info = path.lstat()
                if not stat.S_ISDIR(info.st_mode) or stat.S_ISLNK(info.st_mode):
                    raise OSError("Directory is not real")
            yield path

    @staticmethod
    def _stat_at(parent, name):
        return os.stat(name, dir_fd=parent, follow_symlinks=False) if isinstance(parent, int) else (parent / name).lstat()

    def _lstat(self, parts):
        with self._dir(parts[:-1]) as parent:
            return self._stat_at(parent, parts[-1])

    def _entry_stat(self, entry, parent_parts):
        # Windows DirEntry's cached stat can report inode 0; path lstat returns
        # the file identity used by the portable test implementation.
        return entry.stat(follow_symlinks=False) if os.name == "posix" else self._lstat(parent_parts + (entry.name,))

    def _mkdir(self, parts):
        with self._dir(parts[:-1], writable=True) as parent:
            try:
                if isinstance(parent, int):
                    os.mkdir(parts[-1], 0o700, dir_fd=parent)
                else:
                    (parent / parts[-1]).mkdir(mode=0o700)
            except FileExistsError:
                pass
        with self._dir(parts, writable=True):
            pass

    @contextlib.contextmanager
    def _scan(self, parts, writable=False):
        with self._dir(parts, writable=writable) as folder:
            with os.scandir(folder) as entries:
                yield entries

    def _rename_trash(self, parts, expected):
        """All ancestors are O_NOFOLLOW directories; final inode must be exact."""
        parts = self._parts(parts)
        if parts[:2] not in (("shared", "cache"), ("shared", "prepare"), ("shared", "trash")) and not (parts[0] == "jobs" and len(parts) == 3 and parts[-1] == "tmp"):
            raise ValueError("Rename outside download or job temporary storage")
        with self._dir(parts[:-1], writable=True) as source, self._dir(("shared", "trash"), writable=True) as target:
            info = self._stat_at(source, parts[-1])
            if self._identity(info) != tuple(expected):
                raise OSError("Entry changed before quarantine")
            name = uuid.uuid4().hex
            if isinstance(source, int):
                os.rename(parts[-1], name, src_dir_fd=source, dst_dir_fd=target)
            else:
                os.rename(source / parts[-1], target / name)
        return ("shared", "trash", name)

    def repair_roots(self):
        """Repair only documented roots; no failure may stop scheduler progress."""
        good = True
        try:
            with self._dir(("shared", "cache"), writable=True):
                pass
        except (OSError, ValueError):
            self.errors += 1
            return False
        for relative in REPAIR_ROOTS:
            parts = ("shared", "cache") + tuple(relative.split("/"))
            try:
                try:
                    info = self._lstat(parts)
                except FileNotFoundError:
                    info = None
                if info is not None and not stat.S_ISDIR(info.st_mode):
                    self._rename_trash(parts, self._identity(info))
                self._mkdir(parts)
            except (OSError, ValueError):
                self.errors += 1
                good = False
        return good

    def create_args(self, row, preparing=False, network_open=False):
        """Return additional Docker argv; shared caches never contain job code."""
        if preparing:
            self._mkdir(("shared", "prepare", "pip"))
            return ["--mount", f"type=bind,src={self.root / 'shared' / 'prepare'},dst=/prepare-cache",
                    "--env", "PIP_CACHE_DIR=/prepare-cache/pip"]
        if not self.repair_roots():
            self.api.reject("unsafe_state", "Saved download roots could not be repaired safely")
        job = self.api.identifier(row["id"])
        try:
            self._mkdir(("jobs", job, "tmp"))
            with self._dir(("shared", "cache")):
                pass
        except (OSError, ValueError):
            self.api.reject("unsafe_state", "Saved downloads or job temporary directory is unsafe")
        args = ["--mount", f"type=bind,src={self.root / 'shared' / 'cache'},dst=/cache",
                "--mount", f"type=bind,src={self.root / 'jobs' / job / 'tmp'},dst=/var/tmp"]
        env = ("HF_HOME=/cache/huggingface", "HF_HUB_CACHE=/cache/huggingface/hub", "TORCH_HOME=/cache/torch",
               "PIP_CACHE_DIR=/cache/pip", "UV_CACHE_DIR=/cache/uv", "UV_LINK_MODE=copy", "XDG_CACHE_HOME=/cache/xdg",
               "TMPDIR=/var/tmp", "HF_TOKEN_PATH=/tmp/huggingface/token", "HF_MODULES_CACHE=/tmp/huggingface/modules",
               "TORCH_EXTENSIONS_DIR=/var/tmp/torch_extensions", "TRITON_CACHE_DIR=/var/tmp/triton",
               "TORCHINDUCTOR_CACHE_DIR=/var/tmp/torchinductor", "CUDA_CACHE_PATH=/var/tmp/cuda",
               "HF_HUB_DOWNLOAD_TIMEOUT=60", "PIP_TIMEOUT=60", "PIP_RETRIES=10", "UV_HTTP_TIMEOUT=120",
               "HF_HUB_DISABLE_TELEMETRY=1")
        for value in env:
            args += ["--env", value]
        if not network_open:
            for name in ("HF_HUB_OFFLINE", "HF_DATASETS_OFFLINE", "TRANSFORMERS_OFFLINE"):
                args += ["--env", name + "=1"]
        return args

    def status(self, storage=None):
        if storage is None:
            free, total = self.c.disk_probe()
            storage = {"free_bytes": free, "total_bytes": total}
        usage = self._get("downloads_usage", {})
        keep = self._get("downloads_keep_gb", KEEP_GB)
        return {"v": 1, "folder": "/cache", "files_folder": "/cache/files",
                "used_bytes": usage.get("used_bytes"), "keep_bytes": int(keep) * GIB,
                "free_bytes": max(0, int(storage["free_bytes"])), "floor_bytes": FLOOR,
                "measured_at": usage.get("measured_at"), "over_limit": usage.get("over_limit"),
                "clear_pending": bool(self._get("downloads_clear")), "atime": usage.get("atime"),
                "items": usage.get("items", []), "last_cleanup": self._get("downloads_last_cleanup")}

    def downloads(self, request):
        self.api.fields(request, ("v", "op", "client"), ("keep_gb", "free_up"))
        if request["client"] not in self.api.CLIENTS:
            self.api.reject("invalid_request", "Unknown client")
        changes = "keep_gb" in request or "free_up" in request
        if changes and request["client"] != "human":
            self.api.reject("owner_only", "Only the owner may change or clear saved downloads")
        free, total = self.c.disk_probe()
        keep = request.get("keep_gb")
        if "keep_gb" in request and (type(keep) is not int or not 20 <= keep <= int(total) // GIB):
            self.api.reject("invalid_request", "Saved downloads limit must be from 20 GiB up to the Spark disk size")
        if "free_up" in request and type(request["free_up"]) is not bool:
            self.api.reject("invalid_request", "free_up must be boolean")
        if changes:
            with self.c.transaction():
                if keep is not None:
                    self._set("downloads_keep_gb", keep)
                    self.c.audit("downloads_limit_changed", client="human", keep_gb=keep)
                if request.get("free_up"):
                    self._set("downloads_clear", uuid.uuid4().hex)
                    self.c.audit("downloads_clear_requested", client="human")
        return {"downloads": self.status({"free_bytes": free, "total_bytes": total})}

    def _active(self):
        return self.c.db.execute("SELECT 1 FROM jobs WHERE state IN ('starting','running') LIMIT 1").fetchone() is not None

    def _epoch(self):
        row = self.c.db.execute("SELECT COALESCE(MAX(started),0),COUNT(*) FROM jobs WHERE started IS NOT NULL").fetchone()
        return row[0], row[1], self._epoch_extra

    def _guard_empty(self, deadline):
        if self._guard is None:
            try:
                remaining = deadline - self.monotonic()
                if remaining <= 0:
                    return False
                result = self.c.docker.call(["ps", "-q", "--filter", "name=sparkdev-job-"], timeout=remaining)
                self._guard = not result.strip()
            except Exception:
                self.errors += 1
                self._guard = False
        return self._guard

    def _trash_walk(self):
        def delete(parts, depth=0):
            try:
                info = self._lstat(parts)
                yield None
                if stat.S_ISDIR(info.st_mode):
                    if depth >= 256 or len(os.fsencode(str(self.root.joinpath(*parts)))) > 2048:
                        self._rename_trash(parts, self._identity(info))
                        yield None
                        return
                    with self._scan(parts, writable=True) as entries:
                        for entry in entries:
                            yield from delete(parts + (entry.name,), depth + 1)
                    with self._dir(parts[:-1], writable=True) as parent:
                        current = self._stat_at(parent, parts[-1])
                        if self._identity(current) != self._identity(info):
                            raise OSError("Trash directory changed")
                        if isinstance(parent, int):
                            os.rmdir(parts[-1], dir_fd=parent)
                        else:
                            (parent / parts[-1]).rmdir()
                else:
                    with self._dir(parts[:-1], writable=True) as parent:
                        if self._identity(self._stat_at(parent, parts[-1])) != self._identity(info):
                            raise OSError("Trash file changed")
                        if isinstance(parent, int):
                            os.unlink(parts[-1], dir_fd=parent)
                        else:
                            (parent / parts[-1]).unlink()
                yield None
            except FileNotFoundError:
                pass
            except (OSError, ValueError):
                self.errors += 1
        with self._scan(("shared", "trash"), writable=True) as entries:
            for entry in entries:
                yield from delete(("shared", "trash", entry.name))

    def _finished_tmp(self, deadline):
        rows = self.c.db.execute("SELECT id FROM jobs WHERE state IN ('succeeded','failed','cancelled') ORDER BY finished DESC").fetchall()
        candidates = []
        for row in rows:
            if self.monotonic() >= deadline:
                return
            try:
                parts = ("jobs", self.api.identifier(row[0]), "tmp")
                info = self._lstat(parts)
                candidates.append((row[0], parts, self._identity(info)))
            except FileNotFoundError:
                pass
            except (OSError, ValueError):
                self.errors += 1
        if not candidates or self.monotonic() >= deadline:
            return
        try:
            raw = self.c.docker.call(["ps", "--filter", "name=sparkdev-job-", "--format", "{{.Names}}"], timeout=max(0.001, deadline - self.monotonic()))
            names = set(raw.decode("ascii", "strict").split()) if isinstance(raw, bytes) else set(raw.split())
        except Exception:
            self.errors += 1
            return
        for job, parts, identity in candidates:
            if self.monotonic() >= deadline:
                return
            if self.c.container_name(job) not in names:
                try:
                    self._rename_trash(parts, identity)
                except (OSError, ValueError):
                    self.errors += 1

    def _close_walk(self):
        for name in ("_walk", "_remeasure", "_clear"):
            generator = getattr(self, name)
            if generator is not None:
                generator.close()
                setattr(self, name, None)

    def _count(self):
        self._entries += 1
        if self._entries > WALK_MAX:
            raise WalkLimit()

    def _discover(self):
        queue = deque([("cache", ()), ("prepare", ())])
        while queue:
            area, relative = queue.popleft()
            parts = ("shared", area) + relative
            try:
                with self._scan(parts) as entries:
                    children, directories = [], False
                    for entry in entries:
                        info = self._entry_stat(entry, parts)
                        self._count()
                        yield None
                        child = relative + (entry.name,)
                        is_dir = stat.S_ISDIR(info.st_mode)
                        if relative and relative[0] == "pip":
                            children.append((child, is_dir))
                            directories |= is_dir
                        elif is_dir and (child in PARENTS or child == ("pip",)):
                            queue.append((area, child))
                        else:
                            yield area, child
                    if relative and relative[0] == "pip":
                        if not directories:
                            yield area, relative
                        else:
                            for child, is_dir in children:
                                if is_dir:
                                    queue.append((area, child))
                                else:
                                    yield area, child
            except FileNotFoundError:
                continue
            except (OSError, ValueError):
                self.errors += 1
                self._measurement_error = True

    def _measure(self, area, relative):
        parts = ("shared", area) + relative
        root_info = self._lstat(parts)
        now, epoch = self.wall(), self._epoch()
        unit = {"area": area, "relative": relative, "identity": self._identity(root_info),
                "epoch": epoch, "inodes": {}, "other": 0, "last_used": 0, "bytes": 0}
        queue = deque([(parts, root_info)])
        while queue:
            current, info = queue.popleft()
            self._count()
            size = max(0, int(getattr(info, "st_blocks", math.ceil(info.st_size / 512))) * 512)
            regular = stat.S_ISREG(info.st_mode)
            used = max(info.st_mtime, info.st_atime) if regular else info.st_mtime
            unit["last_used"] = max(unit["last_used"], min(now, used))
            if regular:
                inode = self._identity(info)
                if inode not in unit["inodes"]:
                    unit["inodes"][inode] = size
                    unit["bytes"] += size
            else:
                unit["other"] += size
                unit["bytes"] += size
            yield None
            if stat.S_ISDIR(info.st_mode):
                with self._scan(current) as entries:
                    for entry in entries:
                        child_info = self._entry_stat(entry, current)
                        queue.append((current + (entry.name,), child_info))
                        # Limit temporary directory queues too, not only processing.
                        if self._entries + len(queue) > WALK_MAX:
                            raise WalkLimit()
                        yield None
        if self._identity(self._lstat(parts)) != unit["identity"]:
            raise OSError("Unit changed while measuring")
        return unit

    def _drop_unit(self, key):
        for _ in self._drop_steps(key):
            pass

    def _drop_steps(self, key):
        old = self._units.pop(key, None)
        if old is None:
            return
        self._used -= old["other"]
        if old["relative"][0] == "pip":
            self._pip_bytes -= old["bytes"]
        for inode in old["inodes"]:
            reference = self._inodes[inode]
            reference[0] -= 1
            if not reference[0]:
                self._used -= reference[1]
                del self._inodes[inode]
            yield None

    def _add_unit(self, unit):
        for _ in self._add_steps(unit):
            pass

    def _add_steps(self, unit):
        key = unit["area"], unit["relative"]
        yield from self._drop_steps(key)
        self._used += unit["other"]
        for inode, size in unit["inodes"].items():
            if inode in self._inodes:
                self._inodes[inode][0] += 1
            else:
                self._inodes[inode] = [1, size]
                self._used += size
            yield None
        self._units[key] = unit
        self._serial += 1
        unit["serial"] = self._serial
        heapq.heappush(self._oldest, (unit["last_used"], self._serial, key))
        if unit["relative"][0] == "pip":
            self._pip_bytes += unit["bytes"]
            heapq.heappush(self._pip_times, (-unit["last_used"], self._serial, key))
        else:
            heapq.heappush(self._sizes, (-unit["bytes"], self._serial, key))

    def _finish_index(self):
        # Do not yield-from the pending iterator: closing a measurement because
        # a job started must retain this fd-free, resumable bookkeeping.
        while self._pending_index is not None:
            try:
                next(self._pending_index)
                yield None
            except StopIteration:
                self._pending_index = None

    def _measurement(self):
        for candidate in self._discover():
            yield None
            if candidate is None:
                continue
            area, relative = candidate
            existing = self._units.get(candidate)
            try:
                identity = self._identity(self._lstat(("shared", area) + relative))
                if existing is not None and existing["identity"] == identity:
                    continue
                unit = yield from self._measure(area, relative)
                self._pending_index = self._add_steps(unit)
                yield from self._finish_index()
            except FileNotFoundError:
                self._pending_index = self._drop_steps(candidate)
                yield from self._finish_index()
            except (OSError, ValueError):
                self.errors += 1
                self._measurement_error = True
        return True

    def _items(self):
        rows, restore = [], []
        while self._sizes and len(rows) < 20:
            size, serial, key = heapq.heappop(self._sizes)
            unit = self._units.get(key)
            if unit is None or unit["serial"] != serial:
                continue
            restore.append((size, serial, key))
            relative = unit["relative"]
            rows.append({"kind": relative[0], "name": safe_name(relative[-1]), "bytes": unit["bytes"], "last_used_at": unit["last_used"]})
        for item in restore:
            heapq.heappush(self._sizes, item)
        while self._pip_times:
            when, serial, key = self._pip_times[0]
            if key not in self._units or self._units[key]["serial"] != serial:
                heapq.heappop(self._pip_times)
            else:
                rows.append({"kind": "pip", "name": "pip", "bytes": self._pip_bytes, "last_used_at": -when})
                break
        return sorted(rows, key=lambda row: (-row["bytes"], row["name"]))[:20]

    def _atime_check(self):
        name = ".atime-probe-" + uuid.uuid4().hex
        try:
            with self._dir(("shared",), writable=True) as parent:
                if isinstance(parent, int):
                    fd = os.open(name, os.O_RDWR | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600, dir_fd=parent)
                    try:
                        os.write(fd, b"x")
                        past = self.wall() - 3 * 86400
                        os.utime(fd, (past, past))
                        before = os.fstat(fd).st_atime
                        os.lseek(fd, 0, os.SEEK_SET)
                        os.read(fd, 1)
                        after = os.fstat(fd).st_atime
                    finally:
                        os.close(fd)
                        os.unlink(name, dir_fd=parent)
                else:
                    return {"checked_at": self.wall(), "updates": None, "scope": "shared_filesystem", "reason": "linux_only"}
            return {"checked_at": self.wall(), "updates": after > before, "scope": "shared_filesystem"}
        except OSError:
            self.errors += 1
            return {"checked_at": self.wall(), "updates": None, "scope": "shared_filesystem", "reason": "probe_failed"}

    def _save_usage(self, over_limit=None, measured=False):
        previous = self._get("downloads_usage", {})
        incomplete = self._measurement_error or self._walk_limited
        self._set("downloads_usage", {"used_bytes": None if incomplete else max(0, self._used), "measured_at": self.wall() if measured else previous.get("measured_at"),
                  "over_limit": over_limit, "atime": self._atime_check() if measured else previous.get("atime"),
                  "items": self._items(), "entries": self._entries, "errors": self.errors})

    def _clear_walk(self, token):
        for area in ("cache", "prepare"):
            with self._scan(("shared", area), writable=True) as entries:
                for entry in entries:
                    info = self._entry_stat(entry, ("shared", area))
                    self._rename_trash(("shared", area, entry.name), self._identity(info))
                    yield None
        self._mkdir(("shared", "cache", "files"))
        self._units.clear()
        self._inodes.clear()
        self._oldest.clear()
        self._sizes.clear()
        self._pip_times.clear()
        self._pip_bytes = 0
        self._used = self._entries = 0
        self._measurement_done = False
        self._measuring = False
        self._measurement_error = False
        self._walk_limited = False
        with self.c.transaction():
            # A concurrent new owner request remains pending rather than lost.
            self.c.db.execute("UPDATE settings SET value='null' WHERE key='downloads_clear' AND value=?", (json.dumps(token, separators=(",", ":")),))
            cleanup = {"at": self.wall(), "reason": "owner_clear", "errors": self.errors}
            self._set("downloads_last_cleanup", cleanup)
            self.c.audit("downloads_cleared", client="human", errors=self.errors)
            self._save_usage(measured=True)

    def _low_inodes(self):
        if not hasattr(os, "statvfs"):
            return False
        try:
            info = os.statvfs(self.root)
            return bool(info.f_files and info.f_favail / info.f_files < 0.02)
        except OSError:
            return False

    def _new_measurement(self, keep):
        self._close_walk()
        self._units.clear()
        self._inodes.clear()
        self._oldest.clear()
        self._sizes.clear()
        self._pip_times.clear()
        self._pip_bytes = 0
        self._used = self._entries = 0
        self._measurement_done = False
        self._measurement_error = False
        self._walk_limited = False
        self._measuring = True
        self._measured_keep = keep
        self._walk = self._measurement()

    def _advance(self, generator, deadline):
        while self.monotonic() < deadline:
            try:
                next(generator)
            except StopIteration as done:
                return True, done.value
        return False, None

    def _remeasure_unit(self, key):
        try:
            unit = yield from self._measure(*key)
            self._pending_index = self._add_steps(unit)
        except FileNotFoundError:
            self._pending_index = self._drop_steps(key)
        except (OSError, ValueError):
            self.errors += 1
            self._pending_index = self._drop_steps(key)
        except WalkLimit:
            self._walk_limited = True
            self._pending_index = self._drop_steps(key)
        yield from self._finish_index()

    def _evicted_steps(self, key, unit):
        yield from self._drop_steps(key)
        cleanup = {"at": self.wall(), "reason": "automatic", "bytes": unit["bytes"], "errors": self.errors}
        with self.c.transaction():
            self.c.audit("downloads_tidied", bytes=unit["bytes"], kind_name=safe_name(key[1][0]))
            self._set("downloads_last_cleanup", cleanup)
            self._save_usage()

    def _evict(self, keep, free, deadline):
        if not self._evicting:
            self._evicting = self._used > keep or free < FLOOR
        if not self._evicting:
            return
        if self._used <= keep * 9 // 10 and free >= FLOOR:
            self._evicting = False
            self._save_usage()
            return
        if self._remeasure is not None:
            done, _ = self._advance(self._remeasure, deadline)
            if not done:
                return
            self._remeasure = None
        while self._oldest:
            if self.monotonic() >= deadline:
                return
            when, serial, key = self._oldest[0]
            unit = self._units.get(key)
            if unit is None or unit["serial"] != serial:
                heapq.heappop(self._oldest)
                continue
            if unit["last_used"] >= self.wall() - RECENT_SECONDS:
                break
            if unit["epoch"] != self._epoch():
                self._remeasure = self._remeasure_unit(key)
                return
            if not self._guard_empty(deadline):
                return
            try:
                self._rename_trash(("shared", key[0]) + key[1], unit["identity"])
                self._pending_index = self._evicted_steps(key, unit)
                self._advance(self._finish_index(), deadline)
                # Purge this unit from trash next tick before deciding whether
                # actual free space requires another eviction.
                return
            except (OSError, ValueError):
                self.errors += 1
                self._measurement_error = True
                self._pending_index = self._drop_steps(key)
                self._advance(self._finish_index(), deadline)
                return
        self._evicting = False
        self._save_usage(over_limit="walk_limit" if self._walk_limited else ("measurement_error" if self._measurement_error else "recently_used"))

    def tidy_step(self):
        """Bounded, resumable maintenance. ONLY serve() under scheduler.lock."""
        deadline = self.monotonic() + STEP_SECONDS
        self._guard = None
        try:
            active = self._active()
            if active:
                self._epoch_extra += 1
                self._close_walk()
            if self._trash is None:
                self._trash = self._trash_walk()
            done, _ = self._advance(self._trash, deadline)
            if not done:
                return
            self._trash = None
            self._finished_tmp(deadline)
            if active:
                return
            if self.monotonic() >= deadline:
                return
            if self._pending_index is not None:
                done, _ = self._advance(self._finish_index(), deadline)
                if not done:
                    return
            token = self._get("downloads_clear")
            if token:
                if not self._guard_empty(deadline):
                    return
                if self._clear is None:
                    if self._walk is not None:
                        self._walk.close()
                        self._walk = None
                    self._clear = self._clear_walk(token)
                done, _ = self._advance(self._clear, deadline)
                if done:
                    self._clear = None
                return
            free, _ = self.c.disk_probe()
            keep = self._get("downloads_keep_gb", KEEP_GB) * GIB
            usage = self._get("downloads_usage", {})
            stale = self.wall() - (usage.get("measured_at") or 0) > MEASURE_SECONDS
            if not self._measuring and (not self._measurement_done or stale or self._measured_keep != keep or free < FLOOR or self._low_inodes()):
                # Finish eviction using measured units before starting a fresh
                # low-space scan; otherwise large trees could never be evicted.
                if not (self._measurement_done and self._evicting):
                    self._new_measurement(keep)
            if self._measuring:
                if self._walk is None:
                    self._walk = self._measurement()
                try:
                    done, _ = self._advance(self._walk, deadline)
                except WalkLimit:
                    self._walk.close()
                    done = True
                    self._measurement_error = True
                    self._walk_limited = True
                if not done:
                    return
                self._walk = None
                self._measuring = False
                self._measurement_done = True
                self._save_usage(over_limit="walk_limit" if self._walk_limited else ("measurement_error" if self._measurement_error else None), measured=True)
            if self.monotonic() < deadline:
                self._evict(keep, free, deadline)
        except Exception:
            # Cleanup must not break scheduling; never log paths or cache data.
            self.errors += 1
            self._close_walk()
            if self._trash is not None:
                self._trash.close()
                self._trash = None
