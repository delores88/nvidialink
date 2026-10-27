# Provenance

DGXBridge first-party implementation is covered by the repository MIT license. On 2026-10-13 Ben Van Dorn explicitly required both DGXBridge (then named Spark Dev Bridge) and CodaClauda to be MIT projects with public repositories and Marketplace listings, following his explicit direction to reuse his PhaseForge code. That authorization covers the owner-authored portions copied into these projects; preserve copyright attribution. It does not change the PhaseForge repository or relicense third-party dependencies. Their respective licenses/notices remain in force.

Read-only design references: delores88/PhaseForge commit `ac30e04` (full commit to be recorded when vendored), `native/spark-link/docs/PROTOCOL.md`, `docs/SPARK_GPU_PLACEMENT.md`; laptop `D:/PhaseForge-claude/scripts/sparkbuild.py`, `spark-runtimes/pf-build`, `reviews/025/spark-probe.md`. The prototype supplied secret/path policy, Docker isolation and measured-host constraints. Do not copy its PhaseForge paths, resource names, unrestricted networking or mutable state.

Every copied source/runtime file must be added here with source revision or SHA-256, destination and modifications before release.

## 0.2.0 connection topology and updater

The connection visuals use owner-authorized PhaseForge code at exact commit `7a665907901bd797df0eedee961e0d83a41eeabf` (read-only reference `D:/PhaseForge-claude/wt/release-0250`). The three pure JavaScript modules below are copied unchanged into `ui/vendor/`. Hardware SVG geometry and scoped styles are adapted from SparkLinkVisuals into `ui/connection-visual.mjs` and `ui/style.css`, replacing React dependencies with vanilla DOM and mapping only observed DGXBridge connection state. No throughput, GPU utilization or relay route is invented. Reduced motion and hidden/offscreen suspension are preserved.

| Upstream file | SHA-256 |
| --- | --- |
| `frontend/src/lib/sparkLinkParticles.mjs` | `02d33a5c21f2c61304cacbe8bc9ff1fd17ca36182e9fbaec53b5af22a903cbcf` |
| `frontend/src/lib/sparkLinkMotion.mjs` | `0039f58c363517e128c325d536afe6672319202b3f2e7d06427e3fd3b361b2dd` |
| `frontend/src/lib/visible-render-loop.mjs` | `a54d199f5abcdf0770c19f0db1de9d7bd110942f2adc3fdfdb210314c559dab5` |
| `frontend/src/components/settings/SparkLinkVisuals.jsx` | `80612e9b827c2d76e1d0becbaaf318d208f037e124be80a4302e6f30bb21d9d1` |
| `frontend/src/components/settings/SparkLinkVisuals.module.css` | `a566bdf77d35f5ec63e17cc0b35fcfcb9558cc53a4ede97fdd030e1b9f3099c0` |

The drawer updater placement follows `frontend/src/components/shared/DesktopUpdate.jsx` and its stylesheet at the same pin. Native authorization and installer implementation provenance is recorded in [UPDATER-PROVENANCE.md](docs/UPDATER-PROVENANCE.md); it uses the current signed Marketplace contract and its own app identity. All first-party adaptations remain MIT under Ben Van Dorn / DeloresAI copyright.

## 0.6.2 network map

The Devices map uses three.js r170 (MIT, Copyright 2010-2024 three.js authors) from the npm package `three@0.170.0`, vendored under `ui/vendor/three/` with its license, which the installer ships as `notices/THREE-LICENSE.txt`. `build/three.module.min.js` is unchanged. `examples/jsm/controls/OrbitControls.js` is unchanged except that its `three` import names the local module.

The exact upstream commit is `beab9e845f9e5ae11d648f55b24a0e910b56a85a`, resolved independently from the annotated `mrdoob/three.js` tag `r170` and npm's `gitHead`. The npm archive's published SHA-512 integrity was checked before comparing both modules and the license. Only the documented local import substitution differs. The source inventory is retained in [the verification receipt](evidence/three-r170-source.json). Release preparation starts from exact DGXBridge/RTXBridge source commit `f186d3ec7568b476ad1efc63e1ccee975bb9ecf3`; subsequent release corrections and version metadata are recorded separately.

| Vendored file | SHA-256 |
| --- | --- |
| `ui/vendor/three/three.module.min.js` | `08fd7545d13d2c7fb65ab691530a802dafefd638596501854f267d0fb13c39e7` |
| `ui/vendor/three/OrbitControls.js` | `e9c4f26866d39a36c8c9e8215e7d2a90a8a0b50ea6d44a005906e3d1fc726b8f` |

The map replaces the 0.2.0 connection visual: `ui/connection-visual.mjs`, `sparkLinkParticles.mjs`, `sparkLinkMotion.mjs` and the adapted SparkLinkVisuals styles are removed. Only `visible-render-loop.mjs` from that pin remains in use.

`assets/screenshots/rtxbridge-062-devices-preview.png` is an unchanged browser-fixture capture of the 0.6.2 Devices view, with explicitly labeled sample device/job data in its gallery description. It is not an installed/native screenshot or evidence of the sample hardware or link rates. The historical installed 0.5.1 Jobs image retains its original provenance below and is labeled as preceding the rename.

## 0.5.0 Blender runtime

The read-only PhaseForge Blender 4.5.9 image and the owner-authorized recipe/support-file reuse, exact upstream commits, file hashes and third-party license boundaries are recorded in [PROVENANCE-REMOTE.md](PROVENANCE-REMOTE.md). DGXBridge uses a distinct runtime image and never changes the upstream image, repository or service.

## Multi-device AMD64 runtime sources

The AMD64 CPU recipe adapts DGXBridge's own `runtimes/Dockerfile` at exact upstream commit `6c25314c7b69a5fe73aa0ce317d8d6844afd4772`. The independent AMD64 CUDA recipe adapts the PyTorch/Pillow/media portions of `runtimes/Dockerfile.cuda` at that commit, omitting all ARM64 Blender inputs. The original ARM64 recipes, existing base pins, wheel locks, and Blender image identity are retained unchanged. No PhaseForge source, service, runtime, image or volume is modified by this work.

Public registry manifests and configuration blobs were fetched directly on 2026-10-27 from Docker's official registry and NVIDIA NGC; the SHA-256 of each fetched manifest body was checked against its digest. Architecture is also verified against the image configuration before selecting a base:

| Source | Verified identity |
| --- | --- |
| Docker official `library/rust:1.98.1-slim-trixie` multiarch index | `sha256:4cd829461bd5c4d511c32e269da9cb8929223b666519d8004e35fc8d1d771ab7` |
| Independent Linux AMD64 Rust child used by `Dockerfile.amd64` | `sha256:fce83543909cd5409924a61ce296bdf5bab0fca37f004c0f6c8ef6f4a37bbc59`; configuration `sha256:3807bfc7862f0e7e5f64fb10278b09a0c150fb05329afdf1b4878820eef226b8`, architecture `amd64` |
| Existing Linux ARM64 Rust child, retained | `sha256:441452f8033b93f69dce3a3ff5c821c411f0a580723e6343932fc34a1b03ba5d`; configuration `sha256:9aa8fafc925472ff98d14b89a5f4b97adf803fe20a68b2a0a1a22cb68bc6d324`, architecture `arm64` |
| NVIDIA `nvcr.io/nvidia/pytorch:25.09-py3` existing multiarch index, retained for Spark | `sha256:f3b4a33fd60d5e0358287bb44d57e91eda45e8f1681fef1bfd6c969371417130` |
| Independent Linux AMD64 PyTorch child used by `Dockerfile.cuda.amd64` | `sha256:4bf906c628d572681a977681765a44a3c1d7e34b39633b7160a84de707608139`; configuration `sha256:32e1d4fac739042fb889b92ff9678223212931986bdebdd2d836d97eceaaa0d8`, architecture `amd64`, `CUDA_VERSION=13.0.1.012`, `NVIDIA_PYTORCH_VERSION=25.09` |

The existing NGC base is a multiarch index, rather than an ARM64-only manifest. Its AMD64 child is pinned explicitly by the new recipe; no mutable tag is used for execution. Docker registry reference: `https://registry-1.docker.io/v2/library/rust/manifests/1.98.1-slim-trixie`. NVIDIA registry reference: `https://nvcr.io/v2/nvidia/pytorch/manifests/25.09-py3`. NVIDIA's platform/release reference is `https://docs.nvidia.com/deeplearning/frameworks/pytorch-release-notes/rel-25-09.html`. NVIDIA's container and bundled components retain their own licenses; they are not relicensed as DGXBridge MIT source.

The following unyanked wheel filenames and SHA-256 values were read from the package/version JSON returned directly by `https://pypi.org/pypi/numpy/2.4.6/json` and `https://pypi.org/pypi/pillow/13.0.0/json` on 2026-10-27. The CPU installer uses the first two through `requirements-amd64.txt`; the CUDA recipe uses the third with a hash-checked local wheel install and preserves NVIDIA's Torch stack:

| Artifact | SHA-256 |
| --- | --- |
| `numpy-2.4.6-cp313-cp313-manylinux_2_27_x86_64.manylinux_2_28_x86_64.whl` | `a7830bab239b79cda9c08c2da014761cafb48da6150e1da17ac06283f43b6089` |
| `pillow-13.0.0-cp313-cp313-manylinux_2_27_x86_64.manylinux_2_28_x86_64.whl` | `5f56499c1a74d2b862beb0c33b8016f99d095858455df9d21583bbbc1c15b582` |
| `pillow-13.0.0-cp312-cp312-manylinux_2_27_x86_64.manylinux_2_28_x86_64.whl` | `42d63be3af4f9b951b199b0a9d23365d37ab14687e6d77b74940432c8515c4ca` |

Blender is explicitly not provisioned by the AMD64 CUDA recipe. Its receipt reports that unsupported capability separately from observed CUDA compute and media probes. Architecture dispatch, source snapshots, image verification and registry activation fail closed on unsupported hosts, changed sources, cross-architecture images or failed probes. Source-only tests do not establish successful image builds, real RTX 4090 job execution, installation, remote-network behavior or Marketplace acceptance.

## Shared setup bundle validation

The validated raw-file bundle loader and its regression tests move from `src-tauri/src/setup_bundle.rs` to `crates/sparkdev/src/bundle.rs`. Original upstream is DGXBridge exact commit `6c25314c7b69a5fe73aa0ce317d8d6844afd4772`; the desktop now re-exports the shared loader. The closed file set includes the separately recorded AMD64 sources, retaining the all-or-none rule and existing size, hash and path checks. Named-computer provisioning uses that same validated bootstrap source through typed pinned SSH, omits the ARM64 paired-link executable and unit, disables relay pairing, and prepares GPU tools only after an explicit request. The existing Spark desktop bootstrap retains its own owner-controlled pairing behavior. No PhaseForge code was copied or changed by this relocation.

## 0.6.1 release gallery

`assets/screenshots/dgxbridge-051-installed.jpg` is the unchanged historical installed screenshot from DGXBridge artwork commit `fc809df1a4dcd50362ead0d4cedf47a457144fcc`, SHA256 `7152007f7e0a9d122c3e15bde9a9140925a21ff5d4f46d6b9a8c8f2df758e097`. It documents the older installed Jobs layout retained by this release; it is not a new 0.6.1 native capture. `assets/screenshots/dgxbridge-061-computers-preview.png` is a browser-rendered screenshot of this repository's unchanged Computers UI with explicitly labeled sample workstation data, SHA256 `d02f620a8108bbb42273d3dd53e00f92a778107cd67ff3e9e0c33663bd21dfc4`. Its UI source is unchanged from exact commit `27aacc9f50762a24f8fd6ade3d411d56add8c773`. The preview is not installed/native acceptance evidence. Neither image includes a pairing ticket, personal address, SSH path or private key.
