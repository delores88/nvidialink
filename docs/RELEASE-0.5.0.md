# DGXBridge 0.5.0 verification

Status: candidate under physical verification; not yet published or installed.

The complete release includes default automatic result return, GPU video codecs, Blender 4.5.9 with Cycles CUDA/OptiX and OIDN, locked system packages in project images, offload-by-default agent prompts, and Windows Apps version repair. Files return without a download button.

## Evidence so far

- Real Spark Rust source tests: 159 library tests and one return-worker test passed, job `82db297ba22541f1aa8bf50161cac0c2`; cargo-auditable ARM release binaries were produced. This is source validation, not installed automatic-return acceptance.
- Remote Python suite under WSL: 352 tests, 10 skipped, no failures.
- Real Spark media job `48194f8be5a54600aed89ea94493ff0d`: H.264, HEVC and AV1 NVENC each encoded ten 1080p frames; explicit CUVID decoding returned ten frames for each codec.
- Blender 4.5.9 LTS rendered ten frames each with OptiX (`88dfd4eb2e8142da8c78ec4db461829d`), CUDA (`c0045e8afaf44b47a1757c612b5aa71b`) and CPU (`2809eef2a11848ad84fdb7a05f448629`). Both GPU logs record OpenImageDenoise running on GB10 for all ten frames. These jobs used the isolated DGXBridge CUDA image ending `79076222`, which reuses the existing immutable Blender image. They do not prove compilation of the fallback Blender recipe.
- Media outputs were inspected through the pinned chunk RPC and fully hash-verified by a private release harness using the installed 0.4.2 CLI. This does not establish automatic Windows delivery.
- Signed APT resolution produced an exact 41-package lock in job `17e48dd9d6ca48239a449ddcc0831e31`. The first preparation attempt was cancelled after Docker rejected its log configuration. The fix disables compression for a single log file and bounds confirmed start failures; its focused suite passed 34 tests on Windows and 34 under WSL. Physical preparation/use/reuse still needs a new run.
- Spark controller deployment preserved the saved resource policy and pairing-link process. Runtime refresh and physical feature acceptance are separate.
- Initial Windows source tests passed 157 tests. The final compiler/package run is waiting at the unchanged owner build gate. No installed 0.5.0 or Marketplace admission is claimed.

## Remaining release gates

Physical automatic return over pinned SSH and paired transport, detached/reconnect/chunk-resume and conflict checks; locked system-package prepare/use/reuse; Windows build, normal installation, state preservation and Apps version readback; independent complete draft review, immutable publication, Marketplace acceptance and exact served-installer verification. A paired connection test is not phone-hotspot verification.
