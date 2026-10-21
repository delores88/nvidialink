# DGXBridge 0.4.0

Status: published, installed and accepted by the DeloresAI Marketplace at 2026-10-21T03:38:29Z. The owner's Spark already runs the 0.4.0 bridge: controller, worker and preparation files were updated with the bridge's own bootstrap while no jobs were active. The pairing link and resource policy were unchanged and the scheduler is healthy. The live link report shows Wi-Fi 2.4 GHz, channel 11, 20 MHz, 143/287 Mbps and no cable. Publication, installation and Marketplace admission are recorded below once they happen.

## What's new

- **Finds your Spark on a new network.** DGXBridge now also tries the address your SSH settings already know for the Spark. It remembers the address that worked last, and checks every candidate with a quick parallel test, so a dead address costs about 1.5 s instead of 5 s. Every connection still has to present your Spark's pinned host key.
- **Find my Spark.** One button on the Spark page searches only your own home network for the Spark and checks each candidate against the pinned key. You then choose whether to save the address it finds. It never runs on its own, and agents can't trigger it.
- **Honest, self-healing connections.** A connection that drops mid-upload, which is common on Wi-Fi, is now reported as a dropped connection instead of "inspect the configured service and key". It is retried once when that is safe.
- **GPU-ready Python packages.** `prepare --runtime cuda` installs Python packages built for the Spark's GPU image (Python 3.12). Agents no longer have to hand-package wheels. PyTorch is already in that image.
- **Big model files.** A single file can now be up to 16 GiB and a project up to 32 GiB, up from 512 MiB and 8 GiB, so model checkpoints no longer need resharding. Large files are assembled on the Spark in the background, so uploads survive Wi-Fi drops.
- **Slow Wi-Fi warning.** The Spark reports which network it is using. The app shows "Spark network: Wired 10 Gbps" or "Wi-Fi 2.4 GHz 65 Mbps". If the Spark is stuck on slow 2.4 GHz Wi-Fi with no cable plugged in, it tells you to plug one in.
- **Plainer words.** Connection details now say "Connected through: Your home network", "Job service: Running" and "Last heard from" instead of internal terms.

## Requires a Spark bridge update

GPU package preparation, big files and the network report need the updated Spark bridge. Until it's updated, the app shows "Update the Spark bridge to see this". Older bridges keep working with the old limits.

## Not in this release

Internet access for Spark jobs, a shared model cache and long-lived model servers are being built separately and will follow in the next release. The optional Linux ARM64 command-line download is not included in this release. The pairing-link program is unchanged.

## Verification

Measured on the merged source:
- `sparkdev` Rust unit tests: 115 passed, 0 failed. They ran as a DGXBridge job on the Spark (Linux aarch64), outside the laptop build gate.
- Python Spark-side tests: 145 passed with 3 platform skips. UI model tests: 22/22.
- The Windows CLI, app and installer were built through the unchanged laptop build gate.
- Each feature branch passed an adversarial review in which every confirmed finding was fixed. The auto-discovery and connection-handling fixes were independently re-verified.
- Everything uses fakes and fixtures. Real-Spark checks are recorded below after the bridge update.

## Publication

- [v0.4.0](https://github.com/delores88/nvidialink/releases/tag/v0.4.0): release `419116003`. Annotated tag `v0.4.0` resolves to `0d57031bb6e608e4d392d15988777cffe56cd13a`.
- Installer asset `661239462`, 12,929,311 bytes, SHA-256 `6e5b25f3c9479982476d94b673fde33143de5585ba465bd4e2ed267ea172619a`.
- Manifest SHA-256 `fd81503d…ad58`. It was validated and attached while the release was still a draft; the post-publication check produced identical bytes.
- Decoding the installer proves the SBOM-described app (`fe9866287a8a…55d5`) and CLI are inside it.
- Marketplace artifact `og1bL2dH9sw-PNkpK6UuvSiJ` was accepted at 03:38:29Z. File and package identity, publisher authorization, source binding, malware, SBOM, dependencies and secrets all passed.
- The catalog declares minimum Windows and memory "Not specified", runtime `Microsoft Edge WebView2` and 60 MB disk.
- Installed on the owner's laptop as a silent per-user in-place upgrade. The installed app and CLI match the release, and settings, pairing identity and peer record are unchanged. Live: LAN route to the remembered address, network report shows Wi-Fi 2.4 GHz, scheduler healthy.
