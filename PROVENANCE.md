# Provenance

DGXBridge first-party implementation is covered by the repository MIT license. On 2026-10-13 Ben Van Dorn explicitly required both DGXBridge (then named Spark Dev Bridge) and CodaClauda to be MIT projects with public repositories and Marketplace listings, following his explicit direction to reuse his PhaseForge code. That authorization covers the owner-authored portions copied into these projects; preserve copyright attribution. It does not change the PhaseForge repository or relicense third-party dependencies. Their respective licenses/notices remain in force.

Read-only design references: delores88/PhaseForge commit `ac30e04` (full commit to be recorded when vendored), `native/spark-link/docs/PROTOCOL.md`, `docs/SPARK_GPU_PLACEMENT.md`; laptop `D:/PhaseForge-claude/scripts/sparkbuild.py`, `spark-runtimes/pf-build`, `reviews/025/spark-probe.md`. The prototype supplied secret/path policy, Docker isolation and measured-host constraints. Do not copy its PhaseForge paths, resource names, unrestricted networking or mutable state.

Every copied source/runtime file must be added here with source revision or SHA-256, destination and modifications before release.

## 0.2.0 connection topology and updater

The connection visuals use owner-authorized PhaseForge code at exact commit `7a665907901bd797df0eedee961e0d83a41eeabf` (read-only reference `D:/PhaseForge-claude/wt/release-0250`). The three pure JavaScript modules below are copied unchanged into `ui/vendor/`. Hardware SVG geometry and scoped styles are adapted from SparkLinkVisuals into `ui/connection-visual.mjs` and `ui/style.css`, replacing React dependencies with vanilla DOM and mapping only observed DGXBridge connection state. No throughput, GPU utilization or relay route is invented. Reduced motion and hidden/offscreen suspension are preserved.

| Upstream file | SHA-256 |
| --- | --- |
| `frontend/src/lib/sparkLinkParticles.mjs` | `02d33a5c21f2c61304cacbe8bc9ff1fd17ca36182e9fbaec53b5af22a903cbcf` |
| `frontend/src/lib/sparkLinkMotion.mjs` | `0039f58c363517e128c325d536afe6672319202b3f2e7d06427e3fd3b361b2dd` |
| `frontend/src/lib/visible-render-loop.mjs` | `a54d199f5abcdf0770c19f0db1de9d7bd110942f2adc3fdfdb210314c559dab5` |
| `frontend/src/components/settings/SparkLinkVisuals.jsx` | `80612e9b827c2d76e1d0becbaaf318d208f037e124be80a4302e6f30bb21d9d1` |
| `frontend/src/components/settings/SparkLinkVisuals.module.css` | `a566bdf77d35f5ec63e17cc0b35fcfcb9558cc53a4ede97fdd030e1b9f3099c0` |

The drawer updater placement follows `frontend/src/components/shared/DesktopUpdate.jsx` and its stylesheet at the same pin. Native authorization and installer implementation provenance is recorded in [UPDATER-PROVENANCE.md](docs/UPDATER-PROVENANCE.md); it uses the current signed Marketplace contract and its own app identity. All first-party adaptations remain MIT under Ben Van Dorn / DeloresAI copyright.

## 0.5.0 Blender runtime

The read-only PhaseForge Blender 4.5.9 image and the owner-authorized recipe/support-file reuse, exact upstream commits, file hashes and third-party license boundaries are recorded in [PROVENANCE-REMOTE.md](PROVENANCE-REMOTE.md). DGXBridge uses a distinct runtime image and never changes the upstream image, repository or service.
