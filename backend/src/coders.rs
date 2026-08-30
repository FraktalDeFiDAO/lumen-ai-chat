use std::{
    env,
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
};

const MAX_CODER_ROUTES: usize = 100;
const MAX_CODER_OUTPUT_BYTES: usize = 1024 * 1024;
const MAX_CODER_ERROR_BYTES: usize = 64 * 1024;
const MAX_WORKSPACE_COPY_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum CoderKind {
    Hermes,
    Opencode,
    Kilocode,
    Claude,
    Codex,
}

impl CoderKind {
    pub const ALL: [Self; 5] = [
        Self::Hermes,
        Self::Opencode,
        Self::Kilocode,
        Self::Claude,
        Self::Codex,
    ];

    pub fn id(self) -> &'static str {
        match self {
            Self::Hermes => "hermes",
            Self::Opencode => "opencode",
            Self::Kilocode => "kilocode",
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Self::Hermes => "Hermes",
            Self::Opencode => "OpenCode",
            Self::Kilocode => "KiloCode",
            Self::Claude => "Claude Code",
            Self::Codex => "Codex",
        }
    }

    fn executable_name(self) -> &'static str {
        match self {
            Self::Hermes => "hermes",
            Self::Opencode => "opencode",
            Self::Kilocode => "kilocode",
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }

    fn env_suffix(self) -> &'static str {
        match self {
            Self::Hermes => "HERMES",
            Self::Opencode => "OPENCODE",
            Self::Kilocode => "KILOCODE",
            Self::Claude => "CLAUDE",
            Self::Codex => "CODEX",
        }
    }

    fn default_provider(self) -> &'static str {
        match self {
            Self::Hermes | Self::Opencode | Self::Kilocode => "auto",
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }
}

#[derive(Clone, Debug)]
pub struct CoderRoute {
    pub kind: CoderKind,
    pub provider: String,
    pub provider_name: String,
    pub model: String,
    pub model_name: String,
    pub executable: PathBuf,
}

impl CoderRoute {
    pub fn key(&self) -> String {
        format!("{}/{}/{}", self.kind.id(), self.provider, self.model)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CoderRouteInput {
    coder: CoderKind,
    provider: String,
    model: String,
    #[serde(default)]
    provider_name: Option<String>,
    #[serde(default)]
    model_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CoderStatus {
    pub id: String,
    pub name: String,
    pub status: String,
    pub executable: Option<String>,
    pub route_count: usize,
}

#[derive(Debug, Error)]
pub enum CoderError {
    #[error("failed to start {coder}: {message}")]
    Spawn { coder: String, message: String },
    #[error("{coder} timed out after {seconds} seconds")]
    Timeout { coder: String, seconds: u64 },
    #[error("{coder} failed: {message}")]
    Failed { coder: String, message: String },
    #[error("{coder} produced too much output")]
    OutputLimit { coder: String },
    #[error("{coder} returned no assistant response")]
    Empty { coder: String },
    #[error("failed to prepare a read-only workspace: {0}")]
    Workspace(String),
}

#[derive(Debug, Clone, Serialize)]
pub struct CoderExecution {
    pub output: String,
    pub duration_ms: u128,
}

pub fn discover_routes() -> Result<Vec<CoderRoute>, String> {
    let configured = env::var("AI_CODER_ROUTES_JSON")
        .ok()
        .map(|raw| {
            serde_json::from_str::<Vec<CoderRouteInput>>(&raw)
                .map_err(|error| format!("AI_CODER_ROUTES_JSON is invalid: {error}"))
        })
        .transpose()?;
    let inputs = configured.unwrap_or_else(|| {
        CoderKind::ALL
            .into_iter()
            .map(|coder| CoderRouteInput {
                coder,
                provider: coder.default_provider().into(),
                model: "default".into(),
                provider_name: None,
                model_name: None,
            })
            .collect()
    });
    if inputs.len() > MAX_CODER_ROUTES {
        return Err(format!(
            "AI_CODER_ROUTES_JSON supports at most {MAX_CODER_ROUTES} routes"
        ));
    }

    let mut routes = Vec::new();
    for input in inputs {
        let Some(executable) = resolve_executable(input.coder) else {
            continue;
        };
        routes.push(CoderRoute {
            kind: input.coder,
            provider_name: input
                .provider_name
                .unwrap_or_else(|| display_identifier(&input.provider)),
            model_name: input.model_name.unwrap_or_else(|| input.model.clone()),
            provider: input.provider,
            model: input.model,
            executable,
        });
    }
    Ok(routes)
}

pub fn statuses(routes: &[CoderRoute]) -> Vec<CoderStatus> {
    CoderKind::ALL
        .into_iter()
        .map(|kind| {
            let executable = resolve_executable(kind);
            CoderStatus {
                id: kind.id().into(),
                name: kind.display_name().into(),
                status: if executable.is_some() {
                    "installed_unverified".into()
                } else {
                    "unavailable".into()
                },
                executable: executable.map(|path| path.display().to_string()),
                route_count: routes.iter().filter(|route| route.kind == kind).count(),
            }
        })
        .collect()
}

pub async fn execute(
    route: &CoderRoute,
    prompt: &str,
    workspace: &Path,
    allow_writes: bool,
    timeout: Duration,
) -> Result<CoderExecution, CoderError> {
    let isolated_workspace = if allow_writes {
        None
    } else {
        let workspace = workspace.to_owned();
        Some(
            tokio::task::spawn_blocking(move || prepare_read_only_workspace(&workspace))
                .await
                .map_err(|error| CoderError::Workspace(error.to_string()))??,
        )
    };
    let execution_workspace = isolated_workspace
        .as_ref()
        .map_or(workspace, |directory| directory.path());
    let mut command = build_command(route, prompt, execution_workspace, allow_writes);
    command
        .current_dir(execution_workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    apply_clean_environment(&mut command);

    let coder = route.kind.display_name().to_owned();
    let started = Instant::now();
    let mut child = command.spawn().map_err(|error| CoderError::Spawn {
        coder: coder.clone(),
        message: error.to_string(),
    })?;
    let stdout = child.stdout.take().expect("stdout is piped");
    let stderr = child.stderr.take().expect("stderr is piped");
    let stdout_task = tokio::spawn(read_limited(stdout, MAX_CODER_OUTPUT_BYTES));
    let stderr_task = tokio::spawn(read_limited(stderr, MAX_CODER_ERROR_BYTES));

    let status = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(error)) => {
            return Err(CoderError::Failed {
                coder,
                message: error.to_string(),
            });
        }
        Err(_) => {
            let _ = child.kill().await;
            stdout_task.abort();
            stderr_task.abort();
            return Err(CoderError::Timeout {
                coder,
                seconds: timeout.as_secs(),
            });
        }
    };

    let stdout = stdout_task
        .await
        .map_err(|error| CoderError::Failed {
            coder: coder.clone(),
            message: error.to_string(),
        })?
        .map_err(|_| CoderError::OutputLimit {
            coder: coder.clone(),
        })?;
    let stderr = stderr_task
        .await
        .map_err(|error| CoderError::Failed {
            coder: coder.clone(),
            message: error.to_string(),
        })?
        .unwrap_or_default();

    if !status.success() {
        return Err(CoderError::Failed {
            coder,
            message: safe_error_message(&stderr, status.code()),
        });
    }
    let output = normalize_output(route.kind, &stdout).ok_or_else(|| CoderError::Empty {
        coder: coder.clone(),
    })?;
    Ok(CoderExecution {
        output,
        duration_ms: started.elapsed().as_millis(),
    })
}

fn build_command(
    route: &CoderRoute,
    prompt: &str,
    workspace: &Path,
    allow_writes: bool,
) -> Command {
    let mut command = Command::new(&route.executable);
    let explicit_model = route.model != "default";
    match route.kind {
        CoderKind::Hermes => {
            command.args(["chat", "-q", prompt, "-Q", "--in"]);
            command.arg(workspace);
            if explicit_model {
                command.args(["-m", &format!("{}/{}", route.provider, route.model)]);
                if route.provider != "auto" {
                    command.args(["--provider", &route.provider]);
                }
            }
            if allow_writes {
                command.arg("--yolo");
            } else {
                command.args(["-t", "safe"]);
            }
        }
        CoderKind::Opencode => {
            command.args(["run", "--format", "json", "--dir"]);
            command.arg(workspace);
            if explicit_model {
                command.args(["--model", &format!("{}/{}", route.provider, route.model)]);
            }
            if allow_writes {
                command.arg("--auto");
            }
            command.arg(prompt);
        }
        CoderKind::Kilocode => {
            command.args(["run", "--format", "json", "--dir"]);
            command.arg(workspace);
            if explicit_model {
                command.args(["--model", &format!("{}/{}", route.provider, route.model)]);
            }
            if allow_writes {
                command.arg("--auto");
            }
            command.arg(prompt);
        }
        CoderKind::Claude => {
            command.args(["--print", prompt, "--output-format", "json"]);
            if explicit_model {
                command.args(["--model", &route.model]);
            }
            command.args([
                "--permission-mode",
                if allow_writes { "acceptEdits" } else { "plan" },
            ]);
        }
        CoderKind::Codex => {
            command.args([
                "exec",
                "--json",
                "--skip-git-repo-check",
                "--sandbox",
                if allow_writes {
                    "workspace-write"
                } else {
                    "read-only"
                },
            ]);
            if allow_writes {
                command.arg("--full-auto");
            }
            if explicit_model {
                command.args(["--model", &route.model]);
            }
            command.arg(prompt);
        }
    }
    command
}

fn prepare_read_only_workspace(workspace: &Path) -> Result<tempfile::TempDir, CoderError> {
    let directory = tempfile::Builder::new()
        .prefix("lumen-coder-readonly-")
        .tempdir()
        .map_err(|error| CoderError::Workspace(error.to_string()))?;
    let mut copied = 0u64;
    copy_directory(workspace, directory.path(), &mut copied)?;
    Ok(directory)
}

fn copy_directory(source: &Path, destination: &Path, copied: &mut u64) -> Result<(), CoderError> {
    for entry in
        std::fs::read_dir(source).map_err(|error| CoderError::Workspace(error.to_string()))?
    {
        let entry = entry.map_err(|error| CoderError::Workspace(error.to_string()))?;
        let name = entry.file_name();
        let name_text = name.to_string_lossy();
        if excluded_workspace_entry(&name_text) {
            continue;
        }
        let file_type = entry
            .file_type()
            .map_err(|error| CoderError::Workspace(error.to_string()))?;
        if file_type.is_symlink() {
            continue;
        }
        let target = destination.join(&name);
        if file_type.is_dir() {
            std::fs::create_dir(&target)
                .map_err(|error| CoderError::Workspace(error.to_string()))?;
            copy_directory(&entry.path(), &target, copied)?;
        } else if file_type.is_file() {
            let size = entry
                .metadata()
                .map_err(|error| CoderError::Workspace(error.to_string()))?
                .len();
            *copied = copied.saturating_add(size);
            if *copied > MAX_WORKSPACE_COPY_BYTES {
                return Err(CoderError::Workspace(format!(
                    "workspace exceeds the {} MiB read-only copy limit",
                    MAX_WORKSPACE_COPY_BYTES / 1024 / 1024
                )));
            }
            std::fs::copy(entry.path(), target)
                .map_err(|error| CoderError::Workspace(error.to_string()))?;
        }
    }
    Ok(())
}

pub(crate) fn excluded_workspace_entry(name: &str) -> bool {
    matches!(
        name,
        ".git"
            | ".claude-flow"
            | ".venv"
            | "coverage"
            | "data"
            | "dist"
            | "node_modules"
            | "target"
    ) || name == ".env"
        || name.starts_with(".env.")
        || matches!(name, ".netrc" | ".npmrc" | ".pypirc" | "credentials.json")
        || name.ends_with(".pem")
        || name.ends_with(".key")
        || name.ends_with(".p12")
        || name.ends_with(".pfx")
        || name == "auth.json"
}

fn apply_clean_environment(command: &mut Command) {
    command.env_clear();
    for key in [
        "PATH",
        "HOME",
        "USER",
        "LOGNAME",
        "LANG",
        "LC_ALL",
        "TERM",
        "SHELL",
        "TMPDIR",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_STATE_HOME",
        "XDG_CACHE_HOME",
    ] {
        if let Some(value) = env::var_os(key) {
            command.env(key, value);
        }
    }
    command.env("NO_COLOR", "1");
}

async fn read_limited<R: AsyncRead + Unpin>(reader: R, limit: usize) -> Result<Vec<u8>, ()> {
    let mut bytes = Vec::new();
    reader
        .take((limit + 1) as u64)
        .read_to_end(&mut bytes)
        .await
        .map_err(|_| ())?;
    (bytes.len() <= limit).then_some(bytes).ok_or(())
}

fn normalize_output(kind: CoderKind, bytes: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(bytes);
    if kind == CoderKind::Hermes {
        let trimmed = text.trim();
        return (!trimmed.is_empty()).then(|| trimmed.to_owned());
    }

    let mut candidates = Vec::new();
    if let Ok(value) = serde_json::from_str::<Value>(&text) {
        collect_candidates(&value, &mut candidates);
    } else {
        for line in text.lines().filter(|line| !line.trim().is_empty()) {
            if let Ok(value) = serde_json::from_str::<Value>(line) {
                collect_candidates(&value, &mut candidates);
            }
        }
    }
    candidates
        .into_iter()
        .rev()
        .find(|value| !value.trim().is_empty())
        .or_else(|| {
            let trimmed = text.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_owned())
        })
}

fn collect_candidates(value: &Value, output: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for key in ["result", "output", "text", "content"] {
                if let Some(text) = map.get(key).and_then(Value::as_str)
                    && !text.trim().is_empty()
                {
                    output.push(text.to_owned());
                }
            }
            for child in map.values() {
                collect_candidates(child, output);
            }
        }
        Value::Array(values) => {
            for child in values {
                collect_candidates(child, output);
            }
        }
        _ => {}
    }
}

fn resolve_executable(kind: CoderKind) -> Option<PathBuf> {
    let override_name = format!("AI_CODER_{}_BIN", kind.env_suffix());
    let executable = env::var_os(override_name)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(kind.executable_name()));
    if executable.components().count() > 1 {
        return if executable.is_file() {
            executable.canonicalize().ok()
        } else {
            None
        };
    }
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .map(|directory| directory.join(&executable))
        .find(|candidate| candidate.is_file())
        .and_then(|candidate| candidate.canonicalize().ok())
}

fn display_identifier(value: &str) -> String {
    let mut chars = value.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

fn safe_error_message(stderr: &[u8], code: Option<i32>) -> String {
    let text = String::from_utf8_lossy(stderr);
    let line = text
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("process exited without an error message")
        .trim();
    let sanitized = line.chars().take(500).collect::<String>();
    match code {
        Some(code) => format!("exit code {code}: {sanitized}"),
        None => sanitized,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_supported_coder_outputs() {
        assert_eq!(
            normalize_output(CoderKind::Hermes, b"Hermes response\n"),
            Some("Hermes response".into())
        );
        assert_eq!(
            normalize_output(CoderKind::Claude, br#"{"result":"Claude response"}"#),
            Some("Claude response".into())
        );
        assert_eq!(
            normalize_output(
                CoderKind::Codex,
                b"{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"Codex response\"}}\n"
            ),
            Some("Codex response".into())
        );
        assert_eq!(
            normalize_output(
                CoderKind::Opencode,
                b"{\"type\":\"text\",\"part\":{\"text\":\"OpenCode response\"}}\n"
            ),
            Some("OpenCode response".into())
        );
    }

    #[test]
    fn creates_secret_free_read_only_workspace_copy() {
        let source = tempfile::tempdir().unwrap();
        std::fs::write(source.path().join("main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(source.path().join(".env"), "SECRET=do-not-copy\n").unwrap();
        std::fs::create_dir(source.path().join("node_modules")).unwrap();
        std::fs::write(source.path().join("node_modules/package.js"), "ignored").unwrap();
        std::fs::create_dir(source.path().join("data")).unwrap();
        std::fs::write(source.path().join("data/application.db"), "private state").unwrap();

        let copy = prepare_read_only_workspace(source.path()).unwrap();
        assert!(copy.path().join("main.rs").is_file());
        assert!(!copy.path().join(".env").exists());
        assert!(!copy.path().join("node_modules").exists());
        assert!(!copy.path().join("data").exists());
    }
}
