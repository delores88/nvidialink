#!/usr/bin/env python3
"""Skip ISPC's 32-bit ARM (armv7) C++ builtins when building on linux-aarch64.

ISPC v1.21.1 with ARM_ENABLED compiles builtins/builtins-c-cpu.cpp for linux/armv7 as
well as linux/aarch64. That compile includes the system <unistd.h> for a 32-bit ARM
target, which needs a 32-bit ARM glibc (bits/wordsize.h for arm-linux-gnueabihf) that an
aarch64 Ubuntu does not have (attempt 4):

  /usr/include/features-time64.h:20:10: fatal error: 'bits/wordsize.h' file not found
  make[6]: *** [CMakeFiles/ispc.dir/build.make:231: builtins-cpp-32-linux-armv7.cpp] Error 1

Blender's own patches/ispc.diff already skips the equivalent 32-bit x86 (i686) builtins
"due to build issues on Linux and being unnecessary". This does the same for 32-bit ARM on
Linux. OIDN and Embree only use ISPC's 64-bit NEON targets on aarch64.

Usage:
  patch_deps_ispc.py <build_environment dir>     chain this script into ISPC's PATCH_COMMAND
  patch_deps_ispc.py --apply <ispc source dir>   (run by that PATCH_COMMAND) edit ISPC
"""
import pathlib
import sys

SELF = "/build-scripts/patch_deps_ispc.py"

CMAKE_ANCHOR = "    ${BUILD_DIR}/ispc/src/external_ispc <\n    ${PATCH_DIR}/ispc.diff\n"
CMAKE_ADDITION = ("    ${BUILD_DIR}/ispc/src/external_ispc <\n    ${PATCH_DIR}/ispc.diff &&\n"
                  "    python3 " + SELF + " --apply ${BUILD_DIR}/ispc/src/external_ispc\n")

ISPC_ANCHOR = ('    # OS to arch constrains\n'
               '    if (${os_name} STREQUAL "windows" AND ${arch} STREQUAL "arm" AND "${bit}" STREQUAL "32")\n')
ISPC_INSERT = ('    # PhaseForge: no 32-bit ARM sysroot on a linux-aarch64 host (like Blender skips i686).\n'
               '    if (${os_name} STREQUAL "linux" AND ${arch} STREQUAL "arm" AND "${bit}" STREQUAL "32")\n'
               '        set(SKIP ON)\n'
               '    endif()\n')


def chain(env_dir):
    path = pathlib.Path(env_dir) / "cmake" / "ispc.cmake"
    text = path.read_text()
    if text.count(CMAKE_ANCHOR) != 1:
        sys.exit("patch_deps_ispc: ispc.cmake PATCH_COMMAND anchor not found exactly once")
    path.write_text(text.replace(CMAKE_ANCHOR, CMAKE_ADDITION))
    print("patch_deps_ispc: armv7 skip chained into ISPC PATCH_COMMAND")


def apply(src_dir):
    path = pathlib.Path(src_dir) / "cmake" / "GenerateBuiltins.cmake"
    text = path.read_text()
    if text.count(ISPC_ANCHOR) != 1:
        sys.exit("patch_deps_ispc: GenerateBuiltins.cmake anchor not found exactly once")
    path.write_text(text.replace(ISPC_ANCHOR, ISPC_INSERT + ISPC_ANCHOR))
    print("patch_deps_ispc: linux/armv7 builtins skipped")


if __name__ == "__main__":
    if sys.argv[1] == "--apply":
        apply(sys.argv[2])
    else:
        chain(sys.argv[1])
