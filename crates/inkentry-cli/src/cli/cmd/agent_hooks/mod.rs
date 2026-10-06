// `inkentry hooks agent <event>`: the coding agent's hook events, handled by
// the binary so the plugin's hook entries stay one-line commands. It reads one
// JSON object on stdin and writes at most one on stdout.
//
// A hook must never fail or add noise to the agent's action, so nothing here
// reports anything: every error, missing input, unknown project or absent
// store ends in silence and exit 0, on stderr as well as stdout. It never
// starts the inference server, embeds, or touches the network.

mod commit_detect;
mod marker;
mod pathing;
mod render;

use std::io::{IsTerminal as _, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use clap::ValueEnum;
use futures_util::FutureExt as _;
use inkentry_core::config::CallerDeclaration;

use super::events::{self, EventArgs};
use super::memory::{MemoryAnchorArgs, anchor};
use crate::config::{self, Config};
use crate::storage::MemoryStore;
use marker::SessionMarker;

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentEvent {
    /// Add stored context to a starting session
    SessionStart,
    /// Add what is recorded about a file before it is edited
    PreEdit,
    /// Anchor recorded entries to a commit the agent just made
    PostCommit,
    /// Ask once, before the session ends, for anything worth recording
    Stop,
}

const TOOL: &str = "claude-code";

// Room for about a dozen entries without taking a large share of the window on
// every start, clear and compaction.
const SESSION_START_BUDGET_TOKENS: usize = 2500;

const LOOKUP_LIMIT: usize = 20;

const MARKER_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

const MAX_INPUT_BYTES: u64 = 1 << 20;

// Reads the event from stdin, handles it, prints what it produced. Always
// exits 0.
pub(crate) async fn run(event: AgentEvent, config_path: Option<&Path>) {
    // A panic must reach neither stderr nor the exit code.
    std::panic::set_hook(Box::new(|_| {}));
    // Config warnings go to stderr, which a hook must leave empty.
    config::silence_warnings();

    let raw = read_stdin();
    let output = std::panic::AssertUnwindSafe(handle(event, &raw, config_path))
        .catch_unwind()
        .await
        .ok()
        .flatten();
    if let Some(json) = output {
        let mut stdout = std::io::stdout().lock();
        let _ = writeln!(stdout, "{json}");
        let _ = stdout.flush();
    }
    std::process::exit(0);
}

fn read_stdin() -> String {
    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        return String::new();
    }
    let mut raw = String::new();
    let _ = stdin.lock().take(MAX_INPUT_BYTES).read_to_string(&mut raw);
    raw
}

// The fields of the agent's hook input this command reads. Everything else,
// and anything of an unexpected type, is ignored.
struct HookInput {
    session_id: Option<String>,
    cwd: Option<PathBuf>,
    file_path: Option<String>,
    command: Option<String>,
    stop_hook_active: bool,
}

impl HookInput {
    fn parse(raw: &str) -> Option<Self> {
        let value: serde_json::Value = serde_json::from_str(raw).ok()?;
        let object = value.as_object()?;
        let text = |value: Option<&serde_json::Value>| {
            value
                .and_then(serde_json::Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        let tool_input = object.get("tool_input");
        Some(Self {
            session_id: text(object.get("session_id")).and_then(|id| sanitise_session_id(&id)),
            cwd: text(object.get("cwd")).map(PathBuf::from),
            file_path: text(tool_input.and_then(|t| t.get("file_path"))),
            command: text(tool_input.and_then(|t| t.get("command"))),
            stop_hook_active: object
                .get("stop_hook_active")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
        })
    }
}

// The session id ends up inside a shell `export`, so it is cut down to a safe
// alphabet. The hooks hash this form too, so they and the agent's own commands
// land in one session.
fn sanitise_session_id(id: &str) -> Option<String> {
    let clean: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    (!clean.is_empty()).then_some(clean)
}

struct Session {
    cfg: Config,
    mem_path: PathBuf,
    input: HookInput,
}

impl Session {
    fn marker(&self) -> Option<SessionMarker> {
        let session_ref = self.cfg.caller.session_ref.as_deref()?;
        Some(SessionMarker::open(&marker::sessions_dir()?, session_ref))
    }

    // Reads go to the local store only; a store held by a remote server needs
    // the network.
    fn has_local_store(&self) -> bool {
        self.mem_path.exists() && events::is_local_store(&self.cfg, None)
    }
}

async fn handle(event: AgentEvent, raw: &str, config_path: Option<&Path>) -> Option<String> {
    let input = HookInput::parse(raw)?;
    // The project is found from where the agent is, through the same
    // discovery every command uses, which walks up from the process cwd.
    if let Some(cwd) = &input.cwd {
        std::env::set_current_dir(cwd).ok()?;
    }
    let mut cfg = Config::load(config_path).ok()?;
    cfg.caller = CallerDeclaration::agent_hook(TOOL, input.session_id.as_deref());
    let project_db = config::require_project_db(&cfg.db_path, false).ok()?;
    let session = Session {
        cfg,
        mem_path: project_db.with_file_name("memory.db"),
        input,
    };

    match event {
        AgentEvent::SessionStart => session_start(&session).await,
        AgentEvent::PreEdit => pre_edit(&session),
        AgentEvent::PostCommit => post_commit(&session).await,
        AgentEvent::Stop => stop(&session),
    }
}

async fn session_start(session: &Session) -> Option<String> {
    if let Some(env_file) = std::env::var_os("CLAUDE_ENV_FILE").filter(|v| !v.is_empty()) {
        write_env_file(Path::new(&env_file), session.input.session_id.as_deref());
    }
    if let Some(dir) = marker::sessions_dir() {
        marker::prune(&dir, MARKER_MAX_AGE);
    }
    if !session.has_local_store() {
        return None;
    }
    let context = super::context::session_start_text(
        &session.cfg,
        &session.mem_path,
        SESSION_START_BUDGET_TOKENS,
    )
    .await?;
    Some(render::additional_context_json(
        "SessionStart",
        &render::session_start_message(&context),
    ))
}

// The agent's own deliberate commands run in a shell the hook cannot reach, so
// they pick their caller declaration up from a file the agent sources.
fn write_env_file(env_file: &Path, session_id: Option<&str>) {
    let mut lines = String::from(
        "export INKENTRY_TRIGGER=explicit\nexport INKENTRY_ACTOR=agent\nexport INKENTRY_TOOL=claude-code\n",
    );
    if let Some(id) = session_id {
        lines.push_str(&format!("export INKENTRY_SESSION_REF='{id}'\n"));
    }
    let write = || -> std::io::Result<()> {
        let needs_separator = std::fs::read(env_file)
            .map(|existing| !existing.is_empty() && !existing.ends_with(b"\n"))
            .unwrap_or(false);
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(env_file)?;
        if needs_separator {
            file.write_all(b"\n")?;
        }
        file.write_all(lines.as_bytes())
    };
    let _ = write();
}

fn pre_edit(session: &Session) -> Option<String> {
    if !session.mem_path.exists() {
        return None;
    }
    let cwd = std::env::current_dir().ok()?;
    let project_root = session.mem_path.parent()?.parent()?.to_path_buf();
    let worktree_root = gix::discover(&cwd)
        .ok()
        .and_then(|repo| repo.workdir().map(Path::to_path_buf));
    let mut roots: Vec<&Path> = vec![&project_root];
    roots.extend(worktree_root.as_deref());
    let path = pathing::repo_relative(&roots, &cwd, session.input.file_path.as_deref()?)?;

    if let Some(mut marker) = session.marker() {
        if marker.has_path(&path) {
            return None;
        }
        marker.record_path(&path);
    }
    if !session.has_local_store() {
        return None;
    }

    let started = Instant::now();
    let store = MemoryStore::open(&session.mem_path).ok()?;
    let notes = store
        .list_filtered_ext(None, None, None, Some(&path), LOOKUP_LIMIT, false, None)
        .ok()?;
    let returned_ids: Vec<String> = notes.iter().map(|n| n.entity_id.clone()).collect();
    events::record(EventArgs {
        cfg: &session.cfg,
        mem_path: &session.mem_path,
        backend_override: None,
        command: "memory.list",
        code_results: None,
        memory_results: Some(returned_ids.len() as i64),
        returned_ids: &returned_ids,
        tokens_out: None,
        started,
        ok: true,
    });
    if notes.is_empty() {
        return None;
    }
    Some(render::additional_context_json(
        "PreToolUse",
        &render::pre_edit_message(&path, &notes),
    ))
}

async fn post_commit(session: &Session) -> Option<String> {
    if !session.mem_path.exists()
        || !commit_detect::runs_git_commit(session.input.command.as_deref()?)
    {
        return None;
    }
    if let Some(mut marker) = session.marker() {
        marker.record_committed();
    }
    let args = MemoryAnchorArgs {
        commit: "HEAD".to_string(),
        ids: Vec::new(),
    };
    let _ = anchor::memory_anchor(args, &session.mem_path).await;
    None
}

fn stop(session: &Session) -> Option<String> {
    if session.input.stop_hook_active || !session.mem_path.exists() {
        return None;
    }
    let mut marker = session.marker()?;
    if marker.stopped() || !(marker.edited() || marker.committed()) {
        return None;
    }
    // Recorded before the prompt goes out, so the session is asked at most
    // once even if the record cannot be kept.
    if !marker.record_stopped() {
        return None;
    }
    Some(render::stop_block_json())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_input_reads_the_documented_fields_and_ignores_the_rest() {
        let input = HookInput::parse(
            r#"{"session_id":"abc-123","cwd":"/repo","hook_event_name":"PreToolUse",
                "tool_name":"Edit","tool_input":{"file_path":"src/a.rs","other":1},
                "stop_hook_active":true,"unknown":{"x":[1,2]}}"#,
        )
        .unwrap();
        assert_eq!(input.session_id.as_deref(), Some("abc-123"));
        assert_eq!(input.cwd, Some(PathBuf::from("/repo")));
        assert_eq!(input.file_path.as_deref(), Some("src/a.rs"));
        assert_eq!(input.command, None);
        assert!(input.stop_hook_active);
    }

    #[test]
    fn a_bash_tool_input_supplies_the_command() {
        let input = HookInput::parse(r#"{"tool_input":{"command":"git commit -m x"}}"#).unwrap();
        assert_eq!(input.command.as_deref(), Some("git commit -m x"));
    }

    #[test]
    fn fields_of_the_wrong_type_read_as_absent() {
        let input = HookInput::parse(
            r#"{"session_id":7,"cwd":false,"tool_input":"x","stop_hook_active":"yes"}"#,
        )
        .unwrap();
        assert_eq!(input.session_id, None);
        assert_eq!(input.cwd, None);
        assert_eq!(input.file_path, None);
        assert!(!input.stop_hook_active);
    }

    #[test]
    fn input_that_is_not_a_json_object_is_rejected() {
        for raw in ["", "   ", "garbage", "[]", "\"str\"", "42", "{\"a\":"] {
            assert!(HookInput::parse(raw).is_none(), "{raw:?}");
        }
        assert!(HookInput::parse("{}").is_some());
    }

    #[test]
    fn a_session_id_is_cut_down_to_a_shell_safe_alphabet() {
        assert_eq!(
            sanitise_session_id("0a1b-2c_3D").as_deref(),
            Some("0a1b-2c_3D")
        );
        assert_eq!(
            sanitise_session_id("x'; rm -rf / #").as_deref(),
            Some("x___rm_-rf____")
        );
        assert_eq!(sanitise_session_id(""), None);
    }

    #[test]
    fn the_env_file_declares_the_agents_own_commands_and_appends() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("env");
        std::fs::write(&file, "export KEEP=1").unwrap();
        write_env_file(&file, Some("abc-123"));
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "export KEEP=1\n\
             export INKENTRY_TRIGGER=explicit\n\
             export INKENTRY_ACTOR=agent\n\
             export INKENTRY_TOOL=claude-code\n\
             export INKENTRY_SESSION_REF='abc-123'\n"
        );
    }

    #[test]
    fn the_env_file_is_created_and_omits_a_session_ref_it_was_not_given() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("env");
        write_env_file(&file, None);
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.contains("INKENTRY_TRIGGER=explicit"));
        assert!(!text.contains("SESSION_REF"));
    }
}
