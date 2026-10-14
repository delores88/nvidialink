# DGXBridge 0.2.0

Status: release candidate under validation. No publication, Marketplace admission or local installation is claimed by this document yet.

This release adds an owner-approved Marketplace updater in the drawer, hardware-bounded configurable Spark resource limits, LAN-only/Remote-only/Both policy, Connect/Disconnect, PhaseForge's actual connection filament animation, and copyable Claude Code/Codex prompts with installed paths and nonsecret connection references. Job history remains paginated.

Connection changes preserve pairing and already admitted jobs. Resource changes apply to new requests/admissions. Laptop concurrency is configurable; native laptop CPU/memory limits are not enforced. Remote-only selects paired transport and does not prove traffic used a relay. Windows package redirection is diagnosed explicitly; agent prompts select the verified owner state when available.

The updater verifies signed Marketplace acquisition and automated-review metadata, exact source/artifact/hash/size, and fresh eligibility before an explicit interactive installation. It preserves state and refuses installation from a packaged coding-agent context. Tests and installed workflow evidence are separate; a later-version live update remains unproven until exercised.

Validation receipts will record source commit, targeted unit/UI checks, physical Spark job IDs, immutable installer hashes, ordinary Windows installation, Marketplace decision and remaining limitations. Prior releases and receipts are retained. PhaseForge files and services remain unchanged.

Source validation completed: 105 Windows CLI/link/protocol tests, 20 desktop tests (17 updater and three physical-path regressions), 36 Python controller tests, 11 UI model tests and three manifest tests. Synthetic browser checks cover the 1140px and 850px layouts and interaction flow; they do not establish installed Windows behavior.

Physical Spark ARM64 build job `55bc3f13e3674545b89ffbb0493e76e5` succeeded. Resource-policy job `4820a31cbae544be9fde66bd81ab770b` saved/read back owner limits and verified Docker's actual four-core and 8 GiB cgroup limits, then restored the original policy. See `evidence/dgxbridge-020-spark.json`. Only DGXBridge's controller service was updated; the paired link, other services and owner configuration were preserved.
