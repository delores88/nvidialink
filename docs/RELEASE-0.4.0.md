# DGXBridge 0.4.0

Status: release candidate. Publication, the Spark bridge update, installation and Marketplace admission are recorded below only once they happen.

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
- Rust (`sparkdev`) unit tests, Python Spark-side tests and UI model tests: counts recorded at tagging.
- Each feature branch passed an adversarial review in which every confirmed finding was fixed. The auto-discovery and connection-handling fixes were independently re-verified.
- Everything uses fakes and fixtures. Real-Spark checks are recorded below after the bridge update.
