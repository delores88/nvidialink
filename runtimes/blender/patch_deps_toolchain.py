#!/usr/bin/env python3
"""Ubuntu 24.04 toolchain compatibility for Blender v4.5.9's `make deps` superbuild.

Blender sets CMake policy CMP0135 to NEW, so ExternalProject stamps every extracted
file with the extraction time instead of the archive's time. Files extracted later in
an archive then look newer than the generated autotools files that ship with it, and
`make` re-runs automake/aclocal through the package's `missing` script.

Blender's Linux builder (Rocky 8) has libtool 2.4.6, so that regeneration happens to
match the ltmain.sh the tarballs ship. Ubuntu 24.04 has libtool 2.4.7 and the
regenerated aclocal.m4 no longer matches, e.g. (attempt 2, sqlite-autoconf-3450100):

  libtool: Version mismatch error.  This is libtool 2.4.6 Debian-2.4.6-15build2, but the
  libtool: definition of this LT_INIT comes from libtool 2.4.7.

Setting CMP0135 to OLD keeps the archive timestamps, which is how a plain `tar xf` of a
release tarball behaves, so nothing is regenerated. Hashes and sources are unchanged.
"""
import pathlib
import sys

OLD = "  cmake_policy(SET CMP0135 NEW) # CMake 3.24+ Set the date/time for extracted files to time of extraction\n"
NEW = ("  # PhaseForge: keep archive timestamps so shipped autotools output is not regenerated\n"
       "  # with Ubuntu 24.04's libtool 2.4.7 (see build/patch_deps_toolchain.py).\n"
       "  cmake_policy(SET CMP0135 OLD)\n")


def main(env_dir):
    path = pathlib.Path(env_dir) / "CMakeLists.txt"
    text = path.read_text()
    if text.count(OLD) != 1:
        sys.exit("patch_deps_toolchain: CMP0135 policy line not found exactly once")
    path.write_text(text.replace(OLD, NEW))
    print("patch_deps_toolchain: CMP0135 set to OLD (archive timestamps kept)")


if __name__ == "__main__":
    main(sys.argv[1])
