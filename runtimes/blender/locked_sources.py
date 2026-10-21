#!/usr/bin/env python3
"""Prepare fixed Blender inputs; HTTPS, SHA256, size bounds, no upstream fallback."""
import argparse
import bz2
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import tarfile
import tempfile
import urllib.parse
import urllib.request

LOCK = Path(__file__).with_name("inputs.lock.json")
HOSTS = {"codeload.github.com", "projects.blender.org", "files.pythonhosted.org"}


def check_url(url):
    parsed = urllib.parse.urlsplit(url)
    if parsed.scheme != "https" or parsed.hostname not in HOSTS or parsed.username or parsed.password or parsed.port or parsed.query or parsed.fragment:
        raise ValueError("Blender download is outside the fixed HTTPS hosts")


class Redirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, request, response, code, message, headers, url):
        check_url(url)
        return super().redirect_request(request, response, code, message, headers, url)


def relative(name):
    path = PurePosixPath(name)
    if not name or path.is_absolute() or ".." in path.parts or "\\" in name or ":" in name or path.as_posix() != name:
        raise ValueError("Unsafe Blender input path")
    return path


def validate(lock):
    if lock.get("v") != 1 or len(lock.get("sources", [])) != 2 or len(lock.get("dependencies", [])) != 66 or len(lock.get("python_wheels", [])) != 9 or len(lock.get("lfs_assets", [])) != 198:
        raise ValueError("Unexpected Blender input inventory")
    for group in ("sources", "dependencies", "python_wheels", "lfs_assets"):
        names = set()
        for row in lock[group]:
            check_url(row["url"])
            name = row["path"] if group == "lfs_assets" else row["filename"]
            relative(name)
            if name in names or not re.fullmatch(r"[0-9a-f]{64}", row["sha256"]) or type(row["bytes"]) is not int or not 0 < row["bytes"] <= 512 * 1024**2:
                raise ValueError("Invalid Blender hash/size or duplicate input")
            names.add(name)
    return lock


def digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def fetch(row, destination):
    """Retrying a build reuses only fully verified input bytes."""
    check_url(row["url"])
    destination = Path(destination)
    if any(p.is_symlink() for p in (destination, *destination.parents)):
        raise ValueError("Blender download destination is a symlink")
    if destination.exists():
        if destination.stat().st_size != row["bytes"] or digest(destination) != row["sha256"]:
            raise ValueError("Cached Blender input differs from SHA256 lock")
        return destination
    destination.parent.mkdir(parents=True, exist_ok=True)
    fd, temporary = tempfile.mkstemp(prefix=".download-", dir=destination.parent)
    try:
        h, count = hashlib.sha256(), 0
        with os.fdopen(fd, "wb") as out, urllib.request.build_opener(Redirect()).open(row["url"], timeout=90) as response:
            while chunk := response.read(1024 * 1024):
                count += len(chunk)
                if count > row["bytes"]:
                    raise ValueError("Blender download exceeds locked size")
                h.update(chunk)
                out.write(chunk)
            out.flush()
            os.fsync(out.fileno())
        if count != row["bytes"] or h.hexdigest() != row["sha256"]:
            raise ValueError("Blender download differs from SHA256 lock")
        os.chmod(temporary, 0o644)
        os.replace(temporary, destination)
        return destination
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


def unpack(archive, prefix, destination):
    relative(prefix)
    destination = Path(destination)
    if destination.exists():
        raise ValueError("Source extraction requires a new destination")
    destination.mkdir(parents=True)
    with tarfile.open(archive, "r:gz") as source:
        members = []
        for entry in source:
            if entry.name == prefix or entry.name == prefix + "/":
                continue
            if not entry.name.startswith(prefix + "/"):
                raise ValueError("Source archive root differs from its lock")
            entry.name = relative(entry.name[len(prefix) + 1:]).as_posix()
            members.append(entry)
        source.extractall(destination, members=members, filter="data")


def sources(lock, blender, optix, cache):
    for row, destination in zip(lock["sources"], (blender, optix)):
        archive = fetch(row, cache / row["filename"])
        unpack(archive, row["prefix"], destination)
    # Verify each pointer before replacing it; this binds runtime data to the
    # selected source archive rather than trusting an unrelated path inventory.
    for row in lock["lfs_assets"]:
        target = blender.joinpath(*relative(row["path"]).parts)
        expected = f"version https://git-lfs.github.com/spec/v1\noid sha256:{row['sha256']}\nsize {row['bytes']}\n".encode()
        if target.read_bytes() != expected:
            raise ValueError("Blender LFS pointer differs from the selected source")
        payload = fetch(row, cache / "lfs" / row["sha256"]).read_bytes()
        validate_data(row, payload)
        target.write_bytes(payload)
        target.chmod(0o644)


def validate_data(row, data):
    """Bounded format checks supplement exact byte locks, never execute assets."""
    name = row["path"]
    if len(data) != row["bytes"] or hashlib.sha256(data).hexdigest() != row["sha256"]:
        raise ValueError("Blender runtime data differs from SHA256 lock")
    if name.startswith("release/datafiles/icons/") and name.endswith(".dat"):
        # BKE_icon_geom_from_memory: eight-byte VCO header, then packed triangle
        # coordinates/colors in multiples of 18 bytes. These are not models.
        if len(data) <= 8 or data[:8] != b"VCO\x00\xff\xff\x00\x00" or (len(data) - 8) % 18:
            raise ValueError("Unexpected Blender geometry icon format")
    elif name == "release/datafiles/DejaVuSans-Lite.sfd.bz2":
        # Bzip2 is a single compressed font stream, not a path-bearing archive.
        # Bound decompression and reject concatenated streams/trailing payloads.
        decoder = bz2.BZ2Decompressor()
        plain = decoder.decompress(data, max_length=802526)
        if (not decoder.eof or decoder.unused_data or len(plain) != 802525
                or not plain.startswith(b"SplineFontDB: 1.0\nFontName: DejaVuSans\n")
                or not plain.endswith(b"EndSplineFont\n")
                or hashlib.sha256(plain).hexdigest() != "3bedb8bfdd95e69cd9b7b96e15652ca7eccd23883c9e1eb49755d955fc7fb020"):
            raise ValueError("Unexpected compressed Blender font contents")
    elif name.startswith("release/datafiles/studiolights/") and name.endswith(".exr"):
        if not data.startswith(b"\x76\x2f\x31\x01"):
            raise ValueError("Unexpected Blender studio-light image format")
    elif name == "release/datafiles/splash.png":
        if not data.startswith(b"\x89PNG\r\n\x1a\n"):
            raise ValueError("Unexpected Blender splash image format")
    elif name.endswith(".blend"):
        # Some upstream assets are Zstandard-compressed. Exact locked content
        # is separately inspected in Blender with autoexec/use_scripts disabled.
        if not (data.startswith(b"BLENDER") or data.startswith(b"\x28\xb5\x2f\xfd")):
            raise ValueError("Unexpected Blender asset format")
    else:
        raise ValueError("Unclassified Blender runtime data")


def dependencies(lock, env_dir, packages):
    cmake = env_dir / "cmake"
    original = (cmake / "download.cmake").read_text()
    calls = re.findall(r"^download_source\(([A-Z0-9_]+)\)", original, re.M)
    if calls != [r["name"] for r in lock["dependencies"]]:
        raise ValueError("Pruned Blender dependencies differ from the locked inventory")
    checks, overrides = [], []
    for row in lock["dependencies"]:
        if not re.fullmatch(r"[A-Z0-9_]+", row["name"]):
            raise ValueError("Invalid dependency identifier")
        fetch(row, packages / row["filename"])
        # ExternalProject reads these exact values too; no MD5/SHA1 fallback.
        overrides += [f"set({row['name']}_HASH {row['sha256']})", f"set({row['name']}_HASH_TYPE SHA256)"]
        checks += [f'file(SHA256 "${{PACKAGE_DIR}}/{row["filename"]}" DGX_HASH)',
                   f'if(NOT DGX_HASH STREQUAL "{row["sha256"]}")',
                   f'  message(FATAL_ERROR "Locked source changed: {row["name"]}")', 'endif()']
    # Do not leave an executable upstream-download branch inside CMake.
    (cmake / "download.cmake").write_text("# DGXBridge: pre-fetched immutable HTTPS sources; no network fallback.\n" + "\n".join(checks) + "\n")
    with (cmake / "versions.cmake").open("a") as file:
        file.write("\n# DGXBridge SHA256 source locks\n" + "\n".join(overrides) + "\n")
    wheel_dir = packages / "python-wheels"
    requirements = []
    for row in lock["python_wheels"]:
        fetch(row, wheel_dir / row["filename"])
        requirements.append(f"{row['name']}=={row['version']} --hash=sha256:{row['sha256']}")
    (packages / "python-requirements.txt").write_text("\n".join(requirements) + "\n")
    site_file = cmake / "python_site_packages.cmake"
    text = site_file.read_text()
    pattern = r"  INSTALL_COMMAND \$\{PYTHON_BINARY\} -m pip install --no-cache-dir \$\{SITE_PACKAGES_EXTRA\}.*?  --no-binary :all:\n"
    command = ('  INSTALL_COMMAND ${PYTHON_BINARY} -m pip install --no-cache-dir --no-index --no-deps\n'
               '  --require-hashes --only-binary=:all: --find-links=${PACKAGE_DIR}/python-wheels\n'
               '  -r ${PACKAGE_DIR}/python-requirements.txt\n')
    text, count = re.subn(pattern, lambda _: command, text, flags=re.S)
    if count != 1:
        raise ValueError("Unexpected Blender Python package installer")
    site_file.write_text(text)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=("sources", "dependencies"))
    parser.add_argument("paths", nargs=2, type=Path)
    args = parser.parse_args()
    lock = validate(json.loads(LOCK.read_text()))
    if args.action == "sources":
        sources(lock, *args.paths, Path("/build/locked-sources"))
    else:
        dependencies(lock, *args.paths)


if __name__ == "__main__":
    main()
