# DGXBridge updates

The **App updates** control sits at the bottom of the navigation drawer, following PhaseForge's owner-approved update flow. It discovers newer accepted stable Windows x64 NSIS artifacts for this exact DGXBridge repository and publisher. Beta releases and other products are ineligible.

1. Choose **Check for updates**.
2. Choose **Approve in Marketplace**, compare the displayed code in your system browser, sign in there if needed, and approve the exact release.
3. Choose **Download verified update**. The app verifies the acquisition envelope and nested automated review using locally bundled public authorities, exact identity, size and SHA-256.
4. Finish active laptop jobs, then choose **Install update**. The native helper confirms readiness before DGXBridge closes, revalidates current Marketplace admission and rehashes the installer before interactive Windows setup. Detached Spark jobs and services are left running.

Proof and one-use download capabilities stay in native process memory. They are not browser cookies, persistent account tokens or installation authority. An uncertain or interrupted one-use exchange requires a fresh explicit approval. Unknown keys, expired authorization, changed or revoked artifacts, mismatched bytes and unavailable verification stop installation. No fallback to an unreviewed GitHub download is permitted.

Configuration, pairing identity, job receipts and logs remain in their existing owner state directory. The installer changes app files; it does not provision or restart Spark services. A staged helper keeps a bounded local result receipt. Starting the expected new app confirms its version separately from visible UI readiness. Failed handoff keeps the app open; a failure after shutdown attempts to reopen the existing executable and reports the failure.

On Windows, start DGXBridge from its normal Explorer/Start-menu shortcut. Installation from an inherited packaged host such as Codex is refused because that context can redirect AppData and registry writes into a private package cache. The initial upgrade from beta2 must use a normal Explorer-launched installer because beta2 has no updater.

Automated Marketplace review does not certify the complete update lifecycle. Unit/transfer/signature tests, a real browser authorization, actual installer execution, preserved-state checks and visible installed-app verification are distinct evidence. The first updater-capable release cannot prove a later-version upgrade merely by reporting “No newer release.”

See [updater provenance](UPDATER-PROVENANCE.md) and the version-specific release receipt for exact tested scope.
