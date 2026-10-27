# Multiple compute computers

DGXBridge 0.6 adds named Linux and Windows-with-WSL computers alongside the existing laptop and Spark. The existing Spark connection and its pairing, policy, jobs, and returned files keep their original state. Additional computers use independent profiles, controller state, runtimes, and return workers.

## Register a trusted connection

Prepare the computer's SSH server, verify its host public key through an independent owner-controlled channel, and provision Docker on its Linux environment. Keep private keys on the submitting computer. Registration stores SSH references and never copies private keys.

```powershell
sparkdev --client codex --json node add render-box --label "Render computer" --ssh-alias render-box --host-key-alias render-box --ssh-config C:/Users/Example/.ssh/render-box.conf --user builder
sparkdev --client codex --json node list
```

For a Windows SSH endpoint, also identify the existing WSL distribution, its Linux user, and the controller path:

```powershell
sparkdev --client codex --json node add windows-gpu --label "GPU workstation" --ssh-alias windows-gpu --host-key-alias windows-gpu --ssh-config C:/Users/Example/.ssh/windows-gpu.conf --user example --wsl-distro Ubuntu-24.04 --wsl-user builder --controller-path /home/builder/sparkdev/controller.py
```

The WSL distribution and user must already exist. The endpoint runs a fixed validated WSL command. Linux controller paths are not passed to Windows SFTP; this endpoint uses verified RPC chunks for input and result transfer.

For NVIDIA GPU jobs on WSL, verify a small capped container probe as well as `nvidia-smi` on the host. Older Docker Desktop releases can omit a driver library required by newer Windows NVIDIA drivers, causing CUDA initialization error 500 even when CUDA works directly in WSL. This was reproduced on the test workstation with Docker Desktop 4.29. The upstream diagnosis and fix are tracked in [NVIDIA container-toolkit issue 520](https://github.com/NVIDIA/nvidia-container-toolkit/issues/520). Update Docker Desktop through its normal installer; do not register a runtime whose container probe failed.

In the desktop app, **Computers** provides the same registration, enable/disable, and installation controls. The existing Spark connection screen remains separate. Saving a computer or refreshing the board does not install software or prepare runtimes.

## Install this release's controller and runtimes

Use the installed application's inspected resource directory as the bundle source:

```powershell
sparkdev --client codex --json --device render-box bootstrap --bundle "C:/Program Files/DGXBridge"
sparkdev --client codex --json --device windows-gpu bootstrap --bundle "C:/Program Files/DGXBridge" --cuda
```

The desktop **Install bridge** and **Prepare GPU** actions supply its own resource directory. Setup validates the closed file set, sizes, hashes, and bootstrap source before contacting the selected computer. GPU preparation is explicit. Additional-computer setup installs the independent controller without copying the ARM64 paired-link executable. Its network path is pinned SSH; existing Spark pairing continues through the original configuration.

Setup returns a scheduled/building receipt when runtime preparation continues in the background. Inspect the selected device and wait for its runtime registry and scheduler to become ready before submitting work. A setup receipt alone does not prove that Docker, GPU execution, or returned files work.

The CPU runtime supports verified ARM64 and AMD64 images. The AMD64 CUDA runtime provides PyTorch compute and its separately observed video capabilities. AMD64 Blender is not provisioned by this recipe; the planner must not infer Blender availability from the name `cuda`.

## Observe, select, and retain the job's computer

```powershell
sparkdev --client codex --json devices
sparkdev --client codex --json status --all
sparkdev --client codex --json --device windows-gpu devices
sparkdev --client codex --json --device windows-gpu run C:/work/project --runtime cuda --detach --request-id gpu-example-001 -- python3 main.py
```

The returned public job ID for a named computer is qualified, for example `windows-gpu:JOB_ID`. Retain that entire ID:

```powershell
sparkdev --client codex --json wait windows-gpu:JOB_ID --timeout 20
sparkdev --client codex --json logs windows-gpu:JOB_ID --since 0
sparkdev --client codex --json logs windows-gpu:JOB_ID --since NEXT_OFFSET
sparkdev --client codex --json outputs windows-gpu:JOB_ID
```

Results and changed project files return automatically through the original profile. Keep polling while a job is `returning`, inspect its final workload exit code, and retain returned-file hashes. Disabling new jobs on a computer preserves its profile, jobs, cancellation routing, and pending result delivery:

```powershell
sparkdev --client codex --json node enable windows-gpu off
sparkdev --client codex --json node enable windows-gpu on
```

With enabled additional computers registered, `run --where auto` considers compatible authenticated schedulers. Explicit `--where laptop` and `--where spark` retain their existing meaning. `--device DEVICE` makes the choice explicit; `--device auto` requests capability selection. Windows-only native commands stay on the laptop. A named WSL computer executes Linux workloads through its approved container runtimes.

Optional `--min-memory-gb` and `--min-vram-gb` constrain observed job-usable memory. Unknown memory does not satisfy a requirement. A discrete GPU's framebuffer is separate from system RAM; the reported system memory is the Linux environment's observed capacity, which can be smaller than the Windows host's installed RAM. Capacity observations are not reservations. Each selected controller still performs admission and resource enforcement. GPU VRAM limits are not enforced by a Docker system-memory cap.

`plan` remains a source-validation and placement preview: it does not connect, upload, reserve, claim, or submit. Automatic device choice is finalized against live observations at submission.

## Retry and isolation rules

The fleet journal binds the request ID, exact request/source fingerprint, selected profile, and SSH identity before upload or submission. Retry an uncertain submission with the same request ID. A retry cannot move to a different computer because load changed. Host-key verification continues to use the owner's configured known-hosts trust files; their contents are not copied into the fleet journal. Review outstanding work before manually rotating those trust files. A source edit between selection and the child's inventory is refused before submission.

The owner-local fleet claim prevents overlapping requests for the same worktree across the new dispatcher. Unknown submission outcomes retain the claim. A completed workload with pending file delivery retains its claim until delivery reaches a verified terminal state. Old pre-fleet clients cannot participate in the new owner's local claim journal; retained legacy jobs and receipts are checked conservatively before a new cross-device submission.

Every profile has its own config, requests, cancellations, CAS state, return receipts, and worker lock. The dispatcher sets `SPARKDEV_HOME` only in child processes; MCP requests never mutate the server process's environment. Qualified job IDs and retained request mappings keep operations on their original controller after process restart.

On Windows, 0.6.1 recognizes an ordinary drive path and a literal loopback administrative-share path as the same retained state directory only after comparing their actual volume and file identifiers. This allows a packaged coding agent and the ordinary desktop app to inspect the same jobs. It does not rewrite retained paths, SSH identity, requests or worker state. Different directories, remote SMB aliases, reparse ancestors and unknown identifiers do not establish an alternative identity. Keep the original source-project path spelling stable: project IDs and request fingerprints retain the existing canonical source-path convention; this state-directory correction does not merge separately spelled project identities.

Fleet child responses have a separate 40 MiB limit because local returned-file receipts can be larger than the unchanged 8 MiB SSH wire limit. Exceeding the local limit reports `local_metadata_limit` and `observation_limited`, with unknown reachability. The desktop shows unavailable details instead of claiming a network failure. Saved receipts and files are retained. Combined histories above this finite limit still need future pagination.

Separate jobs can execute on separate computers concurrently. Each job still runs on one computer; distributed model training and cross-host GPU memory pooling are separate capabilities.

## Verification

Regression coverage includes legacy no-profile behavior, immutable request/target routing, interrupted submission recovery, claim retention during return, profile identity changes, strict SSH pinning, WSL argument validation, discrete VRAM fit, inspected bundle boundaries, actual CLI subprocesses, and additive desktop UI behavior at both supported widths. Source tests, physical job execution, native installation, off-LAN networking, and Marketplace admission are separate evidence layers.

Physical acceptance and remaining boundaries are recorded in [the 0.6 multi-computer validation receipt](VALIDATION-0.6-MULTIDEVICE.md).
