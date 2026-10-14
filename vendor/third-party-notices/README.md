# Pinned supplemental third-party notices

These files complete license-text evidence omitted from 14 published crate archives. They do not change any dependency's license or the MIT license of Spark Dev Bridge's own code.

`PROVENANCE.json` binds each package version to the exact upstream commit recorded in its published `.cargo_vcs_info.json`, the source URL and SHA-256 of each retained notice. Twelve packages use license texts copied directly from that upstream commit.

Two supplements are explicitly different:

- `enum-assoc` 1.4.1 declares `MIT OR Apache-2.0` in its exact package manifest, but supplies no license-text file in either the archive or recorded repository commit. This distribution selects the Apache-2.0 alternative and includes its full text from the Apache Software Foundation, alongside the unchanged upstream declaration and author attribution. The canonical license text is not claimed to be a file from the enum-assoc repository.
- `selectors` 0.38.0 declares `MPL-2.0`, including the notice in its source header, but omits the full root license text. Its supplement includes Mozilla's canonical MPL 2.0 text, the unchanged upstream declaration/header, and a notice with the exact public crate source URL, archive checksum and upstream source commit. No selectors source changes are made by Spark Dev Bridge.

`scripts/collect-notices.py` verifies every supplemental hash and upstream commit before copying these files into the installer notices. It fails if any resolved package still lacks text evidence. Its dependency inventory deliberately covers the union of locked Cargo packages on the supported build targets, including build and development dependencies; the auditable binary metadata separately identifies compiled dependencies.

The source files retained beside notices are evidence of the original third-party declarations and remain under their upstream licenses. They are not compiled from this directory. Official license texts and copyright notices are retained verbatim.
