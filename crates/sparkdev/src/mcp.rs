//! Bounded stdio MCP transport; all execution uses the same CLI core.
use serde_json::{Value, json};
use std::io::{self, BufRead, Write};

const MAX_LINE: usize = 2 * 1024 * 1024;

fn schema(properties: Value, required: &[&str]) -> Value {
    json!({"type":"object","properties":properties,"required":required,"additionalProperties":false})
}

fn tools() -> Value {
    let job = json!({"type":"string","minLength":1});
    json!([
      {"name":"devices","description":"Observe laptop and Spark availability before submitting work. Reports authenticated connection route, reachability failures, scheduler access and known capacity; unknown capacity remains unknown. This is an observation, not a reservation.","inputSchema":schema(json!({}), &[]),"annotations":{"readOnlyHint":true,"idempotentHint":true,"openWorldHint":true}},
      {"name":"plan","description":"Preview job placement and validate source and exact argv without uploading files, claiming a worktree or launching a job. Does not verify runtime availability or reserve capacity; use devices for current connectivity.","inputSchema":schema(json!({"dir":{"type":"string","minLength":1},"argv":{"type":"array","items":{"type":"string"},"minItems":1},"where":{"type":"string","enum":["auto","laptop","spark"],"default":"auto"},"runtime":{"type":"string"}}), &["dir","argv"]),"annotations":{"readOnlyHint":true,"idempotentHint":true,"openWorldHint":false}},
      {"name":"prepare","description":"Sync a project and submit fixed locked dependency preparation on Spark for Rust, Node or Python. No caller commands accepted. Retain the detached job ID and poll wait/logs to completion before running an offline workload. Reuse request_id when retrying.","inputSchema":schema(json!({"dir":{"type":"string","minLength":1},"runtime":{"type":"string","enum":["rust","node","python"]},"request_id":{"type":"string"}}), &["dir","runtime"]),"annotations":{"readOnlyHint":false,"destructiveHint":false,"idempotentHint":false,"openWorldHint":true}},
      {"name":"run","description":"Sync a project and submit a detached build/test job. Returns durable job ID; follow with wait/logs. Laptop commands run natively with owner permissions and can change files outside the project. Reuse request_id to retry without duplicate execution.","inputSchema":schema(json!({"dir":{"type":"string","minLength":1},"argv":{"type":"array","items":{"type":"string"},"minItems":1},"where":{"type":"string","enum":["auto","laptop","spark"],"default":"auto"},"runtime":{"type":"string"},"request_id":{"type":"string"},"no_cache":{"type":"boolean","default":false}}), &["dir","argv"]),"annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":false,"openWorldHint":true}},
      {"name":"wait","description":"Bound the remote job wait to timeout seconds (0–60) and return status and exit code when terminal. Connection and queued-cancellation delivery add overhead. Reconnection also delivers cancellations previously queued by the owner.","inputSchema":schema(json!({"job":job,"timeout":{"type":"integer","minimum":0,"maximum":60,"default":10}}), &["job"]),"annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":true,"openWorldHint":true}},
      {"name":"logs","description":"Read and save a bounded chunk of retained output from a byte offset; next_offset supports lossless repeated polling. Reconnection also delivers cancellations previously queued by the owner.","inputSchema":schema(json!({"job":job,"since":{"type":"integer","minimum":0,"default":0}}), &["job"]),"annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":true,"openWorldHint":true}},
      {"name":"status","description":"Read the shared job board for Claude Code, Codex and human jobs on the laptop and Spark, with transport and cache details. Reconnection also delivers cancellations previously queued by the owner.","inputSchema":schema(json!({}), &[]),"annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":true,"openWorldHint":true}},
      {"name":"cancel","description":"Cancel an owned job, or queue its cancellation for the next authenticated connection when disconnected.","inputSchema":schema(json!({"job":job}), &["job"]),"annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":true,"openWorldHint":true}}
    ])
}

fn required_string<'a>(v: &'a Value, name: &str) -> Result<&'a str, String> {
    v.get(name)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty() && !s.contains('\0'))
        .ok_or_else(|| format!("{name} must be a nonempty string without NUL"))
}

fn number(v: &Value, name: &str, default: u64, max: u64) -> Result<u64, String> {
    match v.get(name) {
        None => Ok(default),
        Some(n) => n
            .as_u64()
            .filter(|n| *n <= max)
            .ok_or_else(|| format!("{name} must be an integer from 0 to {max}")),
    }
}

fn cli_args(name: &str, v: &Value, client: &str) -> Result<Vec<String>, String> {
    let allowed: &[&str] = match name {
        "run" => &["dir", "argv", "where", "runtime", "request_id", "no_cache"],
        "plan" => &["dir", "argv", "where", "runtime"],
        "prepare" => &["dir", "runtime", "request_id"],
        "wait" => &["job", "timeout"],
        "logs" => &["job", "since"],
        "status" | "devices" => &[],
        "cancel" => &["job"],
        _ => return Err("Unknown tool".into()),
    };
    let object = v.as_object().ok_or("arguments must be an object")?;
    if object.keys().any(|k| !allowed.contains(&k.as_str())) {
        return Err("Unexpected tool argument".into());
    }
    let mut out = vec![
        "--json".into(),
        "--client".into(),
        client.into(),
        name.into(),
    ];
    match name {
        "run" | "plan" | "prepare" => {
            let dir = required_string(v, "dir")?;
            if dir.starts_with('-') {
                return Err("dir must be an absolute or dot-relative project path".into());
            }
            out.push(dir.into());
            if name == "run" {
                out.push("--detach".into());
            }
            if name == "prepare" {
                let runtime = required_string(v, "runtime")?;
                if !["rust", "node", "python"].contains(&runtime) {
                    return Err("prepare runtime must be rust, node or python".into());
                }
                out.extend(["--runtime".into(), runtime.into()]);
                if v.get("request_id").is_some() {
                    out.extend([
                        "--request-id".into(),
                        required_string(v, "request_id")?.into(),
                    ]);
                }
                return Ok(out);
            }
            let location = v
                .get("where")
                .map(|_| required_string(v, "where"))
                .transpose()?
                .unwrap_or("auto");
            if !["auto", "laptop", "spark"].contains(&location) {
                return Err("Invalid where value".into());
            }
            out.extend(["--where".into(), location.into()]);
            for (key, flag) in [("runtime", "--runtime"), ("request_id", "--request-id")] {
                if v.get(key).is_some() {
                    out.extend([flag.into(), required_string(v, key)?.into()]);
                }
            }
            match v.get("no_cache") {
                Some(Value::Bool(true)) => out.push("--no-cache".into()),
                Some(Value::Bool(false)) | None => {}
                _ => return Err("no_cache must be boolean".into()),
            }
            let argv = v
                .get("argv")
                .and_then(Value::as_array)
                .filter(|a| !a.is_empty())
                .ok_or("argv must be a nonempty string array")?;
            out.push("--".into());
            for arg in argv {
                out.push(
                    arg.as_str()
                        .filter(|s| !s.contains('\0'))
                        .ok_or("argv entries must be strings without NUL")?
                        .into(),
                );
            }
        }
        "status" => out.push("--all".into()),
        "devices" => {}
        _ => {
            let job = required_string(v, "job")?;
            if job.starts_with('-') {
                return Err("Invalid job ID".into());
            }
            out.push(job.into());
            if name == "wait" {
                out.extend([
                    "--timeout".into(),
                    number(v, "timeout", 10, 60)?.to_string(),
                ]);
            }
            if name == "logs" {
                out.extend([
                    "--since".into(),
                    number(v, "since", 0, u64::MAX)?.to_string(),
                ]);
            }
        }
    }
    Ok(out)
}

fn error(id: Value, code: i32, message: &str) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
}

pub fn serve(client: &str) -> io::Result<()> {
    if !["claude", "codex", "human"].contains(&client) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid MCP client",
        ));
    }
    let stdin = io::stdin();
    let mut input = stdin.lock();
    let stdout = io::stdout();
    let mut output = stdout.lock();
    let mut initialized = false;
    loop {
        let mut bytes = Vec::new();
        // take() bounds allocation even for a malformed client without newlines.
        use std::io::Read;
        let count = (&mut input)
            .take((MAX_LINE + 1) as u64)
            .read_until(b'\n', &mut bytes)?;
        if count == 0 {
            break;
        }
        if bytes.len() > MAX_LINE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "MCP message exceeds limit",
            ));
        }
        let req: Value = match serde_json::from_slice(&bytes) {
            Ok(v) => v,
            Err(_) => {
                writeln!(output, "{}", error(Value::Null, -32700, "Invalid JSON"))?;
                output.flush()?;
                continue;
            }
        };
        let id = req.get("id").cloned();
        let method = req.get("method").and_then(Value::as_str).unwrap_or("");
        if req.get("jsonrpc").and_then(Value::as_str) != Some("2.0") || !req.is_object() {
            writeln!(
                output,
                "{}",
                error(
                    id.unwrap_or(Value::Null),
                    -32600,
                    "Invalid JSON-RPC request"
                )
            )?;
            output.flush()?;
            continue;
        }
        if id.is_none() {
            continue;
        }
        let id = id.unwrap();
        let params = req.get("params").cloned().unwrap_or(json!({}));
        let response = match method {
            "initialize" => {
                initialized = true;
                let requested = params
                    .get("protocolVersion")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let version = if ["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"]
                    .contains(&requested)
                {
                    requested
                } else {
                    "2025-06-18"
                };
                json!({"jsonrpc":"2.0","id":id,"result":{"protocolVersion":version,"capabilities":{"tools":{"listChanged":false}},"serverInfo":{"name":"sparkdev","version":env!("CARGO_PKG_VERSION")},"instructions":"Check devices for authenticated Spark reachability, connection route and scheduler access; plan previews placement without reservation. Use prepare for locked dependency downloads, then run for offline workloads, bounded wait and incremental logs. Initial sync precedes job admission and may take time. Check status for both machines. Laptop admission also requires the shared Spark scheduler. Client identity is bound by server launch arguments. Agent conversations and delegation belong to CodaClauda, not this scheduler."}})
            }
            "ping" => json!({"jsonrpc":"2.0","id":id,"result":{}}),
            _ if !initialized => error(id, -32002, "Initialize first"),
            "tools/list" => json!({"jsonrpc":"2.0","id":id,"result":{"tools":tools()}}),
            "tools/call" => {
                let name = params.get("name").and_then(Value::as_str).unwrap_or("");
                let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
                match cli_args(name, &arguments, client) {
                    Err(message) => error(id, -32602, &message),
                    Ok(args) => {
                        let (value, failed) = match crate::execute(args) {
                            Ok(v) => (v, false),
                            Err(e) => (
                                json!({"ok":false,"error":{"code":e.code,"message":e.message}}),
                                true,
                            ),
                        };
                        json!({"jsonrpc":"2.0","id":id,"result":{"content":[{"type":"text","text":value.to_string()}],"structuredContent":value,"isError":failed}})
                    }
                }
            }
            _ => error(id, -32601, "Method not found"),
        };
        writeln!(output, "{response}")?;
        output.flush()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_argv_and_client_are_preserved() {
        let args = cli_args(
            "run",
            &json!({"dir":"C:/tree with spaces","argv":["python","-c","print('$() ; &')",""]}),
            "codex",
        )
        .unwrap();
        assert_eq!(
            &args[args.len() - 4..],
            &["python", "-c", "print('$() ; &')", ""]
        );
        assert!(args.windows(2).any(|p| p == ["--client", "codex"]));
        assert!(args.iter().any(|p| p == "--detach"));
    }
    #[test]
    fn rejects_client_spoofing_and_unbounded_wait() {
        assert!(cli_args("status", &json!({"client":"claude"}), "codex").is_err());
        assert!(cli_args("wait", &json!({"job":"a","timeout":61}), "codex").is_err());
        assert!(cli_args("logs", &json!({"job":"--help"}), "codex").is_err());
    }
    #[test]
    fn discovery_and_planning_cannot_submit_or_claim() {
        assert_eq!(
            cli_args("devices", &json!({}), "claude").unwrap(),
            ["--json", "--client", "claude", "devices"]
        );
        let args = cli_args(
            "plan",
            &json!({"dir":"./project","where":"laptop","argv":["cmd","/c","echo a & b",""]}),
            "codex",
        )
        .unwrap();
        assert_eq!(
            args,
            [
                "--json",
                "--client",
                "codex",
                "plan",
                "./project",
                "--where",
                "laptop",
                "--",
                "cmd",
                "/c",
                "echo a & b",
                ""
            ]
        );
        for extra in ["request_id", "no_cache", "detach", "client"] {
            let mut v = json!({"dir":"./project","argv":["cargo","test"]});
            v[extra] = json!(true);
            assert!(cli_args("plan", &v, "codex").is_err());
        }
        assert!(cli_args("devices", &json!({"where":"spark"}), "claude").is_err());
    }
    #[test]
    fn preparation_accepts_only_fixed_runtime_jobs() {
        let args = cli_args(
            "prepare",
            &json!({"dir":"./project","runtime":"rust","request_id":"stable-id"}),
            "claude",
        )
        .unwrap();
        assert_eq!(
            args,
            [
                "--json",
                "--client",
                "claude",
                "prepare",
                "./project",
                "--runtime",
                "rust",
                "--request-id",
                "stable-id"
            ]
        );
        assert!(
            cli_args(
                "prepare",
                &json!({"dir":"./project","runtime":"cuda"}),
                "codex"
            )
            .is_err()
        );
        assert!(
            cli_args(
                "prepare",
                &json!({"dir":"./project","runtime":"rust","argv":["sh"]}),
                "codex"
            )
            .is_err()
        );
        assert!(
            cli_args(
                "prepare",
                &json!({"dir":"./project","runtime":"rust","where":"laptop"}),
                "codex"
            )
            .is_err()
        );
        assert_eq!(tools().as_array().unwrap().len(), 8);
        for tool in tools().as_array().unwrap() {
            let name = tool["name"].as_str().unwrap();
            if ["wait", "logs", "status"].contains(&name) {
                assert_eq!(tool["annotations"]["readOnlyHint"], false);
                assert_eq!(tool["annotations"]["destructiveHint"], true);
            }
        }
    }
}
