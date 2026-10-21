# DGXBridge 0.4.2

Status: released, deployed, physically installed and Marketplace accepted on 2026-10-21. The normal Marketplace download was saved and independently hash-verified against the installed package. Version 0.4.1 was published and installed, then rejected by Marketplace scanning for high/critical Pillow vulnerabilities. Its tag and artifacts are preserved. This document on the main branch records the final outcome; the immutable v0.4.2 tag retains the pre-admission notes.

## Changes

Both Spark runtimes now pin Pillow 13.0.0 to verified official ARM64 wheel hashes. The CPU runtime previously pinned 12.3.0; a physical check found 11.3.0 in the CUDA base image. CUDA replaces only Pillow, with dependency installation disabled so the NVIDIA PyTorch stack stays unchanged. Both image activation probes require the exact version and exercise PNG encoding, decoding and resizing before registering the image.

The fixed version addresses the findings described in the [upstream Pillow advisory](https://github.com/python-pillow/Pillow/security/advisories/GHSA-8g5g-8vmr-9782). No scanner rule or severity threshold was weakened. Existing jobs and stored owner settings are preserved; future jobs use the newly registered immutable runtime identities.

This release includes all [0.4.1 features and their recorded acceptance](RELEASE-0.4.1.md): public internet for Spark jobs with protected destinations blocked, shared downloads, named model services, authenticated route selection and the connection/resource UI. It does not expand those features' verification claims.

## Verification

- Independent review checked both exact wheel hashes against official PyPI and checked the activation probes.
- The ARM64 0.4.2 release build completed on the physical Spark through an actual Codex-submitted managed job. Its source was built using the existing Rust runtime/cache before runtime replacement; no Pillow image-processing acceptance is claimed from this compile.
- The gated Windows build passed 219 workspace tests and 26 desktop tests and produced release binaries. All five binary inventories match their artifact hashes; the installer contains the verified Windows GUI and both sidecars. Its raw Spark payload matches all 15 recorded hashes.
- Physical CPU and CUDA runtime activation succeeded. Separate actual Codex jobs passed PNG/JPEG decode, resize and thumbnail checks with Pillow 13.0.0; the CPU job also passed 28 runtime-refresh tests. NVIDIA torch remained unchanged, and activation exercised a 16 MiB CUDA tensor. Owner limits, internet/download policies and pairing fingerprints were preserved. See `evidence/dgxbridge-042-pillow.json`.

- An ordinary Windows Explorer in-place upgrade installed the exact 0.4.2 package in the physical owner profile. Configuration and pairing hashes remained unchanged. Native UI showed 0.4.2, an authenticated live Spark, 83 retained jobs and the fetched output of the new Pillow validation job. See `evidence/dgxbridge-042-windows.json`.

Installer SHA-256: `a5440c36627152dc5658d7ad375282456a68bdee2e88cac3e01856c9495ef62d`.

## Marketplace and download

The signed-in native Release decisions page accepted release `-cQoMrBHGJFBSxg7bs7lLF9Q` at `2026-10-21T18:02:18.017Z`, with five passed checks and none failed or incomplete. The dependency assessment found zero critical/high findings and one medium advisory for GLib 0.18.5 in the source lockfile (`GHSA-wrw7-89jp-8q8g`). The public immutable catalog independently matches accepted, unrevoked artifact `CxBu2rSsgm6W6Rae5BNgmrEj` to GitHub release `419861349`, installer asset `663298023`, source `442d5873d56d03daac52a697deeb589a487f5009`, the exact 12,766,442-byte installer and the reviewed canonical manifest digest.

The native Marketplace's normal **Download for Windows** completed and reported **Verified Download**. The saved file's independently computed SHA-256 equals the installer hash above. It was not installed a second time. The existing physical installation already matches these bytes. See [the acceptance and served-download receipt](../evidence/dgxbridge-042-marketplace.json).

This establishes static Marketplace admission and the normal download workflow. It does not establish an in-app updater installation cycle or replace the separate Windows and Spark receipts. No release heartbeat was restarted.

## Preserved limits

The broader 0.4.1 Spark, UI and packaging receipts are retained rather than relabeled as new tests. Remote hotspot use, actual model inference/TTS, GPU watchdog pressure killing and an actual Claude Code invocation are not established by these receipts. GB10 CUDA allocation can exceed Docker's memory cap. Shared downloaded files remain one owner trust domain. See [REMOTE.md](REMOTE.md) and [SERVERS.md](SERVERS.md).

The Windows installer is unsigned. DGXBridge has MIT first-party source with third-party notices and remains an ordinary, nonfeatured Marketplace application. PhaseForge remains the featured product.
