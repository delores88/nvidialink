# DGXBridge 0.2.0

Status: published and physically installed on the owner's Windows laptop on 2026-10-14. Marketplace Release decisions reports **scanning**; admission is not yet claimed. The existing five-minute acceptance monitor is active.

This release adds an owner-approved Marketplace updater in the drawer, hardware-bounded configurable Spark resource limits, LAN-only/Remote-only/Both policy, Connect/Disconnect, PhaseForge's actual connection filament animation, and copyable Claude Code/Codex prompts with installed paths and nonsecret connection references. Job history remains paginated.

Connection changes preserve pairing and already admitted jobs. Resource changes apply to new requests/admissions. Laptop concurrency is configurable; native laptop CPU/memory limits are not enforced. Remote-only selects paired transport and does not prove traffic used a relay. Windows package redirection is diagnosed explicitly; agent prompts select the verified owner state when available.

The updater verifies signed Marketplace acquisition and automated-review metadata, exact source/artifact/hash/size, and fresh eligibility before an explicit interactive installation. It preserves state and refuses installation from a packaged coding-agent context. Tests and installed workflow evidence are separate; a later-version live update remains unproven until exercised.

Release [v0.2.0](https://github.com/delores88/nvidialink/releases/tag/v0.2.0) pins source `74595ee70a75e0ee083624cabaaa2d7c5cac02f1`. GitHub release ID is `412846353`; installer asset ID is `640714030`. The 14,028,044-byte installer SHA-256 is `9b6937b9b27c0d97b48da969be482f110d244a58a41cbdc60694a9eaa167260b`. Anonymous installer and manifest downloads match the reviewed bytes. Previous releases and receipts are retained; PhaseForge files and services remain unchanged.

Ordinary Windows Explorer launched the current-user installer. Physical files, sidecars, notices, configuration and original pairing were verified independently of coding-agent package redirection. Native checks confirm the corrected Resources layout, live hardware/policy readback, valid physical CLI references for both agents, successful prompt/configuration copy actions and a live drawer updater check. The updater reported no newer approved stable release; that is not a production upgrade test. Earlier candidate receipts cover LAN-only, Remote-only and Disconnect/Connect, with Both and Connected restored. The release assets include separate installed-package, native-validation, source-scan, Spark and dependency-inventory receipts.

Source validation completed: 105 Windows CLI/link/protocol tests, 20 desktop tests (17 updater and three physical-path regressions), 36 Python controller tests, 11 UI model tests and three manifest tests. Synthetic browser checks cover the 1140px and 850px layouts and interaction flow; they do not establish installed Windows behavior.

Physical Spark ARM64 build job `55bc3f13e3674545b89ffbb0493e76e5` succeeded. Resource-policy job `4820a31cbae544be9fde66bd81ab770b` saved/read back owner limits and verified Docker's actual four-core and 8 GiB cgroup limits, then restored the original policy. See `evidence/dgxbridge-020-spark.json`. Only DGXBridge's controller service was updated; the paired link, other services and owner configuration were preserved.
