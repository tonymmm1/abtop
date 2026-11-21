use super::process::{self, ProcInfo};
use crate::model::{
    AgentSession, ChatMessage, ChatRole, ChildProcess, FileAccess, FileOp, LaunchSurface,
    SessionStatus, SubAgent, ToolCall, MAX_CHAT_MESSAGES, MAX_FILE_ACCESSES,
};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Maximum per-turn history entries retained per session.
const MAX_HISTORY_LEN: usize = 200;
/// Maximum tool-call timeline entries retained per session.
const MAX_TOOL_CALLS: usize = 200;

/// Non-session `agy` subcommands that should not be reported as live sessions
/// when they do not hold an open conversation lock or database.
const NON_SESSION_SUBCOMMANDS: &[&str] = &[
    "agent",
    "agents",
    "changelog",
    "help",
    "install",
    "mcp",
    "mic-serve",
    "models",
    "plugin",
    "plugins",
    "remote-control",
    "update",
];

/// Collector for Google Antigravity CLI (`agy`) sessions.
///
/// Discovery strategy:
/// 1. Find running `agy` processes from shared process data (excluding utility
///    subcommands and wrapper ancestors).
/// 2. Map each PID to its active conversation ID and data directory via open
///    file descriptors (`presence/<id>.lock`, falling back to
///    `conversations/<id>.db`), with a cwd + `cache/last_conversations.json`
///    fallback for platforms without fd inspection (Windows) or freshly
///    opened sessions.
/// 3. Incrementally parse `<data_dir>/brain/<id>/.system_generated/logs/transcript.jsonl`
///    for model, tokens, context window %, status, tool calls, file accesses,
///    and chat history.
pub struct AgyCollector {
    default_data_dir: PathBuf,
    /// Incremental transcript parse state keyed by conversation ID.
    transcript_cache: HashMap<String, TranscriptState>,
    /// Cached `agy` version string per data directory (read from `cli.log`).
    version_cache: HashMap<PathBuf, String>,
    /// Cached subagents keyed by parent conversation ID (refreshed on slow ticks).
    subagents_cache: HashMap<String, Vec<SubAgent>>,
    /// Whether `sqlite3` CLI is available for optional subagent lookup.
    sqlite3_available: Option<bool>,
}

#[derive(Debug, Default)]
struct ProcessOpenInfo {
    cwd: Option<PathBuf>,
    paths: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DiscoveredConv {
    data_dir: PathBuf,
    conv_id: String,
    /// Timestamp from the matched lock/db file as a fallback when the
    /// transcript has no events yet.
    fallback_started_at: u64,
}

#[derive(Debug, Default)]
struct TranscriptState {
    offset: u64,
    file_identity: (u64, u64),
    started_at: u64,
    model: String,
    settings_model_hint: String,
    effort: String,
    total_input: u64,
    total_output: u64,
    total_cache_read: u64,
    last_context_tokens: u64,
    max_context_tokens: u64,
    context_history: Vec<u64>,
    compaction_count: u32,
    turn_count: u32,
    token_history: Vec<u64>,
    initial_prompt: String,
    first_assistant_text: String,
    chat_messages: Vec<ChatMessage>,
    tool_calls: Vec<ToolCall>,
    file_accesses: Vec<FileAccess>,
    /// Number of tool calls from the latest `PLANNER_RESPONSE` still awaiting
    /// matching `GENERIC` result steps.
    pending_tools: usize,
    /// Indices in `tool_calls` for the currently pending tool batch.
    pending_tool_indices: Vec<usize>,
    /// True when the only pending tool call(s) are `ask_question` (waiting for user).
    pending_ask_question: bool,
    /// Timestamp (epoch ms) of the `PLANNER_RESPONSE` that started the current
    /// pending tool batch, or 0 when no tools are in flight.
    last_assistant_ts_ms: u64,
    /// Timestamp (epoch ms) when the model started thinking (last `USER_INPUT`
    /// or completion of the last tool batch), or 0 when waiting for user input.
    last_user_ts_ms: u64,
    /// Last active tool task description, cleared when the batch completes.
    current_task: String,
    /// True if any subagent/workflow/swarm tool was observed in this session.
    saw_subagent_tool: bool,
}

impl AgyCollector {
    pub fn new() -> Self {
        let default_data_dir = std::env::var("ANTIGRAVITY_CLI_DATA_DIR")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                dirs::home_dir()
                    .unwrap_or_default()
                    .join(".gemini")
                    .join("antigravity-cli")
            });
        Self::with_data_dir(default_data_dir)
    }

    pub fn with_data_dir(default_data_dir: PathBuf) -> Self {
        Self {
            default_data_dir,
            transcript_cache: HashMap::new(),
            version_cache: HashMap::new(),
            subagents_cache: HashMap::new(),
            sqlite3_available: None,
        }
    }

    fn collect_sessions(&mut self, shared: &super::SharedProcessData) -> Vec<AgentSession> {
        let self_pid = std::process::id();
        let agy_pids = Self::find_agy_pids(&shared.process_info, self_pid);
        if agy_pids.is_empty() {
            self.transcript_cache.clear();
            self.subagents_cache.clear();
            return Vec::new();
        }

        let open_info = Self::map_pid_to_open_info(&agy_pids);
        let mut sessions = Vec::new();
        let mut claimed_conv_ids = HashSet::new();
        let mut last_convs_cache: HashMap<PathBuf, HashMap<String, String>> = HashMap::new();

        for pid in agy_pids {
            let Some(proc) = shared.process_info.get(&pid) else {
                continue;
            };
            let info = open_info.get(&pid);
            let cwd_path = info.and_then(|i| i.cwd.clone());
            let cwd_str = cwd_path
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();

            // Primary: infer (data_dir, conv_id) from open lock / db file descriptors.
            let mut discovered = info.and_then(|i| extract_conv_from_fd_paths(&i.paths));

            // Skip utility subcommands unless they actually hold an open conversation.
            if discovered.is_none() && is_non_session_command(&proc.command) {
                continue;
            }

            // Secondary fallback (Windows or freshly launched TUI): match cwd via
            // `cache/last_conversations.json`, ensuring each conversation ID is
            // claimed by at most one live PID.
            if discovered.is_none() && !cwd_str.is_empty() && self.default_data_dir.is_dir() {
                let last_map = last_convs_cache
                    .entry(self.default_data_dir.clone())
                    .or_insert_with(|| load_last_conversations(&self.default_data_dir));
                if let Some(cid) = lookup_last_conversation(last_map, &cwd_str) {
                    if is_safe_conv_id(cid) && !claimed_conv_ids.contains(cid) {
                        let fallback_started_at =
                            conv_fallback_mtime_ms(&self.default_data_dir, cid);
                        discovered = Some(DiscoveredConv {
                            data_dir: self.default_data_dir.clone(),
                            conv_id: cid.to_string(),
                            fallback_started_at,
                        });
                    }
                }
            }

            let (data_dir, session_id, fallback_started_at) = match discovered {
                Some(d) => {
                    claimed_conv_ids.insert(d.conv_id.clone());
                    (d.data_dir, d.conv_id, d.fallback_started_at)
                }
                None => {
                    // Interactive `agy` running without a conversation opened yet.
                    (
                        self.default_data_dir.clone(),
                        format!("agy-{}", pid),
                        current_time_ms(),
                    )
                }
            };

            let transcript_path = transcript_path_for(&data_dir, &session_id);
            let has_transcript = !is_symlink(&transcript_path) && transcript_path.is_file();
            if has_transcript {
                let entry = self.transcript_cache.entry(session_id.clone()).or_default();
                update_transcript_state(entry, &transcript_path);
            }

            let state = self.transcript_cache.get(&session_id);
            let model = state
                .map(resolve_display_model)
                .filter(|m| !m.is_empty())
                .unwrap_or_else(|| "-".to_string());
            let effort = state.map(|s| s.effort.clone()).unwrap_or_default();
            let total_input = state.map(|s| s.total_input).unwrap_or(0);
            let total_output = state.map(|s| s.total_output).unwrap_or(0);
            let total_cache_read = state.map(|s| s.total_cache_read).unwrap_or(0);
            let last_context_tokens = state.map(|s| s.last_context_tokens).unwrap_or(0);
            let max_context_tokens = state.map(|s| s.max_context_tokens).unwrap_or(0);
            let turn_count = state.map(|s| s.turn_count).unwrap_or(0);
            let token_history = state.map(|s| s.token_history.clone()).unwrap_or_default();
            let context_history = state.map(|s| s.context_history.clone()).unwrap_or_default();
            let compaction_count = state.map(|s| s.compaction_count).unwrap_or(0);
            let initial_prompt = state.map(|s| s.initial_prompt.clone()).unwrap_or_default();
            let first_assistant_text = state
                .map(|s| s.first_assistant_text.clone())
                .unwrap_or_default();
            let chat_messages = state.map(|s| s.chat_messages.clone()).unwrap_or_default();
            let tool_calls = state.map(|s| s.tool_calls.clone()).unwrap_or_default();
            let file_accesses = state.map(|s| s.file_accesses.clone()).unwrap_or_default();
            let pending_tools = state.map(|s| s.pending_tools).unwrap_or(0);
            let pending_ask_question = state.map(|s| s.pending_ask_question).unwrap_or(false);
            let pending_since_ms = state.map(|s| s.last_assistant_ts_ms).unwrap_or(0);
            let thinking_since_ms = state.map(|s| s.last_user_ts_ms).unwrap_or(0);
            let current_task = state.map(|s| s.current_task.clone()).unwrap_or_default();
            let saw_subagent_tool = state.is_some_and(|s| s.saw_subagent_tool);
            let started_at = state
                .map(|s| s.started_at)
                .filter(|&t| t > 0)
                .unwrap_or(fallback_started_at);

            // Optional subagents lookup (only when subagent/swarm/workflow tools appeared).
            let subagents = if saw_subagent_tool && is_safe_conv_id(&session_id) {
                if shared.slow_tick || !self.subagents_cache.contains_key(&session_id) {
                    let found = self.query_subagents(&data_dir, &session_id);
                    self.subagents_cache.insert(session_id.clone(), found);
                }
                self.subagents_cache
                    .get(&session_id)
                    .cloned()
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            let has_working_subagent = subagents.iter().any(|sa| sa.status == "working");

            // Status derivation: mirrors ClaudeCollector & CodexCollector.
            // No mtime staleness gate because thinking models (e.g. XHigh) can
            // deliberate for >30s before writing PLANNER_RESPONSE atomically.
            let has_active_descendant = process::has_active_descendant(
                pid,
                &shared.children_map,
                &shared.process_info,
                5.0,
            );
            let status = if pending_ask_question && !has_active_descendant {
                SessionStatus::Waiting
            } else if has_active_descendant || pending_tools > 0 || has_working_subagent {
                SessionStatus::Executing
            } else if thinking_since_ms > 0 {
                SessionStatus::Thinking
            } else {
                SessionStatus::Waiting
            };

            let current_tasks = if pending_ask_question {
                vec!["waiting for user answer".to_string()]
            } else if !current_task.is_empty() && status == SessionStatus::Executing {
                vec![current_task]
            } else if matches!(status, SessionStatus::Waiting) {
                vec!["waiting for input".to_string()]
            } else {
                vec!["thinking...".to_string()]
            };

            let context_window = context_window_for_agy_model(&model, max_context_tokens);
            let context_percent = if context_window > 0 && last_context_tokens > 0 {
                (last_context_tokens as f64 / context_window as f64) * 100.0
            } else {
                0.0
            };

            let mem_mb = proc.rss_kb / 1024;
            let project_name = if cwd_str.is_empty() {
                "?".to_string()
            } else {
                process::last_path_segment(&cwd_str)
                    .filter(|s| !s.is_empty())
                    .unwrap_or("?")
                    .to_string()
            };

            // Collect all descendants recursively (matches Claude/Codex/OpenCode).
            let mut children = Vec::new();
            let mut stack: Vec<u32> = shared
                .children_map
                .get(&pid)
                .cloned()
                .unwrap_or_default();
            let mut visited = HashSet::new();
            while let Some(cpid) = stack.pop() {
                if !visited.insert(cpid) {
                    continue;
                }
                if let Some(cproc) = shared.process_info.get(&cpid) {
                    let port = shared.ports.get(&cpid).and_then(|v| v.first().copied());
                    children.push(ChildProcess {
                        pid: cpid,
                        command: cproc.command.clone(),
                        mem_kb: cproc.rss_kb,
                        port,
                    });
                }
                if let Some(grandchildren) = shared.children_map.get(&cpid) {
                    stack.extend(grandchildren);
                }
            }

            let version = self.cached_version(&data_dir, shared.slow_tick);

            sessions.push(AgentSession {
                agent_cli: "agy",
                launch_surface: LaunchSurface::Cli,
                pid,
                session_id,
                cwd: cwd_str,
                project_name,
                started_at,
                status,
                model,
                effort,
                context_percent,
                total_input_tokens: total_input,
                total_output_tokens: total_output,
                total_cache_read,
                total_cache_create: 0,
                turn_count,
                current_tasks,
                mem_mb,
                version,
                // Git branch and dirty stats are filled in by MultiCollector.
                git_branch: String::new(),
                git_added: 0,
                git_modified: 0,
                token_history,
                context_history,
                compaction_count,
                context_window,
                subagents,
                mem_file_count: 0,
                mem_line_count: 0,
                children,
                initial_prompt,
                first_assistant_text,
                chat_messages,
                tool_calls,
                pending_since_ms,
                thinking_since_ms,
                file_accesses,
                config_root: super::abbrev_path(&data_dir),
            });
        }

        // Evict stale cache entries for conversations no longer active.
        let active_ids: HashSet<&str> = sessions.iter().map(|s| s.session_id.as_str()).collect();
        self.transcript_cache
            .retain(|sid, _| active_ids.contains(sid.as_str()));
        self.subagents_cache
            .retain(|sid, _| active_ids.contains(sid.as_str()));

        sessions.sort_by_key(|s| std::cmp::Reverse(s.started_at));
        sessions
    }

    fn cached_version(&mut self, data_dir: &Path, slow_tick: bool) -> String {
        if slow_tick || !self.version_cache.contains_key(data_dir) {
            let ver = read_agy_version_from_log(&data_dir.join("cli.log")).unwrap_or_default();
            self.version_cache.insert(data_dir.to_path_buf(), ver);
        }
        self.version_cache
            .get(data_dir)
            .cloned()
            .unwrap_or_default()
    }

    fn check_sqlite3(&mut self) -> bool {
        if let Some(available) = self.sqlite3_available {
            return available;
        }
        let available = Command::new("sqlite3").arg("--version").output().is_ok();
        self.sqlite3_available = Some(available);
        available
    }

    fn query_subagents(&mut self, data_dir: &Path, parent_conv_id: &str) -> Vec<SubAgent> {
        if !is_safe_conv_id(parent_conv_id) {
            return Vec::new();
        }
        let db_path = data_dir.join("conversation_summaries.db");
        if is_symlink(&db_path) || !db_path.is_file() || !self.check_sqlite3() {
            return Vec::new();
        }
        let Some(db_str) = db_path.to_str() else {
            return Vec::new();
        };
        // `parent_conv_id` is validated to contain only [A-Za-z0-9_-].
        let sql = format!(
            "SELECT conversation_id, agent_name, title, preview, not_fully_idle \
             FROM conversation_summaries \
             WHERE parent_conversation_id = '{}' \
             ORDER BY last_modified_time DESC LIMIT 16;",
            parent_conv_id
        );
        let output = match Command::new("sqlite3")
            .args(["-readonly", "-json", db_str, &sql])
            .output()
        {
            Ok(o) if o.status.success() => o,
            _ => return Vec::new(),
        };
        let stdout = String::from_utf8_lossy(&output.stdout);
        let trimmed = stdout.trim();
        if trimmed.is_empty() {
            return Vec::new();
        }
        let Ok(rows) = serde_json::from_str::<Vec<Value>>(trimmed) else {
            return Vec::new();
        };

        let mut out = Vec::new();
        for row in rows {
            let Some(cid) = row["conversation_id"].as_str() else {
                continue;
            };
            if !is_safe_conv_id(cid) {
                continue;
            }
            let raw_name = [
                row["agent_name"].as_str().unwrap_or(""),
                row["title"].as_str().unwrap_or(""),
                row["preview"].as_str().unwrap_or(""),
            ]
            .into_iter()
            .find(|s| !s.trim().is_empty())
            .unwrap_or("subagent");
            let name = truncate_chars(
                &super::redact_secrets(&super::sanitize_terminal_text(raw_name)),
                30,
            );

            let tp = transcript_path_for(data_dir, cid);
            let mut tokens = 0u64;
            let mut active = row["not_fully_idle"].as_i64().unwrap_or(0) != 0
                || row["not_fully_idle"].as_bool().unwrap_or(false);
            if !is_symlink(&tp) && tp.is_file() {
                if let Ok(meta) = fs::metadata(&tp) {
                    if let Ok(modified) = meta.modified() {
                        if std::time::SystemTime::now()
                            .duration_since(modified)
                            .unwrap_or_default()
                            .as_secs()
                            < 30
                        {
                            active = true;
                        }
                    }
                }
                let mut child_state = TranscriptState::default();
                update_transcript_state(&mut child_state, &tp);
                tokens = child_state.total_input
                    + child_state.total_output
                    + child_state.total_cache_read;
            }

            out.push(SubAgent {
                name,
                status: if active { "working" } else { "done" }.to_string(),
                tokens,
            });
        }
        out
    }

    /// Find PIDs of running `agy` processes from shared process info.
    /// Drops abtop descendants and wrapper ancestors when a child `agy` exists.
    fn find_agy_pids(process_info: &HashMap<u32, ProcInfo>, self_pid: u32) -> Vec<u32> {
        let mut pids: Vec<u32> = process_info
            .iter()
            .filter(|(pid, info)| {
                process::cmd_has_binary(&info.command, "agy")
                    && !info.command.contains("grep")
                    && !process::is_descendant_of(**pid, self_pid, process_info)
            })
            .map(|(pid, _)| *pid)
            .collect();

        // Drop wrapper ancestors (e.g. mise/npm shim -> real `agy` binary)
        // when a descendant `agy` process is already in the candidate set.
        let candidates = pids.clone();
        pids.retain(|pid| {
            !candidates
                .iter()
                .any(|other| *other != *pid && process::is_descendant_of(*other, *pid, process_info))
        });

        pids.sort_unstable();
        pids
    }

    fn map_pid_to_open_info(pids: &[u32]) -> HashMap<u32, ProcessOpenInfo> {
        if pids.is_empty() {
            return HashMap::new();
        }

        #[cfg(target_os = "linux")]
        {
            map_pid_to_proc_open_info(pids)
        }

        #[cfg(target_vendor = "apple")]
        {
            map_pid_to_libproc_open_info(pids)
        }

        #[cfg(target_os = "windows")]
        {
            map_pid_to_sysinfo_open_info(pids)
        }

        #[cfg(all(
            not(target_os = "linux"),
            not(target_vendor = "apple"),
            not(target_os = "windows")
        ))]
        {
            map_pid_to_lsof_open_info(pids)
        }
    }
}

impl Default for AgyCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl super::AgentCollector for AgyCollector {
    fn collect(&mut self, shared: &super::SharedProcessData) -> Vec<AgentSession> {
        self.collect_sessions(shared)
    }
}

#[cfg(target_os = "linux")]
fn map_pid_to_proc_open_info(pids: &[u32]) -> HashMap<u32, ProcessOpenInfo> {
    let mut map = HashMap::new();
    for &pid in pids {
        let cwd = fs::read_link(format!("/proc/{pid}/cwd")).ok();
        let paths = process::scan_proc_fds(pid)
            .into_iter()
            .filter(|p| p.is_absolute())
            .collect();
        map.insert(pid, ProcessOpenInfo { cwd, paths });
    }
    map
}

#[cfg(target_vendor = "apple")]
fn map_pid_to_libproc_open_info(pids: &[u32]) -> HashMap<u32, ProcessOpenInfo> {
    use proc_pidinfo::{
        proc_pidfdinfo, proc_pidinfo_list, Pid, ProcFDInfo, ProcFDType, VnodeFdInfoWithPath,
    };

    let mut map = HashMap::new();
    for &raw_pid in pids {
        let pid = Pid(raw_pid);
        let paths = proc_pidinfo_list::<ProcFDInfo>(pid)
            .unwrap_or_default()
            .into_iter()
            .filter(|fd| fd.proc_fdtype == ProcFDType::VNODE)
            .filter_map(|fd| proc_pidfdinfo::<VnodeFdInfoWithPath>(pid, fd.proc_fd).ok())
            .map(|vnode| PathBuf::from(vnode.pvip.vip_path.to_string()))
            .filter(|p| p.is_absolute())
            .collect();
        let cwd = lsof_process_cwd(raw_pid);
        map.insert(raw_pid, ProcessOpenInfo { cwd, paths });
    }
    map
}

#[cfg(target_vendor = "apple")]
fn lsof_process_cwd(pid: u32) -> Option<PathBuf> {
    let output = Command::new("lsof")
        .args(["-a", "-p", &pid.to_string(), "-d", "cwd", "-Fn"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .find(|l| l.starts_with('n') && l.len() > 1)
        .map(|l| PathBuf::from(&l[1..]))
}

#[cfg(target_os = "windows")]
fn map_pid_to_sysinfo_open_info(pids: &[u32]) -> HashMap<u32, ProcessOpenInfo> {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

    let mut map = HashMap::new();
    let target_pids: Vec<Pid> = pids.iter().map(|&p| Pid::from_u32(p)).collect();
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&target_pids),
        false,
        ProcessRefreshKind::new().with_cwd(UpdateKind::Always),
    );
    for &raw_pid in pids {
        let cwd = sys
            .process(Pid::from_u32(raw_pid))
            .and_then(|p| p.cwd())
            .map(|p| p.to_path_buf());
        map.insert(
            raw_pid,
            ProcessOpenInfo {
                cwd,
                paths: Vec::new(),
            },
        );
    }
    map
}

#[cfg(all(
    not(target_os = "linux"),
    not(target_vendor = "apple"),
    not(target_os = "windows")
))]
fn map_pid_to_lsof_open_info(pids: &[u32]) -> HashMap<u32, ProcessOpenInfo> {
    let mut map: HashMap<u32, ProcessOpenInfo> = HashMap::new();
    let pid_arg = pids
        .iter()
        .map(|p| p.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let Ok(output) = Command::new("lsof")
        .args(["-a", "-p", &pid_arg, "-Ffn"])
        .output()
    else {
        return map;
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut current_pid: Option<u32> = None;
    let mut current_fd = String::new();
    for line in stdout.lines() {
        if let Some(pid_str) = line.strip_prefix('p') {
            current_pid = pid_str.parse().ok();
            current_fd.clear();
        } else if let Some(fd) = line.strip_prefix('f') {
            current_fd = fd.to_string();
        } else if let Some(name) = line.strip_prefix('n') {
            if let Some(pid) = current_pid {
                let path = PathBuf::from(name);
                if path.is_absolute() {
                    let entry = map.entry(pid).or_default();
                    if current_fd == "cwd" {
                        entry.cwd = Some(path);
                    } else {
                        entry.paths.push(path);
                    }
                }
            }
        }
    }
    map
}

/// Returns true if `cmd` invokes a non-session utility subcommand of `agy`
/// (e.g. `agy models`, `agy --version`, `agy mcp list`).
fn is_non_session_command(cmd: &str) -> bool {
    let mut tokens = cmd.split_whitespace();
    let Some(first) = tokens.next() else {
        return false;
    };
    // Skip executable token (and wrapper target if interpreter-wrapped).
    let rest: Vec<&str> = if process::cmd_first_token_has_binary(first, "agy") {
        tokens.collect()
    } else {
        tokens.skip(1).collect()
    };
    for tok in rest {
        if tok == "--version" || tok == "-V" || tok == "--help" || tok == "-h" {
            return true;
        }
        if tok.starts_with('-') {
            continue;
        }
        return NON_SESSION_SUBCOMMANDS.contains(&tok);
    }
    false
}

/// Extract `(data_dir, conversation_id)` from open file descriptor paths of an
/// `agy` process.
///
/// Prefers `<data_dir>/presence/<conv_id>.lock` (the active conversation lock),
/// falling back to `<data_dir>/conversations/<conv_id>.db`. When multiple
/// candidates exist at the same priority level, prefers the one whose
/// transcript (or lock/db file) was modified most recently.
fn extract_conv_from_fd_paths(paths: &[PathBuf]) -> Option<DiscoveredConv> {
    let mut locks: Vec<(PathBuf, String, PathBuf)> = Vec::new();
    let mut dbs: Vec<(PathBuf, String, PathBuf)> = Vec::new();

    for path in paths {
        let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(parent) = path.parent() else {
            continue;
        };
        let Some(parent_name) = parent.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(data_dir) = parent.parent() else {
            continue;
        };

        if parent_name == "presence" {
            if let Some(cid) = file_name.strip_suffix(".lock") {
                if is_safe_conv_id(cid) {
                    locks.push((data_dir.to_path_buf(), cid.to_string(), path.clone()));
                }
            }
        } else if parent_name == "conversations" {
            if let Some(cid) = file_name.strip_suffix(".db") {
                if is_safe_conv_id(cid) {
                    dbs.push((data_dir.to_path_buf(), cid.to_string(), path.clone()));
                }
            }
        }
    }

    let pick = |mut list: Vec<(PathBuf, String, PathBuf)>| -> Option<DiscoveredConv> {
        if list.is_empty() {
            return None;
        }
        if list.len() > 1 {
            list.sort_by_key(|(data_dir, cid, fd_path)| {
                let tp = transcript_path_for(data_dir, cid);
                let ts = file_mtime_ms(&tp).max(file_mtime_ms(fd_path));
                std::cmp::Reverse(ts)
            });
        }
        let (data_dir, conv_id, fd_path) = list.remove(0);
        let fallback_started_at = file_mtime_ms(&fd_path);
        Some(DiscoveredConv {
            data_dir,
            conv_id,
            fallback_started_at: if fallback_started_at > 0 {
                fallback_started_at
            } else {
                current_time_ms()
            },
        })
    };

    pick(locks).or_else(|| pick(dbs))
}

fn transcript_path_for(data_dir: &Path, conv_id: &str) -> PathBuf {
    data_dir
        .join("brain")
        .join(conv_id)
        .join(".system_generated")
        .join("logs")
        .join("transcript.jsonl")
}

fn load_last_conversations(data_dir: &Path) -> HashMap<String, String> {
    let path = data_dir.join("cache").join("last_conversations.json");
    if is_symlink(&path) {
        return HashMap::new();
    }
    fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str::<HashMap<String, String>>(&s).ok())
        .unwrap_or_default()
}

fn lookup_last_conversation<'a>(map: &'a HashMap<String, String>, cwd: &str) -> Option<&'a str> {
    if let Some(id) = map.get(cwd) {
        return Some(id.as_str());
    }
    let trimmed = cwd.trim_end_matches(['/', '\\']);
    map.iter()
        .find(|(k, _)| k.trim_end_matches(['/', '\\']) == trimmed)
        .map(|(_, v)| v.as_str())
}

fn conv_fallback_mtime_ms(data_dir: &Path, conv_id: &str) -> u64 {
    let lock = data_dir.join("presence").join(format!("{conv_id}.lock"));
    let db = data_dir.join("conversations").join(format!("{conv_id}.db"));
    let ts = file_mtime_ms(&lock).max(file_mtime_ms(&db));
    if ts > 0 {
        ts
    } else {
        current_time_ms()
    }
}

fn file_mtime_ms(path: &Path) -> u64 {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn is_safe_conv_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn is_symlink(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
}

fn file_identity(meta: &fs::Metadata) -> (u64, u64) {
    #[cfg(unix)]
    {
        (meta.dev(), meta.ino())
    }
    #[cfg(not(unix))]
    {
        (0, meta.len())
    }
}

/// Incrementally read newly appended complete JSONL lines from `path` into `state`.
/// Resets and re-parses from offset 0 if the file was replaced or truncated.
fn update_transcript_state(state: &mut TranscriptState, path: &Path) {
    let Ok(meta) = fs::metadata(path) else {
        return;
    };
    let file_len = meta.len();
    let ident = file_identity(&meta);

    #[cfg(unix)]
    let replaced = state.file_identity != (0, 0) && state.file_identity != ident;
    #[cfg(not(unix))]
    let replaced = false;

    if replaced || file_len < state.offset {
        *state = TranscriptState::default();
    } else if file_len == state.offset && state.offset > 0 {
        return;
    }

    let Ok(mut file) = fs::File::open(path) else {
        return;
    };
    if state.offset > 0 && file.seek(SeekFrom::Start(state.offset)).is_err() {
        return;
    }

    let mut reader = BufReader::new(file);
    let mut bytes_consumed = 0u64;
    let mut line_buf = Vec::new();
    loop {
        line_buf.clear();
        let Ok(n) = reader.read_until(b'\n', &mut line_buf) else {
            break;
        };
        if n == 0 {
            break;
        }
        // Only consume complete newline-terminated lines so a partial line
        // mid-write is re-read cleanly on the next tick.
        if !line_buf.ends_with(b"\n") {
            break;
        }
        bytes_consumed += n as u64;
        let line = String::from_utf8_lossy(&line_buf);
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            apply_transcript_line(state, trimmed);
        }
    }

    state.offset += bytes_consumed;
    state.file_identity = ident;
}

/// Apply one JSONL record from `agy`'s `transcript.jsonl` to `state`.
fn apply_transcript_line(state: &mut TranscriptState, line: &str) {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return;
    };
    let step_type = v["type"].as_str().unwrap_or("");
    let step_status = v["status"].as_str().unwrap_or("");
    let ts_ms = v["created_at"]
        .as_str()
        .and_then(parse_rfc3339_ms)
        .unwrap_or(0);
    if state.started_at == 0 && ts_ms > 0 {
        state.started_at = ts_ms;
    }

    match step_type {
        "USER_INPUT" => {
            let raw_content = v["content"].as_str().unwrap_or("");
            if let Some(hint) = extract_settings_model(raw_content) {
                state.settings_model_hint = hint;
            }
            let prompt = extract_user_request(raw_content);
            let clean_prompt =
                super::redact_secrets(&super::sanitize_terminal_text(prompt));
            let clean_prompt = clean_prompt.trim();
            if !clean_prompt.is_empty() {
                if state.initial_prompt.is_empty() {
                    state.initial_prompt = truncate_chars(clean_prompt, 200);
                }
                push_chat_message(&mut state.chat_messages, ChatRole::User, clean_prompt);
            }
            state.pending_tools = 0;
            state.pending_tool_indices.clear();
            state.pending_ask_question = false;
            state.last_assistant_ts_ms = 0;
            state.current_task.clear();
            state.last_user_ts_ms = if ts_ms > 0 { ts_ms } else { current_time_ms() };
        }
        "PLANNER_RESPONSE" => {
            state.turn_count = state.turn_count.saturating_add(1);

            if let Some(m) = v["model"].as_str().filter(|s| !s.is_empty()) {
                state.model = m.to_string();
                if let Some(eff) = extract_effort_from_model(m) {
                    state.effort = eff.to_string();
                } else if !state.settings_model_hint.is_empty() {
                    if let Some(eff) = extract_effort_from_model(&state.settings_model_hint) {
                        state.effort = eff.to_string();
                    }
                }
            }

            let in_tok = v["input_tokens"].as_u64().unwrap_or(0);
            let cache_r = v["cache_read_tokens"].as_u64().unwrap_or(0);
            let out_tok = v["output_tokens"].as_u64().unwrap_or(0);
            state.total_input = state.total_input.saturating_add(in_tok);
            state.total_cache_read = state.total_cache_read.saturating_add(cache_r);
            state.total_output = state.total_output.saturating_add(out_tok);

            // In agy transcripts, `input_tokens` is non-cached prompt tokens and
            // `cache_read_tokens` is cached prompt tokens; their sum equals the
            // full prompt context size for this turn.
            let turn_context = in_tok.saturating_add(cache_r);
            if turn_context > 0 {
                if state.last_context_tokens > 0
                    && turn_context.saturating_mul(10) < state.last_context_tokens.saturating_mul(7)
                {
                    state.compaction_count = state.compaction_count.saturating_add(1);
                }
                state.last_context_tokens = turn_context;
                state.max_context_tokens = state.max_context_tokens.max(turn_context);
                push_bounded(&mut state.context_history, turn_context, MAX_HISTORY_LEN);
            }

            let turn_total = in_tok.saturating_add(out_tok).saturating_add(cache_r);
            if turn_total > 0 {
                push_bounded(&mut state.token_history, turn_total, MAX_HISTORY_LEN);
            }

            if let Some(text) = v["content"].as_str() {
                let clean = super::redact_secrets(&super::sanitize_terminal_text(text));
                let clean = clean.trim();
                if !clean.is_empty() {
                    if state.first_assistant_text.is_empty() {
                        state.first_assistant_text = truncate_chars(clean, 200);
                    }
                    push_chat_message(&mut state.chat_messages, ChatRole::Assistant, clean);
                }
            }

            let tool_calls = v["tool_calls"].as_array();
            let has_tools = tool_calls.is_some_and(|a| !a.is_empty());
            state.pending_tool_indices.clear();
            state.pending_ask_question = false;

            if let Some(calls) = tool_calls.filter(|a| !a.is_empty()) {
                let mut all_ask_question = true;
                for tc in calls {
                    let name = tc["name"].as_str().unwrap_or("tool").to_string();
                    if name != "ask_question" {
                        all_ask_question = false;
                    }
                    if matches!(
                        name.as_str(),
                        "invoke_subagent" | "run_workflow" | "swarm"
                    ) {
                        state.saw_subagent_tool = true;
                    }
                    let args = tc.get("args");
                    let (arg_preview, file_op) = summarize_tool_call(&name, args);
                    if let Some((path, op)) = file_op {
                        push_bounded(
                            &mut state.file_accesses,
                            FileAccess {
                                path,
                                operation: op,
                                turn_index: state.turn_count,
                            },
                            MAX_FILE_ACCESSES,
                        );
                    }

                    let task_label = if arg_preview.is_empty() {
                        name.clone()
                    } else {
                        format!("{name} {arg_preview}")
                    };
                    state.current_task = truncate_chars(&task_label, 120);

                    // Keep tool_calls bounded; adjust any stored indices when draining.
                    if state.tool_calls.len() >= MAX_TOOL_CALLS {
                        state.tool_calls.remove(0);
                        state.pending_tool_indices.retain_mut(|idx| {
                            if *idx == 0 {
                                false
                            } else {
                                *idx -= 1;
                                true
                            }
                        });
                    }
                    let idx = state.tool_calls.len();
                    state.tool_calls.push(ToolCall {
                        name,
                        arg: arg_preview,
                        duration_ms: 0,
                    });
                    state.pending_tool_indices.push(idx);
                }

                state.pending_tools = calls.len();
                state.pending_ask_question = all_ask_question;
                state.last_assistant_ts_ms = if ts_ms > 0 { ts_ms } else { current_time_ms() };
                state.last_user_ts_ms = 0;
            } else if !has_tools {
                state.pending_tools = 0;
                state.last_assistant_ts_ms = 0;
                state.current_task.clear();
                // If an explicit in-progress planner step is ever logged, stay Thinking;
                // otherwise the turn is complete and waiting for the user.
                if is_in_progress_status(step_status) {
                    state.last_user_ts_ms = if ts_ms > 0 { ts_ms } else { current_time_ms() };
                } else {
                    state.last_user_ts_ms = 0;
                }
            }
        }
        "GENERIC" => {
            // Tool result step. Extract exact duration if `Created At` / `Completed At`
            // are present in content, else derive from the planner timestamp.
            let content = v["content"].as_str().unwrap_or("");
            let (created_opt, completed_opt) = parse_generic_timestamps(content);
            let duration_ms = match (created_opt, completed_opt) {
                (Some(c), Some(done)) => done.saturating_sub(c).max(1),
                _ if state.last_assistant_ts_ms > 0 && ts_ms >= state.last_assistant_ts_ms => {
                    ts_ms.saturating_sub(state.last_assistant_ts_ms).max(1)
                }
                _ => 1,
            };

            if !state.pending_tool_indices.is_empty() {
                let idx = state.pending_tool_indices.remove(0);
                if let Some(tc) = state.tool_calls.get_mut(idx) {
                    tc.duration_ms = duration_ms;
                }
            }

            if state.pending_tools > 0 {
                state.pending_tools -= 1;
                if state.pending_tools == 0 {
                    state.last_assistant_ts_ms = 0;
                    state.pending_ask_question = false;
                    state.current_task.clear();
                    let done_ts = completed_opt.or((ts_ms > 0).then_some(ts_ms));
                    state.last_user_ts_ms = done_ts.unwrap_or_else(current_time_ms);
                }
            }
        }
        "SYSTEM_MESSAGE" => {
            // System reminders appear between steps and do not alter active turn state.
        }
        _ => {}
    }
}

fn resolve_display_model(state: &TranscriptState) -> String {
    if state.model == "MODEL_GOOGLE_GEMINI_INTERNAL_BYOM" || state.model.is_empty() {
        if !state.settings_model_hint.is_empty() {
            return state.settings_model_hint.clone();
        }
        if state.model == "MODEL_GOOGLE_GEMINI_INTERNAL_BYOM" {
            return "byom".to_string();
        }
    }
    state.model.clone()
}

/// Context window size for `agy` models.
/// Gemini 3.x/4.x models natively have 1M token windows; BYOM/proxy models
/// default to 200K unless `[1m]` or observed usage exceeds 200K.
pub(crate) fn context_window_for_agy_model(model: &str, max_context_tokens: u64) -> u64 {
    let lower = model.to_ascii_lowercase();
    if lower.starts_with("gemini-")
        || lower.contains("gemini ")
        || lower.contains("[1m]")
        || max_context_tokens > 200_000
    {
        1_000_000
    } else if lower.starts_with("gpt-") || lower.contains("gpt ") {
        400_000
    } else {
        200_000
    }
}

fn extract_effort_from_model(model: &str) -> Option<&'static str> {
    let lower = model.to_ascii_lowercase();
    if lower.ends_with("-xhigh") || lower.contains("(xhigh)") {
        Some("xhigh")
    } else if lower.ends_with("-high") || lower.contains("(high)") {
        Some("high")
    } else if lower.ends_with("-medium") || lower.contains("(medium)") {
        Some("medium")
    } else if lower.ends_with("-low") || lower.contains("(low)") {
        Some("low")
    } else {
        None
    }
}

fn extract_user_request(content: &str) -> &str {
    if let Some(start) = content.find("<USER_REQUEST>") {
        let after = &content[start + "<USER_REQUEST>".len()..];
        if let Some(end) = after.find("</USER_REQUEST>") {
            return after[..end].trim();
        }
        return after.trim();
    }
    // Strip trailing metadata tags if present without <USER_REQUEST>.
    let end = content
        .find("<ADDITIONAL_METADATA>")
        .or_else(|| content.find("<USER_SETTINGS_CHANGE>"))
        .unwrap_or(content.len());
    content[..end].trim()
}

fn extract_settings_model(content: &str) -> Option<String> {
    let start = content.find("<USER_SETTINGS_CHANGE>")?;
    let section = &content[start..];
    let marker = "`Model Selection`";
    let pos = section.find(marker)?;
    let rest = &section[pos + marker.len()..];
    let to_pos = rest.find(" to ")?;
    let after_to = &rest[to_pos + 4..];
    let end = after_to
        .find(". ")
        .or_else(|| after_to.find(".\n"))
        .or_else(|| after_to.find("</USER_SETTINGS_CHANGE>"))
        .unwrap_or(after_to.len());
    let val = after_to[..end].trim().trim_end_matches('.');
    if val.is_empty() || val.eq_ignore_ascii_case("none") {
        None
    } else {
        Some(super::sanitize_terminal_text(val))
    }
}

fn is_in_progress_status(status: &str) -> bool {
    matches!(
        status.to_ascii_uppercase().as_str(),
        "RUNNING" | "IN_PROGRESS" | "PENDING" | "GENERATING"
    )
}

fn unquote_arg_value(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => {
            if s.starts_with('"') && s.ends_with('"') && s.len() >= 2 {
                serde_json::from_str::<String>(s)
                    .ok()
                    .or_else(|| Some(s.clone()))
            } else {
                Some(s.clone())
            }
        }
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn arg_str(args: Option<&Value>, keys: &[&str]) -> Option<String> {
    let obj = args?.as_object()?;
    for key in keys {
        if let Some(val) = obj.get(*key).and_then(unquote_arg_value) {
            if !val.trim().is_empty() {
                return Some(val);
            }
        }
    }
    None
}

/// Summarize an `agy` tool call into `(safe_arg_preview, optional_file_access)`.
/// Never exposes file contents (write/edit bodies are ignored).
fn summarize_tool_call(name: &str, args: Option<&Value>) -> (String, Option<(String, FileOp)>) {
    let clean = |s: &str, max: usize| -> String {
        let single_line: String = super::redact_secrets(&super::sanitize_terminal_text(s))
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        truncate_chars(&single_line, max)
    };
    let short_path = |p: &str| -> String {
        let cleaned = super::redact_secrets(&super::sanitize_terminal_text(p));
        let path = Path::new(&cleaned);
        // Show last 2 segments (e.g. `collector/agy.rs`) when path is deep.
        let comps: Vec<&str> = path
            .components()
            .filter_map(|c| c.as_os_str().to_str())
            .filter(|s| *s != "/" && *s != "\\")
            .collect();
        if comps.len() >= 2 {
            format!("{}/{}", comps[comps.len() - 2], comps[comps.len() - 1])
        } else {
            cleaned
        }
    };

    match name {
        "read_file" | "view_file" | "view_file_outline" | "view_code_item" => {
            if let Some(path) = arg_str(args, &["Path", "AbsolutePath", "File"]) {
                let display = short_path(&path);
                let audit_path = super::redact_secrets(&super::sanitize_terminal_text(&path));
                return (display, Some((audit_path, FileOp::Read)));
            }
        }
        "edit_file" | "replace_file_content" | "multi_replace_file_content" => {
            if let Some(path) = arg_str(args, &["Path", "TargetFile"]) {
                let display = short_path(&path);
                let audit_path = super::redact_secrets(&super::sanitize_terminal_text(&path));
                return (display, Some((audit_path, FileOp::Edit)));
            }
        }
        "write_file" | "write_to_file" => {
            if let Some(path) = arg_str(args, &["Path", "TargetFile"]) {
                let display = short_path(&path);
                let audit_path = super::redact_secrets(&super::sanitize_terminal_text(&path));
                return (display, Some((audit_path, FileOp::Write)));
            }
        }
        "run_command" => {
            if let Some(summary) = arg_str(args, &["toolSummary"]) {
                return (clean(&summary, 60), None);
            }
            if let Some(cmd) = arg_str(args, &["CommandLine", "command"]) {
                return (clean(&cmd, 60), None);
            }
        }
        "search_web" => {
            if let Some(q) = arg_str(args, &["query"]) {
                return (clean(&q, 60), None);
            }
        }
        "grep_search" | "find_by_name" | "list_dir" => {
            if let Some(val) =
                arg_str(args, &["Query", "Pattern", "SearchPath", "DirectoryPath"])
            {
                return (clean(&val, 60), None);
            }
        }
        "invoke_subagent" => {
            if let Some(role) = arg_str(args, &["Role", "TypeName"]) {
                return (clean(&role, 60), None);
            }
        }
        "run_workflow" => {
            if let Some(desc) = arg_str(args, &["Description", "ScriptPath"]) {
                return (clean(&desc, 60), None);
            }
        }
        "ask_question" => {
            return ("user question".to_string(), None);
        }
        _ => {
            // Safe generic fallback: pick the first non-content arg.
            if let Some(obj) = args.and_then(|v| v.as_object()) {
                for (k, v) in obj {
                    let kl = k.to_ascii_lowercase();
                    if kl.contains("content")
                        || kl.contains("replacement")
                        || kl.contains("target")
                        || kl.contains("script")
                        || kl.contains("prompt")
                    {
                        continue;
                    }
                    if let Some(s) = unquote_arg_value(v) {
                        if !s.trim().is_empty() {
                            return (clean(&s, 60), None);
                        }
                    }
                }
            }
        }
    }

    (String::new(), None)
}

fn parse_generic_timestamps(content: &str) -> (Option<u64>, Option<u64>) {
    let mut created = None;
    let mut completed = None;
    for line in content.lines().take(6) {
        if created.is_none() {
            if let Some(rest) = line.strip_prefix("Created At:") {
                created = parse_rfc3339_ms(rest.trim());
            }
        }
        if completed.is_none() {
            if let Some(rest) = line.strip_prefix("Completed At:") {
                completed = parse_rfc3339_ms(rest.trim());
            }
        }
        if created.is_some() && completed.is_some() {
            break;
        }
    }
    (created, completed)
}

fn parse_rfc3339_ms(s: &str) -> Option<u64> {
    chrono::DateTime::parse_from_rfc3339(s.trim())
        .ok()
        .and_then(|dt| u64::try_from(dt.timestamp_millis()).ok())
}

/// Read `agy` version from the header of `<data_dir>/cli.log` (reads at most 4 KiB).
fn read_agy_version_from_log(path: &Path) -> Option<String> {
    if !path.is_file() {
        return None;
    }
    let mut file = fs::File::open(path).ok()?;
    let mut buf = [0u8; 4096];
    let n = file.read(&mut buf).ok()?;
    let head = String::from_utf8_lossy(&buf[..n]);
    parse_agy_version_from_log(&head)
}

fn parse_agy_version_from_log(log_head: &str) -> Option<String> {
    for line in log_head.lines() {
        if let Some(pos) = line.find("Language server version:") {
            let ver = line[pos + "Language server version:".len()..].trim();
            let ver = ver.split_whitespace().next()?;
            if !ver.is_empty() {
                return Some(if ver.starts_with('v') {
                    ver.to_string()
                } else {
                    format!("v{ver}")
                });
            }
        }
    }
    None
}

fn push_chat_message(messages: &mut Vec<ChatMessage>, role: ChatRole, text: &str) {
    let single_line: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let truncated = truncate_chars(&single_line, 240);
    if truncated.is_empty() {
        return;
    }
    messages.push(ChatMessage {
        role,
        text: truncated,
    });
    if messages.len() > MAX_CHAT_MESSAGES {
        let excess = messages.len() - MAX_CHAT_MESSAGES;
        messages.drain(..excess);
    }
}

fn push_bounded<T>(vec: &mut Vec<T>, item: T, max: usize) {
    vec.push(item);
    if vec.len() > max {
        let excess = vec.len() - max;
        vec.drain(..excess);
    }
}

fn truncate_chars(s: &str, max_chars: usize) -> String {
    let mut out: String = s.chars().take(max_chars).collect();
    if s.chars().count() > max_chars && max_chars > 1 {
        out.pop();
        out.push('…');
    }
    out
}

fn current_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proc_info(pid: u32, ppid: u32, command: &str) -> ProcInfo {
        ProcInfo {
            pid,
            ppid,
            rss_kb: 102_400,
            cpu_pct: 0.0,
            command: command.to_string(),
        }
    }

    #[test]
    fn find_agy_pids_matches_binary_and_drops_wrappers_and_grep() {
        let mut procs = HashMap::new();
        procs.insert(
            100,
            proc_info(100, 1, "/home/u/.local/share/mise/shims/agy --dangerously-skip-permissions"),
        );
        procs.insert(
            101,
            proc_info(101, 100, "/home/u/.local/share/mise/installs/antigravity-cli/latest/agy --dangerously-skip-permissions"),
        );
        procs.insert(200, proc_info(200, 1, "grep agy"));
        procs.insert(300, proc_info(300, 1, "agy"));
        procs.insert(400, proc_info(400, 999, "agy --print test"));

        let pids = AgyCollector::find_agy_pids(&procs, 999);
        assert_eq!(pids, vec![101, 300]);
    }

    #[test]
    fn non_session_command_filters_utility_subcommands() {
        assert!(is_non_session_command("agy models"));
        assert!(is_non_session_command("/usr/local/bin/agy --version"));
        assert!(is_non_session_command("agy mcp list"));
        assert!(is_non_session_command("agy update"));
        assert!(!is_non_session_command("agy"));
        assert!(!is_non_session_command("agy --dangerously-skip-permissions"));
        assert!(!is_non_session_command("agy --model gemini-4-neon-xhigh"));
        assert!(!is_non_session_command("agy -p \"fix bug\""));
    }

    #[test]
    fn extract_conv_prefers_presence_lock_over_db_and_ignores_summaries() {
        let paths = vec![
            PathBuf::from("/home/u/.gemini/antigravity-cli/conversation_summaries.db"),
            PathBuf::from("/home/u/.gemini/antigravity-cli/conversations/db-conv-1.db"),
            PathBuf::from("/home/u/.gemini/antigravity-cli/conversations/db-conv-1.db-wal"),
            PathBuf::from("/home/u/.gemini/antigravity-cli/presence/lock-conv-2.lock"),
        ];
        let conv = extract_conv_from_fd_paths(&paths).unwrap();
        assert_eq!(conv.data_dir, PathBuf::from("/home/u/.gemini/antigravity-cli"));
        assert_eq!(conv.conv_id, "lock-conv-2");

        let db_only = vec![
            PathBuf::from("/home/u/.gemini/antigravity-cli/conversation_summaries.db"),
            PathBuf::from("/home/u/.gemini/antigravity-cli/conversations/db-conv-1.db"),
        ];
        let conv2 = extract_conv_from_fd_paths(&db_only).unwrap();
        assert_eq!(conv2.conv_id, "db-conv-1");
    }

    #[test]
    fn parse_transcript_tracks_tokens_context_tools_and_status_machine() {
        let mut state = TranscriptState::default();

        // 1. User sends prompt -> Thinking
        apply_transcript_line(
            &mut state,
            r#"{"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","created_at":"2026-11-20T23:57:17Z","content":"<USER_REQUEST>\nAdd agy support\n</USER_REQUEST>\n<ADDITIONAL_METADATA>\nlocal time\n</ADDITIONAL_METADATA>\n<USER_SETTINGS_CHANGE>\nThe user changed setting `Model Selection` from None to Gemini 4 Neon (XHigh). No need to comment.\n</USER_SETTINGS_CHANGE>"}"#,
        );
        assert_eq!(state.initial_prompt, "Add agy support");
        assert!(state.last_user_ts_ms > 0);
        assert_eq!(state.pending_tools, 0);
        assert_eq!(state.chat_messages.len(), 1);
        assert_eq!(state.chat_messages[0].role, ChatRole::User);

        // 2. Planner emits 2 tool calls -> Executing
        apply_transcript_line(
            &mut state,
            r#"{"step_index":1,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-11-20T23:57:18Z","model":"gemini-4-neon-xhigh","input_tokens":15000,"cache_read_tokens":0,"output_tokens":800,"tool_calls":[{"name":"read_file","args":{"Path":"\"/home/u/abtop/Cargo.toml\""}},{"name":"run_command","args":{"CommandLine":"\"cargo test\"","toolSummary":"\"Run tests\""}}]}"#,
        );
        assert_eq!(state.model, "gemini-4-neon-xhigh");
        assert_eq!(state.effort, "xhigh");
        assert_eq!(state.turn_count, 1);
        assert_eq!(state.total_input, 15_000);
        assert_eq!(state.total_output, 800);
        assert_eq!(state.last_context_tokens, 15_000);
        assert_eq!(state.pending_tools, 2);
        assert_eq!(state.last_user_ts_ms, 0);
        assert!(state.last_assistant_ts_ms > 0);
        assert_eq!(state.current_task, "run_command Run tests");
        assert_eq!(state.tool_calls.len(), 2);
        assert_eq!(state.tool_calls[0].arg, "abtop/Cargo.toml");
        assert_eq!(state.tool_calls[1].arg, "Run tests");
        assert_eq!(state.file_accesses.len(), 1);
        assert_eq!(state.file_accesses[0].path, "/home/u/abtop/Cargo.toml");
        assert_eq!(state.file_accesses[0].operation, FileOp::Read);

        // 3. First tool finishes -> still Executing (1 tool left)
        apply_transcript_line(
            &mut state,
            r#"{"step_index":2,"source":"MODEL","type":"GENERIC","status":"DONE","created_at":"2026-11-20T23:57:20Z","content":"Created At: 2026-11-20T18:57:18-05:00\nCompleted At: 2026-11-20T18:57:20-05:00\nok"}"#,
        );
        assert_eq!(state.pending_tools, 1);
        assert_eq!(state.tool_calls[0].duration_ms, 2000);
        assert_eq!(state.tool_calls[1].duration_ms, 0);
        assert_eq!(state.last_user_ts_ms, 0);

        // 4. Second tool finishes -> Thinking again
        apply_transcript_line(
            &mut state,
            r#"{"step_index":3,"source":"MODEL","type":"GENERIC","status":"DONE","created_at":"2026-11-20T23:57:23Z","content":"Created At: 2026-11-20T18:57:19-05:00\nCompleted At: 2026-11-20T18:57:23-05:00\nok"}"#,
        );
        assert_eq!(state.pending_tools, 0);
        assert_eq!(state.tool_calls[1].duration_ms, 4000);
        assert_eq!(state.last_assistant_ts_ms, 0);
        assert!(state.last_user_ts_ms > 0);

        // 5. System reminder does not disrupt Thinking
        apply_transcript_line(
            &mut state,
            r#"{"step_index":4,"source":"SYSTEM","type":"SYSTEM_MESSAGE","status":"DONE","created_at":"2026-11-20T23:57:23Z","content":"Reminder"}"#,
        );
        assert!(state.last_user_ts_ms > 0);

        // 6. Final planner response with no tool calls -> Waiting
        apply_transcript_line(
            &mut state,
            r#"{"step_index":5,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-11-20T23:57:30Z","model":"gemini-4-neon-xhigh","input_tokens":5000,"cache_read_tokens":15000,"output_tokens":400,"content":"All done!"}"#,
        );
        assert_eq!(state.turn_count, 2);
        assert_eq!(state.total_input, 20_000);
        assert_eq!(state.total_cache_read, 15_000);
        assert_eq!(state.total_output, 1_200);
        assert_eq!(state.last_context_tokens, 20_000);
        assert_eq!(state.pending_tools, 0);
        assert_eq!(state.last_user_ts_ms, 0);
        assert_eq!(state.last_assistant_ts_ms, 0);
        assert_eq!(state.first_assistant_text, "All done!");
        assert_eq!(state.chat_messages.len(), 2);
        assert_eq!(state.chat_messages[1].role, ChatRole::Assistant);
    }

    #[test]
    fn ask_question_marks_waiting_for_user() {
        let mut state = TranscriptState::default();
        apply_transcript_line(
            &mut state,
            r#"{"step_index":1,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-11-20T23:59:47Z","model":"gemini-4-neon-xhigh","input_tokens":4000,"cache_read_tokens":20000,"output_tokens":200,"tool_calls":[{"name":"ask_question","args":{"questions":"[...]"}}]}"#,
        );
        assert_eq!(state.pending_tools, 1);
        assert!(state.pending_ask_question);
    }

    #[test]
    fn incremental_reader_buffers_partial_lines_and_handles_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("transcript.jsonl");
        let line1 = "{\"step_index\":0,\"type\":\"USER_INPUT\",\"status\":\"DONE\",\"created_at\":\"2026-11-20T23:57:17Z\",\"content\":\"hello\"}\n";
        let partial = "{\"step_index\":1,\"type\":\"PLANNER_RESPONSE\",\"model\":\"gemini-4-neon-high\",\"input_tokens\":100";
        fs::write(&path, format!("{line1}{partial}")).unwrap();

        let mut state = TranscriptState::default();
        update_transcript_state(&mut state, &path);
        assert_eq!(state.initial_prompt, "hello");
        assert_eq!(state.turn_count, 0);
        assert_eq!(state.offset, line1.len() as u64);

        // Complete the second line.
        let rest = ",\"output_tokens\":20,\"content\":\"hi\"}\n";
        fs::write(&path, format!("{line1}{partial}{rest}")).unwrap();
        update_transcript_state(&mut state, &path);
        assert_eq!(state.turn_count, 1);
        assert_eq!(state.total_input, 100);
        assert_eq!(state.total_output, 20);

        // Truncate/reset file -> resets counters.
        let fresh = "{\"step_index\":0,\"type\":\"PLANNER_RESPONSE\",\"model\":\"gemini-4-neon-low\",\"input_tokens\":5,\"output_tokens\":2}\n";
        fs::write(&path, fresh).unwrap();
        update_transcript_state(&mut state, &path);
        assert_eq!(state.turn_count, 1);
        assert_eq!(state.total_input, 5);
        assert_eq!(state.model, "gemini-4-neon-low");
    }

    #[test]
    fn byom_uses_settings_hint_or_short_fallback() {
        let mut state = TranscriptState::default();
        apply_transcript_line(
            &mut state,
            r#"{"step_index":0,"type":"USER_INPUT","content":"<USER_REQUEST>\nhi\n</USER_REQUEST>\n<USER_SETTINGS_CHANGE>\nThe user changed setting `Model Selection` from None to Claude Opus 5.5 (proxy). No need.\n</USER_SETTINGS_CHANGE>"}"#,
        );
        apply_transcript_line(
            &mut state,
            r#"{"step_index":1,"type":"PLANNER_RESPONSE","model":"MODEL_GOOGLE_GEMINI_INTERNAL_BYOM","input_tokens":100,"output_tokens":10}"#,
        );
        assert_eq!(resolve_display_model(&state), "Claude Opus 5.5 (proxy)");

        let mut state2 = TranscriptState::default();
        apply_transcript_line(
            &mut state2,
            r#"{"step_index":1,"type":"PLANNER_RESPONSE","model":"MODEL_GOOGLE_GEMINI_INTERNAL_BYOM","input_tokens":100,"output_tokens":10}"#,
        );
        assert_eq!(resolve_display_model(&state2), "byom");
    }

    #[test]
    fn redacts_secrets_and_omits_write_file_content() {
        let args: Value = serde_json::from_str(
            r#"{"Path":"\"/tmp/secret.rs\"","Content":"\"sk-ant-supersecret123\""}"#,
        )
        .unwrap();
        let (preview, file_op) = summarize_tool_call("write_file", Some(&args));
        assert_eq!(preview, "tmp/secret.rs");
        assert!(!preview.contains("sk-ant"));
        assert_eq!(
            file_op,
            Some(("/tmp/secret.rs".to_string(), FileOp::Write))
        );

        let cmd_args: Value =
            serde_json::from_str(r#"{"CommandLine":"\"curl -H 'Authorization: Bearer tok123'\""}"#)
                .unwrap();
        let (cmd_preview, _) = summarize_tool_call("run_command", Some(&cmd_args));
        assert!(!cmd_preview.contains("tok123"));
        assert!(cmd_preview.contains("[REDACTED]"));
    }

    #[test]
    fn parses_agy_version_from_cli_log() {
        let log = "I1120 18:58:05.843103 30 server.go:2650] Starting\n\
                   I1120 18:58:05.844371 30 server.go:2709] Language server version: 1.5.13\n";
        assert_eq!(parse_agy_version_from_log(log).as_deref(), Some("v1.5.13"));
    }

    #[test]
    fn context_window_sizes_for_agy_models() {
        assert_eq!(context_window_for_agy_model("gemini-4-neon-xhigh", 0), 1_000_000);
        assert_eq!(context_window_for_agy_model("gemini-3.8-flash-high", 0), 1_000_000);
        assert_eq!(context_window_for_agy_model("Claude Opus 5.5 (proxy)", 0), 200_000);
        assert_eq!(context_window_for_agy_model("Claude Opus 5.5 (proxy)", 250_000), 1_000_000);
        assert_eq!(context_window_for_agy_model("GPT 6.2 Astra (proxy)", 0), 400_000);
    }
}
