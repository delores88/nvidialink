# DGXBridge 0.5.0 verification

Status: candidate under physical verification; not yet published or installed.

The complete release includes default automatic result return, GPU video codecs, Blender 4.5.9 with Cycles CUDA/OptiX and OIDN, locked system packages in project images, offload-by-default agent prompts, and Windows Apps version repair. Files return without a download button.

## Evidence so far

- Real Spark Rust source tests: 156 passed, job `5a8ee0db559a4907b5ea99ee4ed8681e`. This is source validation, not installed automatic-return acceptance.
- Remote Python suite under WSL: 352 tests, 10 skipped, no failures.
- Spark controller deployment preserved the saved resource policy and pairing-link process. Runtime refresh and physical feature acceptance are separate.
- Windows compiler is waiting at the unchanged owner build gate. No installed 0.5.0 or Marketplace admission is claimed.

## Remaining release gates

Physical automatic return over pinned SSH and paired transport, detached/reconnect/chunk-resume and conflict checks; actual GPU encode/decode and ten frames each of Blender OptiX/CUDA/CPU; locked system-package prepare/use/reuse; Windows build, normal installation, state preservation and Apps version readback; independent complete draft review, immutable publication, Marketplace acceptance and exact served-installer verification.
