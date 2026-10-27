//! Coding agents that run on the Spark instead of the laptop; see docs/AGENT-HOST.md.
//!
//! The laptop stages a git bundle of one branch and the prompt, and submits an
//! ordinary `run` job with the `agent` runtime, so sync, placement, waiting and
//! automatic result return are the same as for any job. When that job's results
//! are delivered, the agent's commits are imported into the same repository as
//! `refs/remotes/rtx/agent/<id>`. Nothing else in the repository is touched.
//!
//! There is no separate sign-in. Each agent job carries this laptop's current
//! Claude Code or Codex sign-in to the Spark, so the agent uses the same account.
use crate::{AppError, Result, inventory, state, transport};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
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
    Ok(Staged { dir, argv: argv(tool) })
}

pub fn argv(tool: &str) -> Vec<String> {
    vec!["python3".into(), DRIVER.into(), "run".into(), tool.into()]
}

fn display(tool: &str) -> &'static str {
    if tool == "claude" { "Claude Code" } else { "Codex" }
}

/// Where the tool itself keeps this laptop's sign-in.
fn sign_in_dir(tool: &str) -> Option<PathBuf> {
    let (variable, default) = if tool == "claude" { ("CLAUDE_CONFIG_DIR", ".claude") } else { ("CODEX_HOME", ".codex") };
    std::env::var_os(variable).map(PathBuf::from).or_else(|| {
        std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(|home| PathBuf::from(home).join(default))
    })
}

/// This laptop's current sign-in for a tool, in the form the Spark keeps it.
pub fn laptop_sign_in(tool_name: &str) -> Result<Value> {
    let tool = tool(tool_name)?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
    sign_in_from(tool, sign_in_dir(tool).as_deref(), now)
}

fn sign_in_from(tool: &str, dir: Option<&Path>, now_ms: u64) -> Result<Value> {
    let missing = || {
        AppError::new(3, format!("{0} isn't signed in on this laptop. Open {0} here and sign in, then try again.", display(tool)))
    };
    let file = dir.ok_or_else(missing)?.join(if tool == "claude" { ".credentials.json" } else { "auth.json" });
    if fs::metadata(&file).map_or(true, |meta| !meta.is_file() || meta.len() > 64 * 1024) {
        return Err(missing());
    }
    let raw = fs::read_to_string(&file).map_err(|_| missing())?;
    let value: Value = serde_json::from_str(&raw).map_err(|_| missing())?;
    if tool == "codex" {
        return if value.is_object() { Ok(json!({"auth_json": raw})) } else { Err(missing()) };
    }
    let oauth = &value["claudeAiOauth"];
    let token = oauth["accessToken"]
        .as_str()
        .filter(|token| (20..=512).contains(&token.len()) && token.bytes().all(|c| c.is_ascii_alphanumeric() || b"._~+/=-".contains(&c)))
        .ok_or_else(missing)?;
    if oauth["expiresAt"].as_u64().is_some_and(|expires| expires < now_ms + 15 * 60 * 1000) {
        return Err(AppError::new(
            3,
            "Claude Code's sign-in on this laptop is about to renew. Use Claude Code here once, then try again.",
        ));
    }
    Ok(json!({"token": token}))
}

/// Which tools agents can use: those signed in on this laptop. Nothing secret.
pub fn status() -> Value {
    let mut agents = serde_json::Map::new();
    for tool in TOOLS {
        let ready = laptop_sign_in(tool);
        agents.insert(tool.into(), json!({"signed_in": ready.is_ok(), "message": ready.err().map(|error| error.message)}));
    }
    json!({"ok":true,"agents":agents})
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
    fn agents_use_this_laptops_own_sign_ins() {
        let dir = std::env::temp_dir().join(format!("rtx-sign-ins-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        // Not signed in: a plain message naming the tool, nothing about files.
        let missing = sign_in_from("claude", Some(&dir), 0).unwrap_err().message;
        assert!(missing.starts_with("Claude Code isn't signed in on this laptop"), "{missing}");
        assert!(sign_in_from("codex", None, 0).unwrap_err().message.starts_with("Codex isn't signed in"));
        let token = "fixture-access-token-0123456789";
        fs::write(dir.join(".credentials.json"), format!("{{\"claudeAiOauth\":{{\"accessToken\":\"{token}\",\"refreshToken\":\"r\",\"expiresAt\":5000000}}}}")).unwrap();
        // Only the current access token travels; the refresh token stays here.
        assert_eq!(sign_in_from("claude", Some(&dir), 1_000_000).unwrap(), json!({"token": token}));
        assert!(sign_in_from("claude", Some(&dir), 4_500_000).unwrap_err().message.contains("about to renew"));
        fs::write(dir.join("auth.json"), "{\"tokens\":{}}").unwrap();
        assert_eq!(sign_in_from("codex", Some(&dir), 0).unwrap(), json!({"auth_json": "{\"tokens\":{}}"}));
        fs::write(dir.join("auth.json"), "[1]").unwrap();
        assert!(sign_in_from("codex", Some(&dir), 0).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn agent_jobs_run_only_the_fixed_driver() {
        assert_eq!(argv("claude"), ["python3", "/sparkdev-agent.py", "run", "claude"]);
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
