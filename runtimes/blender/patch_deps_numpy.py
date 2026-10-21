#!/usr/bin/env python3
"""Build NumPy 1.26.4 single-threaded in Blender v4.5.9's `make deps`.

Blender builds NumPy with `setup.py build -j${PYTHON_MAKE_THREADS}` (18 here). In NumPy
1.26's distutils build, the `numpy.linalg.lapack_lite` and `numpy.linalg._umath_linalg`
extensions both compile the same bundled lapack_lite C files to the same object paths
(build/temp.*/numpy/linalg/lapack_lite/*.o). Built in parallel they race, and one
extension can link a half-written object. Measured (BUILD_LOG.md, failure 8): two builds
of the identical Dockerfile gave identical link command lines, but in the --no-cache build

  import numpy -> _umath_linalg.cpython-311-aarch64-linux-gnu.so: undefined symbol: zgesdd_

and the module was 1,392,584 bytes instead of 1,655,664. `import numpy` itself fails, so
every PhaseForge Blender script would fail. `-j1` removes the race. The Dockerfile also
checks NumPy's LAPACK paths right after the build.
"""
import pathlib
import sys

OLD = "      build ${NUMPY_BUILD_OPTION} -j${PYTHON_MAKE_THREADS}\n"
NEW = ("      # PhaseForge: -j1, the parallel distutils build races on shared lapack_lite objects.\n"
       "      build ${NUMPY_BUILD_OPTION} -j1\n")


def main(env_dir):
    path = pathlib.Path(env_dir) / "cmake" / "numpy.cmake"
    text = path.read_text()
    if text.count(OLD) != 1:
        sys.exit("patch_deps_numpy: numpy build line not found exactly once")
    path.write_text(text.replace(OLD, NEW))
    print("patch_deps_numpy: NumPy builds with -j1")


if __name__ == "__main__":
    main(sys.argv[1])
