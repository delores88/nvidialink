#!/usr/bin/env python3
"""Make Blender v4.5.9's OpenImageDenoise dependency build with the CUDA 13 toolkit.

OIDN 2.3.3 (devices/cuda/CMakeLists.txt) compiles cubins for sm_70 (Volta). CUDA 13
removed Maxwell, Pascal and Volta targets: `nvcc --list-gpu-code` on the Spark's
13.0.88 toolkit starts at sm_75 (measured in reviews/025/spark-probe.md). This adds a
step to Blender's OIDN PATCH_COMMAND that drops 70 from the cubin list and compiles the
Volta-only CUTLASS file for sm_75 instead (it is dispatched only on SM 7.0 devices, so
on GB10 it is dead code). GB10 (SM 12.1) runs OIDN's sm_120 cubin by same-major binary
compatibility; OIDN's own isSupported() accepts SM 120-129.
The edit is checked with grep so a changed upstream file fails loudly.
"""
import pathlib
import sys

ANCHOR = "set(ODIN_PATCH_COMMAND\n  ${PATCH_CMD} --verbose -p 1 -N -d\n  ${BUILD_DIR}/openimagedenoise/src/external_openimagedenoise <\n  ${PATCH_DIR}/oidn.diff\n)\n"
ADDITION = r'''
# PhaseForge: CUDA 13 has no sm_70 target (see build/patch_deps_cuda13.py).
set(ODIN_PATCH_COMMAND ${ODIN_PATCH_COMMAND} &&
  sed -i
    -e "s/oidn_set_cuda_sm_flags(OIDN_CUDA_SM_FLAGS 70 75 80 90 100 120)/oidn_set_cuda_sm_flags(OIDN_CUDA_SM_FLAGS 75 80 90 100 120)/"
    -e "s/oidn_set_cuda_sm_flags(OIDN_CUDA_SM70_FLAGS 70)/oidn_set_cuda_sm_flags(OIDN_CUDA_SM70_FLAGS 75)/"
    ${BUILD_DIR}/openimagedenoise/src/external_openimagedenoise/devices/cuda/CMakeLists.txt &&
  grep -q "OIDN_CUDA_SM_FLAGS 75 80 90 100 120)"
    ${BUILD_DIR}/openimagedenoise/src/external_openimagedenoise/devices/cuda/CMakeLists.txt &&
  grep -q "OIDN_CUDA_SM70_FLAGS 75)"
    ${BUILD_DIR}/openimagedenoise/src/external_openimagedenoise/devices/cuda/CMakeLists.txt
)
'''


def main(env_dir):
    path = pathlib.Path(env_dir) / "cmake" / "openimagedenoise.cmake"
    text = path.read_text()
    if text.count(ANCHOR) != 1:
        sys.exit("patch_deps_cuda13: OIDN patch-command anchor not found exactly once")
    path.write_text(text.replace(ANCHOR, ANCHOR + ADDITION))
    print("patch_deps_cuda13: OIDN sm_70 removal added to PATCH_COMMAND")


if __name__ == "__main__":
    main(sys.argv[1])
