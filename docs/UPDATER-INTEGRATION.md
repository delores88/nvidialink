# Native updater integration handoff

For ClaudaCoda, based on the frozen DGXBridge 0.2.0 updater sources on
2026-10-14. The release owner will add the immutable source commit after commit;
this document does not identify an uncommitted snapshot as a released artifact.
Reuse is MIT first-party code with the attribution and upstream pins in
[UPDATER-PROVENANCE.md](UPDATER-PROVENANCE.md). No PhaseForge or Marketplace
repository needs modification.

## Modules and dependencies

| Source | Responsibility |
| --- | --- |
| `src-tauri/src/updates.rs` | Native authorization state, fixed-origin HTTP, serialized actions, safe renderer projection, bounded streaming download |
| `src-tauri/src/update_verify.rs` | Stable artifact selection; separate acquisition/review signature verification; identity, review, expiry and replay checks |
| `src-tauri/src/update_helper.rs` | Same-binary helper, READY handshake, job admission guard, physical installation checks, interactive setup and relaunch receipts |
| `src-tauri/src/update-acquisition-keys.json` | Locally trusted acquisition authority map |
| `src-tauri/src/update-review-keys.json` | Separate locally trusted automated-review authority map |

Use the resolved lockfile with these direct versions from `src-tauri/Cargo.toml`:
Tauri 2.12.3 / tauri-build 2.7.2, serde 1.0.229, serde_json 1.0.152,
base64 0.22.1, reqwest 0.12.28 (`default-features=false`, `rustls-tls`, `json`,
`stream`), sha2 0.10.9, ed25519-dalek 2.2.0, chrono 0.4.45 (`serde`),
semver 1.0.28, url 2.5.8, zeroize 1.9.1, getrandom 0.3.4,
quick-xml 0.42.0, rustls 0.23.46 (`default-features=false`, `ring`, `std`).
Tokio 1.53.2 enables `process`, `time`, `io-util`, `sync`, `fs`, `macros`,
`rt-multi-thread`; tests use tempfile 3.27.0. DGXBridge also uses its local
`sparkdev` library for the state/job adapters below. No Python helper is needed.

## Tauri and renderer contract

Declare the three modules. Call `update_helper::run_if_requested()` before
constructing Tauri, returning immediately when it handles the helper flag.
Manage `updates::UpdateState::default()` and call
`update_helper::confirm_launch()` during setup. Register a main-window-only
`update_action(app, window, action)` command delegating to
`updates::action(&app, &action).await`.

The exact actions are `check`, `authorize`, `poll`, `download`, `install`,
and `status`. Mutating actions start asynchronously and return a JSON snapshot;
the renderer must inspect `busy`, disable duplicate mutations while it is true,
and continue polling `status`. Approval polling is a separate `poll` action;
the native implementation enforces at least five seconds between exchanges.
Do not interpret a returned action snapshot as completion.

Public snapshot fields are `ok`, `status`, `busy`, `current_version`,
`available_version`, `message`, optional `verification_code`, `verification_uri`,
`expires_at`, `progress_bytes`, `total_bytes`, and a bounded `last_update` receipt.
States are `idle`, `checking`, `current`, `available`, `awaiting_approval`,
`approved`, `downloading`, `ready`, `installing`, `denied`, `expired`, `error`.
The receipt projection contains only status/message, versions, installer exit
code and timestamps/readiness; it omits local paths and authorization material.
Never add the verifier, grant, raw request or helper plan to renderer output.

The current DGXBridge command wrapper serializes pairing starts and install
starts with the same lifecycle lock. Its pairing/bootstrap controls also reject
an in-flight update. ClaudaCoda must bind equivalent lifecycle guards to its own
long-lived tasks; copying only the command name is insufficient.

## Authorization and installation sequence

1. Discover at `GET /api/v1/catalog/dgxbridge?acceptancePolicy=immutable-release`
   on fixed origin `https://deloresai.com`. Select only a strictly newer stable
   Windows x64 NSIS artifact for the exact pinned product/publisher/repository.
2. Generate a 32-byte random verifier, encode canonical base64url without padding,
   and hash its ASCII text for S256. Create `/api/v1/native-updates` for the exact
   artifact with `acceptancePolicy: "immutable-release"`. Keep the verifier only
   in zeroizing native memory. Open the validated returned approval URL in the
   system browser; account authentication and consent remain there.
3. Exchange proof through `/api/v1/native-updates/ID/poll`. Verify both signed
   envelopes and their purposes/bindings. The one-use download grant stays in
   memory. An uncertain exchange requires new explicit browser approval.
4. Stream only the fixed `/api/v1/native-updates/ID/download` route with the
   bearer grant and `X-Native-Update-Proof`; no redirects, range/HEAD retry,
   cookies, GitHub fallback or capability query parameters. Verify exact size
   and SHA-256, then atomically finalize the staged installer. Failed transfer
   consumes the local authorization and requires a fresh request.
5. Revalidate current eligibility, copy the current executable as the helper,
   and send the verifier over an anonymous stdin pipe. The nonsecret plan binds
   the original physical executable, parent PID, installer and signed artifact.
   Require the exact `DGXBRIDGE_UPDATE_READY` line within 20 seconds before the
   original application exits. Failure before READY keeps the app open.
6. The helper validates its own session/plan, holds local admission and update
   locks, confirms and waits for the exact parent process, and revalidates again.
   It retains Internet-zone provenance, locks the installer against writes and
   deletion, rehashes it, verifies the reviewed NSIS header and namespace-aware
   PE manifest (`asInvoker`, `uiAccess=false`), and checks approval expiry again.
7. Refuse MSIX/packaged or elevated execution. Launch interactive setup with
   the existing physical install directory as the final raw NSIS `/D=` argument,
   without silent flags. COM is initialized on the helper/browser shell thread.
   Wait for setup, record the exit code, then reopen the exact original path
   unless that executable is already running. A failure after app shutdown
   attempts the same recovery launch.
8. On startup, confirm the running version and original executable path against
   the helper receipt. A bounded retry covers NSIS Finish launching the app
   before setup exits. `visible_readiness` remains `unverified`: process/version
   confirmation is not native UI acceptance.

Requests expire in ten minutes; download grants last at most five minutes and
never outlive the request. Acquisition envelopes must be fresh. Protocol 2
permits the truthful historical immutable-review dates; it does not waive fresh
authorization, current admission, signature or exact artifact checks. Unknown
authority keys fail closed. Both currently bundled maps contain the confirmed
public review key, but remain separate purpose-scoped maps. They confer no
full-lifecycle or unattended-install authority.

## Product and state adapters that must change for ClaudaCoda

Do not carry DGXBridge's app identity into another product. Replace and verify
`APP=dgxbridge`, publisher `idFUpIv8h0zRX8OxpjsAWgQk`, repository `1418188832`,
GitHub owner/repository `delores88/nvidialink`, filename
`DGXBridge_VERSION_x64-setup.exe`, tag `vVERSION`, executable `dgxbridge.exe`,
product messages/User-Agent, helper flag and READY token. Keep the common fixed
Marketplace origin and policy only while its deployed contract supports them.
Verify the destination product's actual publisher, immutable release/asset IDs,
source commit, version/channel and NSIS artifact in its own signed response.

Replace the `sparkdev::state::root()` adapter with the destination product's
existing stable owner state location. DGXBridge uses its existing state root
(including explicit `SPARKDEV_HOME`); its Windows default resolves the unredirected
LocalAppData known folder. The updater uses only an `updates` child directory,
`atomic_json` for receipts, and a locked sequence/payload/time watermark.
It does not copy/import PhaseForge databases, migrate jobs, rotate keys, restart
remote services or remove configuration, pairing identity, logs or projects.

Replace `laptop_guard()` deliberately: it locks `laptop-admission.lock`, reads
`sparkdev::local::status()`, and rejects any job for which `sparkdev::terminal`
is false. The lock stays held through installation/relaunch. Reuse the
destination scheduler's real admission lock and terminal-state test so an
update cannot race new local work. Preserve the destination's existing state
in place and validate its own installer/uninstaller behavior; this adapter is
not a generic database migration or rollback system.

## Frozen source inventory and evidence boundary

These SHA-256 values identify the reviewed source bytes, not compiled artifacts.
The source commit will be supplied separately by the release owner.

| Path | Bytes | SHA-256 |
| --- | ---: | --- |
| `src-tauri/src/updates.rs` | 25792 | `17cbc0e9b9fc51ee54e6ea5597f04badd81b5c3479ce12d9b7c498fd30303d0e` |
| `src-tauri/src/update_verify.rs` | 22494 | `46766689aa880392dfa8a4dd004a4daf3b58c0285bae2c4c26d1bed3a42d5bf8` |
| `src-tauri/src/update_helper.rs` | 40949 | `86aadb54c0d941f6ee3ae0679436e518df80df47423fa231d723cd3ddba7b674` |
| `src-tauri/src/update-acquisition-keys.json` | 85 | `011c1681d43fef3cb7d53d459677236a3f36427b6a107cb93f1235595a980b40` |
| `src-tauri/src/update-review-keys.json` | 85 | `011c1681d43fef3cb7d53d459677236a3f36427b6a107cb93f1235595a980b40` |
| `docs/UPDATER-PROVENANCE.md` | 4774 | `b01bd0c01850100b22e7bf6fab02e78f0efecfc38653af2f3e3e504dadf266b6` |

The coordinating agent reported 17 desktop tests passing before the final COM
and lifecycle-glue changes; its final guarded rerun was pending when this
handoff was written. Tests cover synthetic signatures/bindings/replay,
proof/expiry handling, a real loopback HTTP transfer, byte tampering, manifest
privileges/namespaces, paths, NSIS destination and Windows Internet-zone data.
Synthetic keys are fixtures, never production authority.

No production browser authorization, later-version upgrade, installer execution,
preserved-state comparison or visible installed UI acceptance is established
by these tests or this handoff. Those require separate actual workflow receipts.
The initial upgrade from beta.2 must use ordinary Explorer because beta.2 has
no updater. Local Cargo validation must use the owner's unchanged build gate;
build artifacts remain on C:. Earlier release receipts must not be overwritten.
