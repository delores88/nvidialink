#!/usr/bin/env python3
"""Fixed HTTPS CONNECT allowlist. Deploy only as the isolated dependency proxy.

No configurable targets, plain HTTP, credentials, redirects or private addresses.
TLS stays end-to-end with the package registry. Each preparation job has its own
internal Docker network; only this proxy also has an egress network.
"""
import ipaddress
import json
import os
from pathlib import Path
import re
import select
import selectors
import signal
import socket
import socketserver
import threading
import time
from urllib.parse import urlsplit

HOSTS = frozenset({"crates.io", "static.crates.io", "index.crates.io", "registry.npmjs.org",
                   "pypi.org", "files.pythonhosted.org", "release-assets.githubusercontent.com",
                   "objects.githubusercontent.com", "github-releases.githubusercontent.com"})
MAX_CONNECTIONS = 32
MAX_BYTES = 1024**3
MAX_SECONDS = 900
SLOTS = threading.BoundedSemaphore(MAX_CONNECTIONS)


def target(header):
    if not isinstance(header, bytes) or len(header) > 8192:
        raise ValueError("invalid header")
    lines = header.split(b"\r\n")
    parts = lines[0].split(b" ")
    if len(parts) != 3 or parts[0] != b"CONNECT" or parts[2] not in (b"HTTP/1.0", b"HTTP/1.1"):
        raise ValueError("CONNECT required")
    authority = parts[1].decode("ascii")
    if not authority.endswith(":443") or authority[:-4] not in HOSTS:
        raise ValueError("host refused")
    if any(line.lower().startswith((b"authorization:", b"proxy-authorization:")) for line in lines[1:]):
        raise ValueError("credentials refused")
    return authority[:-4]


def public_addresses(host):
    values = socket.getaddrinfo(host, 443, type=socket.SOCK_STREAM)
    approved = []
    for family, kind, proto, _, address in values:
        if not ipaddress.ip_address(address[0]).is_global:
            raise ValueError("non-public destination")
        approved.append((family, kind, proto, address))
    if not approved:
        raise ValueError("no destination")
    return approved


class Handler(socketserver.BaseRequestHandler):
    def handle(self):
        if not SLOTS.acquire(blocking=False):
            return
        upstream = None
        try:
            self.request.settimeout(10)
            header = bytearray()
            # Read just the header, preserving subsequent TLS bytes for the tunnel.
            while b"\r\n\r\n" not in header and len(header) <= 8192:
                piece = self.request.recv(1)
                if not piece:
                    return
                header.extend(piece)
            host = target(bytes(header))
            for family, kind, proto, address in public_addresses(host):
                attempt = socket.socket(family, kind, proto)
                attempt.settimeout(10)
                try:
                    attempt.connect(address)  # Connect to the validated IP, never re-resolve.
                    upstream = attempt
                    break
                except OSError:
                    attempt.close()
            if upstream is None:
                raise ValueError("destination unavailable")
            self.request.sendall(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            total = 0
            deadline = time.monotonic() + MAX_SECONDS
            while time.monotonic() < deadline:
                readable, _, _ = select.select([self.request, upstream], [], [], 10)
                for source in readable:
                    chunk = source.recv(65536)
                    if not chunk:
                        return
                    total += len(chunk)
                    if total > MAX_BYTES:
                        return
                    (upstream if source is self.request else self.request).sendall(chunk)
        except (OSError, ValueError, UnicodeError):
            try:
                self.request.sendall(b"HTTP/1.1 403 Forbidden\r\nConnection: close\r\nContent-Length: 0\r\n\r\n")
            except OSError:
                pass
        finally:
            if upstream:
                upstream.close()
            SLOTS.release()


class Server(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True
    request_queue_size = 32


PUBLIC_KEYS = ("connections", "http_requests", "bytes_down", "bytes_up", "blocked_private", "refused_invalid", "failed_connect")
DENY = tuple(ipaddress.IPv4Network(value) for value in (
    "0.0.0.0/8", "10.0.0.0/8", "100.64.0.0/10", "127.0.0.0/8", "169.254.0.0/16",
    "172.16.0.0/12", "192.0.0.0/24", "192.0.2.0/24", "192.88.99.0/24", "192.168.0.0/16",
    "198.18.0.0/15", "198.51.100.0/24", "203.0.113.0/24", "224.0.0.0/4", "240.0.0.0/4", "168.63.129.16/32"))
EXTRA = ()
HEADER_LIMIT = 64 * 1024
HEADER_TIMEOUT = 30
BUFFER_LIMIT = 256 * 1024
IDLE_TIMEOUT = 300
HOST = re.compile(r"[A-Za-z0-9._-]{1,253}\Z")
TOKEN = re.compile(rb"[!#$%&'*+.^_`|~0-9A-Za-z-]+\Z")


class Refused(Exception):
    def __init__(self, code, reason):
        self.code, self.reason = code, reason


def load_extra(path):
    with open(path, "rb") as stream:
        raw = stream.read(65537)
    if len(raw) > 65536:
        raise ValueError("deny list too large")
    values = json.loads(raw)
    if not isinstance(values, list) or len(values) > 256 or any(not isinstance(v, str) for v in values):
        raise ValueError("invalid deny list")
    return tuple(ipaddress.IPv4Network(v, strict=True) for v in values)


def allowed(ip):
    try:
        address = ipaddress.ip_address(ip)
        return address.version == 4 and address.is_global and not any(address in net for net in DENY + EXTRA)
    except ValueError:
        return False


def open_upstream(host, port):
    """Resolve once, check every answer, connect only to the checked sockaddr."""
    try:
        answers = socket.getaddrinfo(host, port, socket.AF_INET, socket.SOCK_STREAM)
    except OSError:
        raise Refused(502, "Could not reach that site") from None
    if any(family != socket.AF_INET or not allowed(address[0]) for family, _, _, _, address in answers):
        raise Refused(403, "Blocked: home or private network address")
    deadline = time.monotonic() + 30
    for family, kind, proto, _, address in answers:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            break
        attempt = socket.socket(family, kind, proto)
        try:
            attempt.settimeout(min(10, remaining))
            attempt.connect(address)
            return attempt
        except OSError:
            attempt.close()
    raise Refused(502, "Could not reach that site")


def authority(value, default_port=None):
    if "[" in value or "]" in value:
        raise Refused(403, "Blocked: IPv6 addresses are not supported yet")
    if "@" in value or "%" in value:
        raise Refused(400, "Invalid request")
    host, separator, raw_port = value.partition(":")
    if not HOST.fullmatch(host) or not host.rstrip(".") or (separator and (len(raw_port) > 5 or not raw_port.isascii() or not raw_port.isdecimal())):
        raise Refused(400, "Invalid request")
    port = int(raw_port) if separator else default_port
    if port is None or not 1 <= port <= 65535:
        raise Refused(400, "Invalid request")
    return host.lower().rstrip("."), port


def read_header(sock):
    """Peek until complete, then consume only the header, with an absolute deadline."""
    deadline = time.monotonic() + HEADER_TIMEOUT
    while True:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise Refused(400, "Invalid request")
        sock.settimeout(remaining)
        try:
            raw = sock.recv(HEADER_LIMIT + 1, socket.MSG_PEEK)
        except TimeoutError:
            raise Refused(400, "Invalid request") from None
        if not raw:
            raise Refused(400, "Invalid request")
        end = raw.find(b"\r\n\r\n")
        if end >= 0:
            size = end + 4
            if size > HEADER_LIMIT:
                raise Refused(400, "Invalid request")
            chunks = bytearray()
            while len(chunks) < size:
                sock.settimeout(max(0.001, deadline - time.monotonic()))
                chunk = sock.recv(size - len(chunks))
                if not chunk or time.monotonic() > deadline:
                    raise Refused(400, "Invalid request")
                chunks.extend(chunk)
            return bytes(chunks)
        if len(raw) >= HEADER_LIMIT:
            raise Refused(400, "Invalid request")
        # MSG_PEEK leaves readable bytes queued; avoid a busy loop on partial headers.
        time.sleep(min(0.01, remaining))


def public_request(raw):
    lines = raw[:-4].split(b"\r\n")
    parts = lines[0].split(b" ")
    if len(parts) != 3 or not TOKEN.fullmatch(parts[0]) or parts[2] not in (b"HTTP/1.0", b"HTTP/1.1"):
        raise Refused(400, "Invalid request")
    try:
        destination = parts[1].decode("ascii")
    except UnicodeError:
        raise Refused(400, "Invalid request") from None
    headers = []
    hop = {b"connection", b"keep-alive", b"te", b"trailer", b"upgrade", b"host", b"content-length"}
    lengths = []
    for line in lines[1:]:
        name, colon, value = line.partition(b":")
        if not colon or not TOKEN.fullmatch(name) or any(c < 32 and c != 9 or c == 127 for c in value):
            raise Refused(400, "Invalid request")
        lower, value = name.lower(), value.strip(b" \t")
        if lower == b"transfer-encoding":
            raise Refused(411, "Content-Length required")
        if lower == b"content-length":
            if not value.isdigit() or len(value) > 20:
                raise Refused(400, "Invalid request")
            lengths.append(int(value))
        if lower == b"connection":
            hop.update(v.strip().lower() for v in value.split(b","))
        headers.append((name, lower, value))
    if len(lengths) > 1:
        raise Refused(400, "Invalid request")
    if parts[0] == b"CONNECT":
        host, port = authority(destination)
        return host, port, None, None
    try:
        url = urlsplit(destination)
    except ValueError:
        raise Refused(400, "Invalid request") from None
    if url.scheme != "http" or not url.netloc or url.fragment:
        raise Refused(400, "Invalid request")
    host, port = authority(url.netloc, 80)
    path = (url.path or "/") + (("?" + url.query) if url.query else "")
    if any(ord(c) <= 32 or ord(c) == 127 for c in path):
        raise Refused(400, "Invalid request")
    output = [parts[0] + b" " + path.encode("ascii") + b" " + parts[2]]
    output.extend(name + b": " + value for name, lower, value in headers if lower not in hop and not lower.startswith(b"proxy-"))
    if lengths:
        output.append(b"Content-Length: " + str(lengths[0]).encode("ascii"))
    output += [b"Host: " + (host + (":" + str(port) if port != 80 else "")).encode("ascii"), b"Connection: close", b"", b""]
    return host, port, b"\r\n".join(output), lengths[0] if lengths else 0


class Counters:
    def __init__(self, path):
        self.path, self.lock = Path(path), threading.Lock()
        self.values, self.written = dict.fromkeys(PUBLIC_KEYS, 0), None

    def add(self, key, count=1):
        with self.lock:
            self.values[key] += count

    def flush(self):
        with self.lock:
            values = dict(self.values)
            if values == self.written:
                return
            raw = json.dumps(values, sort_keys=True, separators=(",", ":")).encode()
            temporary = self.path.with_suffix(".tmp")
            with open(temporary, "wb") as stream:
                stream.write(raw)
                stream.flush()
                os.fsync(stream.fileno())
            os.replace(temporary, self.path)
            self.written = values


def relay(client, upstream, counters, response_started, body_remaining=None):
    """Bounded streaming, with half-close and an idle (not lifetime) timeout."""
    sockets = (client, upstream)
    buffers = {client: bytearray(), upstream: bytearray()}
    eof = {client: body_remaining == 0, upstream: False}
    shut = {client: False, upstream: False}
    for sock in sockets:
        sock.setblocking(False)
    last = time.monotonic()
    with selectors.DefaultSelector() as selector:
        while True:
            # EOF from one peer is propagated only after its queued data is delivered.
            for source, destination in ((client, upstream), (upstream, client)):
                if eof[source] and not buffers[destination] and not shut[destination]:
                    try:
                        destination.shutdown(socket.SHUT_WR)
                    except OSError:
                        pass
                    shut[destination] = True
            if eof[upstream] and not buffers[client] and (body_remaining is not None or eof[client] and not buffers[upstream]):
                return
            remaining = IDLE_TIMEOUT - (time.monotonic() - last)
            if remaining <= 0:
                return
            for key in list(selector.get_map().values()):
                selector.unregister(key.fileobj)
            room = BUFFER_LIMIT - sum(map(len, buffers.values()))
            for source, destination in ((client, upstream), (upstream, client)):
                mask = (selectors.EVENT_READ if not eof[source] and room > 0 else 0) | (selectors.EVENT_WRITE if buffers[source] else 0)
                if mask:
                    selector.register(source, mask, destination)
            if not selector.get_map():
                return
            for key, mask in selector.select(min(remaining, 1)):
                source, destination = key.fileobj, key.data
                if mask & selectors.EVENT_WRITE:
                    try:
                        count = source.send(buffers[source])
                    except BlockingIOError:
                        count = 0
                    if count:
                        del buffers[source][:count]
                        counters.add("bytes_down" if source is client else "bytes_up", count)
                        last = time.monotonic()
                if mask & selectors.EVENT_READ:
                    room = BUFFER_LIMIT - sum(map(len, buffers.values()))
                    size = min(65536, room)
                    if source is client and body_remaining is not None:
                        size = min(size, body_remaining)
                    if size <= 0:
                        continue
                    try:
                        chunk = source.recv(size)
                    except BlockingIOError:
                        continue
                    if not chunk:
                        eof[source] = True
                    else:
                        if source is upstream:
                            response_started[0] = True
                        buffers[destination].extend(chunk)
                        last = time.monotonic()
                        if source is client and body_remaining is not None:
                            body_remaining -= len(chunk)
                            if body_remaining == 0:
                                eof[client] = True


class PublicHandler(socketserver.BaseRequestHandler):
    def handle(self):
        counters = self.server.counters
        counters.add("connections")
        upstream, started = None, [False]
        try:
            host, port, header, remaining = public_request(read_header(self.request))
            upstream = open_upstream(host, port)
            if header is None:
                self.request.sendall(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                started[0] = True
            else:
                counters.add("http_requests")
                upstream.sendall(header)
                counters.add("bytes_up", len(header))
            relay(self.request, upstream, counters, started, remaining)
        except (Refused, OSError, ValueError) as error:
            if isinstance(error, Refused):
                code, reason = error.code, error.reason
            else:
                code, reason = 502, "Could not reach that site"
            counters.add("blocked_private" if code == 403 else "failed_connect" if code == 502 else "refused_invalid")
            if not started[0]:
                try:
                    self.request.settimeout(5)
                    self.request.sendall(f"HTTP/1.1 {code} {reason}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n".encode())
                except OSError:
                    pass
        finally:
            if upstream is not None:
                upstream.close()


class PublicServer(Server):
    request_queue_size = 256

    def __init__(self, address, counters):
        self.slots = threading.BoundedSemaphore(256)
        self.counters = counters
        super().__init__(address, PublicHandler)

    def process_request(self, request, address):
        if not self.slots.acquire(blocking=False):
            try:
                request.settimeout(1)
                request.sendall(b"HTTP/1.1 503 Too many connections\r\nConnection: close\r\nContent-Length: 0\r\n\r\n")
            except OSError:
                pass
            self.shutdown_request(request)
            return
        try:
            super().process_request(request, address)
        except BaseException:
            self.slots.release()
            raise

    def process_request_thread(self, request, address):
        try:
            super().process_request_thread(request, address)
        finally:
            self.slots.release()


def main():
    global EXTRA
    mode = os.environ.get("SPARKDEV_PROXY_MODE")
    if mode is None:
        with Server(("0.0.0.0", 3128), Handler) as server:
            server.serve_forever()
        return 0
    if mode != "public":
        return 2
    try:
        EXTRA = load_extra("/sparkdev-deny.json")
    except (OSError, ValueError, TypeError):
        return 3
    counters = Counters("/sparkdev-egress/counters.json")
    stopping = threading.Event()
    def flush_loop():
        while not stopping.wait(5):
            counters.flush()
    def stop(_signum, _frame):
        counters.flush()
        raise SystemExit(0)
    signal.signal(signal.SIGTERM, stop)
    threading.Thread(target=flush_loop, daemon=True).start()
    try:
        with PublicServer(("0.0.0.0", 3128), counters) as server:
            server.serve_forever()
    finally:
        stopping.set()
        counters.flush()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
