# DGXBridge 0.5.1 release preparation

Status: unpublished candidate. Claude's handoff and corrective commit `b881afb` are integrated on `codex/051-release`, based on the accepted 0.5.0 closeout `6f95974`. Independent review passed all five corrections. Windows packaging is in progress. No 0.5.1 deployment, installation or Marketplace admission is claimed.

## Intended changes

- Launch the large-snapshot builder with its sibling Python modules importable under isolated Python. Report builder health so a failed builder does not leave large uploads silently queued.
- Prune queued snapshots at least an hour old when a newer queued snapshot for the same project exists, or queued snapshots older than 24 hours, while preserving active materialization and completed snapshots. Content-addressed chunks remain retained.
- Use pinned SCP packs for large LAN uploads and pinned SFTP `reget` for returned files, retaining hash checks, private-network policy, conflict preservation and resumable delivery. Check disk reserve before copying and limit each copy to the return worker's remaining pump budget. Paired connections retain their existing RPC transfer path.
- Read a USB Ethernet adapter's generation from the actual hub port and clarify request-ID reuse errors.

## Reported handoff evidence

For `b881afb`, Claude reports 385 remote Python tests (10 skipped, no failures), 165 Rust tests plus one return-worker test on Windows through the unchanged build gate and on Spark job `b1b778e0bfa74930baf516f41de101c6`. Claude also reports a real 1 GiB SFTP interruption/resume with a matching SHA-256. These remain handoff reports until their retained physical receipts are independently reconciled.

Independent checks on the integrated source passed: all 242 Windows workspace tests through the unchanged build gate, three Python regressions using real builder subprocesses (success, exception/nonzero exit and SIGKILL), and a fresh default-rule Gitleaks source-tree scan with no findings or suppressions. The source-license union accounts for 449 Cargo-resolved packages with no missing texts; optional runtime dependencies have separate evidence. These checks do not establish installed behavior or Marketplace admission.

The reported 652 Mbit/s upload and 883 Mbit/s download are from a throwaway Spark-state harness and 512 MiB data. They are not measurements of the installed 0.5.1 CLI or proof that the 2 GiB acceptance target is met.

## Release gates

The reviewed disk-reserve, partial-return resume and submission-fairness regressions, bounded materializer diagnostics and name-only USB fallback are corrected. Build and scan the exact candidate, retaining the unchanged local Cargo build gate and separate C: artifacts. The ARM pairing link may retain its unchanged 0.1.0 binary with exact 0.5.0 build provenance; the changed ARM CLI and return worker require fresh artifacts.

Physical acceptance must record a sync larger than 1 GiB, builder health afterwards, 2 GiB wired upload and automatic result return with full file hashes, observed USB generation, and paired-route sync/return. Target at least 600 Mbit/s in each wired transfer direction; report wall-clock scope and actual achieved rates separately from hashing/materialization overhead.

Retain independent receipts for source tests, Spark deployment, normal Windows installation and state preservation, draft review, immutable publication, native Marketplace decision and exact served installer. DGXBridge remains nonfeatured. Preserve prior releases and all PhaseForge files/services. Completed-snapshot retention, slower paired RPC transfers and the unchanged 0.5.0 verification limits must be disclosed.
