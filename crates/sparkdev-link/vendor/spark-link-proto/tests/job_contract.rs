//! The node's compute-job contract: admission, refusal codes and retention order.
use serde_json::json;
use spark_link_proto::job_contract::*;

fn owner() -> Owner {
    Owner { device: "laptop".into(), app: "phaseforge".into() }
}

fn request() -> JobCreate {
    JobCreate {
        workload: PYTHON_ML.into(),
        args: json!({"runner": "python", "script": "train.py", "argv": ["--epochs", "2"], "python_flags": ["-I"], "limits": {"memory_mb": 4096, "process_limit": 64, "storage_mb": 256}}),
        inputs: vec![JobFile { name: "train.py".into(), size: 8, sha256: "a".repeat(64) }],
        expected_outputs: vec!["result.json".into()],
        timeout_s: 60,
        app: "phaseforge".into(),
    }
}

#[test]
fn only_registry_runtimes_are_admitted_and_ready_ones_advertised() {
    assert!(validate(&owner(), &request()).is_ok());
    assert_eq!(advertised_capabilities(&[PYTHON_ML]), [JOBS, JOBS_V2, PYTHON_ML]);
    assert_eq!(advertised_capabilities(&[BLENDER, PYTHON_ML]), [JOBS, JOBS_V2, PYTHON_ML, BLENDER], "registry order");
    assert!(advertised_capabilities(&[]).is_empty());
    assert!(advertised_capabilities(&["openmm@8.5.2"]).is_empty(), "a token outside the registry is never advertised");
    assert_eq!(RUNTIMES.iter().map(|r| r.token).collect::<Vec<_>>(), [PYTHON_ML, BLENDER]);
    for workload in ["blender_render", "illustration_render", "openmm_gpu", "openmm@8.5.2", "", "PYTHON_ML", "blender@4.5"] {
        let mut r = request();
        r.workload = workload.into();
        assert_eq!(validate(&owner(), &r).unwrap_err().code, "not_implemented", "{workload}");
    }
}

fn blender_request() -> JobCreate {
    JobCreate {
        workload: BLENDER.into(),
        args: json!({"runner": "blender", "script": "worker.py", "argv": ["input.json", ".", "300", "4294967296"], "python_exit_code": 2, "limits": {"memory_mb": 16384, "process_limit": 1024, "storage_mb": 4096}}),
        inputs: vec![JobFile { name: "input.json".into(), size: 10, sha256: "a".repeat(64) }, JobFile { name: "worker.py".into(), size: 10, sha256: "b".repeat(64) }],
        expected_outputs: vec!["render.png".into(), "scene.blend".into()],
        timeout_s: 3600,
        app: "phaseforge".into(),
    }
}

#[test]
fn blender_arguments_are_a_closed_schema_with_its_own_bounds() {
    assert!(validate(&owner(), &blender_request()).is_ok());
    let refused = |f: &dyn Fn(&mut JobCreate)| {
        let mut r = blender_request();
        f(&mut r);
        validate(&owner(), &r).unwrap_err().code
    };
    // Callers choose no flag, image, path or runner.
    for key in ["blender_flags", "image", "stdout", "cwd", "command", "env"] {
        assert_eq!(refused(&|r| r.args[key] = json!("x")), "invalid_args", "{key}");
    }
    assert_eq!(refused(&|r| r.args["runner"] = json!("python")), "invalid_args");
    assert_eq!(refused(&|r| r.args["python_exit_code"] = json!(0)), "invalid_args");
    assert_eq!(refused(&|r| r.args["script"] = json!("missing.py")), "invalid_args");
    // pids at least 512 (threads count): the PyTorch bound of 128 is refused here.
    assert_eq!(refused(&|r| r.args["limits"]["process_limit"] = json!(128)), "invalid_args");
    assert_eq!(refused(&|r| r.args["limits"]["memory_mb"] = json!(65537)), "invalid_args");
    assert_eq!(refused(&|r| r.timeout_s = 4 * 3600 + 1), "request_limit");
    assert!(validate(&owner(), &{ let mut r = blender_request(); r.timeout_s = 4 * 3600; r }).is_ok(), "Blender may run up to 4 h");
    // A GPU budget is accepted for Blender only.
    assert!(validate(&owner(), &{ let mut r = blender_request(); r.args["limits"]["gpu_memory_mb"] = json!(32768); r }).is_ok());
    let mut python = request();
    python.args["limits"]["gpu_memory_mb"] = json!(1024);
    assert_eq!(validate(&owner(), &python).unwrap_err().code, "invalid_args", "python_ml is unchanged");
    // Defaults when no limits are declared.
    let mut r = blender_request();
    r.args.as_object_mut().unwrap().remove("limits");
    let limits = effective_limits(&r).unwrap();
    assert_eq!((limits.process_limit, limits.memory_mb), (1024, 16384));
    assert_eq!(effective_limits(&request()).unwrap(), ContainerLimits { memory_mb: 4096, process_limit: 64, storage_mb: 256, gpu_memory_mb: None });
}

#[test]
fn python_arguments_are_a_closed_schema() {
    let refused = |args: serde_json::Value| {
        let mut r = request();
        r.args = args;
        validate(&owner(), &r).unwrap_err().code
    };
    for key in ["image", "host_path", "network", "environment", "command", "blender_flags"] {
        let mut args = request().args;
        args[key] = json!("x");
        assert_eq!(refused(args), "invalid_args", "{key}");
    }
    // The four ways PhaseForge's 0.21 OpenMM request differed from this contract.
    for (key, value) in [("requirements", json!("requirements-science.txt")), ("requirements", json!(["openmm"])), ("python_flags", json!(["-I", "-B"])), ("stdout", json!("stdout.log")), ("stderr", json!("stderr.log")), ("cwd", json!("."))] {
        let mut args = request().args;
        args[key] = value.clone();
        assert_eq!(refused(args), "invalid_args", "{key}={value}");
    }
    let mut args = request().args;
    args["script"] = json!("missing.py");
    assert_eq!(refused(args), "invalid_args");
    let mut args = request().args;
    args["limits"]["process_limit"] = json!(512);
    assert_eq!(refused(args), "invalid_args");
}

#[test]
fn owner_counts_names_and_logs_are_checked_in_order() {
    let mut r = request();
    r.app = "other".into();
    assert_eq!(validate(&owner(), &r).unwrap_err().code, "invalid_owner");
    let mut r = request();
    r.timeout_s = MAX_TIMEOUT_S + 1;
    assert_eq!(validate(&owner(), &r).unwrap_err().code, "request_limit");
    let mut r = request();
    r.expected_outputs = vec!["stdout.txt".into()];
    assert_eq!(validate(&owner(), &r).unwrap_err().code, "invalid_output");
    let mut r = request();
    r.inputs[0].sha256 = "A".repeat(64);
    assert_eq!(validate(&owner(), &r).unwrap_err().code, "invalid_input");
    let mut r = request();
    r.expected_outputs = vec!["../x".into()];
    assert_eq!(validate(&owner(), &r).unwrap_err().code, "invalid_path");
    assert!(valid_id(&"0".repeat(32)).is_ok());
    assert!(valid_id(&"G".repeat(32)).is_err());
}

#[test]
fn job_errors_map_to_node_api_codes() {
    for (job, api) in [("capacity", "too_many_requests"), ("invalid_state", "conflict"), ("cleanup_unconfirmed", "conflict"), ("not_found", "not_found"), ("not_implemented", "bad_request"), ("invalid_args", "bad_request"), ("not_available", "upstream_unavailable")] {
        assert_eq!(api_error_code(job), api, "{job}");
    }
}

fn retained(id: &str, acknowledged: Option<u64>, last_used: u64, bytes: u64) -> Retained {
    Retained { id: id.into(), terminal: true, cleanup_confirmed: true, acknowledged_at: acknowledged, last_used, bytes, staging: false, idle_since: 0 }
}

#[test]
fn retention_prunes_acknowledged_jobs_least_recently_used_first() {
    let mut jobs: Vec<Retained> = (0..RETAINED_JOB_LIMIT).map(|n| retained(&format!("u{n:04}"), None, 0, 1)).collect();
    assert_eq!(retention_plan(&jobs, 1).unwrap_err().code, "capacity", "unacknowledged results are not pruned by retention");
    jobs[10] = retained("b", Some(5), 50, 1);
    jobs[11] = retained("a", Some(9), 20, 1);
    jobs[12] = Retained { terminal: false, ..retained("running", Some(1), 0, 1) };
    jobs[13] = Retained { cleanup_confirmed: false, ..retained("uncertain", Some(1), 0, 1) };
    assert_eq!(retention_plan(&jobs, 1).unwrap(), ["a"], "least recently used, never unfinished or uncertain");
    assert!(retention_plan(&jobs[1..], 1).unwrap().is_empty(), "room left: nothing pruned");
    // Bytes: a job that would exceed the byte bound prunes acknowledged bytes first.
    let full = vec![retained("x", Some(1), 1, RETAINED_BYTE_LIMIT / 2), retained("y", None, 0, RETAINED_BYTE_LIMIT / 2)];
    assert_eq!(retention_plan(&full, INPUT_LIMIT).unwrap(), ["x"]);
    assert_eq!(retention_plan(&full[1..], 0).unwrap(), Vec::<String>::new());
    let stuck = vec![retained("y", None, 0, RETAINED_BYTE_LIMIT)];
    assert_eq!(retention_plan(&stuck, 1).unwrap_err().code, "capacity");
}

#[test]
fn only_finished_jobs_with_confirmed_cleanup_are_removable() {
    assert!(check_removable(true, true).is_ok());
    assert_eq!(check_removable(false, true).unwrap_err().code, "invalid_state");
    assert_eq!(check_removable(true, false).unwrap_err().code, "cleanup_unconfirmed");
}

#[test]
fn unclaimed_jobs_expire_after_their_grace_period_and_nothing_else_does() {
    let grace = DEFAULT_UNCLAIMED_GRACE_S;
    let finished = Retained { idle_since: 1_000, ..retained("finished", None, 0, 1) };
    let staging = Retained { terminal: false, cleanup_confirmed: true, staging: true, idle_since: 2_000, ..retained("staging", None, 0, 1) };
    let acked = Retained { idle_since: 0, ..retained("acked", Some(5), 5, 1) };
    let running = Retained { terminal: false, idle_since: 0, ..retained("running", None, 0, 1) };
    let uncertain = Retained { cleanup_confirmed: false, idle_since: 0, ..retained("uncertain", None, 0, 1) };
    let all = [finished.clone(), staging.clone(), acked, running, uncertain];
    assert_eq!(expires_at(&finished, grace), Some((1_000 + grace, ExpiryReason::Unacknowledged)));
    assert_eq!(expires_at(&staging, grace), Some((2_000 + grace, ExpiryReason::AbandonedStaging)));
    assert!(all[2..].iter().all(|r| expires_at(r, grace).is_none()), "acknowledged, active and uncertain jobs never expire by this rule");
    assert!(expiry_plan(&all, 1_000 + grace - 1, grace).is_empty());
    assert_eq!(expiry_plan(&all, 1_000 + grace, grace), [("finished".to_string(), ExpiryReason::Unacknowledged)]);
    assert_eq!(expiry_plan(&all, u64::MAX, grace), [("finished".to_string(), ExpiryReason::Unacknowledged), ("staging".to_string(), ExpiryReason::AbandonedStaging)], "oldest first");
    assert!(DEFAULT_UNCLAIMED_GRACE_S >= 2 * 86_400 && (UNCLAIMED_GRACE_RANGE_S.0..=UNCLAIMED_GRACE_RANGE_S.1).contains(&DEFAULT_UNCLAIMED_GRACE_S), "a few days by default");
}

/// A declared script's name follows the interpreter in the fixed entry point
/// (`python -I <script> <argv...>`, `blender ... --python <script>`): a name beginning with `-`
/// would be read as an interpreter option (`-Wx.py` takes the next word as the script, `-c...`
/// runs code from the name), so callers would choose flags after all. Such names are refused for
/// the script; other inputs and outputs never stand in that position.
#[test]
fn a_script_name_cannot_be_read_as_an_interpreter_option() {
    for (make, script) in [(request as fn() -> JobCreate, "-Wx.py"), (request, "-c.py"), (request, "-mtrain.py"), (blender_request, "-Wx.py"), (blender_request, "--x.py")] {
        let mut r = make();
        r.inputs.push(JobFile { name: script.into(), size: 4, sha256: "c".repeat(64) });
        r.args["script"] = json!(script);
        let refused = validate(&owner(), &r).unwrap_err();
        assert_eq!(refused.code, "invalid_args", "{script}");
    }
    // A data input whose name begins with '-' is still a valid input (it is never an argument
    // of the interpreter); and a nested script whose path does not begin with '-' is fine.
    let mut r = request();
    r.inputs.push(JobFile { name: "-data.csv".into(), size: 4, sha256: "d".repeat(64) });
    assert!(validate(&owner(), &r).is_ok());
    let mut r = request();
    r.inputs.push(JobFile { name: "src/-x.py".into(), size: 4, sha256: "e".repeat(64) });
    r.args["script"] = json!("src/-x.py");
    assert!(validate(&owner(), &r).is_ok());
}
