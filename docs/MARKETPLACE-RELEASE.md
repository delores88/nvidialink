# Marketplace manifest preparation

`scripts/prepare-manifest.py` prepares a release-specific manifest and verification receipt. It does not create a release, upload assets, modify another repository, submit a Marketplace draft or approve admission. Its command line has no validation bypass.

DGXBridge uses `dgxbridge` for both app ID and slug. Its Windows GUI executable is `dgxbridge.exe`, bundle identifier is `com.deloresai.dgxbridge`, and installed application directory is `DGXBridge`. It is a public, free, MIT first-party application with explicit free-use and Marketplace distribution authorization. Product version, canonical `vVERSION` tag, channel and GitHub prerelease state must agree. Version 0.2.0 uses the stable channel required by the integrated updater; earlier beta releases remain immutable. No `featured` property is emitted. The Marketplace's existing featured-app selection is independent and stays unchanged.

## Supply actual release inputs

1. Commit the generator, lockfiles, provenance, artwork, source and release documentation before creating the immutable source tag. Produce the final per-user Windows x64 NSIS installer, with GUI, CLI, link sidecar, ARM64 service and notices. Record installer hashes after every resource/signing change. The Windows installer requires preinstalled WebView2 and OpenSSH; its WebView bootstrap mode must remain `skip`.
2. Obtain the real publisher ID from the signed-in Marketplace publisher workspace. Create the matching GitHub release as a draft and upload the final NSIS installer. Read its real release ID and installer asset ID; never reuse another app's identity or invent numeric IDs. Generate and validate the manifest against those IDs, attach the complete manifest and release evidence, and verify every uploaded asset before publishing. Publishing before the manifest is attached can trigger an incomplete automatic intake.
3. Supply a real installed application screenshot without QR tokens, pairing codes, personal addresses, credentials or other private data. Publish it, the icon and thumbnail on approved permanent anonymous HTTPS image URLs. Raw GitHub URLs must contain an immutable 40-character commit, not a branch. This can be an artwork commit distinct from the release source commit. The source release tag must not be moved to add the generated manifest.
4. Copy `docs/release-input.template.json` to a private working file such as `.local/release-input.json`. Every `null` must be replaced with an observed value. Local file paths are resolved relative to that input file and are used only for verification; they are not copied into the public manifest or verification receipt. The Marketplace enforces `requirements.minimumOs` and `requirements.memoryGb` as compatibility floors. Use string `"0"` and numeric `0` when minimums have not been established; these mean "Not specified", not compatibility with every system. Record the actual test laptop separately in `testedHost`; the generator retains it in the validation receipt, outside the public manifest. Do not substitute either the laptop or Spark specifications for established minimums. Set positive `diskMb` from the installed application footprint rounded up in decimal MB, and disclose separately that user data, runtimes and temporary download/extraction space are additional. Set `screenshotsReviewed` only after inspecting the actual screenshots and confirming their content and redaction.

The template is intentionally invalid. It is not an example manifest and must not be uploaded under the production manifest filename.

## Generate and validate

Python 3.11+, Pillow, Git, GitHub CLI, Node and an existing Marketplace source checkout with its installed `tsx` dependency are required. The checkout is read-only: the generator imports its actual `manifestSchema` and `publicationArtworkIssues` without installing dependencies or editing any files there.

The release-specific validation receipt must record the actual current contract hashes, including support for optional feature notices. Use the deployed Marketplace checkout confirmed by its release owner; do not infer deployment from an older desktop checkout. The 0.2.0 integration was reviewed against Marketplace commit `44c861d778e6880afebb34407a86798db39a9ada`, including its schema 7 native-update authorization contract.

```powershell
python scripts/prepare-manifest.py --input .local/release-input.json --marketplace-checkout C:/src/DeloresAI --output dist/release/deloresai.manifest.json
```

The generator checks:

- Live repository identity, public visibility and MIT declaration; release/asset IDs; matching channel/prerelease state; immutable tag resolution; tagged GUI/package versions and MIT source license.
- The exact uploaded installer name, size, GitHub SHA-256 metadata and canonical release download URL against the local installer bytes.
- Anonymous image retrieval against the reviewed local bytes, raster format, image size/dimensions, approved host and immutable raw URLs. No GitHub credentials are sent to media servers. A byte/hash check does not establish that an image shows a real installed app; that remains the explicit screenshot review step.
- The runtime OCI digest pins and fixed wheel hashes from the tagged source. Canonical OCI manifest URLs represent the pinned manifest digest, not a tarball digest; Docker verifies each layer. Python wheel URLs are resolved by their exact declared SHA-256 from PyPI.
- Both current shared runtime manifest validation and the separate required-artwork checks. The receipt records the validator commit and source hashes.

It writes `deloresai.manifest.json` and adjacent `manifest-validation.json` only after all checks pass. It refuses to overwrite a different manifest at the same output path. No actual release manifest exists until the real inputs pass; a local structural fixture is not a release artifact.

The publisher ID's ownership is checked by the authenticated Marketplace publisher workflow, not by GitHub. The generator requires it but does not claim to verify that account relationship. A draft GitHub release is still inadmissible: after completing and publishing the release, rerun with `--require-published`, verify the same asset IDs and digests, then reconcile automatic intake before any manual submission or retry. Use the normal publisher workflow and Marketplace release verification. Local validation does not imply acceptance, malware/dependency clearance or native workflow success.

The Windows artifact declares the exact Marketplace-detected runtime name `Microsoft Edge WebView2`. OpenSSH remains a disclosed prerequisite for Spark connections rather than an unrecognized runtime label. Schema validation and a corrected label do not establish runtime detection on every host or automatic installation eligibility; those require separate native evidence.

## Shipped behavior and disclosures

The generated listing describes a hybrid utility with an owner-selected Spark, independent user services, pinned SSH, explicit phone approval and an optional public n0 relay. No personal machine address, private key path, owner VM or embedded model API is declared. Native laptop jobs run with owner permissions and are not sandboxed. Full offline operation is not claimed because the shared scheduler requires the Spark; existing prepared jobs can use the LAN without public internet.

CPU runtime provisioning pins its OCI base and Python wheels. Debian packages are resolved from their repositories during setup, then the final image ID is fixed in the local closed runtime registry. The manifest discloses this distinction rather than inventing package URLs or checksums. Dependency preparation is a separate owner-requested package-download operation with a domain allowlist. Optional CUDA provisioning uses its own OCI pin. The Marketplace reviewer must assess these runtime declarations together with the complete source and receipts.

State, logs, caches and pairing records persist until the owner removes bridge-owned data; Windows uninstall leaves remote services and those records intact. The preview has no automatic retention cleanup. Downgrade compatibility is not promised and backup is required. These statements must be revised if the implementation changes.

Before submission, retain clean source/tag identity, final installer/GUI/CLI/link hashes, Cargo-auditable metadata, third-party notices, actual installation/taskbar evidence and physical Spark job receipts. Distinguish owner phone, hotspot, sleep and agent-session acceptance from source compilation and synthetic tests. Neither a manifest nor an uploaded installer substitutes for those receipts.
