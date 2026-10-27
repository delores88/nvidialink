# Multi-computer candidate validation

Verified 2026-10-27 UTC on branch `codex/060-multidevice`, based on released commit `6c25314c7b69a5fe73aa0ce317d8d6844afd4772`. Implementation commits are `8e13d10`, `3a43827`, `e11f80e`, `9ec2cc8`, and `8564b48`; version and contract documentation are in `f1d2d7b`. This receipt distinguishes development, physical execution, native UI, and release acceptance. It contains no credentials or owner configuration.

## Source regression results

| Layer | Result |
| --- | --- |
| Windows Rust core | 224 library tests, 1 return-worker test, and 6 actual CLI subprocess tests passed |
| Spark Linux Rust core | 226 library tests, 1 return-worker test, and 6 actual CLI subprocess tests passed; workload exit 0 |
| Remote Python controller/runtime suite on Spark | 405 tests passed; workload exit 0 and automatic return complete |
| Windows desktop backend | 25 tests passed; development native executable built |
| UI model | 38 tests passed |
| Browser fixture | Full workflow checks passed at widths 1140 and 850; no renderer exceptions or horizontal overflow |
| Packaging scripts | 31 tests run, 2 platform-specific skips, no failures |

Every local Cargo invocation used the prescribed unchanged build gate and a C: target directory. Linux tests used separate capped `sparkdev` jobs, locked prepared dependencies, retained request/job IDs, bounded waits, byte-offset logs, and inspected final workload exit codes. No PhaseForge services or resources were used for these jobs.

Important source regressions cover immutable request routing; source-change refusal before upload; independent profiles and worker locks; uncertain submission recovery; claims retained during automatic result delivery; client quotas; explicit laptop/remote scope; strict SSH verification; WSL argument validation; the Windows SSH command-length boundary; bundle hashes and closed file sets; unsupported architecture/Blender rejection; discrete VRAM requirements; and bounded local response handling without changing the SSH wire limit.

UI regressions preserve all 165 original DOM IDs, original Spark/laptop panels, connection setup, pairing refusals, resources, drafts, updates, pagination, cancellation, theme, agent instructions, and passive result delivery. They add independent named-computer readiness, target-specific setup, qualified job IDs, and filtering. Failed refreshes stop claiming live readiness. Oversized local history displays unavailable details rather than Offline. When a failed named observation omits jobs from the board, prior rows remain explicitly Last seen; fresh records replace them, and healthy observations can remove old rows.

## Physical execution on the AMD64 Windows/WSL workstation

The verified environment has 64 logical CPUs, approximately 125 GiB visible Linux RAM, and an RTX 4090 with 24 GiB dedicated VRAM. Windows has approximately 256 GiB installed RAM; that is not the WSL memory capacity or GPU VRAM. SSH uses the independently supplied pinned ED25519 identity and key-only authentication. Additional-computer controller state and laptop development state are separate from the installed Spark profile.

| Acceptance | Evidence |
| --- | --- |
| CPU job and automatic binary return | `antonpc:05ee8bdce40c4272b23bc5605e51eb60`, exit 0, return complete, 2,097,152 bytes |
| Binary integrity | SHA256 `91d3beb88a9b2f778a6c44a1c53b63d3c79931845a9aef84b3fb414610bd1938`, independently checked on returned local bytes |
| Retry identity | Same request and original arguments returned the same qualified job; moving that request to Spark was refused |
| Concurrent local edit | Newer laptop `report.txt` remained intact; the remote edit returned beside it with conflict status |
| Cancellation after disabling new jobs | `antonpc:ba89e718ef3c4ab099bb29cd9f6d314d`, cancelled, exit 130, return complete |
| Parallel execution | Two distinct AntonPC jobs overlapped; Spark job `5840954551ef48e98050bf41608d51b3` ran within both intervals after automatic placement observed AntonPC's client quota full |
| Uncached GPU bridge job | `antonpc:1fc35a54cdc3416e87ad23b57204fc3a`, exit 0, return complete, CUDA 13.0 / NVIDIA Torch 2.9 / RTX 4090 |
| GPU result | 512-by-512 matrix multiplication produced 512.0; peak Torch allocation 10,616,832 bytes; returned JSON independently hash-checked |
| GPU result integrity | SHA256 `9a4201b1713d3212b7dfd31d0044d275f311abf029b561d10d74079cfff75d17` |
| Oversized VRAM request | 25 GiB requirement refused with a verified CUDA runtime present, before creating a fleet claim or remote job |

An earlier automatic submission observed a free slot just before a queued job started and consequently queued on AntonPC. The later confirmed-saturation check selected Spark and proved cross-computer overlap. Selection is an observation, not a reservation; controller admission remains authoritative.

CPU image `sha256:7809a24ec1b8a29953061a6386adc67c97d5014908cfeefa0868973e5600391d` passed capped nonroot Rust, Node, Python, NumPy and Pillow probes. CUDA image `sha256:a98f0b7c338df7e94569b210b102002f5394c5ba2e6f2b44a59f5e5a02ee354a` was registered only after normal installer attempt `527236002b8b43a1985996839bd9deb7` passed computation and separate ten-frame H.264, HEVC and AV1 hardware encode/decode checks. Runtime probe evidence is separate from the controller's lazily populated `gpu_video` status, which initially still reported unknown.

The first CUDA probe correctly failed and left the previous runtime registry intact. Docker Desktop 4.29 omitted the NVIDIA WSL driver library described in [upstream issue 520](https://github.com/NVIDIA/nvidia-container-toolkit/issues/520). Direct WSL CUDA worked; a single bounded diagnostic adding only that library read-only proved the cause. The normal durable fix was Docker Desktop 4.97.0, verified against its published SHA256 and valid Docker Authenticode signature. The same CUDA image then passed without a manual mount or library override. All seven previously stopped containers and six original image IDs remained inspectable. Normal Docker settings/data migration preserved the existing integrations. No Windows reboot occurred.

## Native UI and persistence

The development native executable was launched against explicit separate state. Jobs showed both original runner cards and qualified AntonPC history. Computers displayed verified SSH, ready CPU runtime, AMD64, observed Linux memory, and dedicated GPU memory without clipping. The candidate was closed normally; the installed 0.5.1 executable and original app window remained intact.

SSH automatic startup and the owned limited logon task are configured. Its protected helper starts normal Docker Desktop when needed and keeps the existing WSL user session available. Updating the task definition did not interrupt current WSL/controller processes. Future Windows logon/reboot recovery has not been physically acceptance-tested. Docker's own startup preference was preserved.

Raw receipts, returned files, a repeatable receipt/hash verifier, and browser screenshots are retained in owner-local development state outside the repository. The final browser evidence set is `ui-regression/final-cuda-label/receipt.json`. The physical acceptance verifier writes `acceptance/verified-acceptance.json`; sanitized remote setup evidence has its own hash manifest.

## Remaining acceptance boundaries

- This is a development candidate. The laptop installation remains 0.5.1; no new installer, release tag, push, GitHub release or Marketplace submission was performed.
- Native visual inspection is not installed startup/update/uninstall acceptance. Existing 0.5.1 startup dispatch intentionally does not adopt debug return-worker registrations.
- Additional computers were tested over LAN SSH. Off-LAN/hotspot availability and new-node relay pairing are not accepted.
- Each job uses one computer. Distributed model training, cross-host GPU memory pooling, and AMD64 Blender are not implemented.
- Memory observations are not reservations; Docker system-memory caps do not enforce discrete GPU VRAM limits. Keep GPU test workloads bounded.
- Local fleet responses above 40 MiB explicitly report incomplete observation. Full history pagination remains future work; saved receipts are not trimmed or deleted.
