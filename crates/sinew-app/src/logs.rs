use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
    process::Stdio,
    sync::{Arc, OnceLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{anyhow, bail, Context, Result};
use regex::Regex;
use serde::Deserialize;
use serde_json::{json, Value};
use sinew_core::ToolDescriptor;
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::{Child, Command},
    sync::Mutex,
    time::sleep,
};

use crate::tool_run::ToolRunResult;

pub const LOGS_START_TOOL: &str = "logs_start";
pub const LOGS_TAIL_TOOL: &str = "logs_tail";
pub const LOGS_LIST_TOOL: &str = "logs_list";
pub const LOGS_STOP_TOOL: &str = "logs_stop";

/// Maximum number of processes attached to an HttpRequest error by default.
pub const HTTP_ATTACH_PROCESS_CAP: usize = 3;
/// Default number of recent lines attached per process.
pub const HTTP_ATTACH_LINES_DEFAULT: usize = 20;

const MAX_PROCESSES: usize = 8;
const RING_BUFFER_LINES: usize = 5_000;
const DEFAULT_TAIL_LINES: usize = 50;
const MAX_TAIL_LINES: usize = 1_000;
const DEFAULT_READY_TIMEOUT_MS: u64 = 30_000;
const MAX_READY_TIMEOUT_MS: u64 = 300_000;
const TOOL_OUTPUT_LIMIT: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LogStream {
    Stdout,
    Stderr,
}

impl LogStream {
    fn as_str(self) -> &'static str {
        match self {
            LogStream::Stdout => "stdout",
            LogStream::Stderr => "stderr",
        }
    }
}

#[derive(Debug, Clone)]
struct LogLine {
    timestamp: SystemTime,
    stream: LogStream,
    text: String,
}

#[derive(Debug, Clone)]
enum ProcessStatus {
    Running,
    Exited {
        code: Option<i32>,
        #[allow(dead_code)]
        at: SystemTime,
    },
}

impl ProcessStatus {
    fn label(&self) -> String {
        match self {
            ProcessStatus::Running => "running".to_string(),
            ProcessStatus::Exited { code: Some(c), .. } => format!("exited({c})"),
            ProcessStatus::Exited { code: None, .. } => "exited".to_string(),
        }
    }
}

struct ProcessEntry {
    name: String,
    command: String,
    #[allow(dead_code)]
    cwd: PathBuf,
    pid: Option<u32>,
    started_at: SystemTime,
    status: Arc<Mutex<ProcessStatus>>,
    buffer: Arc<Mutex<VecDeque<LogLine>>>,
    child: Arc<Mutex<Option<Child>>>,
}

pub struct LogRegistry {
    processes: Mutex<HashMap<String, Arc<ProcessEntry>>>,
}

impl LogRegistry {
    fn new() -> Self {
        Self {
            processes: Mutex::new(HashMap::new()),
        }
    }

    pub fn global() -> Arc<Self> {
        static GLOBAL: OnceLock<Arc<LogRegistry>> = OnceLock::new();
        GLOBAL
            .get_or_init(|| Arc::new(LogRegistry::new()))
            .clone()
    }

    /// Snapshot of recent lines for selected processes.
    ///
    /// - `names = None` → every running process, capped at [`HTTP_ATTACH_PROCESS_CAP`].
    /// - `names = Some(&[...])` → only those processes (running or exited), no cap.
    pub async fn snapshot_recent(
        &self,
        names: Option<&[String]>,
        lines_per_process: usize,
    ) -> Vec<ProcessLogSnapshot> {
        let processes = self.processes.lock().await;
        let mut out = Vec::new();

        let candidates: Vec<Arc<ProcessEntry>> = match names {
            Some(filter) => filter
                .iter()
                .filter_map(|name| processes.get(name.trim()).cloned())
                .collect(),
            None => {
                let mut running: Vec<Arc<ProcessEntry>> = Vec::new();
                for entry in processes.values() {
                    if matches!(*entry.status.lock().await, ProcessStatus::Running) {
                        running.push(entry.clone());
                    }
                }
                running.sort_by(|a, b| a.name.cmp(&b.name));
                running.truncate(HTTP_ATTACH_PROCESS_CAP);
                running
            }
        };
        drop(processes);

        for entry in candidates {
            let status = entry.status.lock().await.label();
            let buffer = entry.buffer.lock().await;
            let total = buffer.len();
            let skip = total.saturating_sub(lines_per_process);
            let lines: Vec<AttachedLogLine> = buffer
                .iter()
                .skip(skip)
                .map(|line| AttachedLogLine {
                    stream: line.stream.as_str(),
                    text: line.text.clone(),
                })
                .collect();
            let returned = lines.len();
            out.push(ProcessLogSnapshot {
                name: entry.name.clone(),
                status,
                lines,
                total_buffered: total,
                returned,
            });
        }
        out
    }
}

/// Snapshot used by [`LogRegistry::snapshot_recent`] (notably for HttpRequest auto-attach).
#[derive(Debug, Clone)]
pub struct ProcessLogSnapshot {
    pub name: String,
    pub status: String,
    pub lines: Vec<AttachedLogLine>,
    pub total_buffered: usize,
    pub returned: usize,
}

#[derive(Debug, Clone)]
pub struct AttachedLogLine {
    pub stream: &'static str,
    pub text: String,
}

#[derive(Clone)]
pub struct LogsTool {
    registry: Arc<LogRegistry>,
    workspace_root: PathBuf,
}

impl std::fmt::Debug for LogsTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LogsTool")
            .field("workspace_root", &self.workspace_root)
            .finish()
    }
}

impl LogsTool {
    pub fn new(workspace_root: impl Into<PathBuf>) -> Self {
        Self {
            registry: LogRegistry::global(),
            workspace_root: workspace_root.into(),
        }
    }

    pub fn start_descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: LOGS_START_TOOL.into(),
            description: "Spawn a long-running process (dev server, watcher, build) in the background and stream its stdout+stderr into a named ring buffer you can read later with `logs_tail`. Use this together with `HttpRequest` to test endpoints you just coded: start the server, hit it, then read the server logs to debug failures without losing context. The command runs via the system shell relative to the workspace root by default. Set `ready_pattern` to wait for the server to print a readiness line (e.g. \"listening on\") before returning, so subsequent HttpRequest calls don't race the boot.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "Shell command to run, e.g. \"npm run dev\" or \"cargo run -p api\"."
                    },
                    "name": {
                        "type": "string",
                        "description": "Stable identifier used to tail/stop the process later. Defaults to an auto-generated name."
                    },
                    "cwd": {
                        "type": "string",
                        "description": "Working directory relative to the workspace root. Defaults to the workspace root."
                    },
                    "ready_pattern": {
                        "type": "string",
                        "description": "Optional regex. If set, `logs_start` blocks until a stdout/stderr line matches it (or ready_timeout_ms elapses)."
                    },
                    "ready_timeout_ms": {
                        "type": "integer",
                        "minimum": 100,
                        "description": "How long to wait for ready_pattern, in ms. Defaults to 30000, capped at 300000."
                    }
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        }
    }

    pub fn tail_descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: LOGS_TAIL_TOOL.into(),
            description: "Read the most recent lines buffered for a process started by `logs_start`. Optional regex `grep` filters lines (case-insensitive). Use `stream=\"stderr\"` to focus on errors, or `since_ms` to read only lines emitted in the last N milliseconds (handy right after a failing HttpRequest call).".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Process name returned by `logs_start`."
                    },
                    "lines": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Maximum lines to return. Defaults to 50, capped at 1000."
                    },
                    "grep": {
                        "type": "string",
                        "description": "Optional case-insensitive regex filter applied to each line."
                    },
                    "stream": {
                        "type": "string",
                        "enum": ["stdout", "stderr", "all"],
                        "description": "Which stream to return. Defaults to `all`."
                    },
                    "since_ms": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Return only lines emitted in the last N milliseconds."
                    }
                },
                "required": ["name"],
                "additionalProperties": false
            }),
        }
    }

    pub fn list_descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: LOGS_LIST_TOOL.into(),
            description: "List every background process tracked by the logs registry: name, command, status (running/exited), pid, uptime, buffered line count, and last line.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        }
    }

    pub fn stop_descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: LOGS_STOP_TOOL.into(),
            description: "Stop a background process started by `logs_start` and remove it from the registry. The log buffer is dropped; tail it first if you need to keep the output.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Process name to stop."
                    }
                },
                "required": ["name"],
                "additionalProperties": false
            }),
        }
    }

    pub async fn run_start(&self, input: Value) -> ToolRunResult {
        match self.execute_start(input).await {
            Ok((output, meta)) => {
                let mut result = ToolRunResult::ok(output, Vec::new());
                result.meta = Some(meta);
                result
            }
            Err(err) => ToolRunResult::err(err.to_string(), Vec::new()),
        }
    }

    pub async fn run_tail(&self, input: Value) -> ToolRunResult {
        match self.execute_tail(input).await {
            Ok((output, meta)) => {
                let mut result = ToolRunResult::ok(output, Vec::new());
                result.meta = Some(meta);
                result
            }
            Err(err) => ToolRunResult::err(err.to_string(), Vec::new()),
        }
    }

    pub async fn run_list(&self, _input: Value) -> ToolRunResult {
        match self.execute_list().await {
            Ok((output, meta)) => {
                let mut result = ToolRunResult::ok(output, Vec::new());
                result.meta = Some(meta);
                result
            }
            Err(err) => ToolRunResult::err(err.to_string(), Vec::new()),
        }
    }

    pub async fn run_stop(&self, input: Value) -> ToolRunResult {
        match self.execute_stop(input).await {
            Ok((output, meta)) => {
                let mut result = ToolRunResult::ok(output, Vec::new());
                result.meta = Some(meta);
                result
            }
            Err(err) => ToolRunResult::err(err.to_string(), Vec::new()),
        }
    }

    async fn execute_start(&self, input: Value) -> Result<(String, Value)> {
        let parsed: StartInput = serde_json::from_value(input)
            .map_err(|err| anyhow!("invalid logs_start input: {err}"))?;
        let command = parsed.command.trim().to_string();
        if command.is_empty() {
            bail!("command is required");
        }

        let cwd = match parsed.cwd.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            Some(rel) => {
                let candidate = self.workspace_root.join(rel);
                if !candidate.starts_with(&self.workspace_root) {
                    bail!("cwd must stay inside the workspace");
                }
                candidate
            }
            None => self.workspace_root.clone(),
        };

        let mut processes = self.registry.processes.lock().await;
        // Exited processes keep their buffer until logs_stop, but don't count toward the cap.
        let running_count = {
            let mut count = 0usize;
            for entry in processes.values() {
                if matches!(*entry.status.lock().await, ProcessStatus::Running) {
                    count += 1;
                }
            }
            count
        };
        if running_count >= MAX_PROCESSES {
            bail!(
                "too many running processes ({running_count}/{MAX_PROCESSES}). Stop one with logs_stop before starting another."
            );
        }

        let name = parsed
            .name
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| generate_name(&command));
        validate_name(&name)?;
        if let Some(existing) = processes.get(&name) {
            if matches!(*existing.status.lock().await, ProcessStatus::Running) {
                bail!("process `{name}` is already running. Stop it first or pick another name.");
            }
        }

        let mut command_builder = build_shell_command(&command);
        command_builder
            .current_dir(&cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = command_builder
            .spawn()
            .with_context(|| format!("failed to spawn `{command}`"))?;
        let pid = child.id();
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("missing stdout"))?;
        let stderr = child.stderr.take().ok_or_else(|| anyhow!("missing stderr"))?;

        let buffer: Arc<Mutex<VecDeque<LogLine>>> =
            Arc::new(Mutex::new(VecDeque::with_capacity(RING_BUFFER_LINES)));
        let status: Arc<Mutex<ProcessStatus>> = Arc::new(Mutex::new(ProcessStatus::Running));
        let child_arc: Arc<Mutex<Option<Child>>> = Arc::new(Mutex::new(Some(child)));

        let entry = Arc::new(ProcessEntry {
            name: name.clone(),
            command: command.clone(),
            cwd: cwd.clone(),
            pid,
            started_at: SystemTime::now(),
            status: status.clone(),
            buffer: buffer.clone(),
            child: child_arc.clone(),
        });
        processes.insert(name.clone(), entry.clone());
        drop(processes);

        spawn_reader(stdout, LogStream::Stdout, buffer.clone());
        spawn_reader(stderr, LogStream::Stderr, buffer.clone());
        spawn_waiter(child_arc.clone(), status.clone());

        let mut ready_state = "skipped";
        if let Some(pattern) = parsed.ready_pattern.as_deref() {
            let regex = Regex::new(pattern)
                .with_context(|| format!("invalid ready_pattern regex `{pattern}`"))?;
            let timeout_ms = parsed
                .ready_timeout_ms
                .map(|v| v.clamp(100, MAX_READY_TIMEOUT_MS))
                .unwrap_or(DEFAULT_READY_TIMEOUT_MS);
            ready_state = if wait_for_pattern(&buffer, &regex, timeout_ms, &status).await {
                "matched"
            } else {
                "timeout"
            };
        }

        let output = format!(
            "Started `{name}` (pid {})\ncommand: {command}\ncwd: {}\nready: {ready_state}\nUse logs_tail with name=\"{name}\" to read output.",
            pid.map(|v| v.to_string()).unwrap_or_else(|| "?".into()),
            cwd.display(),
        );
        let meta = json!({
            "logs": {
                "action": "start",
                "name": name,
                "pid": pid,
                "ready": ready_state,
                "command": command,
            }
        });
        Ok((output, meta))
    }

    async fn execute_tail(&self, input: Value) -> Result<(String, Value)> {
        let parsed: TailInput = serde_json::from_value(input)
            .map_err(|err| anyhow!("invalid logs_tail input: {err}"))?;
        let entry = self.lookup(&parsed.name).await?;
        let lines_cap = parsed
            .lines
            .map(|v| v.clamp(1, MAX_TAIL_LINES))
            .unwrap_or(DEFAULT_TAIL_LINES);
        let stream_filter = parsed.stream.unwrap_or(StreamFilter::All);
        let grep = match parsed.grep.as_deref() {
            Some(p) if !p.is_empty() => Some(
                Regex::new(&format!("(?i){p}"))
                    .with_context(|| format!("invalid grep regex `{p}`"))?,
            ),
            _ => None,
        };
        let since_cutoff = parsed.since_ms.map(|ms| {
            SystemTime::now()
                .checked_sub(Duration::from_millis(ms))
                .unwrap_or(UNIX_EPOCH)
        });

        let snapshot: Vec<LogLine> = {
            let buffer = entry.buffer.lock().await;
            buffer.iter().cloned().collect()
        };

        let mut selected: Vec<&LogLine> = snapshot
            .iter()
            .filter(|line| match stream_filter {
                StreamFilter::All => true,
                StreamFilter::Stdout => line.stream == LogStream::Stdout,
                StreamFilter::Stderr => line.stream == LogStream::Stderr,
            })
            .filter(|line| match &since_cutoff {
                Some(cutoff) => line.timestamp >= *cutoff,
                None => true,
            })
            .filter(|line| match &grep {
                Some(re) => re.is_match(&line.text),
                None => true,
            })
            .collect();

        let total_matched = selected.len();
        if selected.len() > lines_cap {
            selected = selected.split_off(selected.len() - lines_cap);
        }
        let returned = selected.len();

        let status_label = entry.status.lock().await.label();
        let mut output = format!(
            "process: {} ({})\nreturned: {returned} of {total_matched} matching lines\n",
            entry.name, status_label,
        );
        if selected.is_empty() {
            output.push_str("(no lines match the filter)\n");
        } else {
            output.push('\n');
            for line in &selected {
                output.push_str(&format!("[{}] {}\n", line.stream.as_str(), line.text));
            }
        }

        let meta = json!({
            "logs": {
                "action": "tail",
                "name": entry.name,
                "status": status_label,
                "returned": returned,
                "matched": total_matched,
            }
        });
        Ok((clip(output, TOOL_OUTPUT_LIMIT), meta))
    }

    async fn execute_list(&self) -> Result<(String, Value)> {
        let processes = self.registry.processes.lock().await;
        if processes.is_empty() {
            return Ok((
                "No background processes tracked.".to_string(),
                json!({ "logs": { "action": "list", "processes": [] } }),
            ));
        }
        let mut rows = Vec::new();
        let mut entries = Vec::new();
        for entry in processes.values() {
            let status = entry.status.lock().await.label();
            let buffer_len = entry.buffer.lock().await.len();
            let last_line = entry
                .buffer
                .lock()
                .await
                .back()
                .map(|line| line.text.clone())
                .unwrap_or_default();
            let uptime_ms = entry
                .started_at
                .elapsed()
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            rows.push(format!(
                "- {} · {} · pid={} · uptime={}ms · lines={}\n  cmd: {}\n  last: {}",
                entry.name,
                status,
                entry.pid.map(|v| v.to_string()).unwrap_or_else(|| "?".into()),
                uptime_ms,
                buffer_len,
                entry.command,
                truncate(&last_line, 200),
            ));
            entries.push(json!({
                "name": entry.name,
                "status": status,
                "pid": entry.pid,
                "command": entry.command,
                "uptime_ms": uptime_ms,
                "buffered_lines": buffer_len,
            }));
        }
        rows.sort();
        let output = rows.join("\n");
        let meta = json!({ "logs": { "action": "list", "processes": entries } });
        Ok((output, meta))
    }

    async fn execute_stop(&self, input: Value) -> Result<(String, Value)> {
        let parsed: StopInput = serde_json::from_value(input)
            .map_err(|err| anyhow!("invalid logs_stop input: {err}"))?;
        let name = parsed.name.trim().to_string();
        if name.is_empty() {
            bail!("name is required");
        }

        let entry = {
            let mut processes = self.registry.processes.lock().await;
            processes.remove(&name)
        }
        .ok_or_else(|| anyhow!("no process named `{name}`"))?;

        let was_running = matches!(*entry.status.lock().await, ProcessStatus::Running);
        if was_running {
            let mut child_slot = entry.child.lock().await;
            if let Some(child) = child_slot.as_mut() {
                let _ = child.start_kill();
                // Give the process a brief window to flush.
                drop(child_slot);
                sleep(Duration::from_millis(150)).await;
            }
        }

        let status_after = entry.status.lock().await.label();
        let output = format!("Stopped `{name}` (was {}, now {status_after}).", if was_running { "running" } else { "exited" });
        let meta = json!({
            "logs": {
                "action": "stop",
                "name": name,
                "was_running": was_running,
                "status": status_after,
            }
        });
        Ok((output, meta))
    }

    async fn lookup(&self, name: &str) -> Result<Arc<ProcessEntry>> {
        let processes = self.registry.processes.lock().await;
        processes
            .get(name.trim())
            .cloned()
            .ok_or_else(|| anyhow!("no process named `{name}` — call logs_list to see active ones"))
    }
}

#[derive(Debug, Deserialize)]
struct StartInput {
    command: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    ready_pattern: Option<String>,
    #[serde(default)]
    ready_timeout_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct TailInput {
    name: String,
    #[serde(default)]
    lines: Option<usize>,
    #[serde(default)]
    grep: Option<String>,
    #[serde(default)]
    stream: Option<StreamFilter>,
    #[serde(default)]
    since_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct StopInput {
    name: String,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum StreamFilter {
    All,
    Stdout,
    Stderr,
}

fn build_shell_command(command: &str) -> Command {
    #[cfg(windows)]
    {
        let mut cmd = Command::new("cmd");
        cmd.arg("/C").arg(command);
        cmd
    }
    #[cfg(not(windows))]
    {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(command);
        cmd
    }
}

fn spawn_reader<R>(reader: R, stream: LogStream, buffer: Arc<Mutex<VecDeque<LogLine>>>)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(raw)) => {
                    let clean = strip_ansi(&raw);
                    let line = LogLine {
                        timestamp: SystemTime::now(),
                        stream,
                        text: clean,
                    };
                    let mut buf = buffer.lock().await;
                    if buf.len() == RING_BUFFER_LINES {
                        buf.pop_front();
                    }
                    buf.push_back(line);
                }
                Ok(None) => break,
                Err(_) => break,
            }
        }
    });
}

fn spawn_waiter(child: Arc<Mutex<Option<Child>>>, status: Arc<Mutex<ProcessStatus>>) {
    tokio::spawn(async move {
        // Take ownership of the Child to await it without holding the mutex.
        let mut owned = {
            let mut slot = child.lock().await;
            slot.take()
        };
        if let Some(mut process) = owned.take() {
            let exit = process.wait().await;
            let code = exit.ok().and_then(|status| status.code());
            let mut s = status.lock().await;
            *s = ProcessStatus::Exited {
                code,
                at: SystemTime::now(),
            };
        }
    });
}

async fn wait_for_pattern(
    buffer: &Arc<Mutex<VecDeque<LogLine>>>,
    regex: &Regex,
    timeout_ms: u64,
    status: &Arc<Mutex<ProcessStatus>>,
) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_millis(timeout_ms);
    let mut cursor = 0usize;
    loop {
        {
            let buf = buffer.lock().await;
            // Lines older than cursor were already inspected.
            for line in buf.iter().skip(cursor) {
                if regex.is_match(&line.text) {
                    return true;
                }
            }
            cursor = buf.len();
        }
        if matches!(*status.lock().await, ProcessStatus::Exited { .. }) {
            return false;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        sleep(Duration::from_millis(75)).await;
    }
}

fn strip_ansi(input: &str) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"\x1b\[[0-9;?]*[ -/]*[@-~]").expect("ansi regex"));
    re.replace_all(input, "").into_owned()
}

fn truncate(input: &str, max_chars: usize) -> String {
    if input.chars().count() <= max_chars {
        return input.to_string();
    }
    let mut out: String = input.chars().take(max_chars).collect();
    out.push('…');
    out
}

fn clip(text: String, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text;
    }
    let mut clipped: String = text.chars().take(limit).collect();
    clipped.push_str("\n\n[Output truncated]");
    clipped
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 64 {
        bail!("name must be between 1 and 64 chars");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
    {
        bail!("name must contain only ASCII letters, digits, `_`, `-`, or `.`");
    }
    Ok(())
}

fn generate_name(command: &str) -> String {
    let first_token = command
        .split_whitespace()
        .next()
        .unwrap_or("proc")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect::<String>();
    let base = if first_token.is_empty() {
        "proc".to_string()
    } else {
        first_token
    };
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| (d.as_millis() % 100_000) as u64)
        .unwrap_or(0);
    format!("{base}-{suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_ansi_codes() {
        let raw = "\x1b[31mhello\x1b[0m world";
        assert_eq!(strip_ansi(raw), "hello world");
    }

    #[test]
    fn validates_names() {
        assert!(validate_name("api").is_ok());
        assert!(validate_name("api-v2.1_dev").is_ok());
        assert!(validate_name("").is_err());
        assert!(validate_name("bad name").is_err());
        assert!(validate_name("bad/slash").is_err());
    }

    #[test]
    fn truncates_long_strings() {
        let long = "x".repeat(500);
        let out = truncate(&long, 50);
        assert_eq!(out.chars().count(), 51);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn name_generation_uses_command_token() {
        let name = generate_name("npm run dev");
        assert!(name.starts_with("npm-"));
    }
}
