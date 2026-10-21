#!/usr/bin/env python3
"""Link OIDN 2.3.3's CUDA device against NVIDIA's static CUDA runtime (CUDA 13 fix).

OIDN 2.3.3 builds its CUDA device in "Driver" mode by default: instead of the CUDA
runtime it links `curtn`, a small shim that implements the runtime entry points nvcc's
host stubs call. With CUDA 13, nvcc's stubs call a new entry point, `__cudaLaunchKernel`,
which the 2.3.3 shim does not define (OIDN added it in v2.4.0: "Added support for
compilation with CUDA 13"). The module then fails to dlopen and OIDN silently reports no
CUDA device, so Cycles denoises on the CPU even with denoising_use_gpu=True. Measured in
the first image (BUILD_LOG.md, failure 7):

  dlopen: libOpenImageDenoise_device_cuda.so.2.3.3: undefined symbol: __cudaLaunchKernel
  Cycles: "Will denoise on Unknown CPU (CPU)" for a CUDA render with denoising_use_gpu

`-DOIDN_DEVICE_CUDA_API=RuntimeStatic` (an upstream OIDN option) links libcudart_static
from the same CUDA 13.0.1 toolkit instead, which defines every entry point nvcc 13 emits.
The runtime still needs only the driver's libcuda.so.1, which the NVIDIA container
toolkit injects. OIDN stays at Blender's pinned 2.3.3, so the denoiser matches the laptop.
"""
import pathlib
import sys

ANCHOR = ("  elseif(UNIX)\n"
          "    set(OIDN_EXTRA_ARGS\n"
          "      ${OIDN_EXTRA_ARGS}\n"
          "      -DOIDN_DEVICE_CUDA=ON)\n")
REPLACEMENT = ("  elseif(UNIX)\n"
               "    # PhaseForge: static CUDA runtime; 2.3.3's driver shim lacks CUDA 13's\n"
               "    # __cudaLaunchKernel (see build/patch_deps_oidn_cudart.py).\n"
               "    set(OIDN_EXTRA_ARGS\n"
               "      ${OIDN_EXTRA_ARGS}\n"
               "      -DOIDN_DEVICE_CUDA=ON\n"
               "      -DOIDN_DEVICE_CUDA_API=RuntimeStatic)\n")


def main(env_dir):
    path = pathlib.Path(env_dir) / "cmake" / "openimagedenoise.cmake"
    text = path.read_text()
    if text.count(ANCHOR) != 1:
        sys.exit("patch_deps_oidn_cudart: ARM64 OIDN CUDA anchor not found exactly once")
    path.write_text(text.replace(ANCHOR, REPLACEMENT))
    print("patch_deps_oidn_cudart: OIDN CUDA device uses RuntimeStatic")


if __name__ == "__main__":
    main(sys.argv[1])
