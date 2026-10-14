# DGXBridge updater provenance and authority

The Rust implementation in `src-tauri/src/updates.rs`, `update_verify.rs` and
`update_helper.rs` is first-party DGXBridge code under this repository's MIT
license, copyright Ben Van Dorn / DeloresAI. No referenced repository was edited.

The owner-approved sequence was adapted from PhaseForge at immutable commit
`ac30e04fbf0ddc03ae0144fb06b6350cacdce1fc`: `desktop/updater.cjs`,
`desktop/update-marketplace.cjs`, `desktop/update-approval.cjs`,
`desktop/update-storage.cjs`, `desktop/update_runner.py`, and their tests.
The pinned updater source SHA-256 is
`5d04b2100ee5c92390e9368165c5c6ad91ab119fff6396bea319fdae8ba0b832`.
The release agent separately confirmed that this updater method is unchanged
between shipped PhaseForge `8517a6eb6ef867c5bdc3eb26a345294b4a370160` and
candidate `7a665907901bd797df0eedee961e0d83a41eeabf`.
Ben explicitly authorized MIT reuse of his first-party PhaseForge code in this
product. Third-party licenses remain unchanged.

The retained behavior is exact approved identity, owner consent, bounded
hash/size-checked transfer, revalidation, a staged helper's exact READY handshake
before the original app exits, final verification before setup, durable receipts,
and honest separation of process relaunch from visible application acceptance.
PhaseForge's cookie session, proprietary agreement text, Python runtime,
database backup, research engine and stable-only version parser were not copied.
DGXBridge state remains in its existing location; remote jobs/services are not
stopped or changed by a desktop update.

Signed acquisition/review verification was adapted from DeloresAI Marketplace
commit `44c861d778e6880afebb34407a86798db39a9ada`, principally
`apps/desktop/src-tauri/src/metadata.rs`, `review.rs`, `nsis.rs`,
`services/api/src/catalog.ts`, `native-updates.ts`,
`packages/contracts/src/native-updates.ts`, and
`docs/NATIVE-UPDATE-AUTHORIZATION.md`. The Marketplace agent confirmed the
deployed creation schema accepts `acceptancePolicy: "immutable-release"`.
Browser approval uses the schema-7 native-update protocol, not a transferred
website session or the Marketplace client's bearer credentials.

`update-acquisition-keys.json` and `update-review-keys.json` are separate,
purpose-scoped local authority maps. Both currently contain the public
`deloresai-release-review-20260910` key confirmed by the Marketplace agent against
the tracked `apps/desktop/src-tauri/trusted-review-keys.json` at that commit.
The service uses that review authority for both the outer
`deloresai-native-acquisition` envelope and nested
`deloresai-release-review` envelope. Both signatures and purposes are required.
Neither map is the legacy full-lifecycle `trusted-keys.json` (which is empty).
These maps grant no full installation, migration or unattended-update authority.
Unknown keys fail closed; no remote key is automatically trusted.

Protocol 2 retains an immutable review's truthful historical scan dates while
requiring fresh, short-lived signed acquisition authorization. The verifier
binds app `dgxbridge`, publisher `idFUpIv8h0zRX8OxpjsAWgQk`, repository
`1418188832`, stable version, source commit, exact release/asset IDs, Windows x64
NSIS, SHA-256 and byte size. The only transfer is the fixed proof-bound
Marketplace native-update endpoint, never the GitHub URL carried as evidence.
An ambiguous exchange or any failed transfer requires new explicit browser
approval; neither one-use action is retried automatically.

The verifier and download capability exist only in memory and an anonymous pipe
to the staged helper. They are absent from renderer responses, settings, plans,
receipts and logs. The helper waits for the original process, revalidates with
the in-memory proof, checks the installer again under a file lock, and opens
ordinary interactive Windows setup. Packaged-host/MSIX execution and Codex
LocalCache installation contexts are refused. The owner must launch the
installed app or repair installer from ordinary Windows Explorer.
The helper also requires a non-elevated Windows process token, validates the
PE manifest's exact `asInvoker` / `uiAccess=false` privilege declaration with
the Marketplace's namespace-aware parser, and binds the last raw NSIS `/D=`
argument to the existing physical installation directory. It does not use
silent-install flags. Internet-zone download provenance is retained, and
relaunch/version confirmation uses the original executable path.

Synthetic signing keys in unit tests are deterministic fixtures and are not
production authority keys. Unit tests do not establish production browser
approval, a real update installation, data preservation or visible native
readiness. Those require separate receipts for the actual workflow.
