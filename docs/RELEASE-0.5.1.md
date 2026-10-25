# DGXBridge 0.5.1 release preparation

Status: unpublished candidate. Claude's three-commit handoff through `b15328d` is integrated on `codex/051-release`, based on the accepted 0.5.0 closeout `6f95974`. Independent review found return-path regressions and diagnostics gaps that must be corrected before packaging. No 0.5.1 deployment, installation or Marketplace admission is claimed.

## Intended changes

- Launch the large-snapshot builder with its sibling Python modules importable under isolated Python. Report builder health so a failed builder does not leave large uploads silently queued.
- Prune abandoned queued snapshots when superseded for an hour or older than 24 hours, while preserving active materialization and completed snapshots. Content-addressed chunks remain retained.
- Use pinned SCP packs for large LAN uploads and a validated bulk path for returned files, retaining hash checks, private-network policy, conflict preservation and resumable delivery. Paired connections retain their existing RPC transfer path.
- Read a USB Ethernet adapter's generation from the actual hub port and clarify request-ID reuse errors.

## Reported handoff evidence

Claude reports 384 remote Python tests (10 skipped, no failures), 162 Rust tests plus one return-worker test on Windows through the unchanged build gate and on Spark job `867820a3712149dba6a691194f80e7ab`. These are handoff reports until their retained receipts are independently reconciled.

The reported 652 Mbit/s upload and 883 Mbit/s download are from a throwaway Spark-state harness and 512 MiB data. They are not measurements of the installed 0.5.1 CLI or proof that the 2 GiB acceptance target is met.

## Release gates

Resolve the reviewed disk-reserve, partial-return resume and submission-fairness regressions, bounded materializer diagnostics and the name-only USB fallback. Keep fixes with the Claude implementation owner. Then build and scan the exact candidate, retaining the unchanged local Cargo build gate and separate C: artifacts.

Physical acceptance must record a sync larger than 1 GiB, builder health afterwards, 2 GiB wired upload and automatic result return with full file hashes, observed USB generation, and paired-route sync/return. Target at least 600 Mbit/s in each wired transfer direction; report wall-clock scope and actual achieved rates separately from hashing/materialization overhead.

Retain independent receipts for source tests, Spark deployment, normal Windows installation and state preservation, draft review, immutable publication, native Marketplace decision and exact served installer. DGXBridge remains nonfeatured. Preserve prior releases and all PhaseForge files/services. Completed-snapshot retention, slower paired RPC transfers and the unchanged 0.5.0 verification limits must be disclosed.
