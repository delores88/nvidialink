# DGXBridge 0.6.0

DGXBridge 0.6 adds registered Linux and Windows-with-WSL compute computers alongside the existing Spark and laptop. Agents can select a computer or use compatible authenticated runtime/resource observations for automatic placement. Jobs retain their original computer across retries and return files automatically through independent profiles. The Computers pane and job filters extend the existing interface.

Each job still uses one computer. Distributed model training, shared GPU memory across computers and AMD64 Blender are not included. Additional computers were tested through LAN SSH; new-node relay pairing and hotspot behavior are not accepted. Managed native Windows jobs retain the existing Spark scheduler dependency. See [setup and limits](MULTI-DEVICE.md), [privacy](PRIVACY.md), and [candidate acceptance](VALIDATION-0.6-MULTIDEVICE.md).

## Prepared release evidence

- Windows workspace checks passed 307 tests through the unchanged owner build gate. The desktop backend passed 25 tests. Windows path identity and loopback checks passed separately.
- Physical Linux checks passed 226 core tests, one return-worker test and six CLI subprocess tests. The remote Python suite passed 405 tests.
- The release manifest and packaging suite passed 38 tests on an independent AMD64 controller, with two Windows-only cases checked separately on Windows. This covers independent AMD64 runtime declarations and historical ARM release behavior.
- The UI model passed 38 tests; browser workflows passed at widths 1140 and 850. Native candidate inspection preserved the original Spark/laptop cards and displayed real additional-computer jobs. Failed named observations retain Last seen rows; incomplete local metadata does not masquerade as a network failure.
- Physical AMD64 CPU and uncached RTX 4090 jobs completed with automatically returned, independently hash-verified files. Retry identity, cross-device retry refusal, cancellation after disabling new jobs, concurrent local-edit protection and overlapping Spark/AMD64 execution passed.
- Auditable Windows GUI/CLI/link/return binaries and fresh ARM64 CLI/link/return artifacts were built with locked dependencies. The full remote bundle contains 31 inspected files. License collection accounts for 449 packages with no missing texts. Exact installer, SBOM and source-scan identities accompany the final release assets.
- Spark controller deployment matched all 29 selected support files while preserving pairing, link process, policies, runtime registry and owner state. It did not rebuild GPU runtimes or alter PhaseForge resources.

The Windows and ARM64 production builds used source `fffda12`. Later release-generator, test and documentation commits do not change the compiled Rust, UI, remote-controller or runtime inputs; the final staging receipt records the source comparison. The ARM64 link's independent crate version remains 0.1.0; its fresh source/build identity and hashes are retained separately from the product version.

## Installation and publication

Candidate source, package membership, physical compute, installation, visual inspection, Marketplace static admission and served download are separate evidence layers. Final installed and publication receipts are recorded after those operations complete; preparation alone does not establish them. The installer remains unsigned and requires preinstalled Microsoft Edge WebView2 and Windows OpenSSH. Future Windows logon/reboot recovery and actual new Claude Code session acceptance remain unverified.
