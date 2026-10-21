# Named model services

The 0.4.0 candidate lets an agent load a model once on the Spark and call a named
HTTP service repeatedly. It does not publish a TCP port. Calls travel over the
authenticated DGXBridge route and a private per-service request/reply spool.
This page describes the candidate source contract, not a published installer,
installed update or measured physical Spark TTS result.

## Start, call and stop

```powershell
sparkdev --client codex --json serve C:/work/tts --name tts --runtime cuda --memory 16 --request-id tts-start-001 -- python3 server.py
sparkdev --client codex --json wait tts --timeout 20
sparkdev --client codex --json logs tts --since 0
sparkdev --client codex --json call tts --body-json '{"text":"Hello from the Spark"}' --request-id tts-call-001 --save C:/work/output.wav
sparkdev --client codex --json cancel tts
```

Use `--input request.json --content-type application/json` instead of inline JSON
when shell quoting is inconvenient. Global `--json` controls output;
`--body-json` is the request body. `--path /endpoint` selects an HTTP path, and
`--port 8000` tells the worker which loopback port your program listens on.
`PORT` is supplied to the program. Bind your server to loopback, not a public
interface. The `serve` command returns without waiting for model initialization.

Retain start and call request IDs. After a connection interruption, repeat the
same request with the same ID and payload; changing a known ID's input is an
idempotency error. `call` polls in bounded requests up to its CLI timeout (default
300 seconds); the MCP tool waits at most 45 seconds and can return a waiting
request ID. A timeout is not permission to submit the same inference again under
a new ID. Responses are hash-checked, saved in local application state under
`replies/<name>/`, and optionally copied to `--save`. Small JSON can also appear
inline. CLI exit 0 means a 2xx application response; inspect other HTTP statuses
and the returned call state.

Names are lowercase, start with a letter, contain only letters, digits or hyphens,
and have at most 24 characters. They cannot start with `laptop`. Logs and wait
resolve the active service first, otherwise the latest one with that name.
Cancellation by name requires a live route and is never queued, so a delayed
cancel cannot stop a different future service. A job-ID cancel remains durable.

## Small HTTP adapter example

Save this 15-line example as `server.py` in the submitted project. It loads your
project's model adapter once; implement `tts_model.load_model()` to return a
callable that accepts text and returns complete WAV bytes. Choose and provision
that model and its locked dependencies yourself. DGXBridge supplies neither the
adapter nor model weights, and this example makes no model-quality/latency claim.

```python
import json, os
from http.server import BaseHTTPRequestHandler, HTTPServer
from tts_model import load_model
synthesize_wav = load_model()
class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200); self.end_headers()
    def do_POST(self):
        body = self.rfile.read(int(self.headers.get("Content-Length", "0")))
        audio = synthesize_wav(json.loads(body)["text"])
        self.send_response(200)
        self.send_header("Content-Type", "audio/wav")
        self.end_headers()
        self.wfile.write(audio)
HTTPServer(("127.0.0.1", int(os.environ["PORT"])), Handler).serve_forever()
```

Keep library download cache settings at their defaults so weights are reused from
`/cache`. Put additional large reusable files in `/cache/files`. This cache is
writable by all the owner's Spark jobs; do not store credentials there. HF tokens
and generated executable/module caches have job-private paths. Use an appropriate
provisioned runtime; CUDA is the default when available, otherwise Python.
Internet-enabled services use the same public proxy/private-network guard as jobs.

## Capacity and lifecycle

At most two services can exist concurrently. Each reserves two CPUs and 1–48 GiB
memory within the owner's total memory budget, default 16 GiB, with 2048 MiB
scratch. They count toward total CPU/memory and headroom, while batch per-job and
per-client quotas do not block them. Build caches are read-only in a service.
CUDA unified memory still needs the controller's memory watchdog; a container
memory limit alone does not establish a hard GPU memory cap on the Spark.

The default idle stop is 30 minutes, configurable with `--idle-minutes` from 5 to
240. Idle requires no waiting calls. Lifetime is eight hours and startup is capped
at 30 minutes. The worker also terminates its process group after lifetime plus
60 seconds if the scheduler is unavailable. Low disk, memory pressure, too much
reply data, explicit stop and replacement have separate stop reasons.

A next call may wake a service stopped for idle, lifetime or internet-off within
24 hours if its snapshot still exists. It does not wake a service explicitly
stopped by the owner, a crash, a watchdog failure or a service that never became
ready. Resume an explicit stop with `serve`. An identical active name/request
reuses the service. Automatic replacement requires the same client and project;
otherwise use another name or explicitly pass `--replace`. Unanswered calls are
not moved into a different replacement's code.

## Limits and retained data

There are at most 32 waiting calls, 1 MiB input per call and 256 MiB output.
The service reply spool is limited to 2 GiB or 10,000 entries. A response is read
in chunks of at most 2 MiB and hashed by the controller, which does not trust
container-reported sizes/hashes or follow reply symlinks/FIFOs/hardlinks. Local
loopback forwarding uses `http.client`, independent of proxy environment settings.

Calls and replies may contain sensitive text/audio. Do not submit credentials.
Spark payload files become eligible for cleanup at the earlier of completed
delivery plus 10 minutes or creation plus one hour. Idempotency records become
eligible 24 hours after creation. Actual deletion requires successful controller
housekeeping; a stopped or delayed controller or cleanup errors can extend
on-disk retention. A retry after a payload expires reports
expiry rather than silently executing it again. Saved laptop replies have no
automatic deletion. Source tests do not establish these retention limits on an
installed Spark without a corresponding physical receipt.

Physical acceptance still needs first/repeat TTS latency over LAN and the paired
route, private-network probes, large downloads, cache/token placement and the GB10
memory-pressure test. Local fake-Docker and Linux filesystem tests are separate
evidence; no speedup or remote-network success is implied by this implementation.
