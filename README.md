# DGXBridge

A Windows desktop app and agent CLI for running Linux builds, tests and compute on your own NVIDIA DGX Spark, while Windows work stays on your laptop. Claude Code, Codex and humans share job status, worktree claims and retained logs. First-party source is MIT licensed; this is an independent DeloresAI utility.

**Beta 0.1.0-beta.1:** installed Windows use, LAN SSH, phone-approved pairing and concurrent real Claude Code/Codex MCP jobs have been exercised. Codex's restricted Windows shell check failed on this laptop with missing writable-root capability SIDs; remote hotspot, physical network interruption and sleep acceptance remain unverified. Marketplace admission is recorded separately. Consult [acceptance evidence](docs/ACCEPTANCE.md) and [beta notes](docs/RELEASE-0.1.0-beta.1.md).

## Connect your Spark

Download the [Windows beta installer](https://github.com/delores88/nvidialink/releases/download/v0.1.0-beta.1/DGXBridge_0.1.0-beta.1_x64-setup.exe). The [GitHub release](https://github.com/delores88/nvidialink/releases/tag/v0.1.0-beta.1) includes checksums, notices, dependency inventories and a Linux ARM64 CLI. For a silent current-user installation, run the downloaded installer with `/S`. The installer is unsigned; review the release notes and verify its hash before running it.

1. Start with a working OpenSSH alias and a host key verified against the Spark. Install Docker for your existing user on the Spark; the bridge never uses sudo.
2. Open **Connect a Spark** in the desktop app, enter the alias and optional LAN/known-host overrides, and choose **Connect & install bridge**. This installs separate user services and builds the bridge's pinned CPU runtime under `~/sparkdev`.
3. Choose **Create pairing code**. Scan it with a phone on the same LAN, enter the displayed six digits and approve. Tickets expire; private keys and approval codes are never saved in project files.
4. Public iroh relays are an explicit option. No owner VM is currently configured. LAN SSH remains preferred; a host-key or authentication failure stops the request rather than switching identities.

The Windows installer requires the Microsoft Edge WebView2 runtime already installed and bundles the GUI, `bin/sparkdev.exe` and `bin/sparkdev-link.exe`. It does not download an executable WebView2 bootstrapper. The **Agent setup** screen shows their actual installed path and copyable MCP configuration. See [MCP setup](docs/MCP.md), [CLI reference](docs/CLI.md) and [pairing protocol](docs/LINK.md).

## Run work

Use the installed CLI by absolute path, or put its `bin` directory on your own PATH. Prepare locked dependencies once before offline execution:

```powershell
sparkdev --client codex --json prepare C:/work/project --runtime rust
sparkdev --client codex --json devices
sparkdev --client codex --json plan C:/work/project --where auto -- cargo test --locked
sparkdev --client codex --json run C:/work/project --where auto --detach --request-id test-001 -- cargo test --locked
sparkdev --client codex --json wait JOB_ID --timeout 20
sparkdev --client codex --json logs JOB_ID --since 0
sparkdev --client codex --json status --all
```

Retain the returned job ID and log `next_offset`. Poll until terminal state and inspect `exit_code`. Retry an ambiguous submission with the same request ID. `--argv-json file.json` accepts an exact array of arguments without shell quoting ambiguity. `cancel JOB_ID` queues cancellation when disconnected. The CLI remains the primary interface; `sparkdev --client codex mcp` and `sparkdev --client claude mcp` expose devices/plan/prepare/run/wait/logs/status/cancel over stdio.

`devices` distinguishes reachable Spark, failed authentication, unavailable transport and missing configuration, with LAN SSH versus paired iroh routing and attempted-route reasons. Direct versus relayed iroh traffic is not inferred. `plan` previews placement without upload, claim or submission. Laptop hardware can be available while new managed jobs are unavailable: both runners currently require Spark's shared scheduler. Capacity observations are not reservations.

`--where auto` detects explicit Windows/MSVC targets, PowerShell/cmd, NSIS and Tauri installers. Linux-capable commands use the Spark. Use an explicit placement for scripts whose platform needs cannot be inferred. A single compile is never split between architectures. Independent worktrees can use both machines concurrently.

## Isolation and limits

- Spark jobs use a closed registry of image IDs, fixed resource limits and no network. `prepare` uses a separate allowlisted dependency proxy with lockfiles enforced. Optional CUDA provisioning is distinct from the CPU runtime.
- Default jobs use 4 CPU cores and 8 GiB; the scheduler enforces total and per-client quotas and memory headroom. Owner-controlled project limits may permit larger jobs. Existing PhaseForge and Ollama resources are left alone.
- Sync hashes tracked and non-ignored files into resumable content-addressed chunks. Build output, symlinks, paths outside the root and recognized credential material are excluded or rejected. Never place secrets in a submitted project or command line.
- Successful Spark results can be reused only when source, argv and runtime match. `--no-cache` bypasses reuse. Laptop result caching is disabled because the native host has no immutable runtime digest.
- Native laptop jobs run with the owner's permissions. They require the shared Spark scheduler for leases and use the configured build gate for direct Cargo commands. They are not container-sandboxed. Indirect Cargo calls must also use the gate.
- Jobs and logs are retained in the owner's local state and Spark state. This preview has no automatic retention cleanup or telemetry. The app does not call model APIs or include a model; agent subscriptions and hardware remain the owner's responsibility.

See [architecture](docs/DESIGN.md), [remote controller](docs/REMOTE.md), [branding](docs/BRANDING.md) and the `PROVENANCE*` files for implementation and attribution. CodaClauda is a separate project for agent-to-agent coordination. Both utilities are intended as ordinary public Marketplace listings; PhaseForge remains the featured app.

## Build from source

Use locked dependencies and `cargo-auditable` for release binaries. On this owner's laptop, every local Cargo build/test must pass through the unchanged shared guard:

```powershell
python -I D:/PhaseForge-claude/scripts/buildlock.py -- cargo auditable build --workspace --release --locked
npm ci --ignore-scripts
node scripts/prepare-ui.mjs
python scripts/prepare-bundle.py
python scripts/collect-notices.py
python -I D:/PhaseForge-claude/scripts/buildlock.py -- cargo auditable build --manifest-path src-tauri/Cargo.toml --release --locked
```

Before preparing the bundle, copy the independently built ARM64 link binary to `dist/linux-arm64/sparkdev-link` and the Windows CLI/link release binaries to `dist/bin/`. Keep build targets outside source. Bundle the existing desktop release with the Tauri NSIS bundler. No release should omit the link binary, notices or dependency inventory. Do not modify any PhaseForge checkout to build this project.

## License

Copyright 2026 Ben Van Dorn / DeloresAI. First-party source and original artwork are available under [MIT](LICENSE). Copied first-party PhaseForge code has the owner's explicit reuse/MIT authorization recorded in provenance; its original repository is unchanged. Third-party code retains its own licenses and notices.
