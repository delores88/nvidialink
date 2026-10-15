# DGXBridge 0.3.0

Status: published on GitHub; Marketplace intake needs attention (see Publication). A frontend-only redesign of the desktop window. The bundled `sparkdev` CLI, `sparkdev-link` sidecar and Spark ARM64 payload are the unchanged 0.2.1 builds; the CLI therefore still reports version 0.2.1. Publication, installation and Marketplace admission are recorded below only once they happen.

## What changed

The desktop UI now follows the CodaClauda layout while keeping DGXBridge's green palette:

- **Shell.** A left icon rail (Jobs, Spark, Limits, Agents, Updates) and a top bar with the live Spark connection state, active job count, refresh and a light/dark theme toggle. The large marketing headings and the always-on "Your computers, connected" footer are gone; the footer claimed a connection even when none existed.
- **Jobs.** The paginated table is replaced by a searchable list (Ctrl+K) with All/Active/Failed/Spark/Laptop filters and a detail pane. The detail pane shows retained output inline and keeps following it while the job is active. Laptop and Spark summary cards show job slots and the CPU/memory that running jobs have reserved against the configured budget. They also show the age of the scheduler heartbeat. These figures are reservations, not live hardware usage.
- **Agent-drift signals.** Each job separates the workload exit code from CLI success. A failing workload can still return `ok:true` from the CLI. The detail pane also marks:
  - cached results, so they are not read as fresh runs;
  - jobs recorded as active for more than six hours;
  - pending cancellations;
  - client labels, which are always shown as self-declared.

  An owner can cancel an active job with a two-step confirmation through the existing `cancel` command.
- **Spark.** The connection view lists route, scheduler health, heartbeat, observed hardware and which configured LAN address the authenticated SSH route actually used. Link speed and interface type are not measured. Mode selection, SSH settings, phone pairing, bridge install/repair and the raw observation stay available.
- **Agents.** The Codex/Claude Code instructions and MCP configuration are unchanged in mechanism, with numbered setup steps. The generated prompt now tells agents that they share the board: check `status --all` before heavy work, read another agent's result by job ID instead of re-running it, and treat client labels as self-declared.
- **Limits and updates.** Same presets, budget bars, validation, explicit save and Marketplace-only updater, restyled.

Backend commands, the Tauri command surface, the CSP and all safety behavior are unchanged.

## Verification

Measured on the tagged v0.3.0 source:

- 14 UI model tests pass (`node --test scripts/test-ui-model.mjs`).
- Isolated Playwright fixture `scripts/test-ui-render.mjs` passes 22 checks at 1140px and 850px, with no horizontal overflow and no renderer exceptions. It now also covers job filters and detail, the two-step cancel, the route address used and the theme toggle. It uses a synthetic backend; it is not native acceptance.
- Pinned Gitleaks 8.30.1 reports no findings for `ui/`, `scripts/`, `src-tauri/tauri.conf.json` and `package.json`.

Not verified for this release: the Spark was offline (owner travelling), so no physical Spark job, connection, pairing or resource save was exercised against 0.3.0. Prior Spark evidence remains in `evidence/dgxbridge-021-spark.json` and covers the unchanged 0.2.1 binaries only. Remote hotspot, sleep and restricted Codex shell limitations from earlier releases still apply.

The listing screenshot `assets/screenshots/dgxbridge-030-jobs.png` is the final built 0.3.0 GUI running against a fresh, empty `SPARKDEV_HOME`. It contains no owner hosts, jobs, keys or pairing data. It was captured from the build output, not from a normal Windows installation.

## Publication

- [v0.3.0](https://github.com/delores88/nvidialink/releases/tag/v0.3.0) was published at 2026-10-15T18:38:01Z.
- Release ID `414144303`. Annotated tag `v0.3.0` resolves to source `9b2b84af052931a919ef097baa3fc7c04dea9d67`.
- Installer asset `644795569`, `DGXBridge_0.3.0_x64-setup.exe`, 12,460,461 bytes, SHA-256 `b4d73823921c51fc3272c27111402123646df5844abf3c40409d7fc5f160ab1c`. GitHub's digest metadata matches the local bytes.
- Correction: the uploaded GUI SBOM's source digest (`6f5f0594…`) is the build-folder `dgxbridge.exe`, whose Tauri bundle marker reads `UNK`. The exe actually shipped and installed (`36b8997e2ffe17d218137d7029bcb450ffaf4c537b6c7ba3f4ab39f1446f8ef6`) has marker `NSS`. The two are the same size and differ by exactly 3 bytes, so the component inventory describes the same build. An earlier version of these notes wrongly said the SBOM matched the shipped exe.
- The CLI, link and Linux SBOMs from 0.2.1 were re-verified against the bundled binaries.

The manifest was attached late, and this is a release-process error. The release was published before `deloresai.manifest.json` existed:

- Automatic Marketplace intake ran without a manifest and was blocked ("needs attention").
- The first manifest run was validated against a DeloresAI checkout whose contract rejects `featureNotices`, and the laptop then briefly lost network.
- The manifest was finally generated against the `codex/publisher-release-retry` contract, whose `index.ts` is byte-identical to the contract 0.2.1 was accepted under. It was uploaded at 18:41:10Z as asset `644806959`, SHA-256 `b686950ebf1dc48922993f85be16e45e05880b40043cf6659dba810f0da07ee6`, together with `manifest-validation.json`.

Marketplace admission is not claimed. Automatic intake picked up the manifest: Marketplace release `jZEyZHIVRCAIdwElx36fzy1f` queued at 18:41:23Z, 13 seconds after the upload, as observed by the Marketplace owner. No Retry was used. No duplicate tag, release or submission was created.
