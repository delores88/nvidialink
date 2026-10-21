# DGXBridge 0.4.1

Status: release candidate, 2026-10-21. Source checks, candidate builds and the Spark checks below are recorded. Final internet-guard verification, package refresh, publication, Marketplace admission and physical Windows installation are pending. Earlier versions' acceptance does not establish acceptance of this candidate.

## Changes

- Clearer connection and resource controls, confirmed internet-setting readback, saved-download usage and limits, and named model-service status. Job history and fetched logs remain visible when the connection drops. Claude Code and Codex each get a copyable prompt with the selected connection and owner-state references.
- Authenticated Spark route learning prefers recent verified wired links. Connection details distinguish the Spark's link from the laptop's adapter and warn about a known slow laptop link. Reported link rates are not measured transfer speeds. Finding an unknown Spark remains an explicit owner action.
- New Spark jobs can use a public HTTP/HTTPS proxy when the owner's internet setting is enabled and helpers are available. Turning it off cuts running internet helpers. Direct external sockets and tools that ignore proxy settings are unavailable. CONNECT tunnels can reach any permitted public destination port; their payloads are not inspected or restricted to HTTPS. An enabled setting does not prove connectivity or a successful download. Locked dependency preparation retains its package-host allowlist.
- Saved downloads survive individual jobs and result-cache bypasses. Owner controls manage their space limit and queue clearing while jobs are active. Shared downloads are writable by other jobs in the same owner trust domain; they are not isolated against a malicious job. Do not store credentials there or treat a cached model as inherently trusted.
- Named HTTP model services support retained requests and replies through the authenticated bridge. They use the supplied model adapter and weights, expose no public listening port, and have startup, idle, lifetime, request and reply limits. This is not a general browser-facing or streaming model API. See [SERVERS.md](SERVERS.md) for the consumer contract and limits.

## Verification recorded so far

- The UI's 27 model tests pass. Isolated browser fixtures at 1140px and 850px cover layout, setting readback, stale-response protection, retained logs, services and download controls. These use a synthetic backend, not the installed desktop app.
- Actual Codex-submitted Spark jobs completed the earlier candidate's Rust checks (216 passing tests across the workspace) and Python controller checks (276 passing tests). A subsequent ARM64 build also succeeded. These receipts predate the final internet-guard changes and do not certify those later changes.
- The initial gated Windows build passed 218 workspace tests and 26 desktop tests, built the Windows binaries and produced an NSIS candidate. Final package contents and SBOM identity still require verification after the guard update.
- Physical Spark runtime provisioning completed CPU and CUDA probes, including a small allocation on the GPU. A pinned SSH connection reported the Spark's wired link and the laptop's separate wired link. No end-to-end throughput measurement was made.
- Packaging checks passed 12 tests with one skip. Exact artifact hashes will be recorded only after final package validation.

## Limits and pending acceptance

The internet guard denies private networks, Spark host addresses and cloud metadata. The final guard update also targets the public IPv4 address observed by its own helper and closes connections if that observation changes, expires or fails. Checks are periodic, not instantaneous. Other public router aliases or addresses reached through different routing policies may remain undiscovered. Final guard review and physical denial/download checks are pending; this release note is not a claim that every route into a home network is blocked.

Native installed 0.4.1 behavior, configuration preservation after its upgrade, remote/hotspot use, sleep/disconnect recovery for this candidate, a real model-service workload and actual Claude Code invocation have not yet been established by these receipts. Source tests, Spark probes, installation and Marketplace admission remain separate evidence layers.

DGXBridge is an independent application with MIT first-party source and preserved third-party notices. The Windows application is unsigned; it is not an NVIDIA product or an NVIDIA-signed installer. Publication is intended as an ordinary, nonfeatured Marketplace listing, with PhaseForge remaining featured.
