#!/usr/bin/env python3
"""Fixed HTTPS CONNECT allowlist. Deploy only as the isolated dependency proxy.

No configurable targets, plain HTTP, credentials, redirects or private addresses.
TLS stays end-to-end with the package registry. Each preparation job has its own
internal Docker network; only this proxy also has an egress network.
"""
import ipaddress
import select
import socket
import socketserver
import threading
import time

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


if __name__ == "__main__":
    with Server(("0.0.0.0", 3128), Handler) as server:
        server.serve_forever()
