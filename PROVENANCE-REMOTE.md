# Remote controller provenance

The controller, scheduler, bootstrap, dependency proxy, supervisor and tests under
`remote/` are new DGXBridge code. Their behavioral design was informed by
PhaseForge Spark Link and the owner's read-only SSH prototype. No PhaseForge source
file was edited and no PhaseForge service, container, volume or image was mutated.

The pinned upstream protocol reference is `delores88/PhaseForge`, commit
`ac30e04fbf0ddc03ae0144fb06b6350cacdce1fc` (`claude/0.25-spark-p2`):

- `native/spark-link/docs/PROTOCOL.md`: authenticated bootstrap, digest registry,
  content-addressed chunk transfer, expiry, disconnect recovery, and admission ideas.
- `docs/SPARK_GPU_PLACEMENT.md`: runtime tokens, receipts, unified-memory limitations.

The read-only local prototype files are outside the pinned Git tree, so they cannot
honestly be attributed to that commit. The exact files inspected were:

| Reference | SHA-256 at inspection |
|---|---|
| `D:/PhaseForge-claude/scripts/sparkbuild.py` | `8c6ad28dc5cd81ef903fe9a27e524479e40d2ad357ef8a50759964631b1a5b3f` |
| `D:/PhaseForge-claude/spark-runtimes/pf-build/Dockerfile` | `231a10ae092494cd8fbfcea27ea3d954564697b0a59ef0a30dc8286e9d11bd09` |
| `D:/PhaseForge-claude/spark-runtimes/pf-build/requirements.txt` | `c0174d9d0f4037c0b5f44ebab369b9df0001a33696cf35f4422780a47c41b474` |

`runtimes/Dockerfile` adapts the prototype's Rust base digest, Linux development
packages and Python virtual environment; Node and Git were added and all product paths/tags
were moved to this product's separate `sparkdev` namespace. `runtimes/requirements.txt`
retains the prototype's NumPy 2.4.6 pin. Its original Pillow 12.3.0 pin was replaced
with the official Pillow 13.0.0 ARM64 wheel on 2026-10-21 after Marketplace rejected
0.4.1 for Pillow advisories, including critical GHSA-8g5g-8vmr-9782.
Source credential/path policy was independently implemented from the reviewed policy.

The [upstream advisory](https://github.com/python-pillow/Pillow/security/advisories/GHSA-8g5g-8vmr-9782)
identifies 13.0.0 as patched. Exact non-yanked wheels and SHA-256 values were checked
against [official PyPI release metadata](https://pypi.org/pypi/Pillow/13.0.0/json):

| Runtime interpreter | Official wheel | SHA-256 |
|---|---|---|
| CPU `/opt/pyenv/bin/python3` (CPython 3.13) | `pillow-13.0.0-cp313-cp313-manylinux_2_27_aarch64.manylinux_2_28_aarch64.whl` | `19b66ca0a4d25cc0200b381ce824c7ea090db61427315e73cfb05dbf1b35c369` |
| CUDA `/usr/bin/python3` (CPython 3.12) | `pillow-13.0.0-cp312-cp312-manylinux_2_27_aarch64.manylinux_2_28_aarch64.whl` | `8bf295fe459343bc6093c8e5e0df60bfa189e0fcb775a22e2c2488fdeccc6c0d` |

Pillow 13.0.0 requires Python 3.11 or later. A managed physical Spark probe of the
prior CUDA image confirmed `/usr/bin/python3` 3.12.3, Pillow 11.3.0 and NVIDIA torch
`2.9.0a0+50eac811a6.nv25.9`. Its Dockerfile fetches only the fixed official cp312
wheel and installs with `--require-hashes --no-index --no-deps`, using only the
downloaded wheel and retaining NVIDIA's existing dependencies. Both runtime
activation probes require the exact Pillow 13.0.0 version and a native PNG
decode/resize check before registration.

CPU base: `rust:1.98.1-slim-trixie@sha256:441452f8033b93f69dce3a3ff5c821c411f0a580723e6343932fc34a1b03ba5d`.
Optional CUDA base: `nvcr.io/nvidia/pytorch@sha256:f3b4a33fd60d5e0358287bb44d57e91eda45e8f1681fef1bfd6c969371417130`,
read from PhaseForge's runtime pin. Both are official third-party images. Docker
builds create new distinct `sparkdev/*` images and record the resulting immutable
image ID; they never retag PhaseForge images. The Spark cache may reuse existing
read-only base layers through Docker's normal content-addressed storage.

DGXBridge's MIT license covers its first-party code only. Rust, Debian,
Python, Node.js, Git, NumPy, Pillow and NVIDIA NGC/PyTorch retain their respective
upstream licenses and notices inside their images/packages. This repository contains
runtime recipes and hash pins, not redistributed third-party image layers. Installing
the optional NGC runtime downloads/uses the official image under its published terms.

`remote/build_linux_cli.py` uses the official build-only cargo-auditable 0.7.7
ARM64 archive, SHA-256
`bcb2d46002a8dd8e94d0d669cb39d38c9651db2ae5d1f3f1f1bab516ee1e212c`.
It verifies the extracted tool digest as well. The release URL and checksum binding
are recorded in `evidence/link/2026-10-13-arm64-auditable.json`. The tool is not
redistributed with DGXBridge; compiled binary dependency metadata identifies
the libraries actually linked into each release.

## 0.5.0 Blender and GPU video

`runtimes/Dockerfile.blender` and `runtimes/blender/` adapt the owner's read-only
PhaseForge Blender build artifact. That artifact resides outside the PhaseForge
Git checkout, so no PhaseForge commit is claimed. `runtimes/blender/PROVENANCE.json`
records every original file's SHA-256 and the narrow adaptations (two compiler
workers, flattened bootstrap paths, separate `sparkdev` image name). Blender is
fetched as a SHA256-locked source archive at upstream commit `8bf95cbd38d1456c94e9d751f3804eea4d5b137b`
(`v4.5.9`); NVIDIA OptiX headers are pinned at
`f60c1e44f18426f426a2ed948f28515b3cf67b8a` (`v8.0.0`). The archive SHA256 values are
`41e8682083bd096cb25ee6c5ab2e484351030af90919b4c38415fddff0a5cf8a` (Blender) and
`eca772dfede56ba073a1048ecf43de5af9c4e13a537222a21ee95ffa2164c5f6` (OptiX), computed
from the actual upstream downloads. These third-party
components retain their own licenses; this repository ships recipes and patches,
not Blender or NVIDIA image layers.

`runtimes/blender/inputs.lock.json` records all 66 dependency archives, nine
universal Python wheels at Blender's existing versions, and 198 runtime-data
paths backed by 187 unique Git LFS objects. The dependency SHA256 values and sizes
come from Git LFS pointer blobs verified against mirror tree
`15315f480287ffb8b7de857cadb590680874b5dc`. The original CMake hashes were 39 MD5,
26 SHA256 and one SHA1; the helper upgrades every archive check to SHA256 rather
than relabeling those hashes. It downloads only HTTPS content-addressed mirror
objects, replaces CMake's downloader with local checks, and installs Python
wheels offline with `--require-hashes --no-deps`. There is no HTTP or upstream
source fallback. LFS pointer bytes are checked against the selected Blender
source before hydration; no runtime assets are removed. The original upstream
versions and license metadata remain in the lock and source archive.

The manifest declares 84 fixed executable/build inputs including these archives
and wheels. All 187 distinct runtime/source data objects were downloaded and
verified against their SHA256 and size (19,160,355 bytes). The helper rehashes
cache hits and fails on a mismatch. The 147 `.dat` files are packed VCO geometry
icons consumed by `BKE_icon_geom_from_memory` in the pinned Blender source, not
model weights. The font is one bzip2 stream containing an 802,525-byte
SplineFontDB document. Decompression is bounded; its exact decoded hash, header
and terminator are checked, and trailing streams/content are rejected. PNG and
EXR signatures are checked alongside the exact byte locks.

Physical Spark job `17e6fcbf0fc44ba8a05b2a981e1b3abf` opened all 18 `.blend` assets
using Blender 4.5.9 LTS with `--disable-autoexec` and
`open_mainfile(load_ui=False, use_scripts=False)`. It observed nine identical,
non-module README Text blocks, zero drivers and zero OSL script nodes. The
380-byte README was reconstructed locally and matched to its observed SHA256;
it contains brush editing guidance, not Python. Sequential loads retained three
bundled brush library references also represented among the inspected assets.
The inspector did not invoke texts, drivers or rendering. Scene/node data still
describes built-in Blender behavior. Exact per-path hashes, consumer evidence,
data notices and inspection receipt hashes are in `inputs.lock.json`.

This is source/data evidence, not a Marketplace scanner certificate, completed
fresh fallback build, automatic-return test or complete license/security review.
Signed apt repositories remain moving setup inputs. The standalone recipe has
source-preparation and unit-test evidence; a fresh build is a separate gate.

The installer can reuse existing immutable image
`sha256:69504fa0aa66263511d356be871df4ab56c8332ea629525fbb832a7249abff62`
as a read-only COPY source. It never retags or modifies that image. If absent,
it builds its own `sparkdev/blender:4.5.9-arm64` with two CPUs and 8 GiB memory,
records the immutable result and copies Blender into its own CUDA image. The
source build uses pinned `nvidia/cuda:13.0.1-devel-ubuntu24.04` digest
`5c36750138dc1447a17dafbb397674f167d3b44ce18d9160d769df114577b35d` and Ubuntu
24.04 digest `08571ca13e00ca07a2a84eab83a959b4242e22cceb16486a11bef1428c9e93a7`.
Both paths provide the same Blender version, sm_121 CUDA kernel, OptiX and OIDN.
The activation probe checks these capabilities; separate admitted ten-frame
renders remain the physical acceptance gate.

CUDA setup additionally installs Ubuntu's signed FFmpeg package and Blender
system libraries. Package versions are resolved at setup before the final image
ID is pinned; this is not a reproducible package lock. The Pillow wheel install
retains `--no-index --require-hashes --no-deps`. CUDA image setup now requires
network for system packages; normal jobs still run as non-root with unchanged
network policy. `video` joins the NVIDIA driver capabilities so encode/decode
libraries are mounted. [NVIDIA's Spark hardware documentation](https://docs.nvidia.com/dgx/dgx-spark/hardware.html)
lists NVENC/NVDEC, but DGXBridge reports only its cached physical FFmpeg probes:
encoder enumeration, actual ten-frame encode/verification and explicit CUVID
decode are separate fields. See [NVIDIA's FFmpeg guidance](https://docs.nvidia.com/video-technologies/video-codec-sdk/13.0/ffmpeg-with-nvidia-gpu/index.html).
