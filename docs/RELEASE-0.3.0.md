# DGXBridge 0.3.0

Status: release candidate. A frontend-only redesign of the desktop window. The bundled `sparkdev` CLI, `sparkdev-link` sidecar and Spark ARM64 payload are the unchanged 0.2.1 builds; the CLI therefore still reports version 0.2.1. Publication, installation and Marketplace admission are recorded below only once they happen.

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

Measured on source commit `f54fc2b`:

- 11 UI model tests pass (`node --test scripts/test-ui-model.mjs`).
- Isolated Playwright fixture `scripts/test-ui-render.mjs` passes 22 checks at 1140px and 850px, with no horizontal overflow and no renderer exceptions. It now also covers job filters and detail, the two-step cancel, the route address used and the theme toggle. It uses a synthetic backend; it is not native acceptance.
- Pinned Gitleaks 8.30.1 reports no findings for `ui/`, `scripts/`, `src-tauri/tauri.conf.json` and `package.json`.

Not verified for this release: the Spark was offline (owner travelling), so no physical Spark job, connection, pairing or resource save was exercised against 0.3.0. Prior Spark evidence remains in `evidence/dgxbridge-021-spark.json` and covers the unchanged 0.2.1 binaries only. Remote hotspot, sleep and restricted Codex shell limitations from earlier releases still apply.
