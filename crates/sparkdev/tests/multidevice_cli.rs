//! Exercise the actual CLI process with isolated state and no remote connections.
use serde_json::Value;
use std::{fs, path::Path, process::Command};

fn invoke(home: &Path, arguments: &[&str]) -> (i32, Value) {
    let output = Command::new(env!("CARGO_BIN_EXE_sparkdev"))
        .env("SPARKDEV_HOME", home)
        .env_remove("SPARKDEV_EXPECTED_MANIFEST")
        .args(["--json", "--client", "codex"])
        .args(arguments)
        .output()
        .unwrap();
    let text = String::from_utf8(output.stdout).unwrap();
    let value = serde_json::from_str(text.trim()).unwrap_or_else(|error| {
        panic!("CLI did not return one JSON value: {error}: {text}")
    });
    (output.status.code().unwrap_or(-1), value)
}

fn add(home: &Path, name: &str) {
    let (code, value) = invoke(home, &["node", "add", name, "--label", "Render computer", "--ssh-alias", "fixture-host", "--host-key-alias", "fixture-pin"]);
    assert_eq!(code, 0, "{value}");
}

#[test]
fn registration_is_separate_and_does_not_replace_the_legacy_connection() {
    let temporary = tempfile::tempdir().unwrap();
    let home = fs::canonicalize(temporary.path()).unwrap();
    let (code, value) = invoke(&home, &["node", "list"]);
    assert_eq!(code, 0, "{value}");
    assert!(value["nodes"].as_array().unwrap().is_empty());
    assert!(!home.join("config.json").exists());
    add(&home, "render-one");
    assert!(home.join("nodes/render-one/config.json").is_file());
    assert!(!home.join("config.json").exists());
    let (code, value) = invoke(&home, &["node", "list"]);
    assert_eq!(code, 0, "{value}");
    assert_eq!(value["nodes"][0]["id"], "render-one");
    let retained = fs::read(home.join("nodes/render-one/config.json")).unwrap();
    let (code, _) = invoke(&home, &["node", "add", "render-one", "--ssh-alias", "other-host", "--host-key-alias", "other-pin"]);
    assert_ne!(code, 0);
    assert_eq!(fs::read(home.join("nodes/render-one/config.json")).unwrap(), retained);
}

#[test]
fn named_planning_is_readonly_and_keeps_exact_workload_arguments() {
    let temporary = tempfile::tempdir().unwrap();
    let home = temporary.path().join("state");
    fs::create_dir(&home).unwrap();
    let home = fs::canonicalize(home).unwrap();
    let source = temporary.path().join("project");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("main.py"), "print('fixture')\n").unwrap();
    add(&home, "render-one");
    let (code, value) = invoke(&home, &["--device", "render-one", "plan", source.to_str().unwrap(), "--", "python3", "main.py", "--device", "literal", "a b", ""]);
    assert_eq!(code, 0, "{value}");
    assert_eq!(value["device"], "render-one");
    assert_eq!(value["claimed"], false);
    assert_eq!(value["uploaded"], false);
    assert_eq!(value["submitted"], false);
    assert_eq!(value["argv"], serde_json::json!(["python3", "main.py", "--device", "literal", "a b", ""]));
    assert!(!home.join("fleet.json").exists());
    assert!(!home.join("nodes/render-one/objects").exists());
}

#[test]
fn disabling_retains_profile_and_refuses_new_work_before_submission() {
    let temporary = tempfile::tempdir().unwrap();
    let home = fs::canonicalize(temporary.path()).unwrap();
    add(&home, "render-one");
    let retained = fs::read(home.join("nodes/render-one/config.json")).unwrap();
    let (code, value) = invoke(&home, &["node", "enable", "render-one", "off"]);
    assert_eq!(code, 0, "{value}");
    let (code, _) = invoke(&home, &["--device", "render-one", "run", home.to_str().unwrap(), "--detach", "--request-id", "disabled-fixture", "--", "python3", "main.py"]);
    assert_ne!(code, 0);
    assert_eq!(fs::read(home.join("nodes/render-one/config.json")).unwrap(), retained);
    assert!(!home.join("fleet.json").exists());
    assert!(!home.join("nodes/render-one/requests/disabled-fixture.json").exists());
}

#[test]
fn qualified_jobs_cannot_be_sent_to_a_conflicting_device() {
    let temporary = tempfile::tempdir().unwrap();
    let home = fs::canonicalize(temporary.path()).unwrap();
    add(&home, "render-one");
    add(&home, "render-two");
    let (code, value) = invoke(&home, &["--device", "render-two", "wait", "render-one:fixture-job", "--timeout", "0"]);
    assert_ne!(code, 0, "{value}");
    assert_eq!(value["code"], 2);
    assert!(!home.join("fleet.json").exists());
}

#[test]
fn wsl_registration_rejects_command_injection_without_writing_a_profile() {
    let temporary = tempfile::tempdir().unwrap();
    let home = fs::canonicalize(temporary.path()).unwrap();
    let (code, value) = invoke(&home, &["node", "add", "render-one", "--ssh-alias", "fixture-host", "--host-key-alias", "fixture-pin", "--wsl-distro", "Ubuntu&whoami", "--wsl-user", "builder", "--controller-path", "/home/builder/sparkdev/controller.py"]);
    assert_ne!(code, 0, "{value}");
    assert!(!home.join("nodes/render-one/config.json").exists());
}

#[test]
fn bootstrap_requires_an_explicit_named_device() {
    let temporary = tempfile::tempdir().unwrap();
    let home = fs::canonicalize(temporary.path()).unwrap();
    let (code, value) = invoke(&home, &["bootstrap", "--bundle", home.to_str().unwrap()]);
    assert_ne!(code, 0, "{value}");
    assert!(!home.join("config.json").exists());
}
