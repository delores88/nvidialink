# Provenance

DGXBridge first-party implementation is covered by the repository MIT license. On 2026-10-13 Ben Van Dorn explicitly required both DGXBridge (then named Spark Dev Bridge) and CodaClauda to be MIT projects with public repositories and Marketplace listings, following his explicit direction to reuse his PhaseForge code. That authorization covers the owner-authored portions copied into these projects; preserve copyright attribution. It does not change the PhaseForge repository or relicense third-party dependencies. Their respective licenses/notices remain in force.

Read-only design references: delores88/PhaseForge commit `ac30e04` (full commit to be recorded when vendored), `native/spark-link/docs/PROTOCOL.md`, `docs/SPARK_GPU_PLACEMENT.md`; laptop `D:/PhaseForge-claude/scripts/sparkbuild.py`, `spark-runtimes/pf-build`, `reviews/025/spark-probe.md`. The prototype supplied secret/path policy, Docker isolation and measured-host constraints. Do not copy its PhaseForge paths, resource names, unrestricted networking or mutable state.

Every copied source/runtime file must be added here with source revision or SHA-256, destination and modifications before release.
