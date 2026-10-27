//! Coding agents that run on the Spark instead of the laptop; see docs/AGENT-HOST.md.
//!
//! The laptop stages a git bundle of one branch and the prompt, and submits an
//! ordinary `run` job with the `agent` runtime, so sync, placement, waiting and
//! automatic result return are the same as for any job. When that job's results
//! are delivered, the agent's commits are imported into the same repository as
//! `refs/remotes/rtx/agent/<id>`. Nothing else in the repository is touched.
use crate::{AppError, Result, inventory, state, transport};
use serde_json::{Value, json};
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::Command,
};

pub const TOOLS: [&str; 2] = ["claude", "codex"];
pub const DRIVER: &str = "/sparkdev-agent.py";
const MAX_PROMPT: u64 = 256 * 1024;

fn invalid(message: impl Into<String>) -> AppError {
    AppError::new(2, message)
}

fn tool(name: &str) -> Result<&'static str> {
    TOOLS
        .iter()
        .find(|tool| **tool == name)
        .copied()
        .ok_or_else(|| invalid("Choose the agent tool: claude or codex"))
}

fn git(repo: &Path, args: &[&str]) -> Result<String> {
    let mut command = Command::new("git");
    command.arg("-C").arg(repo).args(args);
    transport::quiet(&mut command);
    let output = command
        .output()
        .map_err(|_| AppError::new(3, "Git is not available on this laptop"))?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr);
        return Err(AppError::new(
            3,
            format!("git {} failed: {}", args[0], detail.trim().lines().last().unwrap_or("no detail")),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn folder(name: &str) -> Result<PathBuf> {
    let path = state::root()?.join("agents").join(name);
    fs::create_dir_all(&path)?;
    Ok(path)
}

/// The agent's own name for one request; a retry of the same request reuses it.
pub fn agent_id(request_id: &str) -> String {
    inventory::hash(format!("agent:{request_id}").as_bytes())[..32].to_string()
}

/// What `run` submits for one agent request.
pub struct Staged {
    pub dir: PathBuf,
    pub argv: Vec<String>,
}

/// Stage the branch, prompt and request for one agent run, and remember which
/// repository its commits come back to.
pub fn stage(repo: &Path, tool_name: &str, prompt_file: &Path, branch: Option<&str>, request_id: &str) -> Result<Staged> {
    let tool = tool(tool_name)?;
    state::validate_id(request_id)?;
    let repo = PathBuf::from(git(&repo.canonicalize()?, &["rev-parse", "--show-toplevel"])?);
    let branch = match branch {
        Some(branch) => branch.to_string(),
        None => git(&repo, &["symbolic-ref", "--short", "HEAD"])
            .map_err(|_| invalid("This repository has no current branch; pass --branch"))?,
    };
    git(&repo, &["check-ref-format", "--branch", &branch]).map_err(|_| invalid("That is not a valid branch name"))?;
    git(&repo, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")])
        .map_err(|_| invalid(format!("There is no local branch named {branch}")))?;
    let meta = fs::metadata(prompt_file).map_err(|_| invalid("The prompt file was not found"))?;
    if !meta.is_file() || meta.len() == 0 || meta.len() > MAX_PROMPT {
        return Err(invalid("The prompt file must be non-empty text of at most 256 KiB"));
    }
    let prompt = fs::read_to_string(prompt_file).map_err(|_| invalid("The prompt file must be UTF-8 text"))?;
    let id = agent_id(request_id);
    let dir = folder("staging")?.join(&id);
    fs::create_dir_all(&dir)?;
    let bundle = dir.join("in.bundle");
    git(&repo, &["bundle", "create", &bundle.to_string_lossy(), &branch])?;
    state::atomic_bytes(&dir.join("prompt.md"), prompt.as_bytes())?;
    state::atomic_json(&dir.join("agent.json"), &json!({"v":1,"branch":branch,"job":id}))?;
    state::atomic_json(
        &folder("requests")?.join(format!("{request_id}.json")),
        &json!({"v":1,"repo":repo,"branch":branch,"tool":tool,"id":id}),
    )?;
    Ok(Staged { dir, argv: argv("run", tool) })
}

pub fn argv(mode: &str, tool: &str) -> Vec<String> {
    vec!["python3".into(), DRIVER.into(), mode.into(), tool.into()]
}

/// An empty project for Codex's sign-in job, which needs no files.
pub fn login_dir() -> Result<PathBuf> {
    let dir = folder("login")?;
    state::atomic_bytes(&dir.join("README.txt"), b"Codex device sign-in for RTXBridge agents.\n")?;
    Ok(dir)
}

/// Claude Code's token from stdin: one line, or the desktop app's {"token": ...}.
pub fn read_token() -> Result<String> {
    let mut raw = Vec::new();
    std::io::stdin().take(4097).read_to_end(&mut raw)?;
    token_from(&String::from_utf8(raw).map_err(|_| invalid("The token must be text"))?)
}

fn token_from(text: &str) -> Result<String> {
    // PowerShell can put a byte-order mark in front of what it pipes.
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let token = if text.trim_start().starts_with('{') {
        serde_json::from_str::<Value>(text)
            .ok()
            .and_then(|value| value["token"].as_str().map(str::to_string))
            .ok_or_else(|| invalid("Send the token as JSON with a token field"))?
    } else {
        text.trim().to_string()
    };
    if token.is_empty() || token.len() > 512 || token.chars().any(char::is_whitespace) {
        return Err(invalid("Paste the token that `claude setup-token` printed"));
    }
    Ok(token)
}

/// Bring a delivered agent result into its repository. Called once its job's
/// results are fully returned; a failure is recorded, never retried blindly.
pub fn import_result(request: &Value, returned: &[Value]) -> Result<Option<Value>> {
    if request["runtime"] != "agent" || request["argv"][2] != "run" {
        return Ok(None);
    }
    let Some(request_id) = request["request_id"].as_str() else {
        return Ok(None);
    };
    state::validate_id(request_id)?;
    let path = folder("requests")?.join(format!("{request_id}.json"));
    let Ok(raw) = fs::read(&path) else {
        return Ok(None);
    };
    let mut record: Value = serde_json::from_slice(&raw)?;
    if record["imported"].is_object() {
        return Ok(Some(record));
    }
    let (Some(repo), Some(id)) = (record["repo"].as_str().map(PathBuf::from), record["id"].as_str().map(str::to_string)) else {
        return Err(AppError::new(3, "The agent request record is incomplete"));
    };
    let bundle = returned
        .iter()
        .find(|receipt| receipt["source_path"].as_str().is_some_and(|source| source == "result.bundle" || source.ends_with("/result.bundle")))
        .and_then(|receipt| receipt["path"].as_str());
    let local = format!("refs/remotes/rtx/agent/{id}");
    record["imported"] = match bundle {
        None => json!({"ref":null,"commits":0,"message":"The agent made no commits"}),
        Some(bundle) => match git(&repo, &["fetch", "--no-tags", "--quiet", bundle, &format!("+agent/{id}:{local}")]) {
            Ok(_) => json!({"ref":local,"message":format!("The agent's commits are in {local}")}),
            Err(error) => json!({"ref":null,"bundle":bundle,"message":format!("The agent's commits could not be added to the repository: {}", error.message)}),
        },
    };
    state::atomic_json(&path, &record)?;
    Ok(Some(record))
}

/// The import outcome for a request, for wait and status to report.
pub fn outcome(request_id: &str) -> Option<Value> {
    let path = state::root().ok()?.join("agents").join("requests").join(format!("{request_id}.json"));
    let record: Value = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
    record.get("imported").cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_arrive_as_one_line_or_the_apps_json() {
        assert_eq!(token_from("sk-ant-oat01-abc\r\n").unwrap(), "sk-ant-oat01-abc");
        assert_eq!(token_from("{\"token\":\"sk-ant-oat01-abc\"}").unwrap(), "sk-ant-oat01-abc");
        assert_eq!(token_from("\u{feff}sk-ant-oat01-abc\r\n").unwrap(), "sk-ant-oat01-abc");
        for bad in ["", "two words", "{\"tok\":\"x\"}", &"x".repeat(513)] {
            assert!(token_from(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn agent_jobs_run_only_the_fixed_driver() {
        assert_eq!(argv("run", "claude"), ["python3", "/sparkdev-agent.py", "run", "claude"]);
        assert!(tool("bash").is_err());
        assert_eq!(agent_id("req-1"), agent_id("req-1"));
        assert_ne!(agent_id("req-1"), agent_id("req-2"));
        assert_eq!(agent_id("req-1").len(), 32);
    }

    #[test]
    fn only_agent_runs_are_imported() {
        let other = json!({"runtime":"rust","argv":["cargo","test"],"request_id":"r"});
        assert!(import_result(&other, &[]).unwrap().is_none());
        let login = json!({"runtime":"agent","argv":["python3",DRIVER,"login","codex"],"request_id":"r"});
        assert!(import_result(&login, &[]).unwrap().is_none());
    }
}
