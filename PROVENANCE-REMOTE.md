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
wheel and installs with `--require-hashes --no-index --no-deps`, retaining the
network-disabled build step and NVIDIA's existing dependencies. Both runtime
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
