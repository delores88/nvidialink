# Remote controller provenance

The controller, scheduler, bootstrap, dependency proxy, supervisor and tests under
`remote/` are new Spark Dev Bridge code. Their behavioral design was informed by
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
packages and Python virtual environment; Node was added and all product paths/tags
were changed to Spark Dev Bridge. `runtimes/requirements.txt` copies the exact two
version/hash pins for NumPy 2.4.6 and Pillow 12.3.0, with a new provenance comment.
Source credential/path policy was independently implemented from the reviewed policy.

CPU base: `rust:1.98.1-slim-trixie@sha256:441452f8033b93f69dce3a3ff5c821c411f0a580723e6343932fc34a1b03ba5d`.
Optional CUDA base: `nvcr.io/nvidia/pytorch@sha256:f3b4a33fd60d5e0358287bb44d57e91eda45e8f1681fef1bfd6c969371417130`,
read from PhaseForge's runtime pin. Both are official third-party images. Docker
builds create new distinct `sparkdev/*` images and record the resulting immutable
image ID; they never retag PhaseForge images. The Spark cache may reuse existing
read-only base layers through Docker's normal content-addressed storage.

Spark Dev Bridge's MIT license covers its first-party code only. Rust, Debian,
Python, Node.js, NumPy, Pillow and NVIDIA NGC/PyTorch retain their respective
upstream licenses and notices inside their images/packages. This repository contains
runtime recipes and hash pins, not redistributed third-party image layers. Installing
the optional NGC runtime downloads/uses the official image under its published terms.
