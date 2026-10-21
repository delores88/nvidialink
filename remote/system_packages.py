#!/usr/bin/env python3
"""Locked Debian packages: unprivileged resolution/fetch, offline image assembly.

No Docker socket is mounted in a container. Only the controller commits an image.
The controller supplies its API to avoid importing its __main__ instance twice.
"""
from __future__ import annotations

import argparse
import contextlib
import hashlib
import http.client
import ipaddress
import json
import os
from pathlib import Path
import re
import shlex
import signal
import socket
import ssl
import stat
import subprocess
import sys
import tempfile
import time
from urllib.parse import urlsplit

LOCK = "sparkdev-system.lock"
LIMIT = 1024 * 1024
PACKAGE_LIMIT = 2048
DEB_LIMIT = 2 * 1024**3
TOTAL_LIMIT = 16 * 1024**3
TIME_LIMIT = 3600
NAME = re.compile(r"[a-z0-9][a-z0-9+.-]{0,127}\Z")
VERSION = re.compile(r"[0-9][A-Za-z0-9.+:~\-]{0,199}\Z")
HASH = re.compile(r"[0-9a-f]{64}\Z")
IMAGE = re.compile(r"sha256:[0-9a-f]{64}\Z")
DISTROS = {("debian", "12"): "bookworm", ("debian", "13"): "trixie",
           ("ubuntu", "22.04"): "jammy", ("ubuntu", "24.04"): "noble"}
MARKER = Path("/usr/local/share/sparkdev/system-image.json")
PREFIX = "com.delores.sparkdev.system."


class Invalid(ValueError):
    """Only fixed, nonsecret messages escape this boundary."""


def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=True).encode()


def sha(data):
    return hashlib.sha256(data).hexdigest()


def fields(value, required, optional=()):
    if not isinstance(value, dict) or not set(required) <= value.keys() or set(value) - set(required) - set(optional):
        raise Invalid("System package lock has unsupported or missing fields")


def bounded(path, limit=LIMIT):
    before = os.lstat(path)
    if not stat.S_ISREG(before.st_mode):
        raise Invalid("System package file must be regular")
    fd = os.open(path, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_NONBLOCK", 0))
    with os.fdopen(fd, "rb") as stream:
        info = os.fstat(stream.fileno())
        if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1 or info.st_size > limit or (info.st_dev, info.st_ino) != (before.st_dev, before.st_ino):
            raise Invalid("System package file must be a bounded single-link regular file")
        raw = stream.read(limit + 1)
        if len(raw) > limit:
            raise Invalid("System package file is too large")
        return raw


def url(value):
    if not isinstance(value, str) or len(value) > 4096 or any(ord(c) <= 32 or ord(c) >= 127 for c in value):
        raise Invalid("Package URL must be a bounded HTTPS URL")
    try:
        parsed = urlsplit(value)
        if parsed.scheme != "https" or not parsed.hostname or parsed.port not in (None, 443) or parsed.username or parsed.password or parsed.query or parsed.fragment:
            raise ValueError()
        if not re.fullmatch(r"[A-Za-z0-9.-]+", parsed.hostname) or not parsed.path.endswith(".deb"):
            raise ValueError()
        try:
            address = ipaddress.ip_address(parsed.hostname)
        except ValueError:
            address = None
        if address is not None and not address.is_global:
            raise Invalid("Private package URLs are refused")
    except (ValueError, TypeError):
        raise Invalid("Package URL must be HTTPS without credentials or query parameters") from None
    return value


def validate(value):
    fields(value, ("v", "manager", "base_image", "distro", "architecture", "packages"), ("requested",))
    if type(value["v"]) is not int or value["v"] != 1 or value["manager"] != "apt" or not isinstance(value["base_image"], str) or not IMAGE.fullmatch(value["base_image"]):
        raise Invalid("System package lock requires v1 apt and an immutable base image")
    fields(value["distro"], ("id", "version_id"))
    if any(not isinstance(v, str) for v in value["distro"].values()) or (value["distro"]["id"], value["distro"]["version_id"]) not in DISTROS or value["architecture"] not in ("arm64", "amd64"):
        raise Invalid("System packages require a supported Debian or Ubuntu base and architecture")
    packages = value["packages"]
    if not isinstance(packages, list) or len(packages) > PACKAGE_LIMIT:
        raise Invalid("Too many system packages in the lock")
    names, hashes, total = set(), set(), 0
    for item in packages:
        fields(item, ("name", "version", "url", "sha256", "bytes"))
        if not isinstance(item["name"], str) or not NAME.fullmatch(item["name"]) or not isinstance(item["version"], str) or not VERSION.fullmatch(item["version"]):
            raise Invalid("Every system package needs an exact name and version")
        if not isinstance(item["sha256"], str) or not HASH.fullmatch(item["sha256"]) or type(item["bytes"]) is not int or not 1 <= item["bytes"] <= DEB_LIMIT:
            raise Invalid("Every system package needs its SHA-256 and bounded byte size")
        url(item["url"])
        if item["name"] in names or item["sha256"] in hashes:
            raise Invalid("Duplicate system package in lock")
        names.add(item["name"])
        hashes.add(item["sha256"])
        total += item["bytes"]
    if total > TOTAL_LIMIT:
        raise Invalid("System package downloads exceed 16 GiB")
    requested = value.get("requested", [])
    if not isinstance(requested, list) or len(requested) > 128:
        raise Invalid("Too many requested system packages")
    for name in requested:
        package_argument(name)
    return value


def read_lock(path):
    try:
        return validate(json.loads(bounded(path)))
    except (UnicodeError, json.JSONDecodeError):
        raise Invalid("System package lock is not valid UTF-8 JSON") from None


def package_argument(value):
    if not isinstance(value, str):
        raise Invalid("Package request must be NAME or NAME=VERSION")
    parts = value.split("=")
    if len(parts) not in (1, 2) or not NAME.fullmatch(parts[0]) or (len(parts) == 2 and not VERSION.fullmatch(parts[1])):
        raise Invalid("Package request must be NAME or NAME=VERSION")
    return value


def run(args, *, timeout=1200, env=None, capture=False):
    result = subprocess.run(args, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE if capture else None,
                            stderr=subprocess.PIPE if capture else None, env=env, timeout=timeout, check=False)
    if result.returncode:
        raise Invalid("System package command failed; check the package lock and complete dependency closure")
    if capture and len(result.stdout) > 8 * LIMIT:
        raise Invalid("System package metadata exceeded its bound")
    return result.stdout if capture else b""


def platform():
    values = {}
    # Debian commonly links /etc/os-release to this fixed image-owned file.
    release = "/usr/lib/os-release" if Path("/etc/os-release").is_symlink() else "/etc/os-release"
    for line in bounded(release, 16384).decode().splitlines():
        if "=" in line:
            key, value = line.split("=", 1)
            values[key] = value.strip('"\'')
    distro = {"id": values.get("ID"), "version_id": values.get("VERSION_ID")}
    if (distro["id"], distro["version_id"]) not in DISTROS:
        raise Invalid("Pinned runtime is not a supported Debian or Ubuntu distribution")
    arch = run(["dpkg", "--print-architecture"], capture=True).decode().strip()
    if arch not in ("arm64", "amd64"):
        raise Invalid("Pinned runtime has an unsupported package architecture")
    return distro, arch


def check_platform(lock):
    distro, arch = platform()
    if distro != lock["distro"] or arch != lock["architecture"]:
        raise Invalid("System package lock does not match the pinned base distribution and architecture")


def deb_metadata(path):
    text = run(["dpkg-deb", "--show", "--showformat=${Package}\t${Version}\t${Architecture}\n", str(path)], capture=True).decode().strip()
    fields_ = text.split("\t")
    if len(fields_) != 3 or not NAME.fullmatch(fields_[0]) or not VERSION.fullmatch(fields_[1]):
        raise Invalid("Downloaded file is not a valid Debian package")
    return fields_


def digest_file(path, expected_size):
    before = os.lstat(path)
    if not stat.S_ISREG(before.st_mode):
        raise Invalid("Downloaded package must be regular")
    fd = os.open(path, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_NONBLOCK", 0))
    with os.fdopen(fd, "rb") as stream:
        info = os.fstat(stream.fileno())
        if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1 or info.st_size != expected_size or (info.st_dev, info.st_ino) != (before.st_dev, before.st_ino):
            raise Invalid("Downloaded package size or file type differs from lock")
        h, count = hashlib.sha256(), 0
        while data := stream.read(1024 * 1024):
            count += len(data)
            if count > expected_size:
                raise Invalid("Downloaded package changed during verification")
            h.update(data)
        if count != expected_size or os.fstat(stream.fileno()).st_mtime_ns != info.st_mtime_ns:
            raise Invalid("Downloaded package changed during verification")
        return h.hexdigest()


def verify_debs(lock, directory):
    for item in lock["packages"]:
        file = Path(directory) / (item["sha256"] + ".deb")
        if digest_file(file, item["bytes"]) != item["sha256"]:
            raise Invalid("Downloaded package SHA-256 differs from lock")
        name, version, arch = deb_metadata(file)
        if name != item["name"] or version != item["version"] or arch not in (lock["architecture"], "all"):
            raise Invalid("Downloaded package identity differs from lock")


def download(item, destination, connection=http.client.HTTPSConnection):
    """Fixed proxy only; TLS remains end-to-end. No ambient proxy or redirects."""
    target = urlsplit(url(item["url"]))
    conn = connection("sparkdev-proxy", 3128, timeout=30, context=ssl.create_default_context())
    conn.set_tunnel(target.hostname, 443)
    pending = destination.with_suffix(".pending")
    try:
        with pending.open("xb") as stream:
            conn.request("GET", target.path, headers={"Host": target.hostname, "Connection": "close"})
            reply = conn.getresponse()
            if reply.status != 200:
                raise Invalid("Locked package URL did not return HTTP 200; redirects are not followed")
            declared = reply.getheader("Content-Length")
            if declared is not None and declared != str(item["bytes"]):
                raise Invalid("Locked package download size differs")
            total, h, deadline = 0, hashlib.sha256(), time.monotonic() + 1200
            while data := reply.read(min(1024 * 1024, item["bytes"] - total + 1)):
                if time.monotonic() > deadline:
                    raise Invalid("Locked package download exceeded its time budget")
                total += len(data)
                if total > item["bytes"]:
                    raise Invalid("Locked package download exceeds its byte size")
                stream.write(data)
                h.update(data)
            if total != item["bytes"] or h.hexdigest() != item["sha256"]:
                raise Invalid("Locked package download failed SHA-256 verification")
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(pending, destination)
    finally:
        conn.close()
        with contextlib.suppress(FileNotFoundError):
            pending.unlink()


def fetch(lock, directory):
    if hasattr(os, "geteuid") and os.geteuid() == 0:
        raise Invalid("System package fetching must run without root")
    check_platform(lock)
    directory = Path(directory)
    directory.mkdir(exist_ok=True)
    for item in lock["packages"]:
        destination = directory / (item["sha256"] + ".deb")
        if destination.exists() and digest_file(destination, item["bytes"]) == item["sha256"]:
            continue
        print("sparkdev: fetching locked package " + item["name"], flush=True)
        download(item, destination)
    verify_debs(lock, directory)


def apt_options(temp, online):
    """Discard base hooks/preferences; signatures still use distro keyrings."""
    options = {"Dir::Etc::main": "-", "Dir::Etc::parts": "-", "Dir::Etc::preferences": "-",
               "Dir::Etc::preferencesparts": "-", "Dir::Etc::sourcelist": str(temp / "sources.list"),
               "Dir::Etc::sourceparts": "-", "Dir::State::lists": str(temp / "lists"),
               "Dir::Cache::archives": str(temp / "archives"), "Dir::Cache::pkgcache": "",
               "Dir::Cache::srcpkgcache": "", "Debug::NoLocking": "true",
               "Acquire::Languages": "none", "APT::Get::AllowUnauthenticated": "false",
               "Acquire::AllowInsecureRepositories": "false", "Acquire::AllowDowngradeToInsecureRepositories": "false",
               "Acquire::Retries": "2", "Acquire::http::Timeout": "30", "Acquire::https::Timeout": "30"}
    if online:
        options.update({"Acquire::http::Proxy": "http://sparkdev-proxy:3128", "Acquire::https::Proxy": "http://sparkdev-proxy:3128"})
    return [arg for key, value in options.items() for arg in ("-o", key + "=" + value)]


def apt_environment(temp):
    # APT_CONFIG is read before other files, so it disables inherited apt hooks.
    config = temp / "apt.conf"
    config.write_text('Dir::Etc::main "-"; Dir::Etc::parts "-";\n')
    return {"PATH": "/usr/sbin:/usr/bin:/sbin:/bin", "HOME": str(temp), "LANG": "C.UTF-8", "LC_ALL": "C.UTF-8",
            "APT_CONFIG": str(config), "DEBIAN_FRONTEND": "noninteractive", "TZ": "UTC"}


def make_apt_dirs(temp):
    for name in ("lists/partial", "archives/partial"):
        (temp / name).mkdir(parents=True)


def sources(distro, arch):
    codename = DISTROS[(distro["id"], distro["version_id"])]
    if distro["id"] == "ubuntu":
        host = "ports.ubuntu.com/ubuntu-ports" if arch == "arm64" else "archive.ubuntu.com/ubuntu"
        keyring = "/usr/share/keyrings/ubuntu-archive-keyring.gpg"
        return "".join(f"deb [signed-by={keyring}] https://{host} {suite} main universe restricted multiverse\n"
                       for suite in (codename, codename + "-updates", codename + "-security"))
    keyring = "/usr/share/keyrings/debian-archive-keyring.gpg"
    return (f"deb [signed-by={keyring}] https://deb.debian.org/debian {codename} main\n"
            f"deb [signed-by={keyring}] https://deb.debian.org/debian {codename}-updates main\n"
            f"deb [signed-by={keyring}] https://security.debian.org/debian-security {codename}-security main\n")


def parse_uris(raw):
    result = {}
    for line in raw.decode().splitlines():
        if not line.startswith("'"):
            continue
        parts = shlex.split(line)
        # Some authenticated Debian security indices omit the legacy printed
        # checksum. APT still verifies the download; generate_lock hashes the
        # actual archive with SHA-256 rather than trusting this optional token.
        if len(parts) not in (3, 4) or not parts[2].isdigit() or not 1 <= int(parts[2]) <= DEB_LIMIT:
            raise Invalid("APT download plan has an invalid record")
        address, filename = url(parts[0]), parts[1]
        if Path(filename).name != filename or "/" in filename or "\\" in filename or not filename.endswith(".deb") or filename in result:
            raise Invalid("APT download plan has an unsafe filename")
        result[filename] = (address, int(parts[2]))
    if len(result) > PACKAGE_LIMIT or sum(size for _, size in result.values()) > TOTAL_LIMIT:
        raise Invalid("Resolved system packages exceed the download budget")
    return result


def generate_lock(packages, output, base_image):
    if hasattr(os, "geteuid") and os.geteuid() == 0:
        raise Invalid("System lock generation must run without root")
    if MARKER.exists():
        raise Invalid("Generate a new system lock in a fresh project without an existing system lock, using the same base runtime")
    if not isinstance(base_image, str) or not IMAGE.fullmatch(base_image):
        raise Invalid("Lock generation needs the controller's pinned base image identity")
    if not 1 <= len(packages) <= 128:
        raise Invalid("Choose one to 128 system packages")
    packages = [package_argument(item) for item in packages]
    distro, arch = platform()
    with tempfile.TemporaryDirectory(prefix="sparkdev-system-") as folder:
        temp = Path(folder)
        make_apt_dirs(temp)
        (temp / "sources.list").write_text(sources(distro, arch))
        options, env = apt_options(temp, True), apt_environment(temp)
        run(["apt-get", *options, "update"], env=env)
        install = ["apt-get", *options, "--yes", "--no-install-recommends", "--no-remove", "--reinstall", "--download-only"]
        planned = parse_uris(run([*install, "--print-uris", "install", *packages], env=env, capture=True))
        run([*install, "install", *packages], env=env)
        rows = []
        for filename, (address, size) in planned.items():
            file = temp / "archives" / filename
            digest = digest_file(file, size)
            name, version, package_arch = deb_metadata(file)
            if package_arch not in (arch, "all"):
                raise Invalid("Resolved package architecture differs from the runtime")
            rows.append({"name": name, "version": version, "url": address, "sha256": digest, "bytes": size})
        lock = validate({"v": 1, "manager": "apt", "base_image": base_image, "distro": distro, "architecture": arch,
                         "packages": sorted(rows, key=lambda item: item["name"]), "requested": packages})
        # Never overwrite a source lock merely because a resolver was invoked twice.
        with Path(output).open("xb") as stream:
            stream.write(json.dumps(lock, indent=2, sort_keys=True).encode() + b"\n")
            stream.flush()
            os.fsync(stream.fileno())
        return lock


def installed():
    text = run(["dpkg-query", "-W", "-f=${Package}\t${Version}\t${db:Status-Abbrev}\n"], capture=True).decode()
    result = {}
    for line in text.splitlines():
        parts = line.split("\t")
        if len(parts) != 3:
            raise Invalid("Package database returned invalid metadata")
        if parts[2].startswith("ii"):
            result[parts[0]] = parts[1]
    return result


def install(lock, directory, marker=MARKER):
    if not hasattr(os, "geteuid") or os.geteuid() != 0:
        raise Invalid("Image assembly requires container root; normal jobs must remain nonroot")
    check_platform(lock)
    verify_debs(lock, directory)
    before = installed()
    with tempfile.TemporaryDirectory(prefix="sparkdev-system-install-") as folder:
        temp = Path(folder)
        make_apt_dirs(temp)
        (temp / "sources.list").write_text("")
        env = apt_environment(temp)
        # No repositories, network or downloads; missing closure fails rather than floating.
        paths = [str(Path(directory) / (item["sha256"] + ".deb")) for item in lock["packages"]]
        command = ["apt-get", *apt_options(temp, False), "--no-download", "--no-install-recommends", "--no-remove", "--yes",
                   "-o", "Dpkg::Options::=--force-confold", "install", *paths]
        if paths:
            run(command, env=env, timeout=TIME_LIMIT - 60)
    after = installed()
    expected = {item["name"]: item["version"] for item in lock["packages"]}
    if any(after.get(name) != version for name, version in expected.items()) or any(
            name not in expected and after.get(name) != version for name, version in before.items()) or set(after) - set(before) - set(expected):
        raise Invalid("Installed package set differs from the complete pinned lock")
    marker.parent.mkdir(parents=True, exist_ok=True)
    marker.write_bytes(canonical({"v": 1, "base_image": lock["base_image"], "lock_sha256": sha(canonical(lock))}))


class SystemPackages:
    """Scheduler-owned stages; no work runs until a normal job admission exists."""
    def __init__(self, controller, api):
        self.c, self.a, self.db = controller, api, controller.db
        self.db.execute("CREATE TABLE IF NOT EXISTS system_images(key TEXT PRIMARY KEY,project TEXT,base TEXT,lock_sha TEXT,image TEXT,created REAL)")
        self.db.execute("CREATE TABLE IF NOT EXISTS system_preparations(job TEXT PRIMARY KEY,plan TEXT,phase TEXT,started REAL,failures INTEGER NOT NULL DEFAULT 0)")

    def _reject(self, message):
        self.a.reject("system_packages_invalid", message)

    def _image(self, reference):
        result = self.c.docker.invoke(["image", "inspect", reference], 15)
        if result.returncode:
            if b"No such image" in result.stderr or b"No such object" in result.stderr:
                return None
            self.a.reject("docker_unavailable", "System image state is unavailable")
        try:
            value = json.loads(result.stdout)
            if not isinstance(value, list) or len(value) != 1 or not IMAGE.fullmatch(value[0]["Id"]):
                raise ValueError()
            return value[0]
        except (ValueError, KeyError, TypeError):
            self.a.reject("docker_unavailable", "System image state is invalid")

    def _verify_image(self, info, plan):
        labels = info.get("Config", {}).get("Labels") or {}
        user = info.get("Config", {}).get("User")
        if labels.get(PREFIX + "key") != plan["key"] or labels.get(PREFIX + "base") != plan["base"] or user != self.user():
            self.a.reject("system_image_conflict", "Derived image identity or nonroot user does not match")
        return info["Id"]

    @staticmethod
    def user():
        uid = os.getuid() if hasattr(os, "getuid") else 1000
        gid = os.getgid() if hasattr(os, "getgid") else 1000
        if uid == 0:
            raise Invalid("Run the Spark controller as its ordinary owner, not root")
        return f"{uid}:{gid}"

    def runtime(self, project, manifest, runtime, preparing=False):
        path = self.c.snapshots / manifest / LOCK
        if not os.path.lexists(path):
            return dict(runtime), None
        try:
            lock = read_lock(path)
            if self.a.SECRET.search(canonical(lock)):
                raise Invalid("System package lock contains credential material")
            base = runtime["image"]
            if lock["base_image"] != base:
                raise Invalid("Runtime base changed; regenerate the system lock using the new pinned base")
            lock_sha = sha(canonical(lock))
            key = sha(canonical({"v": 1, "project": project, "base": base, "lock": lock_sha,
                                 "installer": sha(Path(__file__).read_bytes())}))
            plan = {"key": key, "project": project, "base": base, "lock_sha": lock_sha, "lock": lock, "image": None}
            cached = self.db.execute("SELECT image FROM system_images WHERE key=?", (key,)).fetchone()
            if cached and (info := self._image(cached[0])) is not None:
                plan["image"] = self._verify_image(info, plan)
            if not lock["packages"]:
                plan["image"] = base
            if not preparing and not plan["image"]:
                self.a.reject("system_packages_unprepared", "Run sparkdev prepare for this project/runtime before using its system package lock")
            return {**runtime, "image": plan["image"] or base}, plan
        except Invalid as error:
            self._reject(str(error))
        except (OSError, ValueError):
            self._reject("System package lock is invalid, unreadable or does not match the pinned runtime")

    def stage(self, row, plan):
        if plan is None:
            return
        root = self.c.root / "jobs" / row["id"]
        self.a.atomic_write(root / "system_packages.py", Path(__file__).read_bytes(), 0o444)
        self.a.atomic_write(root / LOCK, canonical(plan["lock"]), 0o444)
        (root / "system-debs").mkdir(mode=0o700, exist_ok=True)
        phase = "ready" if plan["image"] else "fetch"
        self.db.execute("INSERT INTO system_preparations(job,plan,phase) VALUES(?,?,?)", (row["id"], canonical(plan).decode(), phase))

    def status(self, row):
        entry = self.db.execute("SELECT plan,phase FROM system_preparations WHERE job=?", (row["id"],)).fetchone()
        if not entry:
            return {}
        plan = json.loads(entry[0])
        return {"system_packages": {"phase": entry[1], "lock_sha256": plan["lock_sha"], "base_image": plan["base"],
                                    "image": plan["image"], "count": len(plan["lock"]["packages"])}}

    def create_args(self, row, args):
        root = self.c.root / "jobs" / row["id"]
        # Caller stages this fixed script for every normal run, allowing lock generation.
        if not (root / "system_packages.py").exists():
            self.a.atomic_write(root / "system_packages.py", Path(__file__).read_bytes(), 0o444)
        args += ["--mount", f"type=bind,src={root / 'system_packages.py'},dst=/sparkdev-system.py,readonly",
                 "--env", "SPARKDEV_BASE_IMAGE=" + row["image"]]
        entry = self.db.execute("SELECT phase FROM system_preparations WHERE job=?", (row["id"],)).fetchone()
        if entry and entry[0] == "ready":
            args += ["--env", "SPARKDEV_SYSTEM_PREPARED=1"]

    def _args(self, row, plan, phase, name):
        root = self.c.root / "jobs" / row["id"]
        builder = phase == "build"
        args = ["create", "--name", name, "--pull", "never", "--network", "none" if builder else "sparkdev-fetch-" + row["id"],
                "--user", "0:0" if builder else self.user(), "--cap-drop", "ALL", "--security-opt", "no-new-privileges",
                "--cpus", str(row["cpu"]), "--cpu-shares", "128", "--memory", f"{row['memory']}g", "--memory-swap", f"{row['memory']}g",
                "--pids-limit", "512", "--init", "--restart", "no", "--log-driver", "local", "--log-opt", "max-size=8m", "--log-opt", "max-file=1",
                "--ulimit", f"fsize={DEB_LIMIT}:{DEB_LIMIT}", "--tmpfs", "/tmp:rw,nosuid,nodev,size=" + str(row["scratch_mb"] * 1024 * 1024),
                "--env", "NVIDIA_VISIBLE_DEVICES=void", "--env", "PYTHONDONTWRITEBYTECODE=1", "--env", "PYTHONUNBUFFERED=1",
                "--label", "com.delores.sparkdev.job=" + row["id"], "--label", PREFIX + "key=" + plan["key"],
                "--label", PREFIX + "phase=" + phase,
                "--mount", f"type=bind,src={root / 'system_packages.py'},dst=/sparkdev-system.py,readonly",
                "--mount", f"type=bind,src={root / LOCK},dst=/sparkdev-system.lock,readonly",
                "--mount", f"type=bind,src={root / 'system-debs'},dst=/sparkdev-debs" + (",readonly" if builder else "")]
        if builder:
            for capability in ("CHOWN", "DAC_OVERRIDE", "FOWNER", "FSETID", "SETUID", "SETGID", "SETFCAP"):
                args += ["--cap-add", capability]
        else:
            args += ["--read-only", "--dns", "127.0.0.1"]
            for key, value in self.a.INTERNET_IPV6_SYSCTLS.items():
                args += ["--sysctl", key + "=" + value]
        return args + ["--entrypoint", "/usr/bin/python3", plan["base"], "/sparkdev-system.py", "install" if builder else "fetch"]

    def _matches(self, info, row, plan, phase):
        config, host = info.get("Config") or {}, info.get("HostConfig") or {}
        labels = config.get("Labels") or {}
        if info.get("Image") != plan["base"] or labels.get("com.delores.sparkdev.job") != row["id"] or labels.get(PREFIX + "key") != plan["key"] or labels.get(PREFIX + "phase") != phase or config.get("User") != ("0:0" if phase == "build" else self.user()):
            self.a.reject("container_conflict", "System preparation container does not match its job")
        expected_network = "none" if phase == "build" else "sparkdev-fetch-" + row["id"]
        if host.get("NetworkMode") != expected_network:
            self.a.reject("container_conflict", "System preparation network does not match its phase")
        if phase == "fetch" and any((host.get("Sysctls") or {}).get(k) != v for k, v in self.a.INTERNET_IPV6_SYSCTLS.items()):
            self.a.reject("container_conflict", "System package fetch is missing its IPv6 guard")

    def _finish(self, row, name, state, code, reason):
        # The identity was checked before this method; never remove unrelated containers.
        if state == "failed":
            # A failed apt/fetch phase must have ordinary `logs JOB` diagnostics.
            # The container uses a bounded local log driver and a fixed program.
            try:
                raw = self.c.docker.call(["logs", "--tail", "1000", name], timeout=15)
                output = self.c.root / "jobs" / row["id"] / "output"
                output.mkdir(mode=0o700, exist_ok=True)
                self.a.atomic_write(output / "stdout.log", b"sparkdev: " + reason.encode() + b"\n" + raw[-8 * LIMIT:], 0o600)
            except (self.a.Rejected, OSError, ValueError):
                pass  # Failure to read logs cannot retain a failed root builder.
        self.c.docker.call(["rm", "--force", name])
        self.db.execute("UPDATE system_preparations SET phase='failed' WHERE job=?", (row["id"],))
        self.c.finish(row, state, code, reason)

    def cleanup(self, row):
        """Terminal preparation cleanup must precede helper/network cleanup."""
        entry = self.db.execute("SELECT plan FROM system_preparations WHERE job=?", (row["id"],)).fetchone()
        if not entry:
            return
        plan = json.loads(entry[0])
        for phase in ("fetch", "build"):
            name = "sparkdev-system-" + phase + "-" + row["id"]
            info = self.c.docker.inspect(name)
            if info is not None:
                self._matches(info, row, plan, phase)
                self.c.docker.call(["rm", "--force", name])

    def reconcile(self, row):
        entry = self.db.execute("SELECT * FROM system_preparations WHERE job=?", (row["id"],)).fetchone()
        if not entry or entry["phase"] not in ("fetch", "build"):
            return False
        plan, phase = json.loads(entry["plan"]), entry["phase"]
        name = "sparkdev-system-" + phase + "-" + row["id"]
        info = self.c.docker.inspect(name)
        if info is not None:
            self._matches(info, row, plan, phase)
        if row["cancel"]:
            if info is not None:
                self._finish(row, name, "cancelled", 130, "system_packages_cancelled")
            else:
                self.db.execute("UPDATE system_preparations SET phase='failed' WHERE job=?", (row["id"],))
                self.c.finish(row, "cancelled", 130, "system_packages_cancelled")
            return True
        if entry["started"] and time.time() - entry["started"] >= TIME_LIMIT:
            if info is not None:
                self._finish(row, name, "failed", 124, "system_packages_timeout")
            else:
                self.c.finish(row, "failed", 124, "system_packages_timeout")
            return True
        if info is None:
            if entry["started"] is not None:
                self.c.finish(row, "failed", 125, "system_packages_container_lost")
                return True
            if phase == "build":
                # Phase advances durably before deleting a successful fetch. A
                # restart finishes this cleanup without downloading again.
                fetched_name = "sparkdev-system-fetch-" + row["id"]
                fetched = self.c.docker.inspect(fetched_name)
                if fetched is not None:
                    self._matches(fetched, row, plan, "fetch")
                    if fetched["State"]["Status"] not in ("exited", "dead") or fetched["State"].get("ExitCode") != 0:
                        self.a.reject("container_conflict", "Finished system fetch identity changed before image assembly")
                    self.c.docker.call(["rm", fetched_name])
                self.c.remove_proxy(row)
            if phase == "fetch":
                if not self.c.internet_policy()["enabled"]:
                    self.c.finish(row, "failed", 125, "system_packages_internet_disabled")
                    return True
                try:
                    self.c.require_disk(sum(p["bytes"] for p in plan["lock"]["packages"]))
                    self.c.ensure_proxy({**dict(row), "egress": "internet"})
                except self.a.Rejected as error:
                    if error.code in ("proxy_starting", "docker_unavailable"):
                        raise
                    self.c.finish(row, "failed", 125, "system_packages_" + ("insufficient_disk" if error.code == "insufficient_disk" else "network_guard_failed"))
                    return True
            try:
                self.c.docker.call(self._args(row, plan, phase, name))
            except self.a.Rejected:
                if self.c.docker.inspect(name) is None:
                    self.db.execute("UPDATE system_preparations SET failures=failures+1 WHERE job=?", (row["id"],))
                    if entry["failures"] + 1 >= 3:
                        self.c.finish(row, "failed", 125, "system_packages_launch_failed")
                raise
            info = self.c.docker.inspect(name)
            if info is None:
                self.a.reject("docker_unavailable", "System preparation container is not inspectable")
            self._matches(info, row, plan, phase)
        state = info["State"]["Status"]
        if phase == "fetch" and not self.c.internet_policy()["enabled"]:
            self._finish(row, name, "failed", 125, "system_packages_internet_disabled")
            self.c.remove_proxy(row)
            return True
        if phase == "fetch":
            try:
                # Refresh the captured host/subnet guard throughout the download,
                # not only once before launch. The helper enforces public-IP expiry.
                self.c.ensure_proxy({**dict(row), "egress": "internet"})
            except self.a.Rejected as error:
                if error.code == "proxy_starting" and state == "created":
                    return True
                if error.code == "docker_unavailable":
                    raise
                self._finish(row, name, "failed", 125, "system_packages_network_guard_failed")
                self.c.remove_proxy(row)
                return True
        if state == "created":
            fresh = self.db.execute("SELECT cancel FROM jobs WHERE id=?", (row["id"],)).fetchone()
            if fresh and fresh[0]:
                self._finish(row, name, "cancelled", 130, "system_packages_cancelled")
                return True
            self.c.docker.call(["start", name])
            self.db.execute("UPDATE system_preparations SET started=COALESCE(started,?) WHERE job=?", (time.time(), row["id"]))
            self.db.execute("UPDATE jobs SET state='running',started=COALESCE(started,?) WHERE id=?", (time.time(), row["id"]))
        elif state in ("exited", "dead"):
            code = int(info["State"]["ExitCode"])
            if code:
                self._finish(row, name, "failed", min(255, max(1, code)), "system_packages_" + phase + "_failed")
            elif phase == "fetch":
                # Do not execute package code while any public network is attached.
                self.db.execute("UPDATE system_preparations SET phase='build',started=NULL,failures=0 WHERE job=?", (row["id"],))
                self.c.docker.call(["rm", name])
                self.c.remove_proxy(row)
            else:
                reference = "sparkdev-system:" + plan["key"]
                image = self._image(reference)
                if image is None:
                    self.c.require_disk(sum(p["bytes"] for p in plan["lock"]["packages"]))
                    base_info = self._image(plan["base"])
                    if base_info is None:
                        self.a.reject("runtime_unavailable", "Pinned base image was removed during preparation")
                    base_env = dict(item.split("=", 1) for item in (base_info.get("Config", {}).get("Env") or []) if "=" in item)
                    # Do not bake preparation's no-GPU override into CUDA runs.
                    gpu_env = "ENV NVIDIA_VISIBLE_DEVICES=" + base_env.get("NVIDIA_VISIBLE_DEVICES", "")
                    self.c.docker.call(["commit", "--change", "USER " + self.user(), "--change", gpu_env,
                                        "--change", "LABEL " + PREFIX + "key=" + plan["key"],
                                        "--change", "LABEL " + PREFIX + "base=" + plan["base"], name, reference], timeout=120)
                    image = self._image(reference)
                if image is None:
                    self.a.reject("docker_unavailable", "Derived image commit cannot be verified")
                plan["image"] = self._verify_image(image, plan)
                with self.c.transaction():
                    self.db.execute("INSERT OR REPLACE INTO system_images VALUES(?,?,?,?,?,?)", (plan["key"], plan["project"], plan["base"], plan["lock_sha"], plan["image"], time.time()))
                    self.db.execute("UPDATE system_preparations SET phase='ready',plan=?,started=NULL WHERE job=?", (canonical(plan).decode(), row["id"]))
                    self.db.execute("UPDATE jobs SET image=?,state='starting',proxy_cleaned=0 WHERE id=?", (plan["image"], row["id"]))
                self.c.docker.call(["rm", name])
        elif state != "running":
            self.a.reject("container_state_unknown", "System preparation container state is unknown")
        return True


def main(argv=None):
    parser = argparse.ArgumentParser(description="Resolve locked system packages without installing them in a job")
    commands = parser.add_subparsers(dest="command", required=True)
    lock = commands.add_parser("lock")
    lock.add_argument("--package", action="append", required=True)
    lock.add_argument("--output", default="/work/" + LOCK)
    for name in ("fetch", "install"):
        commands.add_parser(name)
    args = parser.parse_args(argv)
    try:
        if hasattr(signal, "alarm"):
            # Container PID 1 exits even if a scheduler is temporarily unavailable.
            signal.alarm(TIME_LIMIT)
        if args.command == "lock":
            generate_lock(args.package, args.output, os.environ.get("SPARKDEV_BASE_IMAGE"))
            print("sparkdev: wrote an exact system package lock; run prepare on the target project", flush=True)
        elif args.command == "fetch":
            fetch(read_lock("/sparkdev-system.lock"), Path("/sparkdev-debs"))
        else:
            install(read_lock("/sparkdev-system.lock"), Path("/sparkdev-debs"))
        return 0
    except Invalid as error:
        print("sparkdev: " + str(error), flush=True)
        return 65
    except (OSError, subprocess.SubprocessError):
        # Never echo URL, environment, apt stderr or arbitrary exception contents.
        print("sparkdev: system package step failed; verify the pinned base, complete lock and available disk", flush=True)
        return 65
    finally:
        if hasattr(signal, "alarm"):
            signal.alarm(0)


if __name__ == "__main__":
    raise SystemExit(main())
