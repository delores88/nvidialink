# DGXBridge 0.3.1

Status: release candidate. A Marketplace compatibility correction plus startup status wording. The bundled `sparkdev` CLI, `sparkdev-link` sidecar and Spark ARM64 payload are the unchanged 0.2.1 builds. Publication and Marketplace admission are recorded below only once they happen.

## What changed

- **Compatibility floors removed.** DGXBridge 0.1.0-beta.2 through 0.3.0 declared `minimumOs 10.0.26200` and `memoryGb 32`. Those were the owner's test laptop, copied in as requirements. The Marketplace enforces both values: the native setup refuses a PC with less memory or an older Windows build. 0.3.1 declares the Marketplace's "Not specified" values (`minimumOs "0"`, `memoryGb 0`). The observed test host (Windows 10.0.26200, 32 GB) is recorded separately as `testedHost` in `manifest-validation.json`, never as a minimum. This does not claim compatibility with every system.
- **Recognized runtime name.** The manifest now declares the runtime as exactly `Microsoft Edge WebView2`, the name Marketplace setup detects. The previous free-text strings could never match a host's runtime list. Windows OpenSSH Client is disclosed in the description as a separately installed prerequisite for Spark connections, not as a Marketplace-detected runtime.
- **Startup wording.** Until the first device check returns, the laptop and Spark cards show "Checking…" instead of "— active", and the status pill shows "Checking jobs…". The first check can take tens of seconds when the Spark is unreachable.
- **Release tooling.**
  - `scripts/prepare-manifest.py` validates a complete draft release before publication. GitHub serves draft assets from an `untagged-…` path, which it accepts only for drafts and only without `--require-published`. The manifest is therefore attached before Marketplace intake can observe the release.
  - `.local/nsis-contains.py` (stdlib only, not shipped) decodes the installer's LZMA data and confirms the exact shipped GUI bytes. The GUI SBOM is bound to that executable, fixing the 0.3.0 SBOM-digest mistake.

## Verification

Measured on the tagged v0.3.1 source:

- `scripts/test_manifest.py`: 8 tests pass. They cover unknown minima, rejected negative or zero values, a mandatory `testedHost` that never becomes a requirement, draft-only untagged URLs, and canonical URLs after publication.
- 14 UI model tests pass. The Playwright fixture passes 22 checks at 1140px and 850px, using a synthetic backend; it is not native acceptance.
- A dry run of the new manifest shape against the deployed Marketplace contract (`publisher-release-retry`, `index.ts` sha256 `6a4f72ce…`) passed with no artwork issues.
- The installer contains the shipped GUI `1f0d71f79cd0ca5d7e667d1e0a57624fbb3b566e3d9f6f68359224a789971f4c` as one of its 359 file records. The GUI SBOM's source digest is that value.

Not verified: compatibility on lower-spec or older Windows machines. The Marketplace values only stop declaring false floors. The listing screenshot is the 0.3.0 build-output capture; the UI differs only in the startup wording.
