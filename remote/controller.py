#!/usr/bin/env python3
"""Owner-only Spark Dev Bridge controller. Transport authentication is external.

SSH invokes `controller.py rpc`; a separate user service invokes `serve`.
Only the scheduler launches containers. Workload argv never becomes a shell string.
"""
from __future__ import annotations

import argparse
import base64
import contextlib
import errno
import hashlib
import ipaddress
import itertools
import json
import math
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import sqlite3
import stat
import subprocess
import sys
import tempfile
import threading
import time
import uuid

import model_cache
import model_server

CHUNK = 1024 * 1024
GIB = 1024**3
MAX_REQUEST = 32 * CHUNK
MAX_FILES = 100000
# Synced source limits, mirrored by the CLI. Files over LARGE_FILE are "large";
# MAX_SMALL_TREE caps the combined size of every other file.
MAX_FILE = 16 * GIB
MAX_TREE = 32 * GIB
MAX_SMALL_TREE = 8 * GIB
LARGE_FILE = 512 * CHUNK
# Controller only.
JOB_FILE_LIMIT = 64 * GIB  # Runs may download model shards; preparations keep their old 4 GiB cap.
PREPARATION_FILE_LIMIT = 4 * GIB
DISK_STOP_FLOOR = 16 * GIB
DISK_RESERVE = 64 * GIB  # Free state-filesystem space kept back; syncs and uploads into it are refused, admissions wait.
PUT_MANY_BODY = 12 * CHUNK
INLINE_BYTES = 512 * CHUNK  # Snapshots needing at most this many new bytes assemble inside the RPC.
# Seconds a sync RPC waits for a background snapshot, counted from controller process start.
# The CLI gives a sync 30 s from spawning ssh or the sidecar, and every poll resends the whole
# manifest. On the paired route the sidecar may take 12 s to connect and the Spark daemon 10 s
# to read that frame before this process starts, which leaves 8.
MATERIALIZE_WAIT = 8
MATERIALIZE_POLL = 0.25
MAX_PENDING = 4
SCHEDULER_FRESH = 15  # Seconds since its last heartbeat for the scheduler to count as alive.
MISSING_BATCH = 10000
PROGRESS_INTERVAL = 1.0
PENDING_READ = 2 * MAX_REQUEST
MAX_JOBS = 4096
MAX_LOG = 256 * CHUNK
CPUS = 4
MEMORY_GB = 8
HEADROOM_GB = 32
PROXY_CPU = 0.25
PROXY_MEMORY_GB = 0.125
RESOURCE_FIELDS = ("cpu_per_job", "memory_gb_per_job", "cpu_total", "memory_gb_total", "client_jobs", "client_cpu", "headroom_gb")
CLIENTS = ("claude", "codex", "human")
ACTIVE = ("queued", "starting", "running")
TERMINAL = ("succeeded", "failed", "cancelled")
HASH = re.compile(r"^[0-9a-f]{64}$")
# A queued record is canonical JSON with sorted keys, so its outstanding bytes end it.
RECORD_TAIL = re.compile(rb'"remaining":(\d{1,20}),"v":1\}$')
IDENTIFIER = re.compile(r"^[A-Za-z0-9][A-Za-z0-9_.-]{0,127}$")
RUNTIME_NAMES = {"rust", "node", "python", "cuda"}
DENY_DIRS = {".git", "target", "node_modules", "dist", "build", "__pycache__",
             ".ssh", ".aws", ".azure", ".kube", ".gnupg", ".docker", "secrets", "credentials"}
DENY_SUFFIX = {".pem", ".key", ".p12", ".pfx", ".jks", ".keystore", ".kdbx", ".ppk", ".ovpn", ".tfvars"}
DENY_NAMES = re.compile(r"^(?:\.env(?:\..*)?|id_(?:rsa|dsa|ecdsa|ed25519)(?:_sk)?(?:\.pub)?|\.netrc|\.npmrc|\.pypirc|\.git-credentials|credentials(?:\..*)?|authorized_keys|known_hosts|nvsync\..*)$", re.I)
SECRET = re.compile(rb"-----BEGIN (?:[A-Z0-9]+ )*PRIVATE KEY(?: BLOCK)?-----|PuTTY-User-Key-File-\d|(?:AKIA|ASIA)[A-Z0-9]{16}|gh[pousr]_[A-Za-z0-9]{30,}|github_pat_[A-Za-z0-9_]{30,}|hf_[A-Za-z]{34}")
# Network link report. A leading letter or digit keeps a name from reading as an iw/nmcli option.
IFNAME = re.compile(r"^[A-Za-z0-9][A-Za-z0-9_.-]{0,14}$")
# systemd's MAC naming policy (Ubuntu's 73-usb-net-by-mac.rules for USB adapters) embeds the MAC.
MAC_NAME = re.compile(r"^(en|wl|ww|ib|sl)x[0-9a-f]{12}$", re.I)
MAC_ALIASES = {"en": "usb-ethernet", "wl": "usb-wifi", "ww": "usb-wwan", "ib": "usb-ib", "sl": "usb-slip"}
DRIVER = re.compile(r"^[a-z0-9_-]{1,32}$")
IW_PATHS = ("/usr/sbin/iw", "/sbin/iw")
NMCLI_PATHS = ("/usr/bin/nmcli",)
TOOL_ENV = {"LC_ALL": "C", "PATH": "/usr/sbin:/usr/bin:/sbin:/bin"}
SIOCGIFADDR = 0x8915
MAX_IFACES = 1024
MAX_WIRED = 16
PROBE_DEADLINE = 2.5
PROBE_LIMIT = PROBE_DEADLINE + 0.5  # Hard bound on the whole probe, sysfs reads included.
REAP_WAIT = 0.2
SYS_READ = 4096
PROC_READ = CHUNK
TCP_READ = 8 * CHUNK
TOOL_READ = 64 * 1024
RTF_UP, RTF_GATEWAY, RTF_REJECT = 0x0001, 0x0002, 0x0200
LINK_KINDS = ("wired", "wifi", "virtual", "unknown")
OPERSTATES = ("up", "down", "dormant", "lowerlayerdown", "notpresent", "testing", "unknown")
WIFI_BANDS = ("2.4", "5", "6", "multi")
WIFI_WIDTHS = (20, 40, 80, 160, 320)
WIFI_SOURCES = ("iw", "nmcli", "proc")
# Line-anchored: iw escapes control characters in SSIDs, so an SSID cannot start a line.
IW_FREQ = re.compile(r"^[ \t]*freq:[ \t]*(\d{4})(?:\.\d+)?[ \t]*$", re.M)
IW_SIGNAL = re.compile(r"^[ \t]*signal:[ \t]*(-\d{1,3})(?:\.\d+)?[ \t]*(?:\[[-\d, ]*\][ \t]*)?dBm", re.M)
IW_RATE = re.compile(r"^[ \t]*(tx|rx) bitrate:[ \t]*(\d{1,5}(?:\.\d+)?)[ \t]*MBit/s", re.M)
IW_CHANNEL = re.compile(r"^[ \t]*channel[ \t]+(\d{1,3})[ \t]+\((\d{4})(?:\.\d+)?[ \t]*MHz\),[ \t]*width:[ \t]*(\d{2,3})[ \t]*MHz", re.M)
NMCLI_ARGS = ("-t", "-e", "yes", "-f", "ACTIVE,FREQ,CHAN,BANDWIDTH", "device", "wifi", "list", "ifname")
# nmcli prints fields in the requested order (FREQ before CHAN); the other order is also accepted.
NMCLI_FREQ_FIRST = re.compile(r"^yes:(\d{4}) MHz:(\d{1,3}):(\d{2,3}) MHz$", re.M)
NMCLI_CHAN_FIRST = re.compile(r"^yes:(\d{1,3}):(\d{4}) MHz:(\d{2,3}) MHz$", re.M)


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
    fd = os.open(path, flags | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_NONBLOCK", 0) | getattr(os, "O_BINARY", 0), mode)
    if not stat.S_ISREG(os.fstat(fd).st_mode):
        os.close(fd)
        reject("unsafe_state", "State file is not regular")
    return fd


def regular_size(path):
    """Size of a regular file found by lstat, or None; never follows a link or reads content."""
    try:
        info = os.lstat(path)
    except FileNotFoundError:
        return None
    return info.st_size if stat.S_ISREG(info.st_mode) else None


def remove_tree(path):
    """Remove a directory this controller generated below its own state root."""
    path = Path(path)
    if not os.path.lexists(path):
        return
    if path.is_symlink() or not path.is_dir():
        os.unlink(path)
        return

    def onerror(func, name, exc):
        # Snapshot files are read-only; directory links are never followed by rmtree.
        os.chmod(name, 0o700)
        func(name)
    shutil.rmtree(path, onerror=onerror)


def unlink_quietly(path):
    with contextlib.suppress(FileNotFoundError):
        os.unlink(path)


def gib(value):
    return f"{value / GIB:.1f}"


def sync_limits():
    return {"chunk": CHUNK, "max_file": MAX_FILE, "max_tree": MAX_TREE, "max_small_tree": MAX_SMALL_TREE,
            "large_file": LARGE_FILE, "max_files": MAX_FILES, "max_request": MAX_REQUEST,
            "put_many_body": PUT_MANY_BODY, "missing_batch": MISSING_BATCH}


def disk_space(path):
    """(free, total) bytes of the state filesystem. An unreadable filesystem reads as full."""
    try:
        if hasattr(os, "statvfs"):
            info = os.statvfs(path)
            return info.f_bavail * info.f_frsize, info.f_blocks * info.f_frsize
        usage = shutil.disk_usage(path)
        return usage.free, usage.total
    except OSError:
        return 0, 0


def disk_refusal(free, needed):
    reject("insufficient_disk", f"Spark has {gib(free)} GiB free; this needs {gib(needed)} GiB and DGXBridge keeps "
                                f"{DISK_RESERVE // GIB} GiB in reserve. Free Spark disk space, then retry")


def validate_manifest(manifest):
    """Validate one sync manifest; return (sorted entries, canonical bytes, manifest ID, total bytes)."""
    if not isinstance(manifest, list) or len(manifest) > MAX_FILES:
        reject("size_limit", "Manifest is too large")
    total, small, paths, folded = 0, 0, set(), set()
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
        if size <= LARGE_FILE:
            small += size
    if total > MAX_TREE:
        reject("size_limit", f"Project exceeds {MAX_TREE // GIB} GiB")
    if small > MAX_SMALL_TREE:
        reject("size_limit", f"Files of {LARGE_FILE // CHUNK} MiB or less exceed {MAX_SMALL_TREE // GIB} GiB in total")
    for name in paths:
        if any(str(parent) in paths for parent in PurePosixPath(name).parents if str(parent) != "."):
            reject("unsafe_path", "A file conflicts with a parent directory")
    manifest = sorted(manifest, key=lambda e: e["path"])
    encoded = canonical(manifest)
    return manifest, encoded, sha(encoded), total


def store_name(entry):
    return f"{entry['sha256']}-{entry['mode']:o}"


def read_only_mode(entry):
    return 0o555 if entry["mode"] == 0o755 else 0o444


def spawn_materializer(root):
    """Start one detached background materializer; it outlives this RPC and the SSH session.

    A new session is not a new cgroup: the child stays in the unit of whatever process
    spawned it, so syncs leave spawning to the scheduler while it is alive (see wake()).
    """
    subprocess.Popen([sys.executable, "-I", str(Path(__file__).resolve()), "materialize", "--state", str(root)],
                     stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                     start_new_session=True, close_fds=True, env=dict(TOOL_ENV), cwd=str(root))


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


def bounded_text(path, limit=SYS_READ):
    # sysfs reports EINVAL for carrier/speed on a link that is down; that is unknown, not an error.
    try:
        with open(path, "rb") as stream:
            return stream.read(limit).decode("utf-8", "replace")
    except (OSError, ValueError):
        return None


def uevent_values(path):
    values = {}
    for line in (bounded_text(path) or "").splitlines():
        key, separator, value = line.partition("=")
        if separator:
            values[key] = value
    return values


def sys_positive(path):
    try:
        value = int((bounded_text(path) or "").strip())
    except ValueError:
        return None
    return value if value > 0 else None


def sys_carrier(path):
    return {"1": True, "0": False}.get((bounded_text(path) or "").strip())


def sys_operstate(path):
    value = (bounded_text(path) or "").strip()
    return value if value in OPERSTATES else None


def link_kind(entry):
    if not os.path.lexists(entry / "device"):
        return "virtual"
    if os.path.lexists(entry / "phy80211") or uevent_values(entry / "uevent").get("DEVTYPE") == "wlan":
        return "wifi"
    return "wired" if (bounded_text(entry / "type") or "").strip() in ("1", "32") else "unknown"


def native_ipv4(text):
    # /proc/net/route and /proc/net/tcp print a network-order address as a host-order word.
    raw = bytes.fromhex(text)
    if len(raw) != 4:
        raise ValueError("IPv4 field")
    return ipaddress.IPv4Address(raw[::-1] if sys.byteorder == "little" else raw)


def native_ipv6(text):
    # /proc/net/tcp6 prints four host-order words.
    raw = bytes.fromhex(text)
    if len(raw) != 16:
        raise ValueError("IPv6 field")
    if sys.byteorder == "little":
        raw = b"".join(raw[i:i + 4][::-1] for i in range(0, 16, 4))
    return ipaddress.IPv6Address(raw)


def network_ipv6(text):
    # /proc/net/ipv6_route and /proc/net/if_inet6 print network byte order.
    raw = bytes.fromhex(text)
    if len(raw) != 16:
        raise ValueError("IPv6 field")
    return ipaddress.IPv6Address(raw)


def route_table(proc_root):
    """Usable main-table routes as (interface, network, metric, flags)."""
    routes = []
    for line in (bounded_text(proc_root / "net" / "route", PROC_READ) or "").splitlines()[1:]:
        parts = line.split()
        try:
            network = ipaddress.IPv4Network((native_ipv4(parts[1]), str(native_ipv4(parts[7]))), strict=False)
            routes.append((parts[0], network, int(parts[6]), int(parts[3], 16)))
        except (IndexError, ValueError):
            continue
    for line in (bounded_text(proc_root / "net" / "ipv6_route", PROC_READ) or "").splitlines():
        parts = line.split()
        try:
            network = ipaddress.IPv6Network((network_ipv6(parts[0]), int(parts[1], 16)), strict=False)
            routes.append((parts[9], network, int(parts[5], 16), int(parts[8], 16)))
        except (IndexError, ValueError):
            continue
    return [r for r in routes if IFNAME.fullmatch(r[0]) and r[0] != "lo" and r[3] & RTF_UP and not r[3] & RTF_REJECT]


def lowest_default(routes, allowed):
    # IPv4 first: an IPv6 router advertisement must not outrank the LAN default.
    for version in (4, 6):
        candidates = [(metric, name) for name, network, metric, _ in routes
                      if network.version == version and not network.prefixlen and allowed(name)]
        if candidates:
            return min(candidates)[1]
    return None


def route_lookup(routes, address, connected=False):
    best = None
    for name, network, metric, flags in routes:
        if network.version != address.version or address not in network:
            continue
        if connected and (flags & RTF_GATEWAY or not network.prefixlen):
            continue
        key = (-network.prefixlen, metric, name)
        best = key if best is None or key < best else best
    return best[2] if best else None


def ssh_address(token):
    address, _, zone = token.partition("%")
    if zone and not IFNAME.fullmatch(zone):
        raise ValueError("zone")
    ip = ipaddress.ip_address(address)
    if ip.version == 6 and ip.ipv4_mapped:
        return ip.ipv4_mapped, None
    if zone and ip.version != 6:
        raise ValueError("zone")
    return ip, zone or None


def ssh_session(environ):
    """Parse SSH_CONNECTION; these addresses stay local to the probe and never reach output."""
    value = environ.get("SSH_CONNECTION")
    if not isinstance(value, str) or len(value) > 256:
        return None
    tokens = value.split()
    if len(tokens) != 4 or not all(re.fullmatch(r"[0-9]{1,5}", tokens[i]) for i in (1, 3)):
        return None
    try:
        client, _ = ssh_address(tokens[0])
        server, zone = ssh_address(tokens[2])
    except ValueError:
        return None
    client_port, server_port = int(tokens[1]), int(tokens[3])
    if client.version != server.version or not (0 < client_port < 65536 and 0 < server_port < 65536):
        return None
    return client, client_port, server, server_port, zone


def tcp_endpoint(text, decode):
    address, _, port = text.partition(":")
    ip = decode(address)
    if ip.version == 6 and ip.ipv4_mapped:
        ip = ip.ipv4_mapped
    return ip, int(port, 16)


def tcp_established(proc_root, client, client_port, server, server_port):
    # An inherited SSH_CONNECTION (for example under sparkdev-link) is stale unless
    # its exact 4-tuple is a live ESTABLISHED socket.
    local, remote, suffix = (server, server_port), (client, client_port), ":%04X" % server_port
    for name, decode in (("tcp", native_ipv4), ("tcp6", native_ipv6)):
        for line in (bounded_text(proc_root / "net" / name, TCP_READ) or "").splitlines()[1:]:
            parts = line.split()
            if len(parts) < 4 or parts[3] != "01" or not parts[1].upper().endswith(suffix):
                continue
            try:
                if tcp_endpoint(parts[1], decode) == local and tcp_endpoint(parts[2], decode) == remote:
                    return True
            except ValueError:
                continue
    return False


def interface_ipv4(name):
    """Primary IPv4 address of one interface via SIOCGIFADDR on an unbound socket."""
    import fcntl
    import socket
    import struct
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as sock:
        try:
            reply = fcntl.ioctl(sock.fileno(), SIOCGIFADDR, struct.pack("256s", name.encode("ascii")))
        except OSError:
            return None
    return ipaddress.IPv4Address(reply[20:24])


def interface_netmask(name):
    """Primary IPv4 netmask via SIOCGIFNETMASK; no packet is sent."""
    import fcntl
    import socket
    import struct
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as sock:
        try:
            reply = fcntl.ioctl(sock.fileno(), 0x891b, struct.pack("256s", name.encode("ascii")))
        except OSError:
            return None
    return str(ipaddress.IPv4Address(reply[20:24]))


def lan_addresses(sys_root=Path("/sys"), ipv4_addr=None, netmask=None, os_name=None):
    """Private dial candidates, returned only by authenticated lan_routes, never status."""
    if (os_name or os.name) != "posix":
        return []
    ipv4_addr, netmask = ipv4_addr or interface_ipv4, netmask or interface_netmask
    base = Path(sys_root) / "class" / "net"
    with os.scandir(base) as entries:
        names = sorted(e.name for e in itertools.islice(entries, MAX_IFACES) if IFNAME.fullmatch(e.name))
    kinds = {n: link_kind(base / n) for n in names}
    private = tuple(ipaddress.IPv4Network(n) for n in ("10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16"))
    rows = []
    for name in names:
        kind, member = kinds[name], name
        if kind == "virtual":
            member = carrier_member(base, name, kinds)
            kind = kinds.get(member)
        if kind not in ("wired", "wifi"):
            continue
        if kind == "wired" and sys_carrier(base / member / "carrier") is not True:
            continue
        if kind == "wifi" and sys_operstate(base / member / "operstate") != "up":
            continue
        try:
            address = ipaddress.IPv4Address(str(ipv4_addr(name)))
            mask = netmask(name)
            if mask is None:
                continue
            network = ipaddress.IPv4Network((str(address), str(mask)), strict=False)
        except (ValueError, OSError):
            continue
        if not any(address in n for n in private) or not 8 <= network.prefixlen <= 30:
            continue
        speed = sys_positive(base / member / "speed")
        rows.append({"kind": kind, "ipv4": str(address), "prefix": network.prefixlen,
                     "speed_mbps": speed if speed and speed <= 1000000 else None})
    rows.sort(key=lambda r: (r["kind"] != "wired", -(r["speed_mbps"] or 0), r["ipv4"], r["prefix"]))
    result, seen = [], set()
    for row in rows:
        if row["ipv4"] not in seen:
            seen.add(row["ipv4"])
            result.append(row)
    return result[:8]


def lan_routes_view(root, probe=None):
    """Salted change signal and private addresses, with no audit or logging."""
    routes = (probe or lan_addresses)()
    result = {"v": 1, "routes": routes, "shared_network": any(
        a["kind"] != b["kind"] and
        ipaddress.IPv4Network((a["ipv4"], a["prefix"]), strict=False) ==
        ipaddress.IPv4Network((b["ipv4"], b["prefix"]), strict=False)
        for a in routes for b in routes)}
    salt_path = Path(root) / "lan-epoch.salt"
    try:
        if not os.path.lexists(salt_path):
            fd, name = tempfile.mkstemp(prefix=".lan-salt-", dir=root)
            try:
                with os.fdopen(fd, "wb") as stream:
                    os.chmod(name, 0o600)
                    stream.write(os.urandom(16))
                    stream.flush()
                    os.fsync(stream.fileno())
                try:
                    os.link(name, salt_path)  # Atomic publication without replacing another RPC's salt.
                except FileExistsError:
                    pass
            finally:
                os.unlink(name)
        with os.fdopen(safe_open(salt_path), "rb") as stream:
            info = os.fstat(stream.fileno())
            if info.st_size != 16 or info.st_nlink != 1 or (os.name == "posix" and
                    (info.st_uid != os.getuid() or stat.S_IMODE(info.st_mode) != 0o600)):
                return result
            salt = stream.read(17)
        if len(salt) == 16:
            signal = sorted([r["kind"], r["ipv4"], r["prefix"]] for r in routes)
            result["epoch"] = sha(salt + canonical(signal))[:16]
    except (OSError, ValueError, Rejected):
        pass
    return result


def address_owner(server, zone, names, proc_root, routes, ipv4_addr):
    if zone:
        return zone if zone in names else None
    if server.version == 6:
        for line in (bounded_text(proc_root / "net" / "if_inet6", PROC_READ) or "").splitlines():
            parts = line.split()
            try:
                if len(parts) >= 6 and parts[5] in names and network_ipv6(parts[0]) == server:
                    return parts[5]
            except ValueError:
                continue
        return None
    for name in names:
        try:
            found = ipv4_addr(name)
            if found is not None and ipaddress.ip_address(str(found)) == server:
                return name
        except (OSError, ValueError):
            continue
    # SIOCGIFADDR reports only the primary address; a secondary one falls back to its connected route.
    return route_lookup(routes, server, connected=True)


def carrier_member(base, name, kinds):
    """First physical lower_* member with carrier, through at most three stacked layers."""
    frontier, seen = [name], {name}
    for _ in range(3):
        physical, virtual = [], []
        for current in frontier:
            try:
                members = sorted(entry[6:] for entry in os.listdir(base / current) if entry.startswith("lower_"))
            except OSError:
                continue
            for member in members[:MAX_IFACES]:
                if member in kinds and member not in seen:
                    seen.add(member)
                    (virtual if kinds[member] == "virtual" else physical).append(member)
        for member in physical:
            if sys_carrier(base / member / "carrier"):
                return member
        frontier = virtual
    return None


def upper_masters(base, name):
    """Bridge, bond and VLAN masters above one port via its upper_* links, through at most three layers."""
    frontier, seen = [name], {name}
    for _ in range(3):
        found = []
        for current in frontier:
            try:
                uppers = sorted(entry[6:] for entry in os.listdir(base / current) if entry.startswith("upper_"))
            except OSError:
                continue
            for upper in uppers[:MAX_IFACES]:
                if upper not in seen:
                    seen.add(upper)
                    found.append(upper)
        frontier = found
    return seen - {name}


def frequency_band(frequency):
    if frequency == 2484:
        return "2.4", 14
    if frequency == 5935:
        return "6", 2
    if 2412 <= frequency <= 2472:
        band, base = "2.4", 2407
    elif 5150 <= frequency <= 5895:
        band, base = "5", 5000
    elif 5925 <= frequency <= 7125:
        band, base = "6", 5950
    else:
        return None, None
    channel, remainder = divmod(frequency - base, 5)
    return band, channel if not remainder and channel >= 1 else None


def wireless_level(proc_root, name):
    for line in (bounded_text(proc_root / "net" / "wireless", PROC_READ) or "").splitlines()[2:]:
        label, separator, rest = line.partition(":")
        if separator and label.strip() == name:
            try:
                level = float(rest.split()[2])
            except (IndexError, ValueError):
                return None
            return int(level) if math.isfinite(level) else None
    return None


def run_tool(argv, timeout):
    """Run one fixed-path probe tool: argv only, fixed environment, no stdin or stderr.

    A tool wedged in the kernel (a driver holding rtnl or the wiphy mutex) can ignore
    SIGKILL, so after its timeout it is killed, given REAP_WAIT to exit, then abandoned.
    """
    process = subprocess.Popen(argv, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, env=TOOL_ENV)
    try:
        stdout, _ = process.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        process.kill()
        with contextlib.suppress(subprocess.TimeoutExpired):
            process.wait(timeout=REAP_WAIT)
        raise
    return stdout[:TOOL_READ] if process.returncode == 0 else None


def bounded_call(function, limit):
    """function() on a daemon thread; None unless it returns within `limit` seconds.

    A sysfs carrier or speed read can block while a driver holds rtnl, and nothing else
    bounds it; the abandoned thread dies with this short-lived RPC process.
    """
    box = []

    def target():
        with contextlib.suppress(Exception):
            box.append(function())
    thread = threading.Thread(target=target, name="sparkdev-link-probe", daemon=True)
    thread.start()
    thread.join(limit)
    return box[0] if box else None


def wifi_detail(name, run, proc_root, deadline):
    """Radio facts for one Wi-Fi interface. Raw tool text is parsed here and discarded, never logged."""
    detail = {"interface": name, "band": None, "channel": None, "frequency_mhz": None, "width_mhz": None,
              "tx_bitrate_mbps": None, "rx_bitrate_mbps": None, "signal_dbm": None, "links": None, "source": None}

    def tool(paths, args):
        for path in paths:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                return ""
            try:
                output = run([path, *args], min(1.0, remaining))
            except OSError:
                continue  # Absent or not executable at this fixed path.
            except (subprocess.SubprocessError, ValueError):
                return ""
            return output[:TOOL_READ].decode("utf-8", "replace") if isinstance(output, bytes) else ""
        return ""

    def tune(channels):
        if len(channels) >= 2:
            # Multi-link operation: never claim one band unless it is the only link.
            detail.update(band="multi", links=len(channels))
        elif channels:
            channel, frequency, width = channels[0]
            band, computed = frequency_band(frequency)
            detail.update(band=band, channel=channel or computed, frequency_mhz=frequency, width_mhz=width, links=1)

    link = tool(IW_PATHS, ["dev", name, "link"])
    info = tool(IW_PATHS, ["dev", name, "info"])
    channels = [(int(c), int(f), int(w)) for c, f, w in IW_CHANNEL.findall(info)]
    frequencies = [int(f) for f in IW_FREQ.findall(link)]
    if len(frequencies) > max(1, len(channels)):
        channels = [(None, f, None) for f in frequencies]
    elif not channels and frequencies:
        channels = [(None, frequencies[0], None)]
    tune(channels)
    rates = {}
    for direction, value in IW_RATE.findall(link):
        rates.setdefault(direction, round(float(value), 1))
    signal = IW_SIGNAL.search(link)
    detail.update(tx_bitrate_mbps=rates.get("tx"), rx_bitrate_mbps=rates.get("rx"),
                  signal_dbm=int(signal.group(1)) if signal else None)
    if any(detail[key] is not None for key in ("band", "tx_bitrate_mbps", "rx_bitrate_mbps", "signal_dbm")):
        detail["source"] = "iw"
    if detail["band"] is None:
        # Requests only these four fields with no rescan, so SSID and BSSID are never read.
        text = tool(NMCLI_PATHS, [*NMCLI_ARGS, name, "--rescan", "no"])
        active = [(int(c), int(f), int(w)) for f, c, w in NMCLI_FREQ_FIRST.findall(text)]
        active += [(int(c), int(f), int(w)) for c, f, w in NMCLI_CHAN_FIRST.findall(text)]
        tune(active)
        if active:
            detail["source"] = detail["source"] or "nmcli"
    if detail["signal_dbm"] is None:
        detail["signal_dbm"] = wireless_level(proc_root, name)
        if detail["signal_dbm"] is not None:
            detail["source"] = detail["source"] or "proc"
    return detail


def network_link(sys_root=Path("/sys"), proc_root=Path("/proc"), environ=os.environ, run=None, ipv4_addr=None, os_name=None):
    """Observe which link carries this controller's traffic. The raw result is only for network_view."""
    if (os_name or os.name) != "posix":
        return {"observed": False}
    deadline = time.monotonic() + PROBE_DEADLINE
    run, ipv4_addr = run or run_tool, ipv4_addr or interface_ipv4
    base, proc_root = Path(sys_root) / "class" / "net", Path(proc_root)
    try:
        with os.scandir(base) as entries:
            listed = [entry.name for entry in itertools.islice(entries, MAX_IFACES)]
    except OSError:
        return {"observed": False}
    names = sorted(name for name in listed if IFNAME.fullmatch(name))
    # Classify every entry before any cap: Docker veth*/br-* names sort before wl*.
    kinds = {name: link_kind(base / name) for name in names}
    routes = route_table(proc_root)
    defaults = {name for name, network, _, _ in routes if not network.prefixlen}
    basis, active, egress = "unknown", None, None
    session = ssh_session(environ)
    if session and tcp_established(proc_root, *session[:4]):
        client, _, server, _, zone = session
        active = address_owner(server, zone, names, proc_root, routes, ipv4_addr)
        if active:
            basis = "ssh_session"
            # A tunnel's policy routing is invisible in the main table, and a link-local
            # zone already fixes the egress; only a physical owner gets an egress check.
            if not zone and kinds.get(active) != "virtual":
                route = route_lookup(routes, client)
                egress = route if route and route != active else None
    if active is None:
        active = lowest_default(routes, lambda name: name in kinds)
        basis = "default_route" if active else "unknown"
    underlay = None
    if active and kinds.get(active) == "virtual":
        member = carrier_member(base, active, kinds) or lowest_default(routes, lambda name: kinds.get(name, "virtual") != "virtual")
        if member:
            underlay = {"interface": member, "kind": kinds[member]}

    def port(name):
        # Never read `address`: the report carries no MAC.
        return {"operstate": sys_operstate(base / name / "operstate"), "speed_mbps": sys_positive(base / name / "speed")}

    active_view = None
    if active:
        active_view = {"interface": active, "kind": kinds.get(active, "unknown"), **port(active)}
        if egress:
            active_view["egress_interface"] = egress
    # A port is LAN-routed when it, or a bridge/bond/VLAN master stacked on it, holds a default route.
    wired = [{"interface": name, "carrier": sys_carrier(base / name / "carrier"), **port(name),
              "driver": uevent_values(base / name / "device" / "uevent").get("DRIVER"),
              "default_route": name in defaults or bool(upper_masters(base, name) & defaults)}
             for name in names if kinds[name] == "wired"]
    wired.sort(key=lambda p: (p["carrier"] is not True, p["interface"]))
    radio = next((n for n in (active, underlay and underlay["interface"]) if n and kinds.get(n) == "wifi"), None)
    return {"observed": True, "basis": basis, "active": active_view, "underlay": underlay, "wired": wired[:MAX_WIRED],
            "wired_up": sum(p["carrier"] is True for p in wired),
            "wired_lan_up": sum(p["carrier"] is True and p["default_route"] for p in wired),
            "wifi": wifi_detail(radio, run, proc_root, deadline) if radio else None}


def network_view(raw):
    """The only path from probe data to RPC output: rebuild from whitelisted, bounded fields.

    Additive fields keep v:1; only a breaking change bumps v.
    """
    epoch = raw.get("lan_epoch") if isinstance(raw, dict) else None
    epoch_field = {"lan_epoch": epoch} if isinstance(epoch, str) and re.fullmatch(r"[0-9a-f]{16}", epoch) else {}
    if not isinstance(raw, dict) or raw.get("observed") is not True:
        return {"v": 1, "observed": False, "basis": "unknown", **epoch_field}

    def mapping(value):
        return value if isinstance(value, dict) else {}

    ports = raw.get("wired")
    ports = ports[:MAX_IFACES] if isinstance(ports, list) else []
    active_raw, radio = mapping(raw.get("active")), mapping(raw.get("wifi"))
    # A MAC-derived name (enx<MAC>, wlx<MAC>) becomes a kind-and-index alias, numbered in
    # sorted name order and identical in every field; probe argv still uses the real name.
    names = [active_raw.get("interface"), active_raw.get("egress_interface"), mapping(raw.get("underlay")).get("interface"),
             radio.get("interface"), *(mapping(item).get("interface") for item in ports)]
    aliases, counts = {}, {}
    for value in sorted({v for v in names if isinstance(v, str) and IFNAME.fullmatch(v) and MAC_NAME.fullmatch(v)}):
        prefix = MAC_NAME.fullmatch(value).group(1).lower()
        counts[prefix] = counts.get(prefix, 0) + 1
        aliases[value] = f"{MAC_ALIASES[prefix]}-{counts[prefix]}"

    def name(value):
        if not isinstance(value, str) or not IFNAME.fullmatch(value):
            return None
        return aliases.get(value) if MAC_NAME.fullmatch(value) else value

    def bounded(value, low, high, real=False):
        if type(value) is int or (real and type(value) is float and math.isfinite(value)):
            return value if low <= value <= high else None
        return None

    def choice(value, options):
        return value if isinstance(value, str) and value in options else None

    def link(value, detailed):
        value = mapping(value)
        interface = name(value.get("interface"))
        if interface is None:
            return None
        view = {"interface": interface, "kind": choice(value.get("kind"), LINK_KINDS) or "unknown"}
        if detailed:
            view.update(operstate=choice(value.get("operstate"), OPERSTATES), speed_mbps=bounded(value.get("speed_mbps"), 1, 1000000))
            egress = name(value.get("egress_interface"))
            if egress and egress != interface:
                view["egress_interface"] = egress
        return view

    wired = []
    for item in ports:
        item = mapping(item)
        interface = name(item.get("interface"))
        if interface is None:
            continue
        driver = item.get("driver")
        wired.append({"interface": interface, "carrier": item.get("carrier") if type(item.get("carrier")) is bool else None,
                      "operstate": choice(item.get("operstate"), OPERSTATES), "speed_mbps": bounded(item.get("speed_mbps"), 1, 1000000),
                      "driver": driver if isinstance(driver, str) and DRIVER.fullmatch(driver) else None,
                      "default_route": item.get("default_route") is True})
        if len(wired) == MAX_WIRED:
            break
    wired_up = bounded(raw.get("wired_up"), 0, MAX_IFACES)
    wired_lan_up = bounded(raw.get("wired_lan_up"), 0, MAX_IFACES)
    wifi = None
    if name(radio.get("interface")):
        wifi = {"interface": name(radio.get("interface")), "band": choice(radio.get("band"), WIFI_BANDS),
                "channel": bounded(radio.get("channel"), 1, 233), "frequency_mhz": bounded(radio.get("frequency_mhz"), 2400, 7200),
                "width_mhz": radio.get("width_mhz") if type(radio.get("width_mhz")) is int and radio.get("width_mhz") in WIFI_WIDTHS else None,
                "tx_bitrate_mbps": bounded(radio.get("tx_bitrate_mbps"), 0, 50000, real=True),
                "rx_bitrate_mbps": bounded(radio.get("rx_bitrate_mbps"), 0, 50000, real=True),
                "signal_dbm": bounded(radio.get("signal_dbm"), -120, -1), "links": bounded(radio.get("links"), 1, 3),
                "source": choice(radio.get("source"), WIFI_SOURCES)}
    active = link(raw.get("active"), True)
    basis = choice(raw.get("basis"), ("ssh_session", "default_route")) if active else None
    return {"v": 1, "observed": True, "basis": basis or "unknown", "active": active, **epoch_field,
            "underlay": link(raw.get("underlay"), False), "wired": wired,
            "wired_up": sum(p["carrier"] is True for p in wired) if wired_up is None else wired_up,
            "wired_lan_up": sum(p["carrier"] is True and p["default_route"] for p in wired) if wired_lan_up is None else wired_lan_up,
            "wifi": wifi}


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


def reserved_cpu(row):
    return row["cpu"] + (PROXY_CPU if is_preparation(row) else 0)


def reserved_memory(row):
    return row["memory"] + (PROXY_MEMORY_GB if is_preparation(row) else 0)


def is_preparation(row):
    return json.loads(row["request"])["op"] == "prepare"


INTERNET_COUNTERS = ("connections", "http_requests", "bytes_down", "bytes_up", "blocked_private", "refused_invalid", "failed_connect")
INTERNET_IPV6_SYSCTLS = {"net.ipv6.conf.all.disable_ipv6": "1", "net.ipv6.conf.default.disable_ipv6": "1"}


def internet_deny(proc_root=Path("/proc")):
    """Snapshot connected prefixes and all local IPv4 addresses before binding the file."""
    # On the real Linux runner both files must be readable: missing host facts fail closed.
    routes_text = bounded_text(proc_root / "net" / "route", PROC_READ + 1)
    fib = bounded_text(proc_root / "net" / "fib_trie", PROC_READ + 1)
    if routes_text is None or fib is None or len(routes_text.encode()) > PROC_READ or len(fib.encode()) > PROC_READ:
        reject("network_facts", "Spark network addresses could not be checked")
    values = {str(network) for _, network, _, flags in route_table(proc_root)
              if network.version == 4 and network.prefixlen > 0 and not flags & RTF_GATEWAY}
    previous = None
    for line in fib.splitlines():
        match = re.search(r"(?:\+--|\|--)\s+(\d+\.\d+\.\d+\.\d+)(?:\s|$)", line)
        if match:
            try:
                previous = ipaddress.IPv4Address(match.group(1))
            except ValueError:
                previous = None
        elif re.search(r"/32\s+host\s+LOCAL\s*$", line) and previous is not None:
            values.add(str(previous) + "/32")
            previous = None
    if len(values) > 256:
        reject("network_facts", "Spark has too many network addresses to check")
    return sorted(values)


def guard_bridge_ipv6(network, proc_root=Path("/proc")):
    root = proc_root / "sys" / "net" / "ipv6"
    if not root.exists():
        return
    bridge = (network.get("Options") or {}).get("com.docker.network.bridge.name")
    if not bridge:
        identity = network.get("Id", "")
        bridge = "br-" + identity[:12] if re.fullmatch(r"[0-9a-f]{64}", identity) else ""
    if not IFNAME.fullmatch(bridge):
        reject("ipv6_link", "The private job network could not be checked for IPv6 isolation")
    conf = root / "conf" / bridge
    if (bounded_text(conf / "disable_ipv6") or "").strip() != "1" and (bounded_text(conf / "addr_gen_mode") or "").strip() != "1":
        reject("ipv6_link", "The private job network does not disable IPv6 link-local access")


class Docker:
    @staticmethod
    def invoke(args, timeout):
        try:
            return subprocess.run(["docker", *args], capture_output=True, timeout=timeout, check=False)
        except (OSError, subprocess.SubprocessError):
            reject("docker_unavailable", "Docker command could not complete")

    def call(self, args, timeout=30):
        result = self.invoke(args, timeout)
        if result.returncode:
            reject("docker_failed", "Docker operation failed; owner may inspect local service logs")
        return result.stdout

    def inspect(self, name):
        result = self.invoke(["container", "inspect", name], 10)
        if result.returncode:
            # Only a definite not-found permits a new launch. Daemon failures must never
            # be interpreted as absence, or restart reconciliation could duplicate work.
            if b"No such container" in result.stderr or b"No such object" in result.stderr:
                return None
            reject("docker_unavailable", "Docker state is unavailable")
        try:
            return json.loads(result.stdout)[0]
        except (ValueError, IndexError, KeyError, TypeError):
            reject("docker_unavailable", "Docker state is invalid")

    def network_inspect(self, name):
        result = self.invoke(["network", "inspect", name], 10)
        if result.returncode:
            if b"not found" in result.stderr or b"No such network" in result.stderr:
                return None
            reject("docker_unavailable", "Docker network state is unavailable")
        try:
            return json.loads(result.stdout)[0]
        except (ValueError, IndexError, KeyError, TypeError):
            reject("docker_unavailable", "Docker network state is invalid")


class Progress:
    """Snapshot assembly progress, written at most once per second for sync and status."""

    def __init__(self, controller, manifest_id, total):
        self.controller, self.path = controller, controller.pending / (manifest_id + ".progress")
        self.total, self.done, self.written, self.last = total, 0, 0, None

    def add(self, size, written=False):
        self.done += size
        if written:
            self.written += size
        self.write()

    def write(self, force=False):
        now = self.controller.clock()
        if not force and self.last is not None and now - self.last < PROGRESS_INTERVAL:
            return
        self.last = now
        atomic_write(self.path, canonical({"v": 1, "done_bytes": min(self.done, self.total),
                                           "written_bytes": min(self.written, self.total), "total_bytes": self.total}))


class Controller:
    def __init__(self, root=None, docker=None, memory_probe=None, hardware_probe=None, link_probe=None, *,
                 disk_probe=None, spawn=None, clock=None, sleep=None, lan_probe=None):
        self.root = Path(root or os.environ.get("SPARKDEV_STATE", Path.home() / "sparkdev/state")).absolute()
        self.root.mkdir(parents=True, exist_ok=True, mode=0o700)
        if self.root.is_symlink():
            reject("unsafe_state", "State root must not be a symlink")
        if os.name == "posix":
            if self.root.stat().st_uid != os.getuid():
                reject("unsafe_state", "State root has a different owner")
            os.chmod(self.root, 0o700)
        for name in ("objects", "snapshots", "jobs", "caches", "local", "pending", "files"):
            path = self.root / name
            if path.is_symlink():
                reject("unsafe_state", "State directory is a symlink")
            path.mkdir(exist_ok=True, mode=0o700)
            if os.name == "posix" and path.lstat().st_uid != os.getuid():
                reject("unsafe_state", "State directory has a different owner")
        self.pending, self.files, self.snapshots = self.root / "pending", self.root / "files", self.root / "snapshots"
        self.disk_probe = disk_probe or (lambda: disk_space(self.root))
        self.spawn = spawn or (lambda: spawn_materializer(self.root))
        self.clock = clock or time.monotonic
        self.sleep = sleep or time.sleep
        self.started = self.clock()
        self._shapes = {}
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
        for name in ("egress", "internet_use"):
            if name not in columns:
                self.db.execute(f"ALTER TABLE jobs ADD COLUMN {name} TEXT")
        if "scratch_mb" not in columns:
            self.db.execute("ALTER TABLE jobs ADD COLUMN scratch_mb INTEGER NOT NULL DEFAULT 512")
        if "headroom_gb" not in columns:
            self.db.execute("ALTER TABLE jobs ADD COLUMN headroom_gb INTEGER NOT NULL DEFAULT 32")
        self.docker = docker or Docker()
        self.memory_probe = memory_probe or available_gb
        self.hardware_probe = hardware_probe or hardware_capacity
        self.link_probe = link_probe or network_link
        self.lan_probe = lan_probe or lan_addresses
        self.services = model_server.ServiceManager(self, sys.modules[__name__])
        self.download_cache = model_cache.CacheManager(self, sys.modules[__name__])
        if "launch_failures" not in columns:
            self.db.execute("ALTER TABLE jobs ADD COLUMN launch_failures INTEGER NOT NULL DEFAULT 0")
        self._docker_unavailable = False

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
        return {"limits": limits, "hardware": hardware, "policy_saved": bool(stored), "internet": self.internet_policy(),
                "applies_to": "new_jobs_and_admissions", "running_jobs_unchanged": True,
                "legacy_project_overrides": "clamped_to_per_job_caps" if stored else "preserved"}

    def resources(self, request):
        fields(request, ("v", "op"), ("limits", "internet"))
        limits = validate_resources(request["limits"], self.hardware_probe()) if "limits" in request else None
        if "internet" in request and type(request["internet"]) is not bool:
            reject("invalid_request", "internet must be boolean")
        if limits is not None or "internet" in request:
            with self.transaction():
                if limits is not None:
                    self.db.execute("INSERT OR REPLACE INTO settings VALUES('resource_policy',?)", (canonical(limits).decode(),))
                    self.audit("resource_policy_updated", limits=limits)
                if "internet" in request:
                    self.db.execute("INSERT OR REPLACE INTO settings VALUES('internet_policy',?)", (canonical({"v": 1, "enabled": request["internet"]}).decode(),))
                    self.audit("internet_policy_updated", enabled=request["internet"])
        return self.resource_policy()

    def internet_policy(self):
        stored = self.db.execute("SELECT value FROM settings WHERE key='internet_policy'").fetchone()
        enabled = True
        if stored:
            value = json.loads(stored[0])
            fields(value, ("v", "enabled"))
            if type(value["v"]) is not int or value["v"] != 1 or type(value["enabled"]) is not bool:
                reject("invalid_internet_policy", "Saved internet policy is invalid")
            enabled = value["enabled"]
        return {"enabled": enabled, "saved": bool(stored), "available": "python" in self.registry(),
                "blocks": "home and private networks, this Spark, cloud metadata",
                "applies": "new jobs; turning off also cuts running jobs",
                "guard": {"mode": "ipv4_observed_egress", "host_network_checks": "before_start_and_each_scheduler_tick",
                          "observed_public_egress_blocked": True, "refresh_seconds": 30, "max_age_seconds": 60,
                          "limitation": "Additional router public aliases or addresses used by different outbound routes are not discovered. Network changes are detected periodically, not instantly."}}

    def job_network(self):
        policy = self.internet_policy()
        image = self.registry().get("python", {}).get("image")
        egress = "off" if not policy["enabled"] else "internet" if image else "no_helper"
        return {"egress": egress, "proxy_image": image if egress == "internet" else None}

    def stage_network(self, job_root, network):
        if network["egress"] == "internet":
            atomic_write(job_root / "proxy.py", Path(__file__).with_name("proxy.py").read_bytes(), 0o444)

    def internet_status(self, row):
        names = {"internet": "on", "off": "off", "cut": "cut_by_owner", "no_helper": "unavailable", "setup_failed": "unavailable", "network_changed": "unavailable", "guard_failed": "unavailable"}
        reason = {"cut": "Internet was turned off by the owner", "no_helper": "The Python runtime needed for internet is not installed",
                  "setup_failed": "The private connection could not be safely prepared; this job runs offline",
                  "network_changed": "Spark network addresses changed or could not be checked; this job's internet was stopped. Submit a new job to reconnect.",
                  "guard_failed": "The job's IPv6 isolation could not be verified; it was stopped before further execution."}.get(row["egress"])
        return {"internet": names.get(row["egress"], "package_registries_only" if is_preparation(row) else None),
                "internet_reason": reason, "internet_use": json.loads(row["internet_use"]) if row["internet_use"] else None}

    @contextlib.contextmanager
    def transaction(self):
        self.db.execute("BEGIN IMMEDIATE")
        try:
            yield
            self.db.execute("COMMIT")
        except BaseException:
            self.db.execute("ROLLBACK")
            raise

    @contextlib.contextmanager
    def runtime_gate(self):
        """Only DGXBridge's runtime installer owns the exclusive side of this gate."""
        if os.name != "posix":
            yield
            return
        import fcntl
        fd = safe_open(self.root.parent / "runtime-refresh.lock", os.O_RDWR | os.O_CREAT)
        try:
            info = os.fstat(fd)
            if info.st_uid != os.getuid() or info.st_nlink != 1:
                reject("unsafe_state", "Runtime refresh lock ownership is unsafe")
            try:
                fcntl.flock(fd, fcntl.LOCK_SH | fcntl.LOCK_NB)
            except BlockingIOError:
                reject("runtime_refresh_in_progress", "DGXBridge is updating its Spark runtime; retry this request afterward")
            yield
        finally:
            os.close(fd)

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

    def object_present(self, value):
        """Presence is metadata only: a regular object of at most one chunk. Materialization re-verifies."""
        size = regular_size(self.object_path(value))
        return size is not None and size <= CHUNK

    def free_bytes(self):
        free, _ = self.disk_probe()
        return max(0, int(free))

    def require_disk(self, incoming, queued=0):
        free = self.free_bytes()
        if free - incoming - queued < DISK_RESERVE:
            disk_refusal(free, incoming + queued)

    def storage_view(self, queued=None):
        free, total = self.disk_probe()
        view = {"free_bytes": max(0, int(free)), "total_bytes": max(0, int(total)), "reserve_bytes": DISK_RESERVE,
                "below_reserve": int(free) < DISK_RESERVE}
        if queued is not None:
            view["queued_bytes"] = queued
        return view

    def chunk_bytes(self, value, data):
        self.object_path(value)
        if not isinstance(data, str) or len(data) > 4 * ((CHUNK + 2) // 3):
            reject("size_limit", "Chunk exceeds one MiB")
        try:
            raw = base64.b64decode(data, validate=True)
        except (ValueError, TypeError):
            reject("invalid_request", "Chunk encoding is invalid")
        if len(raw) > CHUNK or sha(raw) != value:
            reject("hash_mismatch", "Chunk hash or length does not match")
        if SECRET.search(raw):
            reject("secret_detected", "Chunk contains credential material")
        return raw

    def put(self, request):
        fields(request, ("v", "op", "sha256", "data"))
        raw = self.chunk_bytes(request["sha256"], request["data"])
        self.require_disk(len(raw), self.queued_bytes())
        atomic_write(self.object_path(request["sha256"]), raw)
        return {"sha256": request["sha256"], "size": len(raw)}

    def put_many(self, request):
        fields(request, ("v", "op", "chunks"))
        chunks = request["chunks"]
        if not isinstance(chunks, list) or not 1 <= len(chunks) <= 128 or len(canonical(request)) > PUT_MANY_BODY:
            reject("size_limit", f"Upload batch exceeds its count or {PUT_MANY_BODY // CHUNK}-MiB body limit")
        for chunk in chunks:
            fields(chunk, ("sha256", "data"))
        # Every chunk is verified before any is stored, and the disk guard sees the whole batch.
        raws = [self.chunk_bytes(chunk["sha256"], chunk["data"]) for chunk in chunks]
        # Space a queued snapshot will still write is already promised; uploads cannot spend it too.
        self.require_disk(sum(len(raw) for raw in raws), self.queued_bytes())
        accepted = []
        for chunk, raw in zip(chunks, raws):
            atomic_write(self.object_path(chunk["sha256"]), raw)
            accepted.append({"sha256": chunk["sha256"], "size": len(raw)})
        return {"accepted": accepted, "bytes": sum(c["size"] for c in accepted)}

    def missing(self, request):
        fields(request, ("v", "op", "hashes"))
        values = request["hashes"]
        if not isinstance(values, list) or len(values) > MISSING_BATCH:
            reject("size_limit", "Too many chunk hashes")
        absent = [value for value in values if not self.object_present(value)]
        # An empty probe tells a new CLI which limits apply; older controllers omit both keys.
        return {"missing": absent, "limits": sync_limits(), "storage": self.storage_view()}

    # Snapshot assembly. A sync validates and queues; one global materializer assembles.

    def snapshot_ready(self, manifest_id):
        path = self.snapshots / manifest_id
        return path.is_dir() and not path.is_symlink()

    def commit_snapshot(self, project, manifest_id, encoded, total):
        with self.transaction():
            self.db.execute("INSERT OR IGNORE INTO manifests VALUES(?,?,?)", (manifest_id, encoded.decode(), total))
            self.db.execute("INSERT OR IGNORE INTO projects VALUES(?,?)", (project, manifest_id))
            self.audit("snapshot_committed", project=project, manifest=manifest_id, bytes=total)

    def pending_tree(self, manifest_id):
        return self.snapshots / (".pending-" + manifest_id)

    def partial_dir(self, manifest_id):
        return self.snapshots / (".partial-" + manifest_id)

    def take_failure(self, manifest_id):
        """Return a background failure exactly once, then forget it."""
        marker = self.pending / (manifest_id + ".failed")
        if not os.path.lexists(marker):
            return
        try:
            with os.fdopen(safe_open(marker), "rb") as stream:
                value = json.loads(stream.read(65536))
            code, message = value["code"], value["message"]
            if not isinstance(code, str) or not IDENTIFIER.fullmatch(code) or not isinstance(message, str):
                raise ValueError()
        except (OSError, ValueError, TypeError, KeyError, Rejected):
            code, message = "materialize_failed", "Spark could not assemble the snapshot; run the same command again"
        unlink_quietly(marker)
        reject(code, message[:512])

    def store_ready(self, entry):
        return regular_size(self.files / store_name(entry)) == entry["size"]

    def snapshot_needs(self, manifest_id, manifest):
        """(new bytes assembly writes, bytes of chunks still to upload, kept small bytes it re-verifies).

        Absent chunks are counted, each hash once, instead of refusing at the first, so the
        disk guard can run before the CLI uploads anything.
        """
        tree, remaining, absent, reverify, seen = self.pending_tree(manifest_id), 0, 0, 0, set()
        for entry in manifest:
            large = entry["size"] > LARGE_FILE
            if large and self.store_ready(entry):
                continue
            if os.path.lexists(tree.joinpath(*entry["path"].split("/"))):
                if not large:
                    reverify += entry["size"]  # Kept from an interrupted pass; never fsynced, so re-hashed.
                continue
            for chunk in entry["chunks"]:
                if chunk["sha256"] not in seen:
                    seen.add(chunk["sha256"])
                    if not self.object_present(chunk["sha256"]):
                        absent += chunk["size"]
            remaining += entry["size"]
        return remaining, absent, reverify

    def read_pending(self, manifest_id):
        with os.fdopen(safe_open(self.pending / (manifest_id + ".json")), "rb") as stream:
            raw = stream.read(PENDING_READ + 1)
        if len(raw) > PENDING_READ:
            reject("size_limit", "Pending manifest is too large")
        record = json.loads(raw)
        fields(record, ("v", "project", "manifest", "remaining"))
        if record["v"] != 1:
            reject("invalid_request", "Unsupported pending manifest version")
        integer(record["remaining"], 0, MAX_TREE)
        digest(record["project"])
        return record

    def progress(self, manifest_id):
        try:
            with os.fdopen(safe_open(self.pending / (manifest_id + ".progress")), "rb") as stream:
                value = json.loads(stream.read(65536))
            return {key: integer(value[key], 0, MAX_TREE) for key in ("done_bytes", "written_bytes", "total_bytes")}
        except (OSError, ValueError, TypeError, KeyError, Rejected):
            return None

    def pending_ids(self):
        """Queued manifest IDs, oldest first (a queued record is written once and never rewritten)."""
        entries = []
        try:
            with os.scandir(self.pending) as found:
                for item in found:
                    name = item.name
                    if name.endswith(".json") and HASH.fullmatch(name[:-5]) and item.is_file(follow_symlinks=False):
                        entries.append((item.stat(follow_symlinks=False).st_mtime_ns, name[:-5]))
        except FileNotFoundError:
            return []
        return [manifest_id for _, manifest_id in sorted(entries)]

    def record_remaining(self, manifest_id):
        """One queued record's outstanding bytes, read from its tail without parsing its manifest."""
        try:
            with os.fdopen(safe_open(self.pending / (manifest_id + ".json")), "rb") as stream:
                size = os.fstat(stream.fileno()).st_size
                stream.seek(max(0, size - 128))
                match = RECORD_TAIL.search(stream.read(128))
        except (OSError, Rejected):
            return 0
        progress = self.progress(manifest_id)
        remaining = min(int(match.group(1)), MAX_TREE) if match else 0
        return max(0, remaining - (progress["written_bytes"] if progress else 0))

    def queued_bytes(self):
        """Bytes queued snapshots will still write: cheap enough for every upload batch and admission."""
        return sum(self.record_remaining(manifest_id) for manifest_id in self.pending_ids())

    def queue_view(self):
        """Pending snapshots in FIFO order with their outstanding new bytes."""
        view = []
        for position, manifest_id in enumerate(self.pending_ids()):
            try:
                record = self.read_pending(manifest_id)
            except (OSError, ValueError, TypeError, Rejected):
                record = {"project": None, "remaining": 0, "manifest": []}
            progress = self.progress(manifest_id)
            total = sum(entry.get("size", 0) for entry in record["manifest"] if isinstance(entry, dict) and type(entry.get("size")) is int)
            written = progress["written_bytes"] if progress else 0
            view.append({"manifest_id": manifest_id, "project": record["project"], "position": position,
                         "state": "assembling" if progress else "queued",
                         "done_bytes": progress["done_bytes"] if progress else 0,
                         "total_bytes": progress["total_bytes"] if progress else total,
                         "remaining_bytes": max(0, record["remaining"] - written)})
        return view

    @contextlib.contextmanager
    def queue_lock(self):
        """Serialize queue admission: reading the queue, the disk and cap checks, and writing a record.

        Each RPC is its own process, so without it two syncs could pass both checks against the
        same queue. Never the materialize lock, which a background build holds for minutes.
        """
        if os.name != "posix":
            yield
            return
        import fcntl
        fd = os.open(self.pending / ".queue.lock", os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, 0o600)
        try:
            fcntl.flock(fd, fcntl.LOCK_EX)
            yield
        finally:
            os.close(fd)

    @contextlib.contextmanager
    def record_lock(self, manifest_id):
        """Yield True while this process exclusively holds one queued record, else False.

        A record is written once and removed by whoever built it, so its inode names one
        queued assembly: an inline RPC and the background materializer never build the same
        snapshot at once. Off posix this only checks that the record exists.
        """
        path = self.pending / (manifest_id + ".json")
        if os.name != "posix":
            yield os.path.lexists(path)
            return
        import fcntl
        try:
            fd = safe_open(path)
        except (FileNotFoundError, Rejected):
            yield False
            return
        try:
            try:
                fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                yield False
                return
            # Built and removed (or never this inode) before the lock was ours.
            try:
                current = os.lstat(path)
            except FileNotFoundError:
                yield False
                return
            mine = os.fstat(fd)
            yield (current.st_ino, current.st_dev) == (mine.st_ino, mine.st_dev)
        finally:
            os.close(fd)

    def claimable(self):
        """Whether some queued record is free for a builder; an inline RPC may hold the others."""
        for manifest_id in self.pending_ids():
            with self.record_lock(manifest_id) as held:
                if held:
                    return True
        return False

    @contextlib.contextmanager
    def materialize_lock(self):
        """Yield True while this process holds the one global materialization lock (a no-op off posix)."""
        if os.name != "posix":
            yield True
            return
        import fcntl
        fd = os.open(self.snapshots / ".materialize.lock", os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, 0o600)
        try:
            try:
                fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                yield False
                return
            yield True
        finally:
            os.close(fd)

    def kick(self):
        """Spawn the background materializer when work is queued and none is running."""
        if not self.claimable():
            return False
        with self.materialize_lock() as free:
            pass
        if free:
            self.spawn()
        return free

    def scheduler_alive(self):
        last = self.db.execute("SELECT value FROM settings WHERE key='scheduler_heartbeat'").fetchone()
        try:
            return bool(last) and time.time() - float(last[0]) < SCHEDULER_FRESH
        except (TypeError, ValueError):
            return False

    def wake(self):
        """Get queued work built without tying the builder to this RPC's service.

        The scheduler's tick, at most about a second away, spawns the materializer inside
        sparkdev-controller.service, whose KillMode=process keeps it alive. A child of this
        RPC would land in the cgroup of whatever served it (sparkdev-link.service on the
        paired route) and die when that unit restarts. Only a scheduler that has stopped
        beating makes the RPC spawn the materializer itself.
        """
        return False if self.scheduler_alive() else self.kick()

    def sync(self, request):
        fields(request, ("v", "op", "project", "manifest"))
        project = digest(request["project"])
        manifest, encoded, manifest_id, total = validate_manifest(request["manifest"])
        result = {"project": project, "manifest_id": manifest_id, "files": len(manifest), "bytes": total}
        if self.snapshot_ready(manifest_id):
            self.commit_snapshot(project, manifest_id, encoded, total)
            return result
        self.take_failure(manifest_id)
        remaining, absent, reverify = self.snapshot_needs(manifest_id, manifest)
        # Small snapshots assemble inside this RPC, never queued behind a large background
        # build. Kept small files count: each one is re-hashed here.
        inline = remaining + reverify <= INLINE_BYTES
        record = self.pending / (manifest_id + ".json")
        with self.queue_lock():
            if not os.path.lexists(record):
                queue = self.queue_view()
                free = self.free_bytes()
                # Before any upload: chunks still to send count with what assembly will write.
                needed = absent + remaining + sum(item["remaining_bytes"] for item in queue)
                if free - needed < DISK_RESERVE:
                    disk_refusal(free, needed)
                if absent:
                    reject("missing_chunks", "Upload missing chunks before committing the snapshot")
                if not inline and len(queue) >= MAX_PENDING:
                    reject("materializing", f"Spark is already assembling {len(queue)} snapshots; run the same command again to continue")
                atomic_write(record, canonical({"v": 1, "project": project, "manifest": manifest, "remaining": remaining}))
            elif absent:
                reject("missing_chunks", "Upload missing chunks before committing the snapshot")
        if inline:
            self.materialize_one(manifest_id)
        self.wake()
        deadline = self.started + MATERIALIZE_WAIT
        while True:
            if self.snapshot_ready(manifest_id):
                self.commit_snapshot(project, manifest_id, encoded, total)
                return result
            self.take_failure(manifest_id)
            # Another process held the record a moment ago; whoever holds it next builds it.
            if inline and self.materialize_one(manifest_id) is not None:
                continue
            if self.clock() >= deadline:
                break
            self.sleep(MATERIALIZE_POLL)
        progress = self.progress(manifest_id) or {"done_bytes": 0, "total_bytes": total}
        reject("materializing", f"Spark is assembling this snapshot: {progress['done_bytes'] // CHUNK} of "
                                f"{-(-progress['total_bytes'] // CHUNK)} MiB; run the same command again to continue")

    def materialize_queue(self):
        """Assemble queued snapshots oldest first under the global lock; return how many were processed."""
        processed = 0
        while True:
            with self.materialize_lock() as held:
                if not held:
                    return processed
                while self.materialize_next() is not None:
                    processed += 1
            # A sync that saw the lock held may have queued work after the last look.
            if not self.claimable():
                return processed

    def materialize_next(self):
        """Build the oldest record no inline RPC holds: True built, False failed, None when none is free."""
        for manifest_id in self.pending_ids():
            done = self.materialize_one(manifest_id)
            if done is not None:
                return done
        return None

    def materialize_one(self, manifest_id):
        """Build one queued snapshot unless another process holds its record (then None)."""
        with self.record_lock(manifest_id) as held:
            return self.build_snapshot(manifest_id) if held else None

    def build_snapshot(self, manifest_id):
        """Build one queued snapshot. Call only while holding its record lock."""
        record_path = self.pending / (manifest_id + ".json")
        tree, partial = self.pending_tree(manifest_id), self.partial_dir(manifest_id)
        try:
            record = self.read_pending(manifest_id)
            manifest, encoded, recomputed, total = validate_manifest(record["manifest"])
            if recomputed != manifest_id:
                reject("invalid_request", "Pending manifest does not match its identifier")
            if not self.snapshot_ready(manifest_id):
                if tree.is_symlink() or partial.is_symlink():
                    reject("unsafe_state", "Pending snapshot path is a symlink")
                remove_tree(partial)  # A partial file always restarts from zero.
                tree.mkdir(mode=0o700, exist_ok=True)
                partial.mkdir(mode=0o700)
                meter = Progress(self, manifest_id, total)
                meter.write(force=True)
                for index, entry in enumerate(manifest):
                    self.materialize_entry(tree, partial, index, entry, meter)
                try:
                    os.rename(tree, self.snapshots / manifest_id)
                except OSError:
                    if not self.snapshot_ready(manifest_id):
                        raise
            self.commit_snapshot(record["project"], manifest_id, encoded, total)
        except Exception as error:
            if isinstance(error, Rejected):
                code, message = error.code, error.message
            elif isinstance(error, OSError) and error.errno == errno.ENOSPC:
                code, message = "insufficient_disk", f"Spark ran out of disk space while assembling the snapshot; DGXBridge keeps {DISK_RESERVE // GIB} GiB in reserve. Free Spark disk space, then retry"
            else:
                code, message = "materialize_failed", "Spark could not assemble the snapshot; run the same command again"
            remove_tree(tree)
            remove_tree(partial)
            # The content store is kept: its files are complete, fsynced and verified.
            atomic_write(self.pending / (manifest_id + ".failed"), canonical({"code": code, "message": message}))
            unlink_quietly(record_path)
            unlink_quietly(self.pending / (manifest_id + ".progress"))
            return False
        remove_tree(tree)
        remove_tree(partial)
        unlink_quietly(record_path)
        unlink_quietly(self.pending / (manifest_id + ".progress"))
        return True

    def read_object(self, chunk):
        """Today's exact per-chunk check; an object whose content does not match its name is removed."""
        path = self.object_path(chunk["sha256"])
        try:
            with os.fdopen(safe_open(path), "rb") as source:
                raw = source.read(CHUNK + 1)
        except FileNotFoundError:
            reject("missing_chunks", "Upload missing chunks before committing the snapshot")
        if len(raw) != chunk["size"] or sha(raw) != chunk["sha256"]:
            if sha(raw) != chunk["sha256"]:
                unlink_quietly(path)
            reject("hash_mismatch", "Stored chunk failed verification; it was removed for upload again")
        return raw

    def assemble(self, entry, target, meter, durable):
        """Write one file from verified chunks, scanning every chunk with the previous chunk's tail."""
        whole, tail = hashlib.sha256(), b""
        flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_BINARY", 0)
        with os.fdopen(os.open(target, flags, 0o600), "wb") as stream:
            for chunk in entry["chunks"]:
                raw = self.read_object(chunk)
                if SECRET.search(tail + raw):
                    reject("secret_detected", "Project contains credential material")
                tail = raw[-512:]
                whole.update(raw)
                stream.write(raw)
                meter.add(len(raw), written=True)
            if durable:
                stream.flush()
                os.fsync(stream.fileno())
        if whole.hexdigest() != entry["sha256"]:
            reject("hash_mismatch", "Whole-file hash does not match")
        os.chmod(target, read_only_mode(entry))

    def verify_file(self, path, entry):
        """Re-hash and rescan a small file kept from an interrupted pass; it was never fsynced."""
        whole, tail, size = hashlib.sha256(), b"", 0
        try:
            with os.fdopen(safe_open(path), "rb") as stream:
                while raw := stream.read(CHUNK):
                    if SECRET.search(tail + raw):
                        return False
                    tail, size = raw[-512:], size + len(raw)
                    whole.update(raw)
        except (OSError, Rejected):
            return False
        return size == entry["size"] and whole.hexdigest() == entry["sha256"]

    def materialize_entry(self, tree, partial, index, entry, meter):
        out = tree.joinpath(*entry["path"].split("/"))
        large = entry["size"] > LARGE_FILE
        if os.path.lexists(out):
            # Resume: the whole file is the unit. Large files were fsynced in the store before linking.
            if regular_size(out) == entry["size"] and (large or self.verify_file(out, entry)):
                meter.add(entry["size"])
                return
            if os.name != "posix" and not out.is_symlink():
                os.chmod(out, 0o600)  # Windows refuses to unlink a read-only file; posix needs no chmod.
            os.unlink(out)
        out.parent.mkdir(parents=True, exist_ok=True)
        temporary = partial / str(index)
        if not large:
            # Uploads and jobs may have spent space since this snapshot was admitted:
            # stop before any file would cut into the reserve.
            self.require_disk(entry["size"])
            self.assemble(entry, temporary, meter, durable=False)
            os.replace(temporary, out)
            return
        store = self.files / store_name(entry)
        if not self.store_ready(entry):
            self.require_disk(entry["size"])
            self.assemble(entry, temporary, meter, durable=True)
            os.replace(temporary, store)
        else:
            meter.add(entry["size"])
        try:
            os.link(store, out)
        except OSError as error:
            if error.errno != errno.EMLINK:
                raise
            # The store inode reached its link limit: give this snapshot its own durable copy.
            self.require_disk(entry["size"])
            shutil.copyfile(store, temporary, follow_symlinks=False)
            with open(temporary, "rb+") as stream:
                os.fsync(stream.fileno())
            os.chmod(temporary, read_only_mode(entry))
            os.replace(temporary, out)

    def get_job(self, job):
        identifier(job)
        row = self.db.execute("SELECT * FROM jobs WHERE id=?", (job,)).fetchone()
        if row is None and model_server.NAME.fullmatch(job) and not job.startswith("laptop"):
            row = self.services.resolve(job)
        if row is None:
            reject("job_not_found", "Job does not exist")
        return row

    def job_status(self, row):
        return {"job": row["id"], "project": row["project"], "manifest_id": row["manifest"],
                "runtime": row["runtime"], "runtime_digest": row["image"], "client": row["client"],
                "where": "spark", "state": row["state"], "exit_code": row["exit_code"],
                "created_at": row["created"], "started_at": row["started"], "finished_at": row["finished"],
                "cancel_requested": bool(row["cancel"]), "cache_hit": row["cached_from"] is not None,
                "cached_from": row["cached_from"], "error": row["error"], "cpu": row["cpu"], "memory_gb": row["memory"],
                "reserved_cpu": reserved_cpu(row), "reserved_memory_gb": reserved_memory(row),
                "scratch_mb": row["scratch_mb"], "headroom_gb": row["headroom_gb"],
                **self.internet_status(row), **model_server.status_fields(row)}

    def claim_conflict(self, project):
        remote = self.db.execute("SELECT id FROM jobs WHERE project=? AND kind<>'service' AND state IN ('queued','starting','running')", (project,)).fetchone()
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
            if self.db.execute("SELECT 1 FROM service_requests WHERE request_id=?", (request_id,)).fetchone():
                reject("idempotency_conflict", "Request ID already belongs to a model server")
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
            if profile.exists():
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
            if cpu > limits["client_cpu"] or cpu > limits["cpu_total"] or memory > limits["memory_gb_total"]:
                reject("resource_policy_exceeded", "Job and preparation proxy must fit owner total and client caps; adjust resources before submitting")
            if request["op"] == "prepare":
                # The fixed proxy consumes part of the owner's job budget, not
                # extra quota above it. Minimum accepted budgets leave a worker
                # with 0.75 CPU and 0.875 GiB; Docker enforces both limits.
                cpu -= PROXY_CPU
                memory -= PROXY_MEMORY_GB
                scratch_mb = min(scratch_mb, int(memory * 512))
            network = self.job_network() if request["op"] == "run" else {"egress": None, "proxy_image": registry["python"]["image"]}
            cache_body = {"project": project, "manifest": manifest_id, "argv": args,
                          "runtime": runtime, "worker": sha(worker), "policy": 2,
                          "network": "open" if network["egress"] == "internet" else "none",
                          "cpu": cpu, "memory": memory, "scratch_mb": scratch_mb}
            if network["egress"] == "internet":
                cache_body["egress"] = "internet"
            cache_key = sha(canonical(cache_body))
            cached = None if request.get("no_cache") else self.db.execute(
                "SELECT id FROM jobs WHERE cache_key=? AND egress IS ? AND state='succeeded' AND cached_from IS NULL ORDER BY finished DESC LIMIT 1", (cache_key, network["egress"])).fetchone()
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
            elif not cached:
                self.stage_network(job_root, network)
            self.db.execute("""INSERT INTO jobs(id,request_id,request,project,manifest,runtime,image,client,state,created,
                            finished,exit_code,cache_key,cached_from) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?)""",
                            (job, request_id, encoded, project, manifest_id, request["runtime"], runtime["image"], client,
                             "succeeded" if cached else "queued", now, now if cached else None, 0 if cached else None,
                             cache_key, cached[0] if cached else None))
            self.db.execute("UPDATE jobs SET proxy_image=?,egress=? WHERE id=?", (network["proxy_image"] if not cached else None, network["egress"], job))
            self.db.execute("UPDATE jobs SET cpu=?,memory=?,scratch_mb=?,headroom_gb=? WHERE id=?", (cpu, memory, scratch_mb, limits["headroom_gb"], job))
            self.audit("job_cached" if cached else "job_queued", job, client, cache_key=cache_key, cached_from=cached[0] if cached else None)
        return self.job_status(self.get_job(job))

    def prepare(self, request):
        fields(request, ("v", "op", "project", "manifest_id", "runtime", "client", "request_id"))
        if request["runtime"] not in ("rust", "node", "python", "cuda"):
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
        if "name" in request:
            return self.services.cancel_name(request)
        fields(request, ("v", "op", "job"))
        with self.transaction():
            row = self.get_job(request["job"])
            if row["state"] not in TERMINAL and not row["cancel"]:
                self.db.execute("UPDATE jobs SET cancel=1 WHERE id=?", (row["id"],))
                if model_server.is_service(row):
                    self.db.execute("UPDATE jobs SET stop_reason='stopped' WHERE id=?", (row["id"],))
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
        registry = self.registry()
        jobs = [self.job_status(r) for r in self.db.execute("SELECT * FROM jobs WHERE state IN ('queued','starting','running') OR id IN (SELECT id FROM jobs WHERE state NOT IN ('queued','starting','running') ORDER BY created DESC LIMIT 200) ORDER BY created DESC")]
        queue = self.queue_view()
        storage = self.storage_view(queued=sum(item["remaining_bytes"] for item in queue))
        for job in jobs:
            if job["kind"] != "service" and job["state"] == "queued" and policy["policy_saved"] and (job["reserved_cpu"] > policy["limits"]["cpu_per_job"] or job["reserved_memory_gb"] > policy["limits"]["memory_gb_per_job"]):
                job["admission_blocked"] = "Saved per-job caps are below this existing job's immutable allocation; raise caps or cancel and resubmit"
            elif job["state"] == "queued":
                small = self.work_bytes(job["manifest_id"])
                if small is not None and storage["free_bytes"] - small - storage["queued_bytes"] < DISK_RESERVE:
                    job["admission_blocked"] = (f"Spark has {gib(storage['free_bytes'])} GiB free; this job's work copy needs {gib(small)} GiB, "
                                                f"snapshots being assembled {gib(storage['queued_bytes'])} GiB, and DGXBridge keeps "
                                                f"{DISK_RESERVE // GIB} GiB in reserve. Free Spark disk space; the job stays queued")
        for row in self.db.execute("SELECT * FROM leases WHERE state='running' OR id IN (SELECT id FROM leases WHERE state<>'running' ORDER BY created DESC LIMIT 200) ORDER BY created DESC"):
            jobs.append({"job": row["job"] or row["id"], "project": row["project"], "client": row["client"], "where": "laptop",
                         "state": "lease_lost" if row["state"] == "running" and row["expires"] < time.time() else row["state"],
                         "exit_code": row["exit_code"], "created_at": row["created"], "expires_at": row["expires"], "next_offset": row["log_offset"]})
        last = self.db.execute("SELECT value FROM settings WHERE key='scheduler_heartbeat'").fetchone()
        # The link report is observation only: a probe failure never fails status.
        network = {"v": 1, "observed": False, "basis": "unknown"}
        try:
            network = network_view(bounded_call(self.link_probe, PROBE_LIMIT))
        except Exception:
            pass
        try:
            epoch = bounded_call(lambda: lan_routes_view(self.root, self.lan_probe), PROBE_LIMIT).get("epoch")
            if epoch:
                network["lan_epoch"] = epoch
        except Exception:
            pass
        # Every preparation, whatever its runtime, needs the Python image for its proxy.
        return {"jobs": sorted(jobs, key=lambda j: j["created_at"], reverse=True), "runtimes": list(registry),
                "preparable_runtimes": list(registry) if "python" in registry else [],
                "scheduler": {"last_heartbeat": float(last[0]) if last else None, "healthy": self.scheduler_alive()},
                **policy, "dependency_preparation": "isolated_allowlist_proxy", "network": network,
                # `limits` above is the resource policy; transfer limits use their own key.
                "storage": storage, "sync_limits": sync_limits(), "materializing": queue,
                "downloads": self.download_cache.status(storage)}

    def lan_routes(self, request):
        fields(request, ("v", "op"))
        return bounded_call(lambda: lan_routes_view(self.root, self.lan_probe), PROBE_LIMIT)

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

    def rpc(self, request, started=None):
        # A sync's background wait is bounded from process start when main() passes it.
        self.started = self.clock() if started is None else started
        try:
            if not isinstance(request, dict) or type(request.get("v")) is not int or request["v"] != 1:
                reject("version_unsupported", "Protocol version 1 is required")
            op = request.get("op")
            if op in {"run", "prepare", "serve"}:
                with self.runtime_gate():
                    return {"ok": True, **(self.services.serve(request) if op == "serve" else getattr(self, op)(request))}
            if op in {"call", "reply"}:
                return {"ok": True, **getattr(self.services, op)(request)}
            if op == "downloads":
                return {"ok": True, **self.download_cache.downloads(request)}
            if op not in {"put", "put_many", "missing", "sync", "run", "prepare", "logs", "wait", "cancel", "status", "doctor", "claim", "heartbeat", "release", "resources", "lan_routes"}:
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
            if model_server.is_service(row) and error in ("memory_watchdog", "disk_full"):
                self.db.execute("UPDATE jobs SET stop_reason=? WHERE id=?", (error, row["id"]))
            self.audit("job_finished", row["id"], row["client"], state=state, exit_code=code, error=error)

    def manifest_entries(self, manifest_id):
        row = self.db.execute("SELECT manifest FROM manifests WHERE id=?", (digest(manifest_id),)).fetchone()
        if row is None:
            reject("manifest_not_found", "Snapshot manifest is not recorded")
        return json.loads(row[0])

    def manifest_shape(self, manifest_id):
        """(bytes of files a job copies, number of large files a job reads through the source mount)."""
        key = (manifest_id, LARGE_FILE)
        if key not in self._shapes:
            entries = self.manifest_entries(manifest_id)
            if len(self._shapes) >= 256:
                self._shapes.clear()
            self._shapes[key] = (sum(e["size"] for e in entries if e["size"] <= LARGE_FILE),
                                 sum(e["size"] > LARGE_FILE for e in entries))
        return self._shapes[key]

    def work_bytes(self, manifest_id):
        """Bytes a job's work copy needs, or None when its manifest is unreadable (prepare_work then fails the job)."""
        try:
            return self.manifest_shape(manifest_id)[0]
        except (Rejected, ValueError, TypeError, KeyError):
            return None

    def disk_admits(self, row):
        needed = self.work_bytes(row["manifest"])
        return needed is None or self.free_bytes() - needed - self.queued_bytes() >= DISK_RESERVE

    def prepare_work(self, row):
        """Build the writable /work copy from the manifest alone; it never stats through a link.

        Small files are copied. A large file becomes a symlink into the read-only
        /sparkdev-source mount, so its store inode is never writable by the job.
        """
        root = self.root / "jobs" / identifier(row["id"])
        root.mkdir(mode=0o700, exist_ok=True)
        work, staging = root / "work", root / "work.pending"
        if os.path.lexists(work):
            return
        remove_tree(staging)
        entries = self.manifest_entries(row["manifest"])
        small = sum(entry["size"] for entry in entries if entry["size"] <= LARGE_FILE)
        self.require_disk(small, self.queued_bytes())
        source = self.snapshots / digest(row["manifest"])
        staging.mkdir(mode=0o700)
        try:
            for entry in entries:
                name = path_name(entry["path"])
                parts = name.split("/")
                target = staging.joinpath(*parts)
                target.parent.mkdir(parents=True, exist_ok=True)
                if entry["size"] > LARGE_FILE:
                    os.symlink("/sparkdev-source/" + name, target)
                else:
                    shutil.copy2(source.joinpath(*parts), target, follow_symlinks=False)
                    os.chmod(target, 0o755 if entry["mode"] == 0o755 else 0o644)
            os.rename(staging, work)
        except BaseException:
            remove_tree(staging)
            raise

    def create_args(self, row):
        root = self.root / "jobs" / row["id"]
        root.mkdir(mode=0o700, exist_ok=True)
        work = root / "work"
        self.prepare_work(row)
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
        preparing = is_preparation(row)
        online = row["egress"] == "internet"
        file_limit = PREPARATION_FILE_LIMIT if preparing else JOB_FILE_LIMIT
        args = ["create", "--name", self.container_name(row["id"]), "--pull", "never", "--network", "sparkdev-fetch-" + row["id"] if preparing or online else "none", "--read-only",
                "--cap-drop", "ALL", "--security-opt", "no-new-privileges", "--user", f"{uid}:{gid}",
                "--cpus", str(row["cpu"]), "--cpu-shares", "256", "--memory", f"{row['memory']}g", "--memory-swap", f"{row['memory']}g",
                "--pids-limit", "1024", "--init", "--restart", "no", "--log-driver", "none", "--ulimit", f"fsize={file_limit}:{file_limit}",
                "--tmpfs", "/tmp:rw,nosuid,nodev,size=" + str(row["scratch_mb"] * 1024 * 1024), "--workdir", "/work",
                "--label", "com.delores.sparkdev.job=" + row["id"], "--label", "com.delores.sparkdev.client=" + row["client"],
                "--mount", f"type=bind,src={work},dst=/work", "--mount", f"type=bind,src={output},dst=/sparkdev-output",
                "--mount", f"type=bind,src={root / 'argv.json'},dst=/sparkdev-argv.json,readonly",
                "--mount", f"type=bind,src={root / 'worker.py'},dst=/sparkdev-worker.py,readonly"]
        if self.manifest_shape(row["manifest"])[1]:
            # One fixed read-only mount of the whole snapshot; no user path reaches Docker arguments.
            args += ["--mount", f"type=bind,src={self.snapshots / digest(row['manifest'])},dst=/sparkdev-source,readonly"]
        for name, destination in (("target", "/target"), ("cargo", "/cargo-home"), ("npm", "/npm-cache"), ("pip", "/pip-cache")):
            path = cache / name
            if path.is_symlink():
                reject("unsafe_state", "Cache root is a symlink")
            path.mkdir(exist_ok=True)
            args += ["--mount", f"type=bind,src={path},dst={destination}"]
        for value in ("HOME=/tmp", "CARGO_HOME=/cargo-home", "CARGO_TARGET_DIR=/target", "CARGO_NET_OFFLINE=false" if online else "CARGO_NET_OFFLINE=true",
                      "npm_config_cache=/npm-cache", "npm_config_offline=false" if online else "npm_config_offline=true",
                      *(("PIP_NO_INDEX=1",) if not online else ()),
                      "PYTHONDONTWRITEBYTECODE=1", "PYTHONUNBUFFERED=1", "SPARKDEV_RUNTIME_NAME=" + row["runtime"]):
            args += ["--env", value]
        if online:
            # Docker's embedded resolver still knows the helper alias, but must
            # not forward workload DNS queries through the host's resolver.
            args += ["--dns", "127.0.0.1"]
            # Docker applies these only inside this job's private network
            # namespace. A host bridge may keep its own link-local address;
            # disabling IPv6 here removes the workload's route to that address.
            for key, value in INTERNET_IPV6_SYSCTLS.items():
                args += ["--sysctl", key + "=" + value]
            scratch = root / "scratch"
            if scratch.is_symlink():
                reject("unsafe_state", "Job scratch is a symlink")
            scratch.mkdir(mode=0o700, exist_ok=True)
            args += ["--mount", f"type=bind,src={scratch},dst=/sparkdev-scratch"]
            for name in ("HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy", "ALL_PROXY"):
                args += ["--env", name + "=http://sparkdev-proxy:3128"]
            for value in ("NO_PROXY=localhost,127.0.0.1,::1,127.0.0.0/8", "no_proxy=localhost,127.0.0.1,::1,127.0.0.0/8",
                          "GIT_CONFIG_COUNT=2", "GIT_CONFIG_KEY_0=url.https://github.com/.insteadOf", "GIT_CONFIG_VALUE_0=git@github.com:",
                          "GIT_CONFIG_KEY_1=url.https://github.com/.insteadOf", "GIT_CONFIG_VALUE_1=ssh://git@github.com/",
                          "CARGO_NET_GIT_FETCH_WITH_CLI=true", "CARGO_NET_RETRY=10", "PIP_TIMEOUT=60", "npm_config_fetch_retries=5",
                          "HF_HUB_DOWNLOAD_TIMEOUT=60", "HF_HUB_DISABLE_TELEMETRY=1", "NODE_USE_ENV_PROXY=1", "SPARKDEV_INTERNET=on",
                          "PYTHONUSERBASE=/sparkdev-scratch/python", "PIP_BREAK_SYSTEM_PACKAGES=1"):
                args += ["--env", value]
        for name in ("RAYON_NUM_THREADS", "OMP_NUM_THREADS", "OPENBLAS_NUM_THREADS", "MKL_NUM_THREADS", "CARGO_BUILD_JOBS"):
            args += ["--env", name + "=" + str(max(1, int(row["cpu"])))]
        if row["runtime"] == "cuda" and preparing:
            # A network-attached preparation never gets a GPU. "void" also overrides an image
            # NVIDIA_VISIBLE_DEVICES under a default NVIDIA runtime; prepare.py re-checks /dev.
            # No "--runtime runc": a failing create would raise out of reconcile() and stall tick().
            args += ["--env", "NVIDIA_VISIBLE_DEVICES=void"]
        elif row["runtime"] == "cuda":
            args += ["--gpus", "all", "--env", "NVIDIA_DRIVER_CAPABILITIES=compute,utility,graphics"]
        if preparing:
            args += ["--mount", f"type=bind,src={root / 'prepare.py'},dst=/sparkdev-prepare.py,readonly"]
            for name in ("HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy", "ALL_PROXY"):
                args += ["--env", name + "=http://sparkdev-proxy:3128"]
            for value in ("CARGO_NET_OFFLINE=false", "npm_config_offline=false", "PIP_NO_INDEX=0", "NO_PROXY=", "no_proxy="):
                args += ["--env", value]
        args += self.download_cache.create_args(row, preparing=preparing, network_open=online)
        self.services.create_args(row, args)
        args += ["--entrypoint", "/usr/bin/python3", row["image"], "/sparkdev-worker.py"]
        return args

    def ensure_proxy(self, row):
        if not row["proxy_image"] or row["egress"] in ("cut", "setup_failed", "network_changed", "guard_failed"):
            return
        online = row["egress"] == "internet"
        root = self.root / "jobs" / row["id"]
        if online:
            deny = root / "egress-deny.json"
            if not os.path.lexists(deny):
                atomic_write(deny, canonical(internet_deny()), 0o444)
            elif regular_size(deny) is None or regular_size(deny) > 65536:
                reject("unsafe_state", "Job network deny file is not a bounded regular file")
            elif not self.network_snapshot_matches(row, internet_deny()):
                reject("network_changed", "Spark network addresses changed before this job started")
            for name in ("egress", "scratch"):
                path = root / name
                if path.is_symlink():
                    reject("unsafe_state", "Job network state is a symlink")
                path.mkdir(mode=0o700, exist_ok=True)
        network = "sparkdev-fetch-" + row["id"]
        info = self.docker.network_inspect(network)
        if info is None:
            self.docker.call(["network", "create", "--internal", "--opt", "com.docker.network.bridge.gateway_mode_ipv4=isolated",
                              "--opt", "com.docker.network.bridge.gateway_mode_ipv6=isolated", "--label", "com.delores.sparkdev.job=" + row["id"], network])
            info = self.docker.network_inspect(network)
        if not info or not info.get("Internal") or (info.get("Labels") or {}).get("com.delores.sparkdev.job") != row["id"] or (info.get("Options") or {}).get("com.docker.network.bridge.gateway_mode_ipv4") != "isolated":
            reject("network_conflict", "Dependency network does not match its job")
        # Online workloads disable IPv6 in their own namespace, and reconciliation
        # verifies Docker retained those exact settings before any workload starts.
        name = "sparkdev-proxy-" + row["id"]
        info = self.docker.inspect(name)
        if info is None:
            uid = os.getuid() if hasattr(os, "getuid") else 1000
            gid = os.getgid() if hasattr(os, "getgid") else 1000
            args = ["create", "--name", name, "--pull", "never", "--network", "bridge", "--read-only", "--cap-drop", "ALL",
                              "--security-opt", "no-new-privileges", "--user", f"{uid}:{gid}", "--cpus", "1" if online else str(PROXY_CPU), "--cpu-shares", "512" if online else "128",
                              "--memory", "256m" if online else f"{int(PROXY_MEMORY_GB * 1024)}m", "--memory-swap", "256m" if online else f"{int(PROXY_MEMORY_GB * 1024)}m", "--pids-limit", "512" if online else "64", "--log-driver", "none",
                              "--label", "com.delores.sparkdev.job=" + row["id"], "--mount",
                              f"type=bind,src={root / 'proxy.py'},dst=/sparkdev-proxy.py,readonly"]
            if online:
                args += ["--env", "SPARKDEV_PROXY_MODE=public", "--restart", "on-failure:3", "--mount",
                         f"type=bind,src={root / 'egress-deny.json'},dst=/sparkdev-deny.json,readonly", "--mount",
                         f"type=bind,src={root / 'egress'},dst=/sparkdev-egress",
                         "--health-cmd", "/usr/bin/python3 -c 'import socket; socket.create_connection((\"127.0.0.1\",3128),1).close()'",
                         "--health-interval", "1s", "--health-timeout", "2s", "--health-retries", "15"]
            args += ["--entrypoint", "/usr/bin/python3", row["proxy_image"], "/sparkdev-proxy.py"]
            try:
                self.docker.call(args)
            except (Rejected, OSError, ValueError, subprocess.SubprocessError):
                # A preparation cannot run without its helper. A missing/pruned
                # helper image must not reserve that job's quota forever.
                if is_preparation(row) and self.docker.inspect(name) is None:
                    self.record_launch_failure(row)
                raise
            info = self.docker.inspect(name)
        if info is None or info.get("Image") != row["proxy_image"] or (info.get("Config", {}).get("Labels") or {}).get("com.delores.sparkdev.job") != row["id"]:
            reject("container_conflict", "Dependency proxy identity does not match its job")
        if network not in info.get("NetworkSettings", {}).get("Networks", {}):
            self.docker.call(["network", "connect", "--alias", "sparkdev-proxy", network, name])
        if info["State"]["Status"] == "created":
            self.docker.call(["start", name])
            info = self.docker.inspect(name)
        elif info["State"]["Status"] != "running":
            reject("proxy_failed", "Dependency proxy stopped; cancel and retry preparation")
        if online:
            health = (info or {}).get("State", {}).get("Health", {}).get("Status")
            if health == "starting" and time.time() - (row["started"] or row["created"]) < 30:
                reject("proxy_starting", "The job's internet guard is completing its first safety check")
            if health != "healthy":
                reject("proxy_guard_unavailable", "The job's internet guard could not be verified")

    def remove_proxy(self, row, graceful=False):
        name = "sparkdev-proxy-" + row["id"]
        info = self.docker.inspect(name)
        if info:
            if info.get("Image") != row["proxy_image"] or (info.get("Config", {}).get("Labels") or {}).get("com.delores.sparkdev.job") != row["id"]:
                reject("container_conflict", "Refusing to clean an unrelated proxy")
            if graceful and info.get("State", {}).get("Status") in ("running", "restarting"):
                self.docker.call(["stop", "--time", "5", name], timeout=15)
            self.docker.call(["rm", "--force", name])

    def collect_internet_use(self, row):
        if row["egress"] not in ("internet", "cut", "setup_failed", "network_changed", "guard_failed") or row["internet_use"]:
            return
        path = self.root / "jobs" / row["id"] / "egress" / "counters.json"
        try:
            with os.fdopen(safe_open(path), "rb") as stream:
                raw = stream.read(4097)
            if len(raw) > 4096:
                return
            values = json.loads(raw)
            if not isinstance(values, dict) or set(values) != set(INTERNET_COUNTERS) or any(type(v) is not int or v < 0 for v in values.values()):
                return
        except (OSError, ValueError, Rejected):
            return
        with self.transaction():
            self.db.execute("UPDATE jobs SET internet_use=? WHERE id=?", (canonical(values).decode(), row["id"]))
            self.audit("job_internet_use", row["id"], row["client"], **values)

    def cleanup_proxy(self, row):
        if not row["proxy_image"] or row["proxy_cleaned"]:
            return
        network = "sparkdev-fetch-" + row["id"]
        self.remove_proxy(row, graceful=True)
        self.collect_internet_use(row)
        net = self.docker.network_inspect(network)
        if net:
            if (net.get("Labels") or {}).get("com.delores.sparkdev.job") != row["id"]:
                reject("network_conflict", "Refusing to clean an unrelated network")
            job = self.docker.inspect(self.container_name(row["id"]))
            if job and network in job.get("NetworkSettings", {}).get("Networks", {}):
                self.docker.call(["network", "disconnect", network, self.container_name(row["id"])])
            self.docker.call(["network", "rm", network])
        remove_tree(self.root / "jobs" / row["id"] / "scratch")
        self.db.execute("UPDATE jobs SET proxy_cleaned=1 WHERE id=?", (row["id"],))

    def reconcile(self):
        rows = self.db.execute("SELECT * FROM jobs WHERE state IN ('starting','running') ORDER BY started DESC,created DESC,id DESC").fetchall()
        for row in rows:
            try:
                self.reconcile_row(row)
            except (Rejected, OSError, ValueError, subprocess.SubprocessError) as error:
                if isinstance(error, Rejected) and error.code == "docker_unavailable":
                    self._docker_unavailable = True
                self.scheduler_error("reconcile", error)

    def reconcile_row(self, row):
        name = self.container_name(row["id"])
        info = self.docker.inspect(name)
        if info is None:
            if row["state"] == "running":
                self.finish(row, "failed", 125, "container_lost")
                return
            if row["cancel"]:
                self.finish(row, "cancelled", 130)
                return
            try:
                self.prepare_work(row)
            except (Rejected, OSError, ValueError) as error:
                # A job whose work copy cannot be built fails; it never stays stuck in 'starting'.
                short = (isinstance(error, Rejected) and error.code == "insufficient_disk") or \
                    (isinstance(error, OSError) and error.errno == errno.ENOSPC)
                self.finish(row, "failed", 125, "insufficient_disk" if short else "work_copy_failed")
                return
            if row["egress"] == "internet" and not self.internet_policy()["enabled"]:
                self.db.execute("UPDATE jobs SET egress='cut' WHERE id=?", (row["id"],))
                self.audit("internet_cut", row["id"], row["client"])
                row = self.get_job(row["id"])
            try:
                self.ensure_proxy(row)
            except Rejected as error:
                if error.code == "proxy_starting":
                    return
                if row["egress"] != "internet" or error.code == "docker_unavailable":
                    raise
                self.db.execute("UPDATE jobs SET egress='setup_failed' WHERE id=?", (row["id"],))
                self.audit("internet_unavailable", row["id"], row["client"], code=error.code)
                with contextlib.suppress(Rejected, OSError, subprocess.SubprocessError):
                    self.remove_proxy(row)
                row = self.get_job(row["id"])
            try:
                args = self.create_args(row)
            except (Rejected, OSError, ValueError) as error:
                self.finish(row, "failed", 125, "work_copy_failed")
                self.scheduler_error("job setup", error)
                return
            try:
                self.docker.call(args)
            except (Rejected, OSError, ValueError, subprocess.SubprocessError):
                # Only a fresh, definite not-found may count against launch retries.
                # An unavailable daemon or an existing container leaves identity intact.
                if self.docker.inspect(name) is None:
                    self.record_launch_failure(row)
                raise
            info = self.docker.inspect(name)
            if info is None:
                reject("docker_unavailable", "Created container cannot be inspected")
        self.container_matches(info, row)
        if row["egress"] == "internet":
            settings = (info.get("HostConfig") or {}).get("Sysctls") or {}
            if any(settings.get(key) != value for key, value in INTERNET_IPV6_SYSCTLS.items()):
                if info["State"]["Status"] in ("running", "restarting", "paused"):
                    self.docker.call(["stop", "--time", "5", name], timeout=15)
                self.db.execute("UPDATE jobs SET egress='guard_failed' WHERE id=?", (row["id"],))
                self.finish(row, "failed", 125, "internet_guard_failed")
                self.audit("internet_guard_failed", row["id"], row["client"], code="ipv6_namespace")
                return
        if self.services.reconcile(row, info):
            return
        state = info["State"]["Status"]
        # Preparation has no GPU, so only CUDA runs need the unified-memory watchdog.
        if row["cancel"] or (row["runtime"] == "cuda" and not is_preparation(row) and self.memory_probe() < row["headroom_gb"]):
            if state in ("running", "restarting", "paused"):
                self.docker.call(["stop", "--time", "5", name], timeout=15)
            self.finish(row, "cancelled" if row["cancel"] else "failed", 130 if row["cancel"] else 137,
                        None if row["cancel"] else "memory_watchdog")
        elif state == "created":
            # A cancellation which arrived while Docker creation was underway wins
            # before start. The next tick observes it and removes the proxy.
            if self.get_job(row["id"])["cancel"]:
                return
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

    def record_launch_failure(self, row):
        self.db.execute("UPDATE jobs SET launch_failures=launch_failures+1 WHERE id=?", (row["id"],))
        if self.get_job(row["id"])["launch_failures"] >= 3:
            self.finish(row, "failed", 125, "launch_failed")

    def tick(self):
        try:
            with self.runtime_gate():
                self._tick()
        except Rejected as error:
            if error.code != "runtime_refresh_in_progress":
                raise

    def network_snapshot_matches(self, row, current):
        # A single-file bind keeps its original inode. Never rewrite it under a
        # live helper: terminate that helper if newly local public addresses are
        # not covered. Private/Docker networks are already denied unconditionally,
        # so another job's newly created 172.x bridge does not cut every peer.
        def relevant(values):
            if not isinstance(values, list) or len(values) > 256:
                raise ValueError("Invalid network snapshot")
            networks = [ipaddress.IPv4Network(value, strict=True) for value in values]
            return [network for network in networks if network.is_global]
        path = self.root / "jobs" / row["id"] / "egress-deny.json"
        with os.fdopen(safe_open(path), "rb") as stream:
            raw = stream.read(65537)
        if len(raw) > 65536:
            raise ValueError("Network snapshot is too large")
        previous = relevant(json.loads(raw))
        return all(any(network.subnet_of(old) for old in previous) for network in relevant(current))

    def cut_internet(self, row, reason):
        try:
            self.remove_proxy(row)
            self.db.execute("UPDATE jobs SET egress=? WHERE id=?", (reason, row["id"]))
            self.audit("internet_cut" if reason == "cut" else "internet_network_changed", row["id"], row["client"])
        except (Rejected, OSError, ValueError, subprocess.SubprocessError) as error:
            self.scheduler_error("internet cut", error)
            # No workload may retain internet after a failed safety cut. Stop
            # only its identity-checked container; never unrelated resources.
            try:
                info = self.docker.inspect(self.container_name(row["id"]))
                if info is not None:
                    self.container_matches(info, row)
                    if info["State"]["Status"] in ("running", "restarting", "paused"):
                        self.docker.call(["stop", "--time", "10", self.container_name(row["id"])], timeout=20)
                if model_server.is_service(row):
                    self.db.execute("UPDATE jobs SET stop_reason='internet_off',cancel=1,egress=? WHERE id=?", (reason, row["id"]))
                    self.finish(row, "cancelled", 130)
                else:
                    self.db.execute("UPDATE jobs SET egress=? WHERE id=?", (reason, row["id"]))
                    self.finish(row, "failed", 125, "internet_guard_failed")
            except (Rejected, OSError, ValueError, subprocess.SubprocessError) as stop_error:
                self._docker_unavailable = True
                self.scheduler_error("internet stop", stop_error)

    def _tick(self):
        self._docker_unavailable = False
        internet_rows = self.db.execute("SELECT * FROM jobs WHERE state IN ('starting','running') AND egress='internet'").fetchall()
        if not self.internet_policy()["enabled"]:
            for row in internet_rows:
                self.cut_internet(row, "cut")
        elif internet_rows:
            try:
                current_network = internet_deny()
            except (Rejected, OSError, ValueError):
                current_network = None
            for row in internet_rows:
                # A newly admitted row has no helper yet; ensure_proxy takes
                # its first current snapshot before creation instead.
                if row["state"] == "starting" and not os.path.lexists(self.root / "jobs" / row["id"] / "egress-deny.json"):
                    continue
                try:
                    safe = current_network is not None and self.network_snapshot_matches(row, current_network)
                except (Rejected, OSError, ValueError, TypeError):
                    safe = False
                if not safe:
                    self.cut_internet(row, "network_changed")
        # Stop one most-recently-started workload per tick when disk headroom is exhausted.
        if self.free_bytes() < DISK_STOP_FLOOR:
            row = self.db.execute("SELECT * FROM jobs WHERE state='running' ORDER BY started DESC LIMIT 1").fetchone()
            if row:
                try:
                    info = self.docker.inspect(self.container_name(row["id"]))
                    if info:
                        self.container_matches(info, row)
                        self.docker.call(["stop", "--time", "5", self.container_name(row["id"])], timeout=15)
                    if model_server.is_service(row):
                        self.db.execute("UPDATE jobs SET stop_reason='disk_full',cancel=1 WHERE id=?", (row["id"],))
                        self.finish(row, "cancelled", 130)
                    else:
                        self.finish(row, "failed", 137, "disk_full")
                except (Rejected, OSError, subprocess.SubprocessError) as error:
                    self.scheduler_error("disk floor", error)
        # Backstop: a materializer that died, or one never spawned, restarts from the queue.
        try:
            self.kick()
        except (Rejected, OSError, subprocess.SubprocessError) as error:
            # Never blocks scheduling; like serve(), report only the error kind.
            print("sparkdev materializer: " + (error.code if isinstance(error, Rejected) else type(error).__name__), file=sys.stderr, flush=True)
        self.reconcile()
        for row in self.db.execute("SELECT * FROM jobs WHERE state IN ('succeeded','failed','cancelled') AND proxy_image IS NOT NULL AND proxy_cleaned=0").fetchall():
            try:
                self.cleanup_proxy(row)
            except (Rejected, OSError, subprocess.SubprocessError) as error:
                self.scheduler_error("cleanup", error)
        try:
            self.services.housekeeping()
        except (Rejected, OSError, ValueError, subprocess.SubprocessError) as error:
            self.scheduler_error("service housekeeping", error)
        if self._docker_unavailable:
            return
        with self.transaction():
            policy = self.resource_policy()
            limits = policy["limits"]
            self.db.execute("INSERT OR REPLACE INTO settings VALUES('scheduler_heartbeat',?)", (str(time.time()),))
            active = self.db.execute("SELECT * FROM jobs WHERE state IN ('starting','running')").fetchall()
            cpu, memory = reserved_cpu, reserved_memory
            used_cpu, used_mem = sum(cpu(r) for r in active), sum(memory(r) for r in active)
            live_reserved = self.services.reserved_for_headroom(active)
            server_admitted = self.services.admit(active, limits)
            last = self.db.execute("SELECT value FROM settings WHERE key='last_client'").fetchone()
            start = (CLIENTS.index(last[0]) + 1) % len(CLIENTS) if last else 0
            for client in (() if server_admitted else CLIENTS[start:] + CLIENTS[:start]):
                mine = [r for r in active if r["client"] == client and not model_server.is_service(r)]
                row = self.db.execute("SELECT * FROM jobs WHERE client=? AND kind<>'service' AND state='queued' AND cancel=0 ORDER BY created LIMIT 1", (client,)).fetchone()
                if not row:
                    continue
                if len(mine) >= limits["client_jobs"] or sum(cpu(r) for r in mine) + cpu(row) > limits["client_cpu"]:
                    continue
                if policy["policy_saved"] and (cpu(row) > limits["cpu_per_job"] or memory(row) > limits["memory_gb_per_job"]):
                    continue
                if used_cpu + cpu(row) > limits["cpu_total"] or used_mem + memory(row) > limits["memory_gb_total"] or self.memory_probe() - live_reserved - memory(row) < limits["headroom_gb"]:
                    continue
                if not self.disk_admits(row):
                    continue  # The work copy would cut into the disk reserve; status reports admission_blocked.
                self.db.execute("UPDATE jobs SET state='starting' WHERE id=?", (row["id"],))
                self.db.execute("INSERT OR REPLACE INTO settings VALUES('last_client',?)", (client,))
                # One admission per tick avoids long uninterruptible Docker bursts.
                break
        self.reconcile()

    def scheduler_error(self, operation, error):
        if isinstance(error, Rejected) and error.code == "docker_unavailable":
            self._docker_unavailable = True
        print("sparkdev " + operation + ": " + (error.code if isinstance(error, Rejected) else type(error).__name__), file=sys.stderr, flush=True)

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
                    self.download_cache.tidy_step()
                except (Rejected, OSError, ValueError, sqlite3.Error, subprocess.SubprocessError) as error:
                    # Deliberately never log argv, Docker stderr, paths or user input.
                    print("sparkdev scheduler: " + (error.code if isinstance(error, Rejected) else type(error).__name__), file=sys.stderr, flush=True)
                time.sleep(1)


def main():
    started = time.monotonic()  # Before argument parsing, state setup and reading the request.
    os.umask(0o077)
    parser = argparse.ArgumentParser()
    # `materialize` is the detached snapshot builder; it is not an RPC operation.
    parser.add_argument("mode", choices=("rpc", "serve", "tick", "materialize"))
    parser.add_argument("--state", type=Path)
    args = parser.parse_args()
    controller = Controller(args.state)
    if args.mode == "materialize":
        controller.materialize_queue()
        return 0
    if args.mode == "rpc":
        try:
            raw = sys.stdin.buffer.read(MAX_REQUEST + 1)
            if len(raw) > MAX_REQUEST:
                reject("size_limit", "Request exceeds the protocol body limit")
            request = json.loads(raw)
            response = controller.rpc(request, started=started)
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
