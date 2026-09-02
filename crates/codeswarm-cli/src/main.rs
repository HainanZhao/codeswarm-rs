use std::{
    collections::VecDeque,
    ffi::OsStr,
    io::{Write, stdout},
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::{Duration, Instant},
};

use codeswarm::transcript::{BlockKind, fixtures};
use codeswarm::tui::{
    App, ConfigAction, ConfigKey, FooterAction, Input, Key as TuiKey, LocalCommand,
    PermissionAction, PermissionKey, PromptAction, QueuedPrompt, StoreAction, StoreAgent, StoreKey,
    render,
};
use codeswarm_adapters::PermissionAnswer;
use codeswarm_adapters::agents::{AdapterKind, AgentDefinition, catalog_from_settings};
use codeswarm_adapters::history;
use codeswarm_adapters::launcher::{RosterSlot, parse_saved_slots, resolve_saved_slots};
use codeswarm_adapters::persistence::{SessionMetadata, SessionMetadataStore};
use codeswarm_adapters::relay::{CollaborationStrategy, RelayDecision};
use codeswarm_adapters::settings;
use codeswarm_adapters::{
    AcpAdapter, AdapterError, AdapterHost, AdapterResult, AgentAdapter, AgyAdapter, RelayHost,
    RelayPermissionAnswer, parse_command_line,
};
use codeswarm_adapters::{AgentEvent, BufferedEventLog, EventLog};
use crossterm::{
    cursor::Show,
    event::{
        self, DisableFocusChange, DisableMouseCapture, EnableFocusChange, EnableMouseCapture,
        Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
    },
    execute,
    terminal::{
        EnterAlternateScreen, LeaveAlternateScreen, SetTitle, disable_raw_mode, enable_raw_mode,
    },
};
use ratatui::{Terminal, backend::CrosstermBackend, layout::Rect};
use sha2::{Digest, Sha256};

fn terminal_capture_enabled_for(
    tmux: Option<&OsStr>,
    term: Option<&OsStr>,
    term_program: Option<&OsStr>,
) -> bool {
    if tmux.is_some() {
        return false;
    }
    let term = term.and_then(OsStr::to_str).unwrap_or_default();
    let term_program = term_program.and_then(OsStr::to_str).unwrap_or_default();
    !term.starts_with("tmux")
        && !term.starts_with("screen")
        && !term_program.eq_ignore_ascii_case("tmux")
}

fn terminal_capture_enabled() -> bool {
    terminal_capture_enabled_for(
        std::env::var_os("TMUX").as_deref(),
        std::env::var_os("TERM").as_deref(),
        std::env::var_os("TERM_PROGRAM").as_deref(),
    )
}

fn mouse_scroll_delta(kind: MouseEventKind) -> Option<isize> {
    match kind {
        MouseEventKind::ScrollUp => Some(-3),
        MouseEventKind::ScrollDown => Some(3),
        _ => None,
    }
}

fn apply_mouse_scroll(app: &mut App, kind: MouseEventKind, width: usize, height: usize) -> bool {
    let Some(delta) = mouse_scroll_delta(kind) else {
        return false;
    };
    app.scroll_by(delta, width, height);
    true
}

fn apply_navigation_scroll(app: &mut App, key: KeyCode, width: usize, height: usize) -> bool {
    let delta = match key {
        KeyCode::Up => -1,
        KeyCode::Down => 1,
        _ => return false,
    };
    app.scroll_by(delta, width, height);
    true
}

fn restore_mouse_after_selection_window(
    output: &mut impl Write,
    deadline: &mut Option<Instant>,
    now: Instant,
) -> std::io::Result<bool> {
    if !deadline.is_some_and(|deadline| now >= deadline) {
        return Ok(false);
    }
    execute!(output, EnableMouseCapture)?;
    *deadline = None;
    Ok(true)
}

#[derive(Debug)]
struct ConfigInputDecoder {
    escape_at: Option<Instant>,
    escape_window: Duration,
}

impl Default for ConfigInputDecoder {
    fn default() -> Self {
        Self::new(Duration::from_millis(150))
    }
}

impl ConfigInputDecoder {
    fn new(escape_window: Duration) -> Self {
        Self {
            escape_at: None,
            escape_window,
        }
    }

    fn decode(&mut self, key: KeyEvent, now: Instant) -> Option<ConfigKey> {
        if let Some(escape_at) = self.escape_at.take()
            && now.saturating_duration_since(escape_at) <= self.escape_window
        {
            return match key.code {
                KeyCode::Up => Some(ConfigKey::MoveUp),
                KeyCode::Down => Some(ConfigKey::MoveDown),
                _ => Some(ConfigKey::Cancel),
            };
        }
        match key.code {
            KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                Some(ConfigKey::Save)
            }
            KeyCode::Up
                if key.modifiers.intersects(
                    KeyModifiers::ALT | KeyModifiers::SHIFT | KeyModifiers::CONTROL,
                ) =>
            {
                Some(ConfigKey::MoveUp)
            }
            KeyCode::Down
                if key.modifiers.intersects(
                    KeyModifiers::ALT | KeyModifiers::SHIFT | KeyModifiers::CONTROL,
                ) =>
            {
                Some(ConfigKey::MoveDown)
            }
            KeyCode::Char('[') => Some(ConfigKey::MoveUp),
            KeyCode::Char(']') => Some(ConfigKey::MoveDown),
            KeyCode::Up => Some(ConfigKey::Up),
            KeyCode::Down => Some(ConfigKey::Down),
            KeyCode::Left => Some(ConfigKey::PreviousValue),
            KeyCode::Right => Some(ConfigKey::NextValue),
            KeyCode::Char(' ') => Some(ConfigKey::ToggleSlot),
            KeyCode::Enter => Some(ConfigKey::Confirm),
            KeyCode::Esc => {
                self.escape_at = Some(now);
                None
            }
            _ => None,
        }
    }

    fn take_expired_escape(&mut self, now: Instant) -> bool {
        if self
            .escape_at
            .is_some_and(|escape_at| now.saturating_duration_since(escape_at) > self.escape_window)
        {
            self.escape_at = None;
            true
        } else {
            false
        }
    }

    fn reset(&mut self) {
        self.escape_at = None;
    }
}

/// Own terminal modes from setup through teardown. Drop is the last line of
/// defense for early-return and panic paths, which is especially important
/// when CodeSwarm runs inside a long-lived tmux pane.
struct TerminalSession {
    capture_enabled: bool,
    active: bool,
}

impl TerminalSession {
    fn enter() -> std::io::Result<Self> {
        enable_raw_mode()?;
        let mut session = Self {
            // tmux owns focus/title routing. Mouse reporting remains enabled
            // for wheel and footer taps; terminals retain Shift/long-press
            // as their native text-selection gesture.
            capture_enabled: terminal_capture_enabled(),
            active: true,
        };
        let mut output = stdout();
        if let Err(error) = execute!(output, EnableMouseCapture) {
            let _ = session.restore();
            return Err(error);
        }
        if session.capture_enabled
            && let Err(error) = execute!(output, EnableFocusChange)
        {
            let _ = session.restore();
            return Err(error);
        }
        // Always use the complete terminal. Even a partial write is paired
        // with a best-effort leave by the armed session guard.
        if let Err(error) = execute!(output, EnterAlternateScreen) {
            let _ = session.restore();
            return Err(error);
        }
        Ok(session)
    }

    fn restore(&mut self) -> std::io::Result<()> {
        if !self.active {
            return Ok(());
        }
        let mut output = stdout();
        let terminal_result = if self.capture_enabled {
            execute!(output, SetTitle("CodeSwarm"), Show)
        } else {
            execute!(output, Show)
        };
        let capture_result = if self.capture_enabled {
            execute!(output, DisableFocusChange)
        } else {
            Ok(())
        };
        let mouse_result = execute!(output, DisableMouseCapture);
        let screen_result = execute!(output, LeaveAlternateScreen);
        let raw_result = disable_raw_mode();
        let result = terminal_result
            .and(capture_result)
            .and(mouse_result)
            .and(screen_result)
            .and(raw_result);
        // A failed write may be transient. Keep the guard armed so Drop gets
        // one final best-effort restoration attempt during scope teardown.
        if result.is_ok() {
            self.active = false;
        }
        result
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

#[derive(Debug)]
enum AdapterControl {
    Prompt(String),
    Queue {
        slot: usize,
        prompt: String,
    },
    Direct {
        slot: usize,
        prompt: String,
    },
    Add {
        spec: AgentSpec,
        identity: String,
    },
    Permission {
        slot: usize,
        request_id: String,
        answer: PermissionAnswer,
    },
    SetStrategy(CollaborationStrategy),
    SetMode(String),
    SetModel {
        slot: usize,
        model: String,
    },
    Reload(usize),
    Drop(usize),
    Swap(usize, usize),
    Cancel,
    Stop,
}

fn control_for_queued(prompt: &QueuedPrompt) -> Option<AdapterControl> {
    if prompt.direct {
        return Some(AdapterControl::Direct {
            slot: prompt.target?,
            prompt: prompt.prompt.clone(),
        });
    }
    Some(match prompt.target {
        Some(slot) => AdapterControl::Queue {
            slot,
            prompt: prompt.prompt.clone(),
        },
        None => AdapterControl::Prompt(prompt.prompt.clone()),
    })
}

fn dispatch_queued_prompt(
    controls: Option<&tokio::sync::mpsc::UnboundedSender<AdapterControl>>,
    prompt: &QueuedPrompt,
) -> bool {
    let Some(control) = control_for_queued(prompt) else {
        return false;
    };
    controls.is_some_and(|controls| controls.send(control).is_ok())
}

fn dispatch_permission_action(
    controls: Option<&tokio::sync::mpsc::UnboundedSender<AdapterControl>>,
    action: PermissionAction,
) -> bool {
    let command = match action {
        PermissionAction::Answer {
            slot,
            request_id,
            option_id,
            ..
        } => AdapterControl::Permission {
            slot,
            request_id,
            answer: PermissionAnswer::Selected { option_id },
        },
        PermissionAction::Cancel { slot, request_id } => AdapterControl::Permission {
            slot,
            request_id,
            answer: PermissionAnswer::Cancelled,
        },
        PermissionAction::Ignored | PermissionAction::SelectionChanged { .. } => return false,
    };
    controls.is_some_and(|controls| controls.send(command).is_ok())
}

fn collaboration_strategy(label: &str) -> CollaborationStrategy {
    match label {
        "Manual routing" => CollaborationStrategy::Manual,
        "Pair review" => CollaborationStrategy::Pair,
        _ => CollaborationStrategy::Roster,
    }
}

fn canonical_mode_policy(policy: &str) -> String {
    match policy {
        "plan" => "codeswarm:mode:plan",
        "default" | "manual" => "codeswarm:mode:manual",
        "accept-edits" => "codeswarm:mode:accept-edits",
        "full-access" | "auto" | "autopilot" => "codeswarm:mode:full-access",
        other => other,
    }
    .to_owned()
}

fn normalize_selected_slot(app: &App, selected: Option<usize>) -> Option<usize> {
    let active = app.active_roster_slots();
    match selected {
        Some(slot) if active.contains(&slot) => Some(slot),
        Some(_) => active.first().copied(),
        None => None,
    }
}

fn consume_one_shot_route(app: &App, selected: &mut Option<usize>) {
    if app.collaboration() != "Manual routing" {
        *selected = None;
    }
}

fn interaction_height(frame_area: Rect) -> usize {
    usize::from(frame_area.height)
}

enum Launch {
    Preview,
    Store,
    Agy {
        prompt: Option<String>,
    },
    Acp {
        program: String,
        prompt: Option<String>,
    },
    Roster {
        specs: Vec<AgentSpec>,
        identities: Vec<String>,
        models: Vec<Option<String>>,
        session_ids: Vec<Option<String>>,
        prompt: Option<String>,
        first_slot: usize,
        max_rounds: usize,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum AgentSpec {
    Agy(String),
    Acp(String),
}

fn agent_spec_command(spec: &AgentSpec) -> &str {
    match spec {
        AgentSpec::Agy(command) | AgentSpec::Acp(command) => command,
    }
}

fn agent_spec_identity(spec: &AgentSpec) -> String {
    catalog_identity_for_command(agent_spec_command(spec))
}

fn main() -> std::io::Result<()> {
    let raw_arguments = std::env::args().skip(1).collect::<Vec<_>>();
    let resume_requested = raw_arguments.first().is_some_and(|value| value == "resume");
    let arguments = if resume_requested {
        match raw_arguments.as_slice() {
            [_] => Vec::new(),
            [_, path] => vec!["--project-dir".into(), path.clone()],
            _ => {
                eprintln!("Usage: codeswarm resume [PATH]");
                return Ok(());
            }
        }
    } else {
        prepare_launch_arguments(raw_arguments)
    };
    if arguments
        .iter()
        .any(|argument| argument == "-h" || argument == "--help")
    {
        print_help();
        return Ok(());
    }
    if arguments
        .iter()
        .any(|argument| argument == "-v" || argument == "--version")
    {
        println!("{}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if let Some(path) = project_dir_argument(&arguments) {
        validate_project_directory(&path)?;
        std::env::set_current_dir(path)?;
    } else if arguments.len() == 1
        && !arguments[0].starts_with('-')
        && PathBuf::from(&arguments[0]).is_dir()
    {
        let path = PathBuf::from(&arguments[0]);
        validate_project_directory(&path)?;
        std::env::set_current_dir(path)?;
    }
    let launch = if resume_requested {
        Some(resume_launch()?)
    } else {
        parse_launch(&arguments).or_else(|| {
            (arguments.is_empty()
                || (arguments.len() == 2 && arguments.first()? == "--project-dir")
                || (arguments.len() == 1
                    && !arguments[0].starts_with('-')
                    && PathBuf::from(&arguments[0]).is_dir()))
            .then(bare_launch)
        })
    };
    let Some(launch) = launch else {
        eprintln!("Unknown or incomplete command. Run `codeswarm --help`.");
        return Ok(());
    };

    // Ask supported terminals to report focus changes. Multiplexers retain
    // their own capture policy; the session guard restores raw/fullscreen
    // modes on normal, error, and unwind paths.
    let mut terminal_session = TerminalSession::enter()?;
    let output = stdout();
    let backend = CrosstermBackend::new(output);
    let mut terminal = Terminal::new(backend)?;
    let result = match launch {
        Launch::Preview => run_preview(&mut terminal),
        Launch::Store => run_store(&mut terminal),
        Launch::Agy { prompt } => run_agy(&mut terminal, prompt, resume_requested),
        Launch::Acp { program, prompt } => {
            run_acp(&mut terminal, program, prompt, resume_requested)
        }
        Launch::Roster {
            specs,
            identities,
            models,
            session_ids,
            prompt,
            first_slot,
            max_rounds,
        } => run_roster(
            &mut terminal,
            specs,
            identities,
            models,
            session_ids,
            prompt,
            first_slot,
            max_rounds,
        ),
    };
    let restore_result = terminal_session.restore();
    result.and(restore_result)
}

/// Keep the compact flag-based interface while accepting the two documented
/// Python-era entry-point spellings (`run` and `acp COMMAND`).
fn normalize_arguments(mut arguments: Vec<String>) -> Vec<String> {
    match arguments.first().map(String::as_str) {
        Some("run") => {
            arguments.remove(0);
            arguments
        }
        Some("acp") => {
            arguments.remove(0);
            let Some(command) = arguments.first().cloned() else {
                return vec!["--acp".into()];
            };
            arguments.remove(0);
            let mut normalized = vec!["--acp".into(), command];
            // The legacy ACP subcommand's optional positional argument was a
            // workspace path, not a prompt. Preserve that distinction.
            if arguments
                .first()
                .is_some_and(|argument| !argument.starts_with('-'))
            {
                normalized.push("--project-dir".into());
                normalized.push(arguments.remove(0));
            }
            normalized.extend(arguments);
            normalized
        }
        _ => {
            normalize_default_project_path(&mut arguments);
            arguments
        }
    }
}

fn prepare_launch_arguments(arguments: Vec<String>) -> Vec<String> {
    let explicit_run = arguments.first().is_some_and(|argument| argument == "run");
    let mut arguments = normalize_arguments(arguments);
    if explicit_run
        && arguments
            .first()
            .is_some_and(|argument| !argument.starts_with('-') && looks_like_project_path(argument))
    {
        arguments.insert(0, "--project-dir".into());
    }
    arguments
}

fn normalize_default_project_path(arguments: &mut Vec<String>) {
    if arguments
        .first()
        .is_some_and(|argument| !argument.starts_with('-') && PathBuf::from(argument).is_dir())
    {
        arguments.insert(0, "--project-dir".into());
    }
}

fn looks_like_project_path(argument: &str) -> bool {
    let path = PathBuf::from(argument);
    path.is_dir()
        || argument.starts_with('/')
        || argument.starts_with("./")
        || argument.starts_with("../")
        || argument == "."
        || argument == ".."
}

fn print_help() {
    println!(
        r#"CodeSwarm — fast full-screen terminal workspace

Usage:
  codeswarm [OPTIONS] [PROMPT]
  codeswarm resume [PATH]
  codeswarm run [PATH] [OPTIONS] [PROMPT]
  codeswarm acp COMMAND [PATH]

Options:
  -a, --agent NAME                Select a catalog agent (repeatable)
  --demo                          Run the local UI preview
  --agy PROMPT                    Run the native Agy adapter
  --acp PROGRAM [PROMPT]          Run an ACP adapter
  --roster KIND:COMMAND           Add a native/ACP roster member (repeatable)
  --first N                       Select the first roster slot (zero-based)
  --first-agent N                 Select the first named agent (one-based)
  --max-rounds N                  Limit automated relay turns
  --project-dir PATH              Use PATH as the workspace
  -h, --help                      Show this help
  -v, --version                   Show the version

Prompt commands include /help, /config, /cancel, /reload, /to, /select,
/export, /clear, and /close."#
    );
}

fn project_dir_argument(arguments: &[String]) -> Option<PathBuf> {
    let index = arguments
        .iter()
        .position(|argument| argument == "--project-dir")?;
    arguments.get(index + 1).map(PathBuf::from)
}

fn validate_project_directory(path: &Path) -> std::io::Result<()> {
    if path.is_dir() {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("Not a directory: {}", path.display()),
        ))
    }
}

fn parse_launch(arguments: &[String]) -> Option<Launch> {
    if let Some(index) = arguments
        .iter()
        .position(|argument| argument == "--project-dir")
    {
        arguments.get(index + 1)?;
        let mut filtered = arguments.to_vec();
        filtered.drain(index..=index + 1);
        return parse_launch(&filtered);
    }
    if arguments.iter().any(|argument| argument == "--demo") {
        return Some(Launch::Preview);
    }
    if arguments
        .iter()
        .any(|argument| argument == "-a" || argument == "--agent")
    {
        return parse_named_agent_launch(arguments);
    }
    if let Some(index) = arguments.iter().position(|argument| argument == "--agy") {
        let prompt = arguments
            .get(index + 1)
            .filter(|prompt| !prompt.starts_with('-'))
            .cloned();
        return Some(Launch::Agy { prompt });
    }
    if arguments.iter().any(|argument| argument == "--roster") {
        return parse_roster_launch(arguments);
    }
    let index = arguments.iter().position(|argument| argument == "--acp")?;
    let program = arguments.get(index + 1)?.clone();
    let prompt = arguments
        .get(index + 2)
        .filter(|prompt| !prompt.starts_with('-'))
        .cloned();
    Some(Launch::Acp { program, prompt })
}

fn parse_named_agent_launch(arguments: &[String]) -> Option<Launch> {
    let settings = settings_path()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .unwrap_or_default();
    let catalog = catalog_from_settings(&settings);
    let mut specs = Vec::new();
    let mut identities = Vec::new();
    let mut first_slot = 0;
    let mut max_rounds = 100;
    let mut prompt = None;
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "-a" | "--agent" => {
                let name = arguments.get(index + 1)?.to_ascii_lowercase();
                let agent = catalog.iter().find(|agent| {
                    agent.active
                        && (agent.identity.eq_ignore_ascii_case(&name)
                            || agent.short_name.eq_ignore_ascii_case(&name)
                            || agent
                                .aliases
                                .iter()
                                .any(|alias| alias.eq_ignore_ascii_case(&name)))
                })?;
                identities.push(agent.identity.clone());
                specs.push(agent_spec(agent));
                index += 2;
            }
            "--first-agent" => {
                first_slot = arguments
                    .get(index + 1)?
                    .parse::<usize>()
                    .ok()?
                    .checked_sub(1)?;
                index += 2;
            }
            "--max-rounds" => {
                max_rounds = arguments.get(index + 1)?.parse().ok()?;
                if max_rounds == 0 {
                    return None;
                }
                index += 2;
            }
            "--project-dir" => index += 2,
            value if !value.starts_with('-') => {
                if prompt.is_some() {
                    return None;
                }
                prompt = Some(value.to_owned());
                index += 1;
            }
            _ => return None,
        }
    }
    if specs.is_empty() || first_slot >= specs.len() {
        return None;
    }
    Some(Launch::Roster {
        specs,
        identities,
        models: Vec::new(),
        session_ids: Vec::new(),
        prompt,
        first_slot,
        max_rounds,
    })
}

fn bare_launch() -> Launch {
    let settings = settings_path()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .unwrap_or_default();
    bare_launch_from_settings(&settings)
}

fn resume_launch() -> std::io::Result<Launch> {
    let cwd = std::env::current_dir()?;
    let current_path = session_metadata_path_for(&cwd);
    let loaded = load_session_metadata_candidates(session_metadata_candidates(&cwd))?;
    let (metadata, source_path) = loaded.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "No resumable CodeSwarm session for this project",
        )
    })?;
    let settings = settings_path()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .unwrap_or_default();
    let launch =
        resume_launch_from_metadata(&metadata, &cwd, &settings).map_err(std::io::Error::other)?;
    if source_path != current_path {
        SessionMetadataStore::open(current_path)
            .write(&metadata)
            .map_err(|error| std::io::Error::other(error.to_string()))?;
    }
    Ok(launch)
}

fn load_session_metadata_candidates(
    paths: impl IntoIterator<Item = PathBuf>,
) -> std::io::Result<Option<(SessionMetadata, PathBuf)>> {
    for path in paths {
        match SessionMetadataStore::open(&path).read() {
            Ok(Some(metadata)) => return Ok(Some((metadata.metadata, path))),
            Ok(None) => {}
            Err(error) => return Err(std::io::Error::other(error.to_string())),
        }
    }
    Ok(None)
}

fn resume_launch_from_metadata(
    metadata: &SessionMetadata,
    cwd: &Path,
    settings: &str,
) -> Result<Launch, String> {
    let stored_cwd = metadata
        .get("cwd")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "Saved session has no workspace".to_owned())?;
    let stored_cwd = Path::new(stored_cwd)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(stored_cwd));
    let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    if stored_cwd != cwd {
        return Err("No resumable CodeSwarm session for this project".into());
    }
    let agents = metadata
        .get("agents")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "Saved session has no agents".to_owned())?;
    if agents.is_empty() {
        return Err("Saved session has no active agents".into());
    }
    let catalog = catalog_from_settings(settings);
    let saved_identities = agents
        .iter()
        .map(|agent| {
            agent
                .get("identity")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| "Saved agent identity is invalid".to_owned())
        })
        .collect::<Result<Vec<_>, _>>()?;
    let specs = agents
        .iter()
        .map(|saved| {
            let identity = saved
                .get("identity")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| "Saved agent identity is invalid".to_owned())?;
            if let Some(spec) = catalog
                .iter()
                .find(|agent| agent.active && agent.identity.eq_ignore_ascii_case(identity))
                .map(agent_spec)
            {
                return Ok(spec);
            }
            let protocol = saved
                .get("protocol")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| "Saved agent protocol is invalid".to_owned())?;
            let command = saved
                .get("command")
                .and_then(serde_json::Value::as_str)
                .filter(|command| !command.trim().is_empty())
                .ok_or_else(|| "Saved agent command is invalid".to_owned())?
                .to_owned();
            match protocol.to_ascii_lowercase().as_str() {
                "native" | "agy" => Ok(AgentSpec::Agy(command)),
                "acp" => Ok(AgentSpec::Acp(command)),
                _ => Err(format!("Saved agent protocol is unsupported: {protocol}")),
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    let session_ids = agents
        .iter()
        .map(|agent| {
            let supports_load = agent
                .get("supports_load_session")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            supports_load
                .then(|| agent.get("session_id")?.as_str().map(str::to_owned))
                .flatten()
        })
        .collect::<Vec<_>>();
    if session_ids.iter().all(Option::is_none) {
        return Err("The previous agents did not provide a resumable session".into());
    }
    Ok(Launch::Roster {
        specs,
        identities: saved_identities,
        models: Vec::new(),
        session_ids,
        prompt: None,
        first_slot: 0,
        max_rounds: 100,
    })
}

fn bare_launch_from_settings(settings: &str) -> Launch {
    let catalog = catalog_from_settings(settings);
    let identities = catalog
        .iter()
        .filter(|agent| agent.active)
        .map(|agent| agent.identity.clone())
        .collect::<Vec<_>>();
    let slots = resolve_saved_slots(settings, &identities);
    if slots.is_empty() {
        Launch::Store
    } else {
        let specs = slots
            .iter()
            .filter_map(|slot| {
                catalog
                    .iter()
                    .find(|candidate| {
                        candidate.active && candidate.identity.eq_ignore_ascii_case(&slot.agent)
                    })
                    .map(agent_spec)
            })
            .collect::<Vec<_>>();
        if specs.is_empty() {
            Launch::Store
        } else {
            Launch::Roster {
                specs,
                identities: slots.iter().map(|slot| slot.agent.clone()).collect(),
                models: slots.iter().map(|slot| slot.model.clone()).collect(),
                session_ids: Vec::new(),
                prompt: None,
                first_slot: 0,
                max_rounds: 100,
            }
        }
    }
}

fn settings_path() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .map(|root| root.join("codeswarm").join("codeswarm.json"))
}

fn agent_spec(agent: &AgentDefinition) -> AgentSpec {
    let command = agent
        .full_access_startup_argument
        .as_deref()
        .filter(|argument| !argument.trim().is_empty())
        .map_or_else(
            || agent.command.clone(),
            |argument| append_command_argument(&agent.command, argument),
        );
    match agent.adapter {
        AdapterKind::Native => AgentSpec::Agy(command),
        AdapterKind::Acp => AgentSpec::Acp(command),
    }
}

/// Append one catalog argument without changing the existing command's
/// shell-free tokenization. Catalog values are parsed by `parse_command_line`,
/// so quote only the argument being added and leave the original command
/// spelling intact for display and identity resolution.
fn append_command_argument(command: &str, argument: &str) -> String {
    let quoted = if argument
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || "-_.=/:@".contains(character))
    {
        argument.to_owned()
    } else {
        format!("'{}'", argument.replace('\'', "'\\''"))
    };
    format!("{command} {quoted}")
}

fn parse_roster_launch(arguments: &[String]) -> Option<Launch> {
    let mut specs = Vec::new();
    let mut prompt = None;
    let mut first_slot = 0;
    let mut max_rounds = 100;
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--roster" => {
                let value = arguments.get(index + 1)?;
                specs.push(parse_agent_spec(value)?);
                index += 2;
            }
            "--first" => {
                first_slot = arguments.get(index + 1)?.parse().ok()?;
                index += 2;
            }
            "--max-rounds" => {
                max_rounds = arguments.get(index + 1)?.parse().ok()?;
                if max_rounds == 0 {
                    return None;
                }
                index += 2;
            }
            "--project-dir" => index += 2,
            "--demo" => index += 1,
            value if !value.starts_with('-') => {
                if prompt.is_some() {
                    return None;
                }
                prompt = Some(value.to_owned());
                index += 1;
            }
            _ => return None,
        }
    }
    if specs.is_empty() || first_slot >= specs.len() {
        return None;
    }
    let identities = specs.iter().map(agent_spec_identity).collect();
    Some(Launch::Roster {
        specs,
        identities,
        models: Vec::new(),
        session_ids: Vec::new(),
        prompt: Some(prompt?),
        first_slot,
        max_rounds,
    })
}

fn parse_agent_spec(value: &str) -> Option<AgentSpec> {
    let (kind, command) = value.split_once(':')?;
    if command.is_empty() {
        return None;
    }
    match kind.to_ascii_lowercase().as_str() {
        "agy" | "native" => Some(AgentSpec::Agy(command.to_owned())),
        "acp" => Some(AgentSpec::Acp(command.to_owned())),
        _ => None,
    }
}

fn display_agent_name(command: &str) -> String {
    let Ok((program, arguments)) = parse_command_line(command) else {
        return "Agent".into();
    };
    let executable = Path::new(&program)
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or(program.as_str())
        .to_ascii_lowercase();
    let arguments = arguments
        .iter()
        .map(|argument| argument.to_ascii_lowercase())
        .collect::<Vec<_>>();
    if executable == "claude"
        || arguments
            .iter()
            .any(|argument| argument.contains("claude-agent-acp"))
    {
        "Claude".into()
    } else if executable == "codex"
        || executable == "codex-acp"
        || arguments
            .iter()
            .any(|argument| argument.contains("codex-acp"))
    {
        "Codex".into()
    } else if executable == "qwen" {
        "Qwen".into()
    } else if executable == "gemini" {
        "Gemini".into()
    } else if executable == "agy" || executable == "antigravity" {
        "Antigravity".into()
    } else {
        executable
    }
}

/// Resolve the catalog identity for a direct command invocation.  Relay
/// launches already have catalog definitions available, but `--agy` and
/// `--acp` intentionally accept arbitrary custom commands and therefore need
/// a small best-effort lookup of their own.
fn catalog_identity_for_command(command: &str) -> String {
    let settings = settings_path()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .unwrap_or_default();
    let normalized = command.trim();
    let executable = command_executable(normalized);
    catalog_from_settings(&settings)
        .into_iter()
        .find(|agent| {
            agent.command.trim().eq_ignore_ascii_case(normalized)
                || agent.detect_command.as_deref().is_some_and(|detect| {
                    detect.trim().eq_ignore_ascii_case(normalized)
                        || executable == command_executable(detect)
                })
                || agent.name.eq_ignore_ascii_case(normalized)
                || agent.short_name.eq_ignore_ascii_case(normalized)
                || agent
                    .aliases
                    .iter()
                    .any(|alias| alias.eq_ignore_ascii_case(normalized))
        })
        .map_or_else(|| normalized.to_owned(), |agent| agent.identity)
}

fn command_executable(command: &str) -> Option<String> {
    parse_command_line(command)
        .ok()
        .map(|(program, _)| program.to_ascii_lowercase())
}

/// Build the coordinator snapshot used by direct (non-relay) launches.
///
/// `RelayHost` owns the equivalent method for a multi-agent session. Keeping
/// this helper at the CLI boundary means custom adapters remain supported by
/// the same `AgentAdapter` contract without forcing them to implement ACP or
/// a second persistence API.
fn standalone_session_metadata(
    cwd: &Path,
    name: &str,
    identity: &str,
    command: &str,
    adapter: &dyn AgentAdapter,
) -> SessionMetadata {
    let mut data = serde_json::Map::new();
    data.insert(
        "cwd".into(),
        serde_json::Value::String(cwd.display().to_string()),
    );
    data.insert(
        "title".into(),
        serde_json::Value::String("CodeSwarm".into()),
    );
    let mut agent = serde_json::json!({
        "name": name,
        "identity": identity,
        "protocol": adapter.protocol(),
        "command": command,
        "supports_load_session": adapter.capabilities().supports_session_load,
    });
    if let Some(session_id) = adapter.session_id() {
        agent["session_id"] = serde_json::Value::String(session_id);
    }
    data.insert("agents".into(), serde_json::json!([agent]));
    SessionMetadata::new(data)
}

fn queue_standalone_metadata(
    writer: Option<&codeswarm_adapters::persistence::BufferedSessionMetadataStore>,
    cwd: &Path,
    name: &str,
    identity: &str,
    command: &str,
    adapter: &dyn AgentAdapter,
) {
    if let Some(writer) = writer {
        let _ = writer.write(standalone_session_metadata(
            cwd, name, identity, command, adapter,
        ));
    }
}

fn run_store(terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>) -> std::io::Result<()> {
    let mut app = App::default();
    let settings = settings_path()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .unwrap_or_default();
    let catalog = catalog_from_settings(&settings);
    let saved_slots = parse_saved_slots(&settings);
    let saved_roster = saved_slots
        .iter()
        .map(|slot| slot.agent.clone())
        .collect::<Vec<_>>();
    let has_saved_roster = !saved_slots.is_empty();
    let mut launchable_catalog = codeswarm_adapters::agents::active_catalog(catalog);
    launchable_catalog.sort_by_key(|agent| {
        saved_roster
            .iter()
            .position(|saved| saved.eq_ignore_ascii_case(&agent.identity))
            .unwrap_or(usize::MAX)
    });
    let default_identities = launchable_catalog
        .iter()
        .filter(|agent| {
            matches!(
                agent.short_name.as_str(),
                "claude" | "codex" | "gemini" | "antigravity"
            ) && command_available(
                agent
                    .detect_command
                    .as_deref()
                    .unwrap_or(agent.command.as_str()),
            )
        })
        .map(|agent| agent.identity.clone())
        .collect::<Vec<_>>();
    let templates = launchable_catalog
        .iter()
        .map(|agent| {
            // Availability follows the catalog's detection command, not the
            // adapter launch command.  ACP bridges commonly launch through
            // `npx`; treating `npx` as proof that Claude/Codex is installed
            // made the store advertise agents that could not actually run.
            let available = command_available(
                agent
                    .detect_command
                    .as_deref()
                    .unwrap_or(agent.command.as_str()),
            );
            StoreAgent {
                identity: agent.identity.clone(),
                name: agent.name.clone(),
                adapter: match agent.adapter {
                    AdapterKind::Native => "native".into(),
                    AdapterKind::Acp => "ACP".into(),
                },
                command: agent.command.clone(),
                available,
                selected: false,
                model: None,
            }
        })
        .collect::<Vec<_>>();
    let source_slots = if has_saved_roster {
        saved_slots.clone()
    } else {
        default_identities
            .into_iter()
            .map(|agent| RosterSlot { agent, model: None })
            .collect()
    };
    let mut agents = source_slots
        .iter()
        .filter_map(|slot| {
            templates
                .iter()
                .find(|agent| agent.identity.eq_ignore_ascii_case(&slot.agent))
                .cloned()
                .map(|mut agent| {
                    agent.selected = true;
                    agent.model = slot.model.clone();
                    agent
                })
        })
        .collect::<Vec<_>>();
    agents.extend(templates);
    app.show_store(agents);
    app.set_store_directory(
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .display()
            .to_string(),
    );
    loop {
        terminal.draw(|frame| render(frame, &mut app))?;
        if !event::poll(Duration::from_millis(50))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        if app.store_editing_directory() {
            if key.code == KeyCode::Esc {
                app.cancel_store_directory_edit();
            } else if let StoreAction::Directory(directory) =
                app.handle_store_directory_input(Input::from(key))
            {
                match PathBuf::from(&directory).canonicalize() {
                    Ok(path) if path.is_dir() => match std::env::set_current_dir(&path) {
                        Ok(()) => {
                            app.set_store_directory(path.display().to_string());
                            app.set_store_status(format!("Workspace: {}", path.display()));
                        }
                        Err(error) => {
                            app.set_store_directory(
                                std::env::current_dir()
                                    .unwrap_or_else(|_| PathBuf::from("."))
                                    .display()
                                    .to_string(),
                            );
                            app.set_store_status(format!("Directory failed: {error}"));
                        }
                    },
                    Ok(path) => {
                        app.set_store_directory(
                            std::env::current_dir()
                                .unwrap_or_else(|_| PathBuf::from("."))
                                .display()
                                .to_string(),
                        );
                        app.set_store_status(format!("Not a directory: {}", path.display()))
                    }
                    Err(error) => {
                        app.set_store_directory(
                            std::env::current_dir()
                                .unwrap_or_else(|_| PathBuf::from("."))
                                .display()
                                .to_string(),
                        );
                        app.set_store_status(format!("Directory failed: {error}"));
                    }
                }
            }
            continue;
        }
        let store_key = match key.code {
            KeyCode::Up if key.modifiers.contains(KeyModifiers::ALT) => Some(StoreKey::MoveUp),
            KeyCode::Down if key.modifiers.contains(KeyModifiers::ALT) => Some(StoreKey::MoveDown),
            KeyCode::Up => Some(StoreKey::Up),
            KeyCode::Down => Some(StoreKey::Down),
            KeyCode::Char(' ') => Some(StoreKey::Toggle),
            KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                Some(StoreKey::Save)
            }
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                app.begin_store_directory_edit();
                None
            }
            KeyCode::Enter => Some(StoreKey::Confirm),
            KeyCode::Esc | KeyCode::Char('q') => Some(StoreKey::Cancel),
            _ => None,
        };
        let Some(store_key) = store_key else { continue };
        match app.handle_store_key(store_key) {
            StoreAction::Save(indices) => {
                let slots = indices
                    .into_iter()
                    .filter_map(|index| app.store_agents().get(index))
                    .map(|agent| RosterSlot {
                        agent: agent.identity.clone(),
                        model: agent.model.clone(),
                    })
                    .collect::<Vec<_>>();
                if let Err(error) = save_roster_slots(&slots) {
                    app.set_store_status(format!("Save failed: {error}"));
                }
            }
            StoreAction::Launch(indices) => {
                let selected = indices
                    .into_iter()
                    .filter_map(|index| app.store_agents().get(index))
                    .collect::<Vec<_>>();
                if selected.is_empty() {
                    continue;
                }
                let identities = selected
                    .iter()
                    .map(|agent| agent.identity.clone())
                    .collect::<Vec<_>>();
                save_roster_slots(
                    &selected
                        .iter()
                        .map(|agent| RosterSlot {
                            agent: agent.identity.clone(),
                            model: agent.model.clone(),
                        })
                        .collect::<Vec<_>>(),
                )?;
                let specs = selected
                    .iter()
                    .filter_map(|agent| {
                        launchable_catalog
                            .iter()
                            .find(|candidate| candidate.identity == agent.identity)
                    })
                    .map(agent_spec)
                    .collect::<Vec<_>>();
                let models = selected.iter().map(|agent| agent.model.clone()).collect();
                return run_roster(
                    terminal,
                    specs,
                    identities,
                    models,
                    Vec::new(),
                    None,
                    0,
                    100,
                );
            }
            StoreAction::Close => return Ok(()),
            StoreAction::Directory(_) => {}
            StoreAction::Ignored | StoreAction::Changed => {}
        }
    }
}

fn command_available(command: &str) -> bool {
    let Ok((program, _)) = parse_command_line(command) else {
        return false;
    };
    program_available(&program, std::env::var_os("PATH").as_deref())
}

fn program_available(program: &str, path: Option<&OsStr>) -> bool {
    if program.is_empty() {
        return false;
    }
    let program_path = Path::new(program);
    if program_path.is_absolute() || program_path.components().count() > 1 {
        return executable_file(program_path);
    }
    path.into_iter()
        .flat_map(std::env::split_paths)
        .any(|directory| executable_file(&directory.join(program)))
}

fn executable_file(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn save_roster_slots(slots: &[RosterSlot]) -> std::io::Result<()> {
    let Some(path) = settings_path() else {
        return Ok(());
    };
    settings::update(path, |settings| {
        let launcher = settings
            .entry("launcher")
            .or_insert_with(|| serde_json::json!({}));
        if !launcher.is_object() {
            *launcher = serde_json::json!({});
        }
        launcher["roster"] = serde_json::to_value(slots).unwrap_or_else(|_| serde_json::json!([]));
    })
}

fn load_ui_preferences(app: &mut App) {
    let Some(path) = settings_path() else { return };
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return;
    };
    if let Some(message) = value
        .get("ui")
        .and_then(|ui| ui.get("prompt_message"))
        .and_then(serde_json::Value::as_str)
        && !message.trim().is_empty()
    {
        app.set_prompt_message(message);
    }
    if let Some(follow) = value
        .get("ui")
        .and_then(|ui| ui.get("follow_output"))
        .and_then(serde_json::Value::as_bool)
    {
        app.follow_tail = follow;
    }
    if let Some(collapsed) = value
        .get("transcript")
        .and_then(|transcript| transcript.get("collapse_details"))
        .and_then(serde_json::Value::as_bool)
    {
        app.set_collapse_details(collapsed);
    }
    apply_notification_preferences(app, &value);
    if let Some(enabled) = value
        .get("notifications")
        .and_then(|notifications| notifications.get("enable_sounds"))
        .and_then(serde_json::Value::as_bool)
    {
        app.set_sounds_enabled(enabled);
    }
    if let Some(enabled) = value
        .get("notifications")
        .and_then(|notifications| notifications.get("blink_title"))
        .and_then(serde_json::Value::as_bool)
    {
        app.set_blink_title_enabled(enabled);
    }
    if let Some(enabled) = value
        .get("agent")
        .and_then(|agent| agent.get("thoughts"))
        .and_then(serde_json::Value::as_bool)
    {
        app.set_thoughts_enabled(enabled);
    }
    if let Some(expand) = value
        .get("tools")
        .and_then(|tools| tools.get("expand"))
        .and_then(serde_json::Value::as_str)
    {
        app.set_tool_expand_policy(expand);
    }
    if let Some(density) = value
        .get("ui")
        .and_then(|ui| ui.get("density"))
        .and_then(serde_json::Value::as_str)
    {
        app.set_density(density);
    }
    if let Some(scrollbar) = value
        .get("ui")
        .and_then(|ui| ui.get("scrollbar"))
        .and_then(serde_json::Value::as_str)
    {
        app.set_scrollbar_visible(!scrollbar.eq_ignore_ascii_case("hidden"));
    }
    if let Some(split) = value
        .get("diff")
        .and_then(|diff| diff.get("view"))
        .and_then(serde_json::Value::as_str)
    {
        app.set_diff_split(split.eq_ignore_ascii_case("split"));
    }
}

/// Seed the in-session config panel from the same catalog used by the launch
/// store. Existing live display names are marked selected so opening and
/// saving the panel cannot silently replace an active roster with defaults.
fn load_config_agents(app: &mut App) {
    let settings = settings_path()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .unwrap_or_default();
    let saved_slots = parse_saved_slots(&settings);
    let saved = saved_slots
        .iter()
        .map(|slot| slot.agent.clone())
        .collect::<Vec<_>>();
    let current_identities = app
        .active_roster_slots()
        .into_iter()
        .filter_map(|slot| app.agent_identity(slot).map(str::to_owned))
        .collect::<Vec<_>>();
    let mut catalog = codeswarm_adapters::agents::active_catalog(catalog_from_settings(&settings));
    catalog.sort_by_key(|agent| {
        saved
            .iter()
            .position(|identity| identity.eq_ignore_ascii_case(&agent.identity))
            .unwrap_or(usize::MAX)
    });
    let templates = catalog
        .into_iter()
        .map(|agent| StoreAgent {
            identity: agent.identity,
            name: agent.name,
            adapter: match agent.adapter {
                AdapterKind::Native => "native".into(),
                AdapterKind::Acp => "ACP".into(),
            },
            available: command_available(
                agent
                    .detect_command
                    .as_deref()
                    .unwrap_or(agent.command.as_str()),
            ),
            command: agent.command,
            selected: false,
            model: None,
        })
        .collect::<Vec<_>>();
    let mut seen = std::collections::BTreeMap::<String, usize>::new();
    let mut agents = current_identities
        .iter()
        .filter_map(|identity| {
            let mut agent = templates
                .iter()
                .find(|agent| agent.identity.eq_ignore_ascii_case(identity))?
                .clone();
            let occurrence = seen.entry(identity.to_ascii_lowercase()).or_default();
            agent.selected = true;
            agent.model = saved_slots
                .iter()
                .filter(|slot| slot.agent.eq_ignore_ascii_case(identity))
                .nth(*occurrence)
                .and_then(|slot| slot.model.clone());
            *occurrence += 1;
            Some(agent)
        })
        .collect::<Vec<_>>();
    agents.extend(templates);
    app.set_config_agents(agents);
}

fn live_slot_name(app: &App, slot: usize) -> String {
    app.raw_agent_names().get(slot).cloned().unwrap_or_default()
}

fn live_slot_identity(app: &App, slot: usize) -> String {
    app.agent_identity(slot)
        .map(str::to_owned)
        .unwrap_or_else(|| live_slot_name(app, slot))
}

fn find_live_slot(app: &App, identity: &str) -> Option<usize> {
    app.active_roster_slots()
        .into_iter()
        .find(|slot| live_slot_identity(app, *slot).eq_ignore_ascii_case(identity))
}

/// Apply a saved catalog roster to an idle live session using the same
/// transactional coordinator controls exposed by slash commands. Unknown
/// ad-hoc adapters are preserved; catalog rows can be added, dropped, and
/// reordered without requiring a session restart.
fn reconcile_config_roster(
    app: &mut App,
    controls: &tokio::sync::mpsc::UnboundedSender<AdapterControl>,
    pending_first: &mut Option<String>,
) -> Result<bool, String> {
    let desired = app
        .config_agents()
        .iter()
        .filter(|agent| agent.selected)
        .cloned()
        .collect::<Vec<_>>();
    if desired.is_empty() {
        return Err("select at least one agent for the roster".into());
    }
    if pending_first.is_some() {
        return Ok(false);
    }

    // Make the selected first agent the first live slot without restarting
    // either process. A missing agent is appended, then swapped after Ready.
    let desired_first = &desired[0].identity;
    let first_live_slot = app.active_roster_slots().first().copied().unwrap_or(0);
    if !live_slot_identity(app, first_live_slot).eq_ignore_ascii_case(desired_first) {
        if let Some(peer_slot) = find_live_slot(app, desired_first) {
            controls
                .send(AdapterControl::Swap(first_live_slot, peer_slot))
                .map_err(|_| "unable to queue roster reorder".to_owned())?;
            return Ok(false);
        } else {
            let agent = &desired[0];
            let spec = if agent.adapter.eq_ignore_ascii_case("native") {
                AgentSpec::Agy(agent.command.clone())
            } else {
                AgentSpec::Acp(agent.command.clone())
            };
            *pending_first = Some(agent.identity.clone());
            controls
                .send(AdapterControl::Add {
                    spec,
                    identity: agent.identity.clone(),
                })
                .map_err(|_| "unable to queue first agent".to_owned())?;
            return Ok(false);
        }
    }

    // Drop catalog agents removed from the desired roster. Ad-hoc names are
    // intentionally retained because they have no catalog identity to map.
    let desired_counts = desired.iter().fold(
        std::collections::BTreeMap::<String, usize>::new(),
        |mut counts, agent| {
            *counts
                .entry(agent.identity.to_ascii_lowercase())
                .or_default() += 1;
            counts
        },
    );
    let mut live_counts = std::collections::BTreeMap::<String, usize>::new();
    for slot in app.active_roster_slots().into_iter() {
        let identity = live_slot_identity(app, slot);
        let count = live_counts
            .entry(identity.to_ascii_lowercase())
            .or_default();
        *count += 1;
        if app
            .config_agents()
            .iter()
            .any(|agent| agent.identity.eq_ignore_ascii_case(&identity))
            && *count
                > desired_counts
                    .get(&identity.to_ascii_lowercase())
                    .copied()
                    .unwrap_or(0)
        {
            controls
                .send(AdapterControl::Drop(slot))
                .map_err(|_| "unable to queue agent removal".to_owned())?;
            return Ok(false);
        }
    }

    // Add selected catalog entries not represented by a live display name.
    let live_counts = app.active_roster_slots().into_iter().fold(
        std::collections::BTreeMap::<String, usize>::new(),
        |mut counts, slot| {
            *counts
                .entry(live_slot_identity(app, slot).to_ascii_lowercase())
                .or_default() += 1;
            counts
        },
    );
    let mut desired_seen = std::collections::BTreeMap::<String, usize>::new();
    for agent in &desired {
        let key = agent.identity.to_ascii_lowercase();
        let occurrence = desired_seen.entry(key.clone()).or_default();
        *occurrence += 1;
        if *occurrence <= live_counts.get(&key).copied().unwrap_or(0) {
            continue;
        }
        let spec = if agent.adapter.eq_ignore_ascii_case("native") {
            AgentSpec::Agy(agent.command.clone())
        } else {
            AgentSpec::Acp(agent.command.clone())
        };
        controls
            .send(AdapterControl::Add {
                spec,
                identity: agent.identity.clone(),
            })
            .map_err(|_| "unable to queue agent addition".to_owned())?;
        return Ok(false);
    }

    // Reorder the currently represented desired agents. Pending additions are
    // left in catalog order and will be available for a subsequent swap once
    // their Ready event arrives.
    for (position, agent) in desired.iter().enumerate() {
        let slots = app.active_roster_slots();
        let Some(target_slot) = slots.get(position).copied() else {
            break;
        };
        let Some(found_slot) = slots
            .into_iter()
            .find(|slot| live_slot_identity(app, *slot).eq_ignore_ascii_case(&agent.identity))
        else {
            continue;
        };
        if found_slot != target_slot {
            controls
                .send(AdapterControl::Swap(target_slot, found_slot))
                .map_err(|_| "unable to queue roster reorder".to_owned())?;
            return Ok(false);
        }
    }
    Ok(true)
}

fn validate_config_roster(app: &App) -> Result<(), String> {
    let selected = app
        .config_agents()
        .iter()
        .filter(|agent| agent.selected)
        .collect::<Vec<_>>();
    if selected.is_empty() {
        return Err("select at least one agent for the roster".into());
    }
    if let Some(agent) = selected
        .into_iter()
        .find(|agent| !agent.available || !command_available(&agent.command))
    {
        return Err(format!("Not detected: {}", agent.name));
    }
    Ok(())
}

fn apply_notification_preferences(app: &mut App, value: &serde_json::Value) {
    if let Some(policy) = value
        .get("notifications")
        .and_then(|notifications| notifications.get("system"))
        .and_then(serde_json::Value::as_str)
    {
        app.set_notification_policy(policy);
    } else if let Some(enabled) = value
        .get("notifications")
        .and_then(|notifications| notifications.get("enabled"))
        .and_then(serde_json::Value::as_bool)
        .or_else(|| {
            value
                .get("notifications")
                .and_then(|notifications| notifications.get("turn_over"))
                .and_then(serde_json::Value::as_bool)
        })
    {
        // `enabled`/`turn_over` are the Rust and Python boolean compatibility
        // keys.  They map to the Python client's safe blur-only policy.
        app.set_notifications_enabled(enabled);
    }
}

fn save_ui_preferences(app: &App) -> std::io::Result<()> {
    let Some(path) = settings_path() else {
        return Ok(());
    };
    settings::update(path, |settings| {
        {
            let ui = settings
                .entry("ui")
                .or_insert_with(|| serde_json::json!({}));
            if !ui.is_object() {
                *ui = serde_json::json!({});
            }
            ui["follow_output"] = serde_json::Value::Bool(app.follow_tail);
            ui["prompt_message"] = serde_json::Value::String(app.prompt_message().into());
            ui["density"] = serde_json::Value::String(app.density().into());
            ui["scrollbar"] = serde_json::Value::String(
                if app.scrollbar_visible() {
                    "normal"
                } else {
                    "hidden"
                }
                .into(),
            );
        }
        let transcript = settings
            .entry("transcript")
            .or_insert_with(|| serde_json::json!({}));
        if !transcript.is_object() {
            *transcript = serde_json::json!({});
        }
        transcript["collapse_details"] = serde_json::Value::Bool(app.collapse_details());
        let notifications = settings
            .entry("notifications")
            .or_insert_with(|| serde_json::json!({}));
        if !notifications.is_object() {
            *notifications = serde_json::json!({});
        }
        notifications["system"] =
            serde_json::Value::String(app.notification_policy().as_str().into());
        notifications["enabled"] = serde_json::Value::Bool(app.notifications_enabled());
        // Keep the legacy Python key readable by either client while the
        // Rust UI uses the shorter `enabled` spelling internally.
        notifications["turn_over"] = serde_json::Value::Bool(app.notifications_enabled());
        notifications["enable_sounds"] = serde_json::Value::Bool(app.sounds_enabled());
        notifications["blink_title"] = serde_json::Value::Bool(app.blink_title_enabled());
        let agent = settings
            .entry("agent")
            .or_insert_with(|| serde_json::json!({}));
        if !agent.is_object() {
            *agent = serde_json::json!({});
        }
        agent["thoughts"] = serde_json::Value::Bool(app.thoughts_enabled());
        let tools = settings
            .entry("tools")
            .or_insert_with(|| serde_json::json!({}));
        if !tools.is_object() {
            *tools = serde_json::json!({});
        }
        tools["expand"] = serde_json::Value::String(app.tool_expand_policy().into());
        let diff = settings
            .entry("diff")
            .or_insert_with(|| serde_json::json!({}));
        if !diff.is_object() {
            *diff = serde_json::json!({});
        }
        diff["view"] =
            serde_json::Value::String(if app.diff_split() { "split" } else { "unified" }.into());
    })
}

fn run_preview(terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>) -> std::io::Result<()> {
    let mut app = App::default();
    app.set_header("CodeSwarm preview", "press q to quit");
    app.transcript.append(
        BlockKind::Notice,
        "Ratatui preview uses a viewport-only transcript.",
        false,
    );
    app.transcript.append(
        BlockKind::Agent,
        fixtures::five_thousand_word_reply(),
        false,
    );
    run_terminal(terminal, &mut app, None, None, None)
}

fn run_agy(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    prompt: Option<String>,
    resume: bool,
) -> std::io::Result<()> {
    run_agy_command(terminal, prompt, "agy", resume)
}

fn run_agy_command(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    prompt: Option<String>,
    command: &str,
    resume: bool,
) -> std::io::Result<()> {
    let initial_prompt = prompt.clone();
    let (events, controls, worker) = spawn_agy_command(prompt, command.to_owned(), resume);
    let mut app = App::default();
    let name = display_agent_name(command);
    app.set_agent_name(0, name.clone());
    app.set_agent_identity(0, catalog_identity_for_command(command));
    app.set_header(name, "starting");
    if let Some(prompt) = initial_prompt {
        app.record_human_message(&prompt, false);
    }
    let shutdown = controls.clone();
    let result = run_terminal(terminal, &mut app, Some(events), Some(controls), None);
    stop_worker(shutdown, Some(worker));
    result
}

fn run_acp(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    program: String,
    prompt: Option<String>,
    resume: bool,
) -> std::io::Result<()> {
    run_acp_program(terminal, program, prompt, resume)
}

fn run_acp_program(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    program: String,
    prompt: Option<String>,
    resume: bool,
) -> std::io::Result<()> {
    let initial_prompt = prompt.clone();
    let (events, controls, worker) = spawn_acp(program.clone(), prompt, resume);
    let mut app = App::default();
    let name = display_agent_name(&program);
    app.set_agent_name(0, name.clone());
    app.set_agent_identity(0, catalog_identity_for_command(&program));
    app.set_header(name, "starting");
    if let Some(prompt) = initial_prompt {
        app.record_human_message(&prompt, false);
    }
    let shutdown = controls.clone();
    let result = run_terminal(terminal, &mut app, Some(events), Some(controls), None);
    stop_worker(shutdown, worker);
    result
}

#[allow(clippy::too_many_arguments)]
fn run_roster(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    specs: Vec<AgentSpec>,
    identities: Vec<String>,
    models: Vec<Option<String>>,
    session_ids: Vec<Option<String>>,
    prompt: Option<String>,
    first_slot: usize,
    max_rounds: usize,
) -> std::io::Result<()> {
    let mut app = App::default();
    for (slot, spec) in specs.iter().enumerate() {
        let name = match spec {
            AgentSpec::Agy(command) | AgentSpec::Acp(command) => command,
        };
        app.set_agent_name(slot, display_agent_name(name));
        if let Some(identity) = identities.get(slot) {
            app.set_agent_identity(slot, identity.clone());
        }
    }
    if let Some(prompt) = prompt.as_ref() {
        app.record_human_message(prompt, false);
    }
    let (events, controls, worker) = spawn_relay(
        specs,
        identities,
        models,
        session_ids,
        prompt,
        first_slot,
        max_rounds,
    );
    app.set_header(app.agent_name(first_slot), "starting");
    let shutdown = controls.clone();
    let result = run_terminal(
        terminal,
        &mut app,
        Some(events),
        Some(controls),
        Some(first_slot),
    );
    stop_worker(shutdown, Some(worker));
    result
}

fn stop_worker(
    shutdown: tokio::sync::mpsc::UnboundedSender<AdapterControl>,
    worker: Option<thread::JoinHandle<()>>,
) {
    let _ = shutdown.send(AdapterControl::Stop);
    let Some(worker) = worker else { return };
    // Release the user's terminal before waiting for complete process-tree
    // cleanup. Shutdown may be proportional to an unlimited roster, but it
    // must never leave the screen/raw mode captured while it finishes.
    let _ = disable_raw_mode();
    let mut output = stdout();
    let _ = execute!(output, LeaveAlternateScreen, Show);
    if terminal_capture_enabled() {
        let _ = execute!(output, DisableFocusChange);
    }
    let _ = execute!(output, DisableMouseCapture);
    let _ = worker.join();
}

fn spawn_agy_command(
    prompt: Option<String>,
    command: String,
    resume: bool,
) -> (
    Receiver<AdapterResult<AgentEvent>>,
    tokio::sync::mpsc::UnboundedSender<AdapterControl>,
    thread::JoinHandle<()>,
) {
    let (sender, receiver) = mpsc::channel();
    let (controls, control_receiver) = tokio::sync::mpsc::unbounded_channel();
    let worker =
        thread::spawn(move || run_agy_task(sender, control_receiver, prompt, command, resume));
    (receiver, controls, worker)
}

/// Hide CodeSwarm's relay marker when an adapter is run directly. Relay turns
/// retain the marker until `RelayHost` decides whether a reviewer may stop;
/// standalone `--agy` and `--acp` sessions have no such semantics and must
/// never expose the control token in the transcript. A short UTF-8-safe tail
/// also handles a marker split across stream chunks.
fn sanitize_direct_event(event: AgentEvent, response_tail: &mut String) -> Vec<AgentEvent> {
    let mut visible = Vec::new();
    match event {
        AgentEvent::Text { slot, text } => {
            response_tail.push_str(&text);
            let token = codeswarm_adapters::relay::STOP_TOKEN;
            let keep = token.len().saturating_sub(1);
            loop {
                if let Some(index) = response_tail.find(token) {
                    let prefix = response_tail[..index].to_owned();
                    if !prefix.is_empty() {
                        visible.push(AgentEvent::Text { slot, text: prefix });
                    }
                    *response_tail = response_tail[index + token.len()..].replace(token, "");
                    continue;
                }
                if response_tail.len() > keep {
                    let mut boundary = response_tail.len() - keep;
                    while boundary > 0 && !response_tail.is_char_boundary(boundary) {
                        boundary -= 1;
                    }
                    let prefix = response_tail[..boundary].to_owned();
                    if !prefix.is_empty() {
                        visible.push(AgentEvent::Text { slot, text: prefix });
                    }
                    *response_tail = response_tail[boundary..].to_owned();
                }
                break;
            }
        }
        AgentEvent::TurnComplete { slot } => {
            let text =
                std::mem::take(response_tail).replace(codeswarm_adapters::relay::STOP_TOKEN, "");
            if !text.is_empty() {
                visible.push(AgentEvent::Text { slot, text });
            }
            visible.push(AgentEvent::TurnComplete { slot });
        }
        other => visible.push(other),
    }
    visible
}

async fn cancel_standalone_turn(
    adapter: &mut dyn AgentAdapter,
    sender: &Sender<AdapterResult<AgentEvent>>,
    response_tail: &mut String,
) {
    // ACP and native adapters may consume their provider completion while
    // waiting for cancellation to settle. Always publish one terminal result
    // to the UI ourselves, and discard text held back by direct-turn token
    // sanitization so it cannot leak into the next prompt.
    response_tail.clear();
    let result = match adapter.cancel().await {
        Ok(true) => AdapterError::Transport("standalone turn cancelled".into()),
        Ok(false) => AdapterError::Unsupported("adapter did not accept cancellation"),
        Err(error) => error,
    };
    let _ = sender.send(Err(result));
}

fn finish_pending_cancellation(app: &mut App, error_text: &str) -> bool {
    let cancelled =
        app.cancellation_pending() || error_text.to_ascii_lowercase().contains("cancelled");
    if cancelled {
        app.finish_turn_cancellation();
    }
    cancelled
}

fn run_agy_task(
    sender: Sender<AdapterResult<AgentEvent>>,
    mut controls: tokio::sync::mpsc::UnboundedReceiver<AdapterControl>,
    prompt: Option<String>,
    command: String,
    resume: bool,
) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = sender.send(Err(codeswarm_adapters::AdapterError::Transport(
                error.to_string(),
            )));
            return;
        }
    };
    runtime.block_on(async move {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let name = display_agent_name(&command);
        let identity = catalog_identity_for_command(&command);
        let session_id = resume
            .then(|| load_agent_session_id(&cwd, &identity))
            .flatten();
        let mut adapter = session_id.map_or_else(
            || AgyAdapter::new(0, cwd.clone(), command.clone()),
            |session_id| AgyAdapter::with_session_id(0, cwd.clone(), command.clone(), session_id),
        );
        let metadata_writer = SessionMetadataStore::open(session_metadata_path_for(&cwd))
            .buffered()
            .ok();
        if let Err(error) = adapter.start().await {
            let _ = sender.send(Err(error));
            return;
        }
        if adapter.capabilities().supports_modes
            && let Err(error) = adapter
                .set_mode(codeswarm_adapters::policy::DEFAULT_POLICY_ID.into())
                .await
        {
            let _ = sender.send(Err(error));
            let _ = adapter.stop().await;
            return;
        }
        queue_standalone_metadata(
            metadata_writer.as_ref(),
            &cwd,
            &name,
            &identity,
            &command,
            &adapter,
        );
        if let Some(prompt) = prompt {
            if let Err(error) = adapter.send_prompt(prompt).await {
                let _ = sender.send(Err(error));
                let _ = adapter.stop().await;
                if let Some(writer) = &metadata_writer {
                    let _ = writer.flush();
                }
                return;
            }
            let _ = sender.send(Ok(AgentEvent::TurnStarted { slot: 0 }));
        }
        let mut response_tail = String::new();
        'events: loop {
            tokio::select! {
                event = adapter.next_event() => match event {
                    Some(event) => {
                        match event {
                            Ok(event) => {
                                let turn_complete =
                                    matches!(&event, AgentEvent::TurnComplete { .. });
                                for event in sanitize_direct_event(event, &mut response_tail) {
                                    if sender.send(Ok(event)).is_err() {
                                        break 'events;
                                    }
                                }
                                if turn_complete {
                                    queue_standalone_metadata(
                                        metadata_writer.as_ref(),
                                        &cwd,
                                        &name,
                                        &identity,
                                        &command,
                                        &adapter,
                                    );
                                }
                            }
                            Err(error) => {
                                response_tail.clear();
                                if sender.send(Err(error)).is_err() {
                                    break 'events;
                                }
                            }
                        }
                    }
                    None => break,
                },
                command = controls.recv() => match command {
                    Some(AdapterControl::Prompt(prompt)) => {
                        if let Err(error) = adapter.send_prompt(prompt).await {
                            let _ = sender.send(Err(error));
                        } else {
                            let _ = sender.send(Ok(AgentEvent::TurnStarted { slot: 0 }));
                        }
                    }
                    Some(AdapterControl::Cancel) => {
                        cancel_standalone_turn(&mut adapter, &sender, &mut response_tail).await;
                    }
                    Some(AdapterControl::Permission { request_id, answer, .. }) => {
                        if let Err(error) = adapter.answer_permission(request_id, answer).await {
                            let _ = sender.send(Err(error));
                        }
                    }
                    Some(AdapterControl::SetMode(mode)) => {
                        if let Err(error) = adapter.set_mode(mode).await {
                            let _ = sender.send(Err(error));
                        }
                    }
                    Some(AdapterControl::SetModel { model, .. }) => {
                        match adapter.set_model(model.clone()).await {
                            Ok(()) => {
                                let _ = sender.send(Ok(AgentEvent::ModelUpdated {
                                    slot: 0,
                                    current_model: model,
                                }));
                            }
                            Err(error) => {
                                let _ = sender.send(Err(error));
                            }
                        }
                    }
                    Some(AdapterControl::Reload(_)) => {
                        if let Err(error) = adapter.reload().await {
                            let _ = sender.send(Err(error));
                        }
                    }
                    Some(AdapterControl::Drop(_))
                    | Some(AdapterControl::Swap(_, _))
                    | Some(AdapterControl::Add { .. }) => {}
                    Some(AdapterControl::Queue { .. })
                    | Some(AdapterControl::Direct { .. })
                    | Some(AdapterControl::SetStrategy(_)) => {}
                    Some(AdapterControl::Stop) | None => break,
                },
            }
        }
        let _ = adapter.stop().await;
        if let Some(writer) = &metadata_writer {
            let _ = writer.flush();
        }
    });
}

fn spawn_acp(
    program: String,
    prompt: Option<String>,
    resume: bool,
) -> (
    Receiver<AdapterResult<AgentEvent>>,
    tokio::sync::mpsc::UnboundedSender<AdapterControl>,
    Option<thread::JoinHandle<()>>,
) {
    let (sender, receiver) = mpsc::channel();
    let (controls, control_receiver) = tokio::sync::mpsc::unbounded_channel();
    let (program, args) = match parse_command_line(&program) {
        Ok(command) => command,
        Err(error) => {
            let _ = sender.send(Err(AdapterError::Spawn(format!(
                "invalid ACP command: {error}"
            ))));
            return (receiver, controls, None);
        }
    };
    let worker = thread::spawn(move || {
        run_acp_task(sender, control_receiver, program, args, prompt, resume)
    });
    (receiver, controls, Some(worker))
}

fn run_acp_task(
    sender: Sender<AdapterResult<AgentEvent>>,
    mut controls: tokio::sync::mpsc::UnboundedReceiver<AdapterControl>,
    program: String,
    args: Vec<String>,
    prompt: Option<String>,
    resume: bool,
) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = sender.send(Err(codeswarm_adapters::AdapterError::Transport(
                error.to_string(),
            )));
            return;
        }
    };
    runtime.block_on(async move {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let command = std::iter::once(program.as_str())
            .chain(args.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" ");
        let name = display_agent_name(&command);
        let identity = catalog_identity_for_command(&command);
        let session_id = resume
            .then(|| load_agent_session_id(&cwd, &identity))
            .flatten();
        let mut adapter = session_id.map_or_else(
            || AcpAdapter::new(0, cwd.clone(), program.clone(), args.clone()),
            |session_id| {
                AcpAdapter::with_session_id(
                    0,
                    cwd.clone(),
                    program.clone(),
                    args.clone(),
                    session_id,
                )
            },
        );
        let metadata_writer = SessionMetadataStore::open(session_metadata_path_for(&cwd))
            .buffered()
            .ok();
        if let Err(error) = adapter.start().await {
            let _ = sender.send(Err(error));
            return;
        }
        if adapter.capabilities().supports_modes
            && let Err(error) = adapter
                .set_mode(codeswarm_adapters::policy::DEFAULT_POLICY_ID.into())
                .await
        {
            let _ = sender.send(Err(error));
            let _ = adapter.stop().await;
            return;
        }
        queue_standalone_metadata(
            metadata_writer.as_ref(),
            &cwd,
            &name,
            &identity,
            &command,
            &adapter,
        );
        if let Some(prompt) = prompt {
            if let Err(error) = adapter.send_prompt(prompt).await {
                let _ = sender.send(Err(error));
                let _ = adapter.stop().await;
                if let Some(writer) = &metadata_writer {
                    let _ = writer.flush();
                }
                return;
            }
            let _ = sender.send(Ok(AgentEvent::TurnStarted { slot: 0 }));
        }
        let mut response_tail = String::new();
        'events: loop {
            tokio::select! {
                event = adapter.next_event() => match event {
                    Some(event) => {
                        match event {
                            Ok(event) => {
                                let turn_complete =
                                    matches!(&event, AgentEvent::TurnComplete { .. });
                                for event in sanitize_direct_event(event, &mut response_tail) {
                                    if sender.send(Ok(event)).is_err() {
                                        break 'events;
                                    }
                                }
                                if turn_complete {
                                    queue_standalone_metadata(
                                        metadata_writer.as_ref(),
                                        &cwd,
                                        &name,
                                        &identity,
                                        &command,
                                        &adapter,
                                    );
                                }
                            }
                            Err(error) => {
                                response_tail.clear();
                                if sender.send(Err(error)).is_err() {
                                    break 'events;
                                }
                            }
                        }
                    }
                    None => break,
                },
                command = controls.recv() => match command {
                    Some(AdapterControl::Prompt(prompt)) => {
                        if let Err(error) = adapter.send_prompt(prompt).await {
                            let _ = sender.send(Err(error));
                        } else {
                            let _ = sender.send(Ok(AgentEvent::TurnStarted { slot: 0 }));
                        }
                    }
                    Some(AdapterControl::Cancel) => {
                        cancel_standalone_turn(&mut adapter, &sender, &mut response_tail).await;
                    }
                    Some(AdapterControl::Permission { request_id, answer, .. }) => {
                        if let Err(error) = adapter.answer_permission(request_id, answer).await {
                            let _ = sender.send(Err(error));
                        }
                    }
                    Some(AdapterControl::SetMode(mode)) => {
                        if let Err(error) = adapter.set_mode(mode).await {
                            let _ = sender.send(Err(error));
                        }
                    }
                    Some(AdapterControl::SetModel { model, .. }) => {
                        match adapter.set_model(model.clone()).await {
                            Ok(()) => {
                                let _ = sender.send(Ok(AgentEvent::ModelUpdated {
                                    slot: 0,
                                    current_model: model,
                                }));
                            }
                            Err(error) => {
                                let _ = sender.send(Err(error));
                            }
                        }
                    }
                    Some(AdapterControl::Reload(_)) => {
                        if let Err(error) = adapter.reload().await {
                            let _ = sender.send(Err(error));
                        }
                    }
                    Some(AdapterControl::Drop(_))
                    | Some(AdapterControl::Swap(_, _))
                    | Some(AdapterControl::Add { .. }) => {}
                    Some(AdapterControl::Queue { .. })
                    | Some(AdapterControl::Direct { .. })
                    | Some(AdapterControl::SetStrategy(_)) => {}
                    Some(AdapterControl::Stop) | None => break,
                },
            }
        }
        let _ = adapter.stop().await;
        if let Some(writer) = &metadata_writer {
            let _ = writer.flush();
        }
    });
}

fn spawn_relay(
    specs: Vec<AgentSpec>,
    identities: Vec<String>,
    models: Vec<Option<String>>,
    session_ids: Vec<Option<String>>,
    prompt: Option<String>,
    first_slot: usize,
    max_rounds: usize,
) -> (
    Receiver<AdapterResult<AgentEvent>>,
    tokio::sync::mpsc::UnboundedSender<AdapterControl>,
    thread::JoinHandle<()>,
) {
    let (sender, receiver) = mpsc::channel();
    let (controls, control_receiver) = tokio::sync::mpsc::unbounded_channel();
    let worker = thread::spawn(move || {
        run_relay_task(
            sender,
            control_receiver,
            specs,
            identities,
            models,
            session_ids,
            prompt,
            first_slot,
            max_rounds,
        )
    });
    (receiver, controls, worker)
}

async fn run_relay_turn_with_controls(
    relay: &mut RelayHost,
    controls: &mut tokio::sync::mpsc::UnboundedReceiver<AdapterControl>,
    sender: &Sender<AdapterResult<AgentEvent>>,
    task: String,
    first_slot: usize,
) -> (bool, Vec<AdapterControl>, Option<RelayDecision>) {
    let cancellation = relay.cancellation();
    let (permission_sender, mut permission_receiver) = tokio::sync::mpsc::unbounded_channel();
    let turn = relay.run_turn_with_permissions(task, first_slot, &mut permission_receiver);
    tokio::pin!(turn);
    let mut deferred = Vec::new();
    let mut stopping = false;
    let result = loop {
        tokio::select! {
            result = &mut turn => break result,
            command = controls.recv(), if !stopping => match command {
                Some(AdapterControl::Cancel) => cancellation.request(),
                Some(AdapterControl::Permission { slot, request_id, answer }) => {
                    if let Err(error) = permission_sender.send(RelayPermissionAnswer {
                        slot,
                        request_id,
                        answer,
                    }) {
                        let permission = error.0;
                        deferred.push(AdapterControl::Permission {
                            slot: permission.slot,
                            request_id: permission.request_id,
                            answer: permission.answer,
                        });
                    }
                }
                Some(AdapterControl::Stop) | None => {
                    stopping = true;
                    cancellation.request();
                }
                Some(command) => deferred.push(command),
            },
        }
    };
    match result {
        Ok(decision) => (stopping, deferred, Some(decision)),
        Err(error) => {
            let _ = sender.send(Err(error));
            (stopping, deferred, None)
        }
    }
}

/// Drain the causal relay ring for one human task.
///
/// `RelayHost::run_turn` deliberately performs one adapter turn at a time;
/// this wrapper is the CLI's handoff loop that invokes it again for the next
/// roster slot. Controls received while a turn is active are returned to the
/// outer command loop so pause, queue, direct, and stop semantics remain
/// ordered at the turn boundary.
async fn run_relay_sequence_with_controls(
    relay: &mut RelayHost,
    controls: &mut tokio::sync::mpsc::UnboundedReceiver<AdapterControl>,
    sender: &Sender<AdapterResult<AgentEvent>>,
    task: String,
    first_slot: usize,
) -> (bool, Vec<AdapterControl>) {
    let mut task = task;
    loop {
        let (stopping, deferred, decision) =
            run_relay_turn_with_controls(relay, controls, sender, task, first_slot).await;
        if stopping {
            return (true, deferred);
        }
        let mut blocking = Vec::new();
        for command in deferred {
            match command {
                AdapterControl::SetStrategy(strategy) => relay.set_strategy(strategy),
                AdapterControl::SetMode(mode) => {
                    if let Err(error) = relay.set_policy(mode).await {
                        let _ = sender.send(Err(error));
                    }
                }
                command => blocking.push(command),
            }
        }
        if !blocking.is_empty() {
            return (false, blocking);
        }
        if !matches!(decision, Some(RelayDecision::Dispatch { .. })) {
            return (false, blocking);
        }
        // A one-agent roster uses the same hot-reload-capable host but never
        // reviews itself. Adding a peer later naturally enables the ring.
        if relay.relay().active_slots().count() == 1 {
            return (false, blocking);
        }
        // The first invocation carries the human task. Subsequent invocations
        // let Relay choose its next slot and use the prior response/context.
        task = String::new();
    }
}

#[allow(clippy::too_many_arguments)]
fn run_relay_task(
    sender: Sender<AdapterResult<AgentEvent>>,
    mut controls: tokio::sync::mpsc::UnboundedReceiver<AdapterControl>,
    specs: Vec<AgentSpec>,
    identities: Vec<String>,
    models: Vec<Option<String>>,
    session_ids: Vec<Option<String>>,
    prompt: Option<String>,
    first_slot: usize,
    max_rounds: usize,
) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = sender.send(Err(AdapterError::Transport(error.to_string())));
            return;
        }
    };
    runtime.block_on(async move {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let roster_names = specs
            .iter()
            .map(|spec| match spec {
                AgentSpec::Agy(command) | AgentSpec::Acp(command) => display_agent_name(command),
            })
            .collect::<Vec<_>>();
        let roster_launch_specs = specs
            .iter()
            .map(|spec| {
                let protocol = match spec {
                    AgentSpec::Agy(_) => "native",
                    AgentSpec::Acp(_) => "acp",
                };
                (protocol.to_owned(), agent_spec_command(spec).to_owned())
            })
            .collect::<Vec<_>>();
        let hosts = specs
            .into_iter()
            .enumerate()
            .map(|(slot, spec)| {
                let session_id = session_ids.get(slot).and_then(Clone::clone);
                let adapter = match spec {
                    AgentSpec::Agy(command) => {
                        let adapter = session_id.as_ref().map_or_else(
                            || AgyAdapter::new(slot, cwd.clone(), command.clone()),
                            |session_id| {
                                AgyAdapter::with_session_id(
                                    slot,
                                    cwd.clone(),
                                    command.clone(),
                                    session_id,
                                )
                            },
                        );
                        Ok(Box::new(adapter) as Box<dyn AgentAdapter>)
                    }
                    AgentSpec::Acp(command) => {
                        let (program, args) = match parse_command_line(&command) {
                            Ok(command) => command,
                            Err(error) => {
                                return Err(AdapterError::Spawn(format!(
                                    "invalid ACP command: {error}"
                                )));
                            }
                        };
                        let adapter = session_id.as_ref().map_or_else(
                            || AcpAdapter::new(slot, cwd.clone(), program.clone(), args.clone()),
                            |session_id| {
                                AcpAdapter::with_session_id(
                                    slot,
                                    cwd.clone(),
                                    program.clone(),
                                    args.clone(),
                                    session_id,
                                )
                            },
                        );
                        Ok(Box::new(adapter) as Box<dyn AgentAdapter>)
                    }
                }?;
                Ok(AdapterHost::new(adapter, None))
            })
            .collect::<Result<Vec<_>, AdapterError>>();
        let hosts = match hosts {
            Ok(hosts) => hosts,
            Err(error) => {
                let _ = sender.send(Err(error));
                return;
            }
        };
        let mut relay = match RelayHost::new(hosts, max_rounds) {
            Ok(relay) => relay,
            Err(error) => {
                let _ = sender.send(Err(error));
                return;
            }
        };
        relay.set_roster_names(roster_names);
        relay.set_roster_identities(identities);
        relay.set_roster_launch_specs(roster_launch_specs);
        relay.set_session_metadata_workspace(cwd.display().to_string());
        if let Ok(writer) = SessionMetadataStore::open(session_metadata_path_for(&cwd)).buffered() {
            relay.set_session_metadata_writer(writer);
        }
        let event_sender = sender.clone();
        relay.set_event_sink(move |event| {
            let _ = event_sender.send(Ok(event));
        });
        if let Err(error) = relay.start().await {
            let _ = sender.send(Err(error));
            return;
        }
        for (slot, model) in models.into_iter().enumerate() {
            if let Some(model) = model
                && let Err(error) = relay.set_model(slot, model).await
            {
                let _ = sender.send(Ok(AgentEvent::RosterUpdated {
                    update: codeswarm_adapters::RosterUpdate::Rejected {
                        action: format!("set model for agent {slot}"),
                        detail: error.to_string(),
                    },
                }));
            }
        }
        let mut pending_commands = VecDeque::new();
        if let Some(prompt) = prompt {
            let (stopping, deferred) = run_relay_sequence_with_controls(
                &mut relay,
                &mut controls,
                &sender,
                prompt,
                first_slot,
            )
            .await;
            if stopping {
                let _ = relay.stop().await;
                return;
            }
            pending_commands.extend(deferred);
        }
        loop {
            let command = match pending_commands.pop_front() {
                Some(command) => Some(command),
                None => controls.recv().await,
            };
            match command {
                Some(AdapterControl::Prompt(prompt)) => {
                    // No explicit target means the relay's live routing state
                    // chooses the next recipient. Footer selections use Queue
                    // and are consumed after one message.
                    let selected = relay.relay().active_slots().next().unwrap_or(first_slot);
                    if !relay.relay_mut().enqueue_human(prompt, None) {
                        let _ = sender.send(Err(AdapterError::Transport(
                            "unable to queue prompt for roster".into(),
                        )));
                        continue;
                    }
                    let (stopping, deferred) = run_relay_sequence_with_controls(
                        &mut relay,
                        &mut controls,
                        &sender,
                        "".into(),
                        selected,
                    )
                    .await;
                    pending_commands.extend(deferred);
                    if stopping {
                        break;
                    }
                }
                Some(AdapterControl::Queue { slot, prompt }) => {
                    if !relay.relay_mut().enqueue_human(prompt, Some(slot)) {
                        let _ = sender.send(Err(AdapterError::Transport(
                            "unable to queue prompt for selected agent".into(),
                        )));
                        continue;
                    }
                    let (stopping, deferred) = run_relay_sequence_with_controls(
                        &mut relay,
                        &mut controls,
                        &sender,
                        "".into(),
                        slot,
                    )
                    .await;
                    pending_commands.extend(deferred);
                    if stopping {
                        break;
                    }
                }
                Some(AdapterControl::Direct { slot, prompt }) => {
                    match relay.relay_mut().enqueue_direct(slot, prompt) {
                        Ok(true) => {}
                        Ok(false) => {
                            let _ = sender.send(Err(AdapterError::Transport(
                                "unable to queue direct prompt".into(),
                            )));
                            continue;
                        }
                        Err(error) => {
                            let _ = sender.send(Err(AdapterError::Transport(error.into())));
                            continue;
                        }
                    }
                    let (stopping, deferred) = run_relay_sequence_with_controls(
                        &mut relay,
                        &mut controls,
                        &sender,
                        "".into(),
                        slot,
                    )
                    .await;
                    pending_commands.extend(deferred);
                    if stopping {
                        break;
                    }
                }
                Some(AdapterControl::Permission {
                    slot,
                    request_id,
                    answer,
                }) => {
                    if let Err(error) = relay.answer_permission(slot, request_id, answer).await {
                        let _ = sender.send(Err(error));
                    }
                }
                Some(AdapterControl::SetStrategy(strategy)) => relay.set_strategy(strategy),
                Some(AdapterControl::SetMode(mode)) => {
                    if let Err(error) = relay.set_policy(mode).await {
                        let _ = sender.send(Err(error));
                    }
                }
                Some(AdapterControl::SetModel { slot, model }) => {
                    if let Err(error) = relay.set_model(slot, model).await {
                        let _ = sender.send(Ok(AgentEvent::RosterUpdated {
                            update: codeswarm_adapters::RosterUpdate::Rejected {
                                action: format!("set model for agent {slot}"),
                                detail: error.to_string(),
                            },
                        }));
                    }
                }
                Some(AdapterControl::Reload(slot)) => {
                    if let Err(error) = relay.reload(slot).await {
                        let _ = sender.send(Ok(AgentEvent::RosterUpdated {
                            update: codeswarm_adapters::RosterUpdate::Rejected {
                                action: format!("reload agent {slot}"),
                                detail: error.to_string(),
                            },
                        }));
                    }
                }
                Some(AdapterControl::Drop(slot)) => {
                    if let Err(error) = relay.drop_agent(slot).await {
                        let _ = sender.send(Ok(AgentEvent::RosterUpdated {
                            update: codeswarm_adapters::RosterUpdate::Rejected {
                                action: format!("drop agent {slot}"),
                                detail: error.to_string(),
                            },
                        }));
                    }
                }
                Some(AdapterControl::Swap(first, second)) => {
                    if let Err(error) = relay.swap_agents(first, second) {
                        let _ = sender.send(Ok(AgentEvent::RosterUpdated {
                            update: codeswarm_adapters::RosterUpdate::Rejected {
                                action: format!("swap agents {first} and {second}"),
                                detail: error.to_string(),
                            },
                        }));
                    }
                }
                Some(AdapterControl::Add { spec, identity }) => {
                    let slot = relay.next_slot();
                    let launch_command = agent_spec_command(&spec).to_owned();
                    let adapter = match spec.clone() {
                        AgentSpec::Agy(command) => {
                            Ok(Box::new(AgyAdapter::new(slot, cwd.clone(), command))
                                as Box<dyn AgentAdapter>)
                        }
                        AgentSpec::Acp(command) => match parse_command_line(&command) {
                            Ok((program, args)) => {
                                Ok(Box::new(AcpAdapter::new(slot, cwd.clone(), program, args))
                                    as Box<dyn AgentAdapter>)
                            }
                            Err(error) => {
                                Err(AdapterError::Spawn(format!("invalid ACP command: {error}")))
                            }
                        },
                    };
                    match adapter {
                        Ok(adapter) => {
                            let name = match spec {
                                AgentSpec::Agy(command) | AgentSpec::Acp(command) => {
                                    display_agent_name(&command)
                                }
                            };
                            if let Err(error) = relay
                                .add_agent(
                                    AdapterHost::new(adapter, None),
                                    name,
                                    identity,
                                    launch_command,
                                )
                                .await
                            {
                                let _ = sender.send(Ok(AgentEvent::RosterUpdated {
                                    update: codeswarm_adapters::RosterUpdate::Rejected {
                                        action: "add agent".into(),
                                        detail: error.to_string(),
                                    },
                                }));
                            }
                        }
                        Err(error) => {
                            let _ = sender.send(Ok(AgentEvent::RosterUpdated {
                                update: codeswarm_adapters::RosterUpdate::Rejected {
                                    action: "add agent".into(),
                                    detail: error.to_string(),
                                },
                            }));
                        }
                    }
                }
                Some(AdapterControl::Cancel) => {
                    let _ = sender.send(Err(AdapterError::Unsupported("no active relay turn")));
                }
                Some(AdapterControl::Stop) | None => break,
            }
        }
        let _ = relay.stop().await;
    });
}

/// Bound the work performed between terminal frames. Adapter output is
/// unbounded, so draining until the channel is empty can otherwise starve
/// rendering and keyboard input indefinitely under a sustained stream.
const MAX_EVENTS_PER_FRAME: usize = 128;

fn next_event_batch<T>(events: &Receiver<T>) -> Vec<T> {
    events.try_iter().take(MAX_EVENTS_PER_FRAME).collect()
}

fn should_apply_configured_models(event: &AgentEvent) -> bool {
    matches!(
        event,
        AgentEvent::Ready { capabilities, .. } if capabilities.supports_models
    )
}

fn run_terminal(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    app: &mut App,
    events: Option<Receiver<AdapterResult<AgentEvent>>>,
    controls: Option<tokio::sync::mpsc::UnboundedSender<AdapterControl>>,
    selected_slot: Option<usize>,
) -> std::io::Result<()> {
    load_ui_preferences(app);
    load_config_agents(app);
    if let Ok(root) = std::env::current_dir() {
        app.set_workspace_root(root);
    }
    // Prompt history belongs to the project that opened this conversation.
    // Keep the root captured at session start so prompt history never leaks
    // between unrelated projects.
    let history_project_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    app.load_prompt_history(load_prompt_history(&history_project_root));
    let completion_candidates = [
        "/cancel", "/clear", "/close", "/config", "/export", "/help", "/reload", "/select", "/to",
    ]
    .into_iter()
    .map(String::from)
    .collect::<Vec<_>>();
    app.set_prompt_completions(completion_candidates);
    let mut selected_slot = selected_slot;
    let event_log = event_log().ok();
    let mut pending_permission: Option<(usize, String)> = None;
    let mut mode_catalog_slots = std::collections::BTreeSet::new();
    let mut mode_capable_slots = std::collections::BTreeSet::new();
    let mut mode_sync_in_flight: Option<String> = None;
    let mut pending_first: Option<String> = None;
    let mut config_reconcile_pending = false;
    let mut config_input = ConfigInputDecoder::new(if terminal_capture_enabled() {
        Duration::from_millis(150)
    } else {
        Duration::from_millis(650)
    });
    let mut selection_until: Option<Instant> = None;
    let mut turn_active = false;
    let mut cancel_requested_at: Option<Instant> = None;
    let mut title_blink_at = Instant::now();
    let mut last_terminal_title = String::new();
    let manage_terminal_title = terminal_capture_enabled();
    loop {
        if app.config_visible() && config_input.take_expired_escape(Instant::now()) {
            let _ = app.handle_config_key(ConfigKey::Cancel);
            continue;
        }
        if !app.config_visible() {
            config_input.reset();
        }
        if restore_mouse_after_selection_window(
            terminal.backend_mut(),
            &mut selection_until,
            Instant::now(),
        )? {
            app.set_mouse_selection_mode(false);
            app.status = "mouse scrolling restored".into();
        }
        selected_slot = normalize_selected_slot(app, selected_slot);
        app.set_selected_agent(selected_slot);
        if let Some(events) = &events {
            for event in next_event_batch(events) {
                match event {
                    Ok(event) => {
                        match &event {
                            AgentEvent::RosterUpdated { update } => match update {
                                codeswarm_adapters::RosterUpdate::Added {
                                    slot, identity, ..
                                } if pending_first
                                    .as_ref()
                                    .is_some_and(|first| first.eq_ignore_ascii_case(identity)) =>
                                {
                                    let first_live =
                                        app.active_roster_slots().first().copied().unwrap_or(0);
                                    if let Some(controls) = &controls
                                        && controls
                                            .send(AdapterControl::Swap(first_live, *slot))
                                            .is_err()
                                    {
                                        app.status =
                                            "new first agent started but reorder could not be queued"
                                                .into();
                                    }
                                }
                                codeswarm_adapters::RosterUpdate::Swapped { first, second } => {
                                    pending_first = None;
                                    if selected_slot == Some(*first) {
                                        selected_slot = Some(*second);
                                    } else if selected_slot == Some(*second) {
                                        selected_slot = Some(*first);
                                    }
                                }
                                codeswarm_adapters::RosterUpdate::Rejected { .. } => {
                                    config_reconcile_pending = false;
                                    pending_first = None;
                                }
                                _ => {}
                            },
                            AgentEvent::TurnStarted { .. }
                            | AgentEvent::Text { .. }
                            | AgentEvent::Thought { .. }
                            | AgentEvent::Tool { .. }
                            | AgentEvent::Permission { .. }
                            | AgentEvent::Terminal { .. }
                            | AgentEvent::UserText { .. } => turn_active = true,
                            AgentEvent::TurnComplete { .. } => {
                                turn_active = false;
                                cancel_requested_at = None;
                            }
                            AgentEvent::Ready { slot, capabilities } => {
                                if capabilities.supports_modes {
                                    mode_capable_slots.insert(*slot);
                                } else {
                                    mode_capable_slots.remove(slot);
                                }
                            }
                            AgentEvent::Failed { slot, .. } => {
                                mode_catalog_slots.remove(slot);
                                mode_capable_slots.remove(slot);
                            }
                            AgentEvent::ModesReplaced { slot, .. } => {
                                mode_catalog_slots.insert(*slot);
                            }
                            AgentEvent::ModeUpdated { .. }
                            | AgentEvent::ModelsReplaced { .. }
                            | AgentEvent::ModelUpdated { .. }
                            | AgentEvent::CommandsReplaced { .. }
                            | AgentEvent::UsageUpdated { .. } => {}
                        }
                        if let AgentEvent::Permission { slot, request } = &event {
                            pending_permission = Some((*slot, request.id.clone()));
                            app.terminal_alert(true);
                        }
                        if matches!(
                            &event,
                            AgentEvent::TurnComplete { .. } | AgentEvent::Failed { .. }
                        ) {
                            pending_permission = None;
                            app.clear_terminal_alerts();
                        }
                        if let Some(log) = &event_log {
                            let _ = log.append(&event);
                            // Checkpoint only at turn boundaries. Streamed
                            // chunks stay off the terminal thread's fsync
                            // path while still making completed turns
                            // recoverable after an abrupt process exit.
                            if matches!(&event, AgentEvent::TurnComplete { .. }) {
                                let _ = log.flush();
                            }
                        }
                        app.apply_event(&event);
                        if should_apply_configured_models(&event)
                            && let Some(controls) = &controls
                        {
                            for (slot, model) in app.take_config_model_changes() {
                                let _ = controls.send(AdapterControl::SetModel { slot, model });
                            }
                        }
                        if matches!(
                            &event,
                            AgentEvent::ModesReplaced { .. } | AgentEvent::ModeUpdated { .. }
                        ) {
                            let active_slots = app
                                .active_roster_slots()
                                .into_iter()
                                .filter(|slot| mode_capable_slots.contains(slot))
                                .collect::<Vec<_>>();
                            let desired = app.mode_policy().map(canonical_mode_policy);
                            let current = app.current_mode_policy();
                            if current == desired {
                                mode_sync_in_flight = None;
                            } else if !active_slots.is_empty()
                                && active_slots
                                    .iter()
                                    .all(|active| mode_catalog_slots.contains(active))
                                && let Some(policy) = desired
                                && mode_sync_in_flight.as_deref() != Some(policy.as_str())
                                && let Some(controls) = &controls
                                && controls
                                    .send(AdapterControl::SetMode(policy.clone()))
                                    .is_ok()
                            {
                                mode_sync_in_flight = Some(policy);
                            }
                        }
                        if matches!(&event, AgentEvent::RosterUpdated { .. })
                            && config_reconcile_pending
                            && let Some(controls) = &controls
                        {
                            match reconcile_config_roster(app, controls, &mut pending_first) {
                                Ok(true) => {
                                    config_reconcile_pending = false;
                                    let roster = app.config_roster_slots();
                                    match save_roster_slots(&roster) {
                                        Ok(()) => {
                                            app.mark_config_roster_saved();
                                            app.status = "roster saved".into();
                                        }
                                        Err(error) => {
                                            app.status = format!("unable to save roster: {error}");
                                        }
                                    }
                                }
                                Ok(false) => {}
                                Err(error) => {
                                    config_reconcile_pending = false;
                                    app.status = format!("unable to apply roster: {error}");
                                }
                            }
                        }
                        if matches!(&event, AgentEvent::Permission { .. })
                            && app.should_notify_system()
                        {
                            notify_permission_request(&app.active_agent);
                            if app.sounds_enabled() {
                                let _ = stdout().write_all(b"\x07");
                                let _ = stdout().flush();
                            }
                        }
                        if matches!(&event, AgentEvent::TurnComplete { .. })
                            && app.should_notify_system()
                        {
                            // Python's turn-over notification deliberately
                            // has no audio attachment.  Keep the terminal
                            // BEL reserved for permission requests, whose
                            // bundled `question.wav` is replaced by this
                            // lightweight tmux-safe signal.
                            notify_turn_complete(&app.active_agent);
                        }
                        if matches!(&event, AgentEvent::TurnComplete { .. })
                            && let Some(queued) = app.next_queued_prompt().cloned()
                            && dispatch_queued_prompt(controls.as_ref(), &queued)
                        {
                            app.remove_queued_prompt(queued.id);
                            turn_active = true;
                            app.status = "queued prompt dispatched".into();
                        }
                    }
                    Err(error) => {
                        let error_text = error.to_string();
                        if let Some(log) = &event_log {
                            let _ = log.flush();
                        }
                        turn_active = false;
                        cancel_requested_at = None;
                        pending_permission = None;
                        app.clear_terminal_alerts();
                        mode_sync_in_flight = None;
                        let cancelled = finish_pending_cancellation(app, &error_text);
                        if !cancelled && app.failed_agent().is_none() {
                            let active_agent = app.active_agent.clone();
                            app.set_header(active_agent, format!("error: {error_text}"));
                        }
                    }
                }
            }
        }
        if app.terminal_alert_active() && app.blink_title_enabled() {
            if title_blink_at.elapsed() >= Duration::from_millis(500) {
                app.toggle_terminal_title_blink();
                title_blink_at = Instant::now();
            }
        } else if app.terminal_title_blink() {
            app.toggle_terminal_title_blink();
            title_blink_at = Instant::now();
        }
        if manage_terminal_title {
            let terminal_title = app.terminal_title();
            if terminal_title != last_terminal_title {
                execute!(terminal.backend_mut(), SetTitle(terminal_title.as_str()))?;
                last_terminal_title = terminal_title;
            }
        }
        if app.take_full_repaint_request() {
            terminal.clear()?;
        }
        let frame_area = terminal.draw(|frame| render(frame, app))?.area;
        if !event::poll(Duration::from_millis(50))? {
            continue;
        }
        match event::read()? {
            Event::FocusGained => {
                app.set_terminal_focused(true);
                continue;
            }
            Event::FocusLost => {
                app.set_terminal_focused(false);
                continue;
            }
            Event::Mouse(mouse) if mouse_scroll_delta(mouse.kind).is_some() => {
                let _ = apply_mouse_scroll(
                    app,
                    mouse.kind,
                    frame_area.width as usize,
                    app.content_height(frame_area.height as usize),
                );
                continue;
            }
            Event::Mouse(_) if app.path_picker_visible() => {
                app.dismiss_path_picker();
                continue;
            }
            Event::Mouse(mouse)
                if mouse.kind == MouseEventKind::Down(MouseButton::Left)
                    && mouse.row == frame_area.bottom().saturating_sub(1)
                    && mouse.column >= frame_area.x
                    && mouse.column < frame_area.right() =>
            {
                match app.footer_action(mouse.column.saturating_sub(frame_area.x), frame_area.width)
                {
                    FooterAction::SelectAgent(slot) => {
                        selected_slot = Some(slot);
                        app.set_selected_agent(selected_slot);
                        app.status = format!("selected {}", app.agent_name(slot));
                    }
                    FooterAction::OpenCollaboration => {
                        app.open_collaboration_config();
                    }
                    FooterAction::OpenMode => {
                        app.open_mode_config();
                    }
                    FooterAction::Ignored => {}
                }
                continue;
            }
            Event::Key(key) => {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                if app.config_visible() {
                    let config_key = config_input.decode(key, Instant::now());
                    if let Some(config_key) = config_key {
                        if config_key == ConfigKey::Save
                            && app.config_roster_dirty()
                            && let Err(error) = validate_config_roster(app)
                        {
                            app.status = error;
                            continue;
                        }
                        if config_key == ConfigKey::Save
                            && let Err(error) = save_ui_preferences(app)
                        {
                            app.status = format!("unable to save preferences: {error}");
                            continue;
                        }
                        let config_action = app.handle_config_key(config_key);
                        if config_action == ConfigAction::Cancel {
                            continue;
                        }
                        if config_action != ConfigAction::Save {
                            continue;
                        }
                        if app.config_roster_dirty() {
                            let roster = app.config_roster_slots();
                            if controls.is_none() {
                                match save_roster_slots(&roster) {
                                    Ok(()) => {
                                        app.mark_config_roster_saved();
                                        app.status = "roster saved for the next launch".into();
                                    }
                                    Err(error) => {
                                        app.status = format!("unable to save roster: {error}");
                                    }
                                }
                            } else if let Some(controls) = &controls {
                                config_reconcile_pending = true;
                                match reconcile_config_roster(app, controls, &mut pending_first) {
                                    Ok(true) => match save_roster_slots(&roster) {
                                        Ok(()) => {
                                            config_reconcile_pending = false;
                                            app.mark_config_roster_saved();
                                            app.status = "roster saved".into();
                                        }
                                        Err(error) => {
                                            config_reconcile_pending = false;
                                            app.status = format!("unable to save roster: {error}");
                                        }
                                    },
                                    Ok(false) => {
                                        app.status = if turn_active {
                                            "roster changes queued for the turn boundary".into()
                                        } else {
                                            "applying roster changes".into()
                                        }
                                    }
                                    Err(error) => {
                                        config_reconcile_pending = false;
                                        app.status = format!("unable to apply roster: {error}");
                                    }
                                }
                            }
                        }
                        if let Some(mode) = app.take_requested_mode()
                            && let Some(controls) = &controls
                        {
                            let _ = controls.send(AdapterControl::SetMode(mode));
                        }
                        if app.take_config_collaboration_changed()
                            && let Some(controls) = &controls
                        {
                            let _ = controls.send(AdapterControl::SetStrategy(
                                collaboration_strategy(app.collaboration()),
                            ));
                        }
                        if let Some(controls) = &controls {
                            for (slot, model) in app.take_config_model_changes() {
                                let _ = controls.send(AdapterControl::SetModel { slot, model });
                            }
                        }
                    }
                    continue;
                }
                let size = terminal.size()?;
                let interaction_height = interaction_height(frame_area);
                match key.code {
                    KeyCode::Char('q') if controls.is_none() && app.prompt.is_empty() => {
                        if let Some(controls) = &controls {
                            let _ = controls.send(AdapterControl::Stop);
                        }
                        return Ok(());
                    }
                    KeyCode::Esc if pending_permission.is_none() && app.path_picker_visible() => {
                        let _ = app.handle_path_picker_key(TuiKey::Esc);
                    }
                    KeyCode::Esc if pending_permission.is_none() && app.keyboard_help_visible() => {
                        app.toggle_keyboard_help();
                    }
                    KeyCode::Esc if pending_permission.is_none() => {
                        app.status.clear();
                    }
                    KeyCode::Esc if pending_permission.is_some() => {
                        let action = app.handle_permission_key(PermissionKey::Cancel);
                        if dispatch_permission_action(controls.as_ref(), action) {
                            app.clear_terminal_alerts();
                            pending_permission = None;
                        }
                    }
                    KeyCode::Up if pending_permission.is_some() => {
                        let _ = app.handle_permission_key(PermissionKey::Up);
                    }
                    KeyCode::Down if pending_permission.is_some() => {
                        let _ = app.handle_permission_key(PermissionKey::Down);
                    }
                    KeyCode::Enter if pending_permission.is_some() => {
                        let action = app.handle_permission_key(PermissionKey::Confirm);
                        if dispatch_permission_action(controls.as_ref(), action) {
                            app.clear_terminal_alerts();
                            pending_permission = None;
                        }
                    }
                    KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        if app.cancel_selected_queued().is_some() {
                            app.status = "queued prompt cancelled".into();
                        } else {
                            app.status = "queue empty".into();
                        }
                    }
                    KeyCode::Char('o') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        match app.toggle_focused_detail() {
                            Some(true) => app.status = "detail closed".into(),
                            Some(false) => app.status = "detail opened".into(),
                            None => app.status = "no detail to open".into(),
                        }
                    }
                    KeyCode::Up if app.path_picker_visible() => {
                        let _ = app.handle_path_picker_key(TuiKey::Up);
                    }
                    KeyCode::Down if app.path_picker_visible() => {
                        let _ = app.handle_path_picker_key(TuiKey::Down);
                    }
                    KeyCode::Enter if app.path_picker_visible() => {
                        let _ = app.handle_path_picker_key(TuiKey::Enter);
                    }
                    KeyCode::Up if key.modifiers.contains(KeyModifiers::ALT) => {
                        if app.move_queue_selection(-1).is_some() {
                            app.status = "selected previous queued prompt".into();
                        }
                    }
                    KeyCode::Down if key.modifiers.contains(KeyModifiers::ALT) => {
                        if app.move_queue_selection(1).is_some() {
                            app.status = "selected next queued prompt".into();
                        }
                    }
                    KeyCode::PageUp => app.scroll_by(
                        -(interaction_height.saturating_sub(1) as isize),
                        size.width as usize,
                        app.content_height(interaction_height),
                    ),
                    KeyCode::PageDown => app.scroll_by(
                        interaction_height.saturating_sub(1) as isize,
                        size.width as usize,
                        app.content_height(interaction_height),
                    ),
                    KeyCode::Down if key.modifiers.contains(KeyModifiers::CONTROL) => app
                        .scroll_by(
                            3,
                            size.width as usize,
                            app.content_height(interaction_height),
                        ),
                    KeyCode::Up if key.modifiers.contains(KeyModifiers::CONTROL) => app.scroll_by(
                        -3,
                        size.width as usize,
                        app.content_height(interaction_height),
                    ),
                    KeyCode::Down if app.prompt.is_empty() => {
                        let _ = apply_navigation_scroll(
                            app,
                            KeyCode::Down,
                            size.width as usize,
                            app.content_height(interaction_height),
                        );
                    }
                    KeyCode::Up if app.prompt.is_empty() => {
                        let _ = apply_navigation_scroll(
                            app,
                            KeyCode::Up,
                            size.width as usize,
                            app.content_height(interaction_height),
                        );
                    }
                    KeyCode::Down => {
                        if matches!(
                            app.handle_prompt_input(Input::from(key)),
                            PromptAction::Ignored
                        ) {
                            app.scroll_by(
                                1,
                                size.width as usize,
                                app.content_height(interaction_height),
                            );
                        }
                    }
                    KeyCode::Up => {
                        if matches!(
                            app.handle_prompt_input(Input::from(key)),
                            PromptAction::Ignored
                        ) {
                            app.scroll_by(
                                -1,
                                size.width as usize,
                                app.content_height(interaction_height),
                            );
                        }
                    }
                    KeyCode::End => {
                        app.follow_tail(size.width as usize, app.content_height(interaction_height))
                    }
                    KeyCode::Tab => {
                        let completion_token = app.prompt.split_whitespace().last().unwrap_or("");
                        if (completion_token.starts_with('/') || completion_token.starts_with('@'))
                            && let PromptAction::Completion { index, total, .. } =
                                app.handle_prompt_input(Input::from(key))
                        {
                            app.status = format!("command completion {}/{}", index + 1, total);
                        }
                    }
                    KeyCode::Char('?') if app.prompt.is_empty() => {
                        let visible = app.toggle_keyboard_help();
                        app.status = if visible {
                            "keyboard help shown".into()
                        } else {
                            "keyboard help hidden".into()
                        };
                    }
                    KeyCode::F(1) => {
                        let visible = app.toggle_keyboard_help();
                        app.status = if visible {
                            "keyboard help shown".into()
                        } else {
                            "keyboard help hidden".into()
                        };
                    }
                    KeyCode::Char(character)
                        if selected_slot.is_some()
                            && character.is_ascii_digit()
                            && key.modifiers.contains(KeyModifiers::ALT) =>
                    {
                        let slot = character.to_digit(10).unwrap_or_default() as usize;
                        if slot > 0 {
                            selected_slot = Some(slot - 1);
                            app.status = format!("selected agent {}", slot - 1);
                        }
                    }
                    KeyCode::Enter
                        if selected_slot.is_some()
                            && key.modifiers.contains(KeyModifiers::CONTROL)
                            && !app.prompt.trim().is_empty() =>
                    {
                        if let Some(controls) = &controls {
                            let prompt = app.prompt.clone();
                            let slot = selected_slot.expect("guarded selected slot");
                            app.record_human_message(&prompt, true);
                            if turn_active {
                                if app.queue_prompt(prompt, Some(slot), true).is_some() {
                                    let _ = app.take_prompt();
                                    consume_one_shot_route(app, &mut selected_slot);
                                    app.status = "direct prompt queued".into();
                                } else {
                                    app.status = "queue full or prompt empty".into();
                                }
                            } else if controls
                                .send(AdapterControl::Direct {
                                    slot,
                                    prompt: app.take_prompt(),
                                })
                                .is_ok()
                            {
                                turn_active = true;
                                consume_one_shot_route(app, &mut selected_slot);
                                app.status = "direct turn queued".into();
                            }
                        }
                    }
                    KeyCode::Enter => {
                        if let PromptAction::Submit(prompt) =
                            app.handle_prompt_input(Input::from(key))
                        {
                            append_prompt_history(&prompt, &history_project_root);
                            if let Some(command) = prompt.trim().strip_prefix('!') {
                                let _ = command;
                                app.status = "local shell commands are not supported".into();
                            } else if let Some(local) = app.handle_local_command(&prompt) {
                                match local {
                                    LocalCommand::Handled => {}
                                    LocalCommand::Close => {
                                        if let Some(controls) = &controls {
                                            let _ = controls.send(AdapterControl::Stop);
                                        }
                                        return Ok(());
                                    }
                                    LocalCommand::Cancel => {
                                        if turn_active {
                                            app.request_turn_cancellation();
                                            if let Some(controls) = &controls {
                                                let _ = controls.send(AdapterControl::Cancel);
                                            }
                                            app.status = "cancelling".into();
                                        } else {
                                            app.status = "nothing to cancel".into();
                                        }
                                    }
                                    LocalCommand::Reload => {
                                        if let Some(slot) = app.failed_agent()
                                            && let Some(controls) = &controls
                                        {
                                            let _ = controls.send(AdapterControl::Reload(slot));
                                        } else {
                                            app.status = "no crashed agent to reload".into();
                                        }
                                    }
                                    LocalCommand::SelectAgent(slot) => {
                                        if app.active_roster_slots().contains(&slot) {
                                            selected_slot = Some(slot);
                                            app.set_selected_agent(selected_slot);
                                            app.status =
                                                format!("next message → {}", app.agent_name(slot));
                                        } else {
                                            app.status = format!("agent {slot} is unavailable");
                                        }
                                    }
                                    LocalCommand::SelectText => {
                                        execute!(terminal.backend_mut(), DisableMouseCapture)?;
                                        selection_until =
                                            Some(Instant::now() + Duration::from_secs(15));
                                        app.set_mouse_selection_mode(true);
                                        app.status = "text selection enabled for 15 seconds".into();
                                    }
                                    LocalCommand::Export => match export_conversation(app) {
                                        Ok(path) => {
                                            app.status = format!(
                                                "conversation exported to {}",
                                                path.display()
                                            )
                                        }
                                        Err(error) => {
                                            app.status = format!("export failed: {error}")
                                        }
                                    },
                                }
                            } else if let Some(controls) = &controls {
                                app.record_human_message(&prompt, false);
                                if turn_active {
                                    if app.queue_prompt(prompt, selected_slot, false).is_some() {
                                        consume_one_shot_route(app, &mut selected_slot);
                                        app.status = "prompt queued".into();
                                    } else {
                                        app.status = "queue full or prompt empty".into();
                                    }
                                } else {
                                    let command = if let Some(slot) = selected_slot {
                                        AdapterControl::Queue { slot, prompt }
                                    } else {
                                        AdapterControl::Prompt(prompt)
                                    };
                                    if controls.send(command).is_ok() {
                                        turn_active = true;
                                        consume_one_shot_route(app, &mut selected_slot);
                                        app.status = "queued".into();
                                    }
                                }
                            }
                        }
                    }
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        if !turn_active {
                            if let Some(controls) = &controls {
                                let _ = controls.send(AdapterControl::Stop);
                            }
                            return Ok(());
                        }
                        if cancel_requested_at
                            .is_some_and(|started| started.elapsed() <= Duration::from_secs(3))
                        {
                            if let Some(controls) = &controls {
                                let _ = controls.send(AdapterControl::Stop);
                            }
                            return Ok(());
                        }
                        if let Some(controls) = &controls {
                            app.request_turn_cancellation();
                            let _ = controls.send(AdapterControl::Cancel);
                        }
                        cancel_requested_at = Some(Instant::now());
                        app.status = "cancelling · press Ctrl+C again to quit".into();
                    }
                    _ => match app.handle_prompt_input(Input::from(key)) {
                        PromptAction::Completion { index, total, .. } => {
                            app.status = format!("command completion {}/{}", index + 1, total);
                        }
                        PromptAction::Changed | PromptAction::Ignored | PromptAction::Submit(_) => {
                        }
                    },
                }
            }
            _ => continue,
        }
    }
}

fn export_conversation(app: &App) -> std::io::Result<PathBuf> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs());
    let mut path = PathBuf::from(format!("codeswarm-conversation-{stamp}.md"));
    let mut suffix = 2;
    while path.exists() {
        path = PathBuf::from(format!("codeswarm-conversation-{stamp}-{suffix}.md"));
        suffix += 1;
    }
    std::fs::write(&path, app.export_markdown())?;
    Ok(path)
}

fn state_directory() -> PathBuf {
    let root = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .unwrap_or_else(|| PathBuf::from(".codeswarm-state"));
    root.join("codeswarm")
}

fn project_path_key(project_root: &Path) -> String {
    let canonical = project_root
        .canonicalize()
        .unwrap_or_else(|_| project_root.to_path_buf());
    let label = canonical
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or("project")
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '-'
            }
        })
        .take(32)
        .collect::<String>();
    #[cfg(unix)]
    let bytes = {
        use std::os::unix::ffi::OsStrExt;
        canonical.as_os_str().as_bytes().to_vec()
    };
    #[cfg(not(unix))]
    let bytes = canonical.to_string_lossy().as_bytes().to_vec();
    let digest = Sha256::digest(&bytes);
    let hash = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("{label}-{}", &hash[..16])
}

fn session_metadata_path_for(project_root: &Path) -> PathBuf {
    state_directory()
        .join("sessions")
        .join(project_path_key(project_root))
        .join("session.json")
}

fn legacy_project_path_key(project_root: &Path) -> String {
    project_root
        .canonicalize()
        .unwrap_or_else(|_| project_root.to_path_buf())
        .to_string_lossy()
        .trim_start_matches('/')
        .replace('/', "-")
}

fn session_metadata_candidates(project_root: &Path) -> Vec<PathBuf> {
    vec![session_metadata_path_for(project_root)]
}

fn load_agent_session_id(cwd: &Path, identity: &str) -> Option<String> {
    session_metadata_candidates(cwd)
        .into_iter()
        .find_map(|path| load_agent_session_id_from(&path, cwd, identity))
}

fn load_agent_session_id_from(metadata_path: &Path, cwd: &Path, identity: &str) -> Option<String> {
    let loaded = SessionMetadataStore::open(metadata_path).read().ok()??;
    let stored_cwd = loaded.get("cwd").and_then(serde_json::Value::as_str)?;
    // A session launched through a symlink still resumes from the real path.
    let stored_cwd = Path::new(stored_cwd)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(stored_cwd));
    let current_cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    if stored_cwd != current_cwd {
        return None;
    }
    loaded
        .get("agents")?
        .as_array()?
        .iter()
        .find(|agent| {
            agent
                .get("identity")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|saved| saved.eq_ignore_ascii_case(identity))
                && agent
                    .get("supports_load_session")
                    .and_then(serde_json::Value::as_bool)
                    == Some(true)
        })?
        .get("session_id")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
}

fn event_log() -> std::io::Result<BufferedEventLog> {
    let directory = state_directory();
    std::fs::create_dir_all(&directory)?;
    EventLog::open(directory.join("rust-events.jsonl")).buffered()
}

fn project_prompt_history_path(data_home: &Path, project_root: &Path) -> PathBuf {
    // Match the Python client's `paths.path_to_name`: an absolute project
    // path becomes one stable, filesystem-safe component.  This avoids a
    // global prompt history leaking commands between unrelated repositories.
    data_home
        .join("codeswarm")
        .join(project_path_key(project_root))
        .join("prompt_history.jsonl")
}

fn prompt_history_path(project_root: &Path) -> Option<PathBuf> {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .map(|root| project_prompt_history_path(&root, project_root))
}

fn legacy_prompt_history_path(project_root: &Path) -> Option<PathBuf> {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .map(|root| {
            root.join("codeswarm")
                .join(legacy_project_path_key(project_root))
                .join("prompt_history.jsonl")
        })
}

fn load_prompt_history(project_root: &Path) -> Vec<String> {
    let Some(current) = prompt_history_path(project_root) else {
        return Vec::new();
    };
    let Some(legacy) = legacy_prompt_history_path(project_root) else {
        return history::read(current).unwrap_or_default();
    };
    load_prompt_history_from(&current, &legacy)
}

fn load_prompt_history_from(current: &Path, legacy: &Path) -> Vec<String> {
    if current.exists() {
        return history::read(current).unwrap_or_default();
    }
    let entries = history::read(legacy).unwrap_or_default();
    for entry in &entries {
        if history::append(current, entry).is_err() {
            break;
        }
    }
    entries
}

fn append_prompt_history(prompt: &str, project_root: &Path) {
    let Some(path) = prompt_history_path(project_root) else {
        return;
    };
    let _ = history::append(path, prompt);
}

fn notify_turn_complete(agent: &str) {
    let agent = agent.to_owned();
    thread::spawn(move || {
        #[cfg(target_os = "linux")]
        {
            let _ = std::process::Command::new("notify-send")
                .args(["CodeSwarm", &format!("{agent} finished a turn")])
                .status();
        }
        #[cfg(target_os = "macos")]
        {
            let message = format!("{agent} finished a turn");
            let _ = std::process::Command::new("osascript")
                .args([
                    "-e",
                    "on run argv\ndisplay notification (item 1 of argv) with title \"CodeSwarm\"\nend run",
                    "--",
                    &message,
                ])
                .status();
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = agent;
        }
    });
}

/// Surface an agent permission request outside the TUI when notifications are
/// enabled.  The Python client uses its bundled `question.wav`; a terminal
/// bell is emitted by the event loop alongside this lightweight OS message so
/// the Rust client has the same useful signal without shipping a media
/// runtime or blocking the render thread.
fn notify_permission_request(agent: &str) {
    let agent = agent.to_owned();
    thread::spawn(move || {
        let message = format!("{agent} is waiting for permission");
        #[cfg(target_os = "linux")]
        {
            let _ = std::process::Command::new("notify-send")
                .args(["CodeSwarm", &message])
                .status();
        }
        #[cfg(target_os = "macos")]
        {
            let _ = std::process::Command::new("osascript")
                .args([
                    "-e",
                    "on run argv\ndisplay notification (item 1 of argv) with title \"CodeSwarm\"\nend run",
                    "--",
                    &message,
                ])
                .status();
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = message;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::{
        AdapterControl, AgentSpec, ConfigInputDecoder, Launch, MAX_EVENTS_PER_FRAME,
        apply_mouse_scroll, apply_navigation_scroll, apply_notification_preferences,
        bare_launch_from_settings, cancel_standalone_turn, consume_one_shot_route,
        dispatch_permission_action, dispatch_queued_prompt, finish_pending_cancellation,
        interaction_height, load_prompt_history_from, load_session_metadata_candidates,
        mouse_scroll_delta, next_event_batch, normalize_arguments, normalize_selected_slot,
        parse_launch, prepare_launch_arguments, program_available, project_dir_argument,
        project_prompt_history_path, reconcile_config_roster, restore_mouse_after_selection_window,
        resume_launch_from_metadata, run_relay_sequence_with_controls, sanitize_direct_event,
        session_metadata_path_for, should_apply_configured_models, standalone_session_metadata,
        terminal_capture_enabled_for, validate_project_directory,
    };
    use async_trait::async_trait;
    use codeswarm::tui::{App, ConfigKey, PermissionAction, QueuedPrompt, StoreAgent};
    use codeswarm_adapters::persistence::{SessionMetadata, SessionMetadataStore};
    use codeswarm_adapters::{
        AdapterHost, AdapterResult, AgentAdapter, RelayHost, ScriptedAdapter,
    };
    use codeswarm_adapters::{AgentCapabilities, AgentEvent, PermissionAnswer};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEventKind};
    use std::ffi::OsStr;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    #[test]
    fn event_batches_leave_excess_adapter_updates_for_the_next_frame() {
        let (sender, receiver) = std::sync::mpsc::channel();
        for value in 0..=MAX_EVENTS_PER_FRAME {
            sender.send(value).expect("queue event");
        }

        let batch = next_event_batch(&receiver);
        assert_eq!(batch.len(), MAX_EVENTS_PER_FRAME);
        assert_eq!(batch.first(), Some(&0));
        assert_eq!(batch.last(), Some(&(MAX_EVENTS_PER_FRAME - 1)));
        assert_eq!(receiver.try_recv(), Ok(MAX_EVENTS_PER_FRAME));
    }

    #[test]
    fn model_catalog_replacement_does_not_reapply_configuration() {
        assert!(should_apply_configured_models(&AgentEvent::Ready {
            slot: 1,
            capabilities: AgentCapabilities {
                supports_models: true,
                ..AgentCapabilities::default()
            },
        }));
        assert!(!should_apply_configured_models(
            &AgentEvent::ModelsReplaced {
                slot: 1,
                config_id: "model".into(),
                models: Vec::new(),
                current_model: None,
            }
        ));
        assert!(!should_apply_configured_models(&AgentEvent::Ready {
            slot: 1,
            capabilities: AgentCapabilities::default(),
        }));
    }

    #[derive(Debug)]
    struct YieldingAdapter {
        slot: usize,
        yielded: bool,
        events: std::collections::VecDeque<AgentEvent>,
    }

    #[async_trait]
    impl AgentAdapter for YieldingAdapter {
        fn slot(&self) -> usize {
            self.slot
        }

        fn capabilities(&self) -> AgentCapabilities {
            AgentCapabilities::default()
        }

        async fn start(&mut self) -> AdapterResult<()> {
            Ok(())
        }

        async fn send_prompt(&mut self, _prompt: String) -> AdapterResult<()> {
            Ok(())
        }

        async fn cancel(&mut self) -> AdapterResult<bool> {
            Ok(true)
        }

        async fn answer_permission(
            &mut self,
            _request_id: String,
            _answer: PermissionAnswer,
        ) -> AdapterResult<()> {
            Ok(())
        }

        async fn set_mode(&mut self, _mode: String) -> AdapterResult<()> {
            Ok(())
        }

        async fn reload(&mut self) -> AdapterResult<()> {
            Ok(())
        }

        async fn stop(&mut self) -> AdapterResult<()> {
            Ok(())
        }

        async fn next_event(&mut self) -> Option<AdapterResult<AgentEvent>> {
            if !self.yielded {
                self.yielded = true;
                tokio::task::yield_now().await;
            }
            self.events.pop_front().map(Ok)
        }
    }

    #[test]
    fn terminal_capture_stays_disabled_for_multiplexers_when_tmux_is_stripped() {
        assert!(!terminal_capture_enabled_for(
            None,
            Some(OsStr::new("screen-256color")),
            None,
        ));
        assert!(!terminal_capture_enabled_for(
            None,
            Some(OsStr::new("xterm-256color")),
            Some(OsStr::new("tmux")),
        ));
        assert!(!terminal_capture_enabled_for(
            Some(OsStr::new("/tmp/tmux/default,1,0")),
            Some(OsStr::new("xterm-256color")),
            None,
        ));
        assert!(terminal_capture_enabled_for(
            None,
            Some(OsStr::new("xterm-256color")),
            None,
        ));
    }

    #[test]
    fn mouse_wheel_maps_to_bounded_transcript_steps() {
        assert_eq!(mouse_scroll_delta(MouseEventKind::ScrollUp), Some(-3));
        assert_eq!(mouse_scroll_delta(MouseEventKind::ScrollDown), Some(3));
        assert_eq!(mouse_scroll_delta(MouseEventKind::Moved), None);
    }

    #[test]
    fn mouse_capture_enables_wheel_and_footer_click_events() {
        let mut output = Vec::new();
        crossterm::execute!(&mut output, crossterm::event::EnableMouseCapture)
            .expect("enable mouse capture");
        let enabled = String::from_utf8(output.clone()).expect("terminal sequence");
        assert!(enabled.contains("?1000h"), "sequence={enabled:?}");

        output.clear();
        crossterm::execute!(&mut output, crossterm::event::DisableMouseCapture)
            .expect("disable mouse capture");
        let disabled = String::from_utf8(output).expect("terminal sequence");
        assert!(disabled.contains("?1000l"), "sequence={disabled:?}");
    }

    #[test]
    fn selection_window_restores_mouse_capture_automatically() {
        let now = Instant::now();
        let mut deadline = Some(now - Duration::from_millis(1));
        let mut output = Vec::new();
        assert!(
            restore_mouse_after_selection_window(&mut output, &mut deadline, now)
                .expect("restore capture")
        );
        assert!(deadline.is_none());
        let sequence = String::from_utf8(output).expect("terminal sequence");
        assert!(sequence.contains("?1000h"), "sequence={sequence:?}");

        let mut future = Some(now + Duration::from_secs(1));
        let mut untouched = Vec::new();
        assert!(
            !restore_mouse_after_selection_window(&mut untouched, &mut future, now)
                .expect("keep selection window")
        );
        assert!(untouched.is_empty());
        assert!(future.is_some());
    }

    #[test]
    fn config_decoder_supports_native_and_escape_prefixed_alt_arrows() {
        let now = Instant::now();
        let mut decoder = ConfigInputDecoder::default();
        assert_eq!(
            decoder.decode(KeyEvent::new(KeyCode::Up, KeyModifiers::ALT), now),
            Some(ConfigKey::MoveUp)
        );
        assert_eq!(
            decoder.decode(KeyEvent::new(KeyCode::Down, KeyModifiers::SHIFT), now),
            Some(ConfigKey::MoveDown)
        );
        assert_eq!(
            decoder.decode(KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE), now),
            Some(ConfigKey::MoveUp)
        );
        assert_eq!(
            decoder.decode(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::NONE), now),
            Some(ConfigKey::MoveDown)
        );
        assert_eq!(
            decoder.decode(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE), now),
            Some(ConfigKey::PreviousValue)
        );
        assert_eq!(
            decoder.decode(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE), now),
            Some(ConfigKey::NextValue)
        );
        assert_eq!(
            decoder.decode(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), now),
            Some(ConfigKey::ToggleSlot)
        );
        let mut decoder = ConfigInputDecoder::new(Duration::from_millis(650));
        assert_eq!(
            decoder.decode(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), now),
            None
        );
        assert_eq!(
            decoder.decode(
                KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
                now + Duration::from_millis(520),
            ),
            Some(ConfigKey::MoveDown)
        );
        assert!(!decoder.take_expired_escape(now + Duration::from_millis(520)));

        assert_eq!(
            decoder.decode(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), now),
            None
        );
        assert!(decoder.take_expired_escape(now + Duration::from_millis(700)));
    }

    #[test]
    fn wheel_event_moves_the_real_transcript_viewport() {
        let mut app = App::default();
        app.transcript.append(
            codeswarm::transcript::BlockKind::Agent,
            (0..300)
                .map(|index| format!("word{index}"))
                .collect::<Vec<_>>()
                .join(" "),
            false,
        );
        app.follow_tail(40, 4);
        let tail = app.scroll_y;
        assert!(tail > 0);
        assert!(apply_mouse_scroll(
            &mut app,
            MouseEventKind::ScrollUp,
            40,
            4
        ));
        assert_eq!(app.scroll_y, tail - 3);
        assert!(!app.follow_tail);
        let scrolled = app.scroll_y;
        assert!(!apply_mouse_scroll(&mut app, MouseEventKind::Moved, 40, 4));
        assert_eq!(app.scroll_y, scrolled);
    }

    #[test]
    fn arrow_fallback_moves_the_real_transcript_viewport() {
        let mut app = App::default();
        app.transcript.append(
            codeswarm::transcript::BlockKind::Agent,
            (0..300)
                .map(|index| format!("word{index}"))
                .collect::<Vec<_>>()
                .join(" "),
            false,
        );
        app.follow_tail(40, 4);
        let tail = app.scroll_y;
        assert!(apply_navigation_scroll(&mut app, KeyCode::Up, 40, 4));
        assert_eq!(app.scroll_y, tail - 1);
        assert!(!app.follow_tail);
        assert!(!apply_navigation_scroll(
            &mut app,
            KeyCode::Char('x'),
            40,
            4
        ));
        assert_eq!(app.scroll_y, tail - 1);
    }

    #[cfg(unix)]
    #[test]
    fn executable_detection_does_not_run_or_print_the_detected_program() {
        use std::os::unix::fs::PermissionsExt;

        let root =
            std::env::temp_dir().join(format!("codeswarm-detection-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("test path");
        let marker = root.join("was-run");
        let executable = root.join("noisy-agent");
        std::fs::write(
            &executable,
            format!(
                "#!/bin/sh\necho /a/leaked/path\ntouch {}\n",
                marker.display()
            ),
        )
        .expect("fake executable");
        let mut permissions = std::fs::metadata(&executable)
            .expect("metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&executable, permissions).expect("executable permissions");

        assert!(program_available("noisy-agent", Some(root.as_os_str())));
        assert!(
            !marker.exists(),
            "availability detection executed the agent"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn full_screen_interaction_uses_the_complete_frame_height() {
        assert_eq!(
            interaction_height(ratatui::layout::Rect::new(0, 0, 120, 48)),
            48
        );
    }

    #[test]
    fn dropped_routing_target_falls_back_and_roster_override_is_one_shot() {
        let mut app = codeswarm::tui::App::default();
        app.set_agent_name(0, "Owner");
        app.set_agent_name(1, "Peer");
        app.mark_agent_dropped(1);
        assert_eq!(normalize_selected_slot(&app, Some(1)), Some(0));

        let mut selected = Some(0);
        consume_one_shot_route(&app, &mut selected);
        assert_eq!(selected, None);
        app.set_collaboration("Manual routing");
        selected = Some(0);
        consume_one_shot_route(&app, &mut selected);
        assert_eq!(selected, Some(0));
    }

    #[test]
    fn resume_launch_requires_matching_workspace_and_a_loadable_agent_session() {
        let root =
            std::env::temp_dir().join(format!("codeswarm-resume-test-{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("create workspace");
        let mut data = serde_json::Map::new();
        data.insert("cwd".into(), serde_json::json!(root));
        data.insert(
            "agents".into(),
            serde_json::json!([{
                "name": "Codex", "identity": "openai.com", "protocol": "acp",
                "command": "codex-acp", "supports_load_session": true,
                "session_id": "session-1"
            }]),
        );
        let metadata = SessionMetadata::new(data);
        assert!(matches!(
            resume_launch_from_metadata(&metadata, &root, "{}"),
            Ok(Launch::Roster { specs, prompt: None, first_slot: 0, .. })
                if specs.len() == 1
        ));

        let other = root.join("other");
        std::fs::create_dir_all(&other).expect("create other workspace");
        assert_ne!(
            session_metadata_path_for(&root),
            session_metadata_path_for(&other)
        );
        let collision_left = root.join("a-b/c");
        let collision_right = root.join("a/b-c");
        std::fs::create_dir_all(&collision_left).expect("create left collision path");
        std::fs::create_dir_all(&collision_right).expect("create right collision path");
        assert_ne!(
            session_metadata_path_for(&collision_left),
            session_metadata_path_for(&collision_right)
        );
        assert!(resume_launch_from_metadata(&metadata, &other, "{}").is_err());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn resume_restores_each_agent_handle_and_rejects_the_removed_owner_schema() {
        let root = std::env::temp_dir().join(format!(
            "codeswarm-multi-agent-resume-test-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).expect("create workspace");
        let current = SessionMetadata::new(
            serde_json::json!({
                "cwd": root.display().to_string(),
                "agents": [
                    {"name": "One", "identity": "one.example", "protocol": "acp", "command": "one", "supports_load_session": true, "session_id": "one-session"},
                    {"name": "Two", "identity": "two.example", "protocol": "acp", "command": "two", "supports_load_session": true, "session_id": "two-session"}
                ]
            })
            .as_object()
            .expect("metadata")
            .clone(),
        );
        assert!(matches!(
            resume_launch_from_metadata(&current, &root, "{}"),
            Ok(Launch::Roster { session_ids, .. })
                if session_ids == [Some("one-session".into()), Some("two-session".into())]
        ));

        let removed = SessionMetadata::new(
            serde_json::json!({
                "cwd": root.display().to_string(),
                "roster": ["one.example"],
                "owner_session_id": "one-session"
            })
            .as_object()
            .expect("old metadata")
            .clone(),
        );
        assert!(resume_launch_from_metadata(&removed, &root, "{}").is_err());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn parses_native_agent_prompt_without_treating_it_as_acp() {
        assert!(matches!(
            parse_launch(&["--agy".into(), "summarize".into()]),
            Some(Launch::Agy { prompt: Some(prompt) }) if prompt == "summarize"
        ));
    }

    #[test]
    fn accepts_help_era_entry_point_aliases_without_reinterpreting_arguments() {
        assert_eq!(
            normalize_arguments(vec!["run".into(), "/tmp".into()]),
            vec![String::from("/tmp")]
        );
        assert_eq!(
            normalize_arguments(vec!["acp".into(), "codex-acp".into(), "/tmp".into()]),
            vec![
                String::from("--acp"),
                String::from("codex-acp"),
                String::from("--project-dir"),
                String::from("/tmp"),
            ]
        );
    }

    #[test]
    fn run_path_stays_separate_from_named_agent_options_and_prompt() {
        let arguments = prepare_launch_arguments(vec![
            "run".into(),
            "/tmp".into(),
            "--agent".into(),
            "claude".into(),
            "review the patch".into(),
        ]);
        assert!(matches!(
            parse_launch(&arguments),
            Some(Launch::Roster { prompt: Some(prompt), .. }) if prompt == "review the patch"
        ));
        assert_eq!(
            project_dir_argument(&arguments),
            Some(PathBuf::from("/tmp"))
        );
    }

    #[test]
    fn explicit_run_can_take_a_prompt_without_a_workspace_path() {
        let arguments = prepare_launch_arguments(vec!["run".into(), "summarize this".into()]);
        assert_eq!(arguments, vec!["summarize this"]);
    }

    #[test]
    fn unqualified_path_uses_the_python_default_run_contract() {
        let arguments = normalize_arguments(vec!["/tmp".into(), "--agent".into(), "claude".into()]);
        assert_eq!(
            project_dir_argument(&arguments),
            Some(PathBuf::from("/tmp"))
        );
    }

    #[test]
    fn bare_prompt_is_not_mistaken_for_a_project_directory() {
        let arguments = normalize_arguments(vec![
            "summarize this change".into(),
            "--agent".into(),
            "claude".into(),
        ]);
        assert_eq!(project_dir_argument(&arguments), None);
    }

    #[test]
    fn invalid_project_directory_is_rejected_before_terminal_start() {
        let error =
            validate_project_directory(PathBuf::from("/definitely/not/a/project").as_path())
                .expect_err("missing project directory");
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
        assert!(error.to_string().contains("Not a directory"));
    }

    #[test]
    fn prompt_history_is_scoped_to_the_project_data_directory() {
        let path = project_prompt_history_path(
            PathBuf::from("/tmp/codeswarm-data").as_path(),
            PathBuf::from("/workspace/project").as_path(),
        );
        assert_eq!(
            path.file_name().and_then(OsStr::to_str),
            Some("prompt_history.jsonl")
        );
        assert!(
            path.parent()
                .and_then(|parent| parent.file_name())
                .and_then(OsStr::to_str)
                .is_some_and(|name| name.starts_with("project-"))
        );
    }

    #[test]
    fn legacy_prompt_history_is_migrated_before_new_entries_are_appended() {
        let root = std::env::temp_dir().join(format!(
            "codeswarm-history-migration-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let current = root.join("new/prompt_history.jsonl");
        let legacy = root.join("old/prompt_history.jsonl");
        codeswarm_adapters::history::append(&legacy, "first").expect("legacy first");
        codeswarm_adapters::history::append(&legacy, "second").expect("legacy second");
        assert_eq!(
            load_prompt_history_from(&current, &legacy),
            ["first", "second"]
        );
        codeswarm_adapters::history::append(&current, "third").expect("new prompt");
        assert_eq!(
            load_prompt_history_from(&current, &legacy),
            ["first", "second", "third"]
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn malformed_current_metadata_does_not_fall_back_to_stale_legacy_state() {
        let root = std::env::temp_dir().join(format!(
            "codeswarm-metadata-candidates-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("metadata root");
        let current = root.join("current.json");
        let legacy = root.join("legacy.json");
        std::fs::write(&current, "not json").expect("corrupt current");
        SessionMetadataStore::open(&legacy)
            .write(&SessionMetadata::new(serde_json::Map::new()))
            .expect("legacy metadata");
        assert!(load_session_metadata_candidates([current, legacy]).is_err());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn direct_catalog_commands_keep_stable_builtin_identity() {
        assert_eq!(
            super::catalog_identity_for_command("agy"),
            "antigravity.google.com"
        );
        assert_eq!(
            super::catalog_identity_for_command("agy --dangerously-skip-permissions"),
            "antigravity.google.com"
        );
        assert_eq!(
            super::catalog_identity_for_command(
                "npx -y --package=@agentclientprotocol/codex-acp codex-acp",
            ),
            "openai.com"
        );
    }

    #[test]
    fn native_catalog_arguments_keep_the_friendly_display_name() {
        assert_eq!(
            super::display_agent_name("/usr/local/bin/codex --acp"),
            "Codex"
        );
        assert_eq!(
            super::display_agent_name("agy --dangerously-skip-permissions"),
            "Antigravity"
        );
        assert_eq!(
            super::display_agent_name("/opt/company/bin/reviewer-agent --stdio"),
            "reviewer-agent"
        );
        assert_eq!(
            super::display_agent_name("/opt/codex-tools/reviewer-agent --stdio"),
            "reviewer-agent"
        );
    }

    #[test]
    fn direct_session_metadata_uses_the_agent_list_schema() {
        let adapter = ScriptedAdapter::new(
            0,
            AgentCapabilities {
                supports_session_load: true,
                ..AgentCapabilities::default()
            },
            [],
        );
        let metadata = standalone_session_metadata(
            PathBuf::from("/tmp/codeswarm-project").as_path(),
            "Custom agent",
            "custom.example",
            "custom-agent --stdio",
            &adapter,
        );
        assert_eq!(
            metadata.get("agents"),
            Some(&serde_json::json!([{
                "name": "Custom agent", "identity": "custom.example", "protocol": "custom",
                "command": "custom-agent --stdio", "supports_load_session": true
            }]))
        );
        assert!(metadata.get("owner").is_none());
    }

    #[test]
    fn custom_acp_session_resumes_from_its_persisted_launch_spec() {
        let root =
            std::env::temp_dir().join(format!("codeswarm-custom-resume-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("project root");
        let metadata = SessionMetadata::new(
            serde_json::json!({
                "cwd": root.display().to_string(),
                "agents": [{
                    "name": "my-agent", "identity": "my-agent --stdio",
                    "protocol": "acp", "command": "my-agent --stdio",
                    "supports_load_session": true, "session_id": "provider-session"
                }]
            })
            .as_object()
            .expect("metadata object")
            .clone(),
        );
        assert!(matches!(
            resume_launch_from_metadata(&metadata, &root, "{}"),
            Ok(Launch::Roster {
                specs,
                identities,
                session_ids,
                ..
            }) if specs == [AgentSpec::Acp("my-agent --stdio".into())]
                && identities == ["my-agent --stdio"]
                && session_ids == [Some("provider-session".into())]
        ));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn agent_session_restore_matches_identity_and_workspace() {
        let root =
            std::env::temp_dir().join(format!("codeswarm-owner-restore-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("project directory");
        let metadata_path = root.join("session.json");
        let mut data = serde_json::Map::new();
        data.insert(
            "cwd".into(),
            serde_json::Value::String(root.display().to_string()),
        );
        data.insert(
            "agents".into(),
            serde_json::json!([{
                "name": "Codex", "identity": "openai.com", "protocol": "acp",
                "command": "codex-acp", "supports_load_session": true,
                "session_id": "session-42"
            }]),
        );
        codeswarm_adapters::persistence::SessionMetadataStore::open(&metadata_path)
            .write(&codeswarm_adapters::persistence::SessionMetadata::new(data))
            .expect("write metadata");
        assert_eq!(
            super::load_agent_session_id_from(&metadata_path, &root, "openai.com"),
            Some("session-42".into())
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn agent_session_restore_skips_non_resumable_handles() {
        let root = std::env::temp_dir().join(format!(
            "codeswarm-owner-nonresumable-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("project directory");
        let metadata_path = root.join("session.json");
        let mut data = serde_json::Map::new();
        data.insert(
            "cwd".into(),
            serde_json::Value::String(root.display().to_string()),
        );
        data.insert(
            "agents".into(),
            serde_json::json!([{
                "name": "Gemini", "identity": "geminicli.com", "protocol": "acp",
                "command": "gemini", "supports_load_session": false,
                "session_id": "stale-session"
            }]),
        );
        SessionMetadataStore::open(&metadata_path)
            .write(&SessionMetadata::new(data))
            .expect("write metadata");
        assert_eq!(
            super::load_agent_session_id_from(&metadata_path, &root, "geminicli.com"),
            None
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn accepts_a_project_directory_flag_without_routing_it_to_the_agent() {
        assert_eq!(
            project_dir_argument(&["--project-dir".into(), "/tmp".into()]),
            Some(PathBuf::from("/tmp"))
        );
        assert!(matches!(
            parse_launch(&[
                "--project-dir".into(),
                "/tmp".into(),
                "--roster".into(),
                "acp:codex-acp".into(),
                "task".into(),
            ]),
            Some(Launch::Roster { prompt: Some(prompt), .. }) if prompt == "task"
        ));
    }

    #[test]
    fn parses_acp_program_and_prompt() {
        assert!(matches!(
            parse_launch(&["--acp".into(), "codex-acp".into(), "summarize".into()]),
            Some(Launch::Acp { program, prompt: Some(prompt) }) if program == "codex-acp" && prompt == "summarize"
        ));
        assert!(matches!(
            parse_launch(&["--acp".into(), "codex-acp".into()]),
            Some(Launch::Acp { program, prompt: None }) if program == "codex-acp"
        ));
    }

    #[test]
    fn catalog_native_launch_preserves_full_access_startup_argument() {
        let catalog = codeswarm_adapters::agents::default_catalog();
        let antigravity = catalog
            .iter()
            .find(|agent| agent.identity == "antigravity.google.com")
            .expect("antigravity catalog entry");
        assert_eq!(
            super::agent_spec(antigravity),
            AgentSpec::Agy("agy --dangerously-skip-permissions".into())
        );
    }

    #[test]
    fn config_roster_reconciliation_swaps_selected_live_first_agent() {
        let mut app = codeswarm::tui::App::default();
        app.set_agent_name(0, "Claude");
        app.set_agent_name(1, "Codex");
        app.set_agent_identity(0, "anthropic.com");
        app.set_agent_identity(1, "openai.com");
        app.set_config_agents(vec![StoreAgent {
            identity: "openai.com".into(),
            name: "Codex".into(),
            adapter: "ACP".into(),
            command: "codex --acp".into(),
            available: true,
            selected: true,
            model: None,
        }]);
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut pending_first = None;
        assert!(
            !reconcile_config_roster(&mut app, &sender, &mut pending_first).expect("reconcile")
        );
        assert!(matches!(
            receiver.try_recv(),
            Ok(AdapterControl::Swap(0, 1))
        ));
        assert_eq!(app.agent_name(0), "Claude");
        assert_eq!(app.active_roster_slots(), vec![0, 1]);
    }

    #[test]
    fn config_roster_reconciliation_uses_identity_for_duplicate_names() {
        let mut app = codeswarm::tui::App::default();
        app.set_agent_name(0, "Reviewer");
        app.set_agent_identity(0, "first.example");
        app.set_agent_name(1, "Reviewer");
        app.set_agent_identity(1, "second.example");
        app.set_config_agents(vec![
            StoreAgent {
                identity: "second.example".into(),
                name: "Reviewer".into(),
                adapter: "ACP".into(),
                command: "second-reviewer".into(),
                available: true,
                selected: true,
                model: None,
            },
            StoreAgent {
                identity: "first.example".into(),
                name: "Reviewer".into(),
                adapter: "ACP".into(),
                command: "first-reviewer".into(),
                available: true,
                selected: true,
                model: None,
            },
        ]);
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut pending_first = None;
        assert!(
            !reconcile_config_roster(&mut app, &sender, &mut pending_first).expect("reconcile")
        );
        assert!(matches!(
            receiver.try_recv(),
            Ok(AdapterControl::Swap(0, 1))
        ));
    }

    #[test]
    fn reordering_live_first_agent_swaps_without_recreating_either_agent() {
        let mut app = codeswarm::tui::App::default();
        app.set_agent_name(0, "Codex");
        app.set_agent_identity(0, "openai.com");
        app.set_agent_name(1, "Qwen");
        app.set_agent_identity(1, "qwen.ai");
        app.set_config_agents(vec![
            StoreAgent {
                identity: "qwen.ai".into(),
                name: "Qwen".into(),
                adapter: "ACP".into(),
                command: "qwen --acp".into(),
                available: true,
                selected: true,
                model: None,
            },
            StoreAgent {
                identity: "openai.com".into(),
                name: "Codex".into(),
                adapter: "ACP".into(),
                command: "codex --acp".into(),
                available: true,
                selected: true,
                model: None,
            },
        ]);
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut pending_first = None;
        assert!(
            !reconcile_config_roster(&mut app, &sender, &mut pending_first).expect("reconcile")
        );
        assert!(matches!(
            receiver.try_recv(),
            Ok(AdapterControl::Swap(0, 1))
        ));

        app.apply_event(&AgentEvent::RosterUpdated {
            update: codeswarm_adapters::RosterUpdate::Swapped {
                first: 0,
                second: 1,
            },
        });
        assert!(reconcile_config_roster(&mut app, &sender, &mut pending_first).expect("settled"));
        assert!(receiver.try_recv().is_err());
        assert_eq!(app.agent_count(), 2);
        assert_eq!(app.agent_name(0), "Qwen");
        assert_eq!(app.agent_name(1), "Codex");
    }

    #[test]
    fn config_roster_reconciliation_starts_a_new_first_agent_before_reorder() {
        let mut app = codeswarm::tui::App::default();
        app.set_agent_name(0, "Claude");
        app.set_agent_name(1, "Gemini");
        app.set_agent_identity(0, "anthropic.com");
        app.set_agent_identity(1, "geminicli.com");
        app.set_config_agents(vec![StoreAgent {
            identity: "openai.com".into(),
            name: "Codex".into(),
            adapter: "ACP".into(),
            command: "codex --acp".into(),
            available: true,
            selected: true,
            model: None,
        }]);
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut pending_first = None;
        assert!(
            !reconcile_config_roster(&mut app, &sender, &mut pending_first).expect("reconcile")
        );
        assert!(matches!(
            receiver.try_recv(),
            Ok(AdapterControl::Add { .. })
        ));
        assert_eq!(pending_first.as_deref(), Some("openai.com"));
        assert_eq!(app.agent_name(2), "Agent 2");
    }

    #[test]
    fn config_roster_reconciliation_hot_adds_to_a_single_agent_session() {
        let mut app = codeswarm::tui::App::default();
        app.set_agent_name(0, "Codex");
        app.set_agent_identity(0, "openai.com");
        app.set_config_agents(vec![
            StoreAgent {
                identity: "openai.com".into(),
                name: "Codex".into(),
                adapter: "ACP".into(),
                command: "codex --acp".into(),
                available: true,
                selected: true,
                model: None,
            },
            StoreAgent {
                identity: "qwen.ai".into(),
                name: "Qwen".into(),
                adapter: "ACP".into(),
                command: "qwen --acp".into(),
                available: true,
                selected: true,
                model: None,
            },
        ]);
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut pending_first = None;
        assert!(
            !reconcile_config_roster(&mut app, &sender, &mut pending_first).expect("reconcile")
        );
        assert!(matches!(
            receiver.try_recv(),
            Ok(AdapterControl::Add { identity, .. }) if identity == "qwen.ai"
        ));
    }

    #[test]
    fn config_order_drives_live_roster_and_pair_peer_order() {
        let mut app = codeswarm::tui::App::default();
        for (slot, name, identity) in [
            (0, "Codex", "openai.com"),
            (1, "Qwen", "qwen.ai"),
            (2, "Gemini", "google.com"),
        ] {
            app.set_agent_name(slot, name);
            app.set_agent_identity(slot, identity);
        }
        app.set_config_agents(
            [
                ("Codex", "openai.com"),
                ("Gemini", "google.com"),
                ("Qwen", "qwen.ai"),
            ]
            .into_iter()
            .map(|(name, identity)| StoreAgent {
                identity: identity.into(),
                name: name.into(),
                adapter: "ACP".into(),
                command: format!("{} --acp", name.to_ascii_lowercase()),
                available: true,
                selected: true,
                model: None,
            })
            .collect(),
        );
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut pending_first = None;
        assert!(
            !reconcile_config_roster(&mut app, &sender, &mut pending_first).expect("reconcile")
        );
        assert!(matches!(
            receiver.try_recv(),
            Ok(AdapterControl::Swap(1, 2))
        ));
    }

    #[test]
    fn parses_repeated_mixed_roster_with_selected_first_and_round_limit() {
        let args = vec![
            "--roster".into(),
            "agy:agy".into(),
            "--roster".into(),
            "acp:codex-acp".into(),
            "--first".into(),
            "1".into(),
            "--max-rounds".into(),
            "12".into(),
            "review the patch".into(),
        ];
        assert!(matches!(
            parse_launch(&args),
            Some(Launch::Roster {
                specs,
                prompt,
                first_slot: 1,
                max_rounds: 12,
                ..
            }) if specs == [AgentSpec::Agy("agy".into()), AgentSpec::Acp("codex-acp".into())]
                && prompt == Some("review the patch".into())
        ));
    }

    #[test]
    fn parses_python_named_agent_selection_into_the_same_mixed_roster() {
        assert!(matches!(
            parse_launch(&[
                "-a".into(),
                "claude".into(),
                "--agent".into(),
                "codex".into(),
                "--first-agent".into(),
                "2".into(),
                "review the patch".into(),
            ]),
            Some(Launch::Roster { specs, prompt: Some(prompt), first_slot: 1, .. })
                if specs == [
                    AgentSpec::Acp("npx -y @agentclientprotocol/claude-agent-acp".into()),
                    AgentSpec::Acp("npx -y --package=@agentclientprotocol/codex-acp codex-acp".into()),
                ] && prompt == "review the patch"
        ));
    }

    #[test]
    fn rejects_invalid_roster_kind_or_selected_slot() {
        assert!(parse_launch(&["--roster".into(), "bogus:agent".into(), "task".into()]).is_none());
        assert!(
            parse_launch(&[
                "--roster".into(),
                "agy:agy".into(),
                "--roster".into(),
                "acp:codex".into(),
                "--first".into(),
                "2".into(),
                "task".into(),
            ])
            .is_none()
        );
    }

    #[test]
    fn bare_launch_restores_catalogued_saved_roster() {
        assert!(matches!(
            bare_launch_from_settings(
                r#"{"launcher":{"roster":"OPENAI.COM\nantigravity.google.com"}}"#
            ),
            Launch::Roster { specs, prompt: None, first_slot: 0, max_rounds: 100, .. }
                if specs == [
                    AgentSpec::Acp("npx -y --package=@agentclientprotocol/codex-acp codex-acp".into()),
                    AgentSpec::Agy("agy --dangerously-skip-permissions".into())
                ]
        ));
    }

    #[test]
    fn bare_launch_restores_duplicate_slots_and_their_models() {
        let launch = bare_launch_from_settings(
            r#"{"launcher":{"roster":[{"agent":"openai.com","model":"fast"},{"agent":"openai.com","model":"smart"}]}}"#,
        );
        assert!(matches!(
            launch,
            Launch::Roster { identities, models, .. }
                if identities == ["openai.com", "openai.com"]
                    && models == [Some("fast".into()), Some("smart".into())]
        ));
    }

    #[test]
    fn config_reconciliation_adds_a_second_slot_for_the_same_agent() {
        let mut app = codeswarm::tui::App::default();
        app.set_agent_name(0, "Claude");
        app.set_agent_identity(0, "anthropic.com");
        app.set_config_agents(vec![
            StoreAgent {
                identity: "anthropic.com".into(),
                name: "Claude".into(),
                adapter: "ACP".into(),
                command: "claude-agent-acp".into(),
                available: true,
                selected: true,
                model: None,
            },
            StoreAgent {
                identity: "anthropic.com".into(),
                name: "Claude".into(),
                adapter: "ACP".into(),
                command: "claude-agent-acp".into(),
                available: true,
                selected: true,
                model: None,
            },
        ]);
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        assert!(!reconcile_config_roster(&mut app, &sender, &mut None).expect("reconcile"));
        assert!(matches!(
            receiver.try_recv(),
            Ok(AdapterControl::Add { identity, .. }) if identity == "anthropic.com"
        ));
    }

    #[test]
    fn bare_launch_opens_store_for_missing_or_stale_settings() {
        assert!(matches!(bare_launch_from_settings("{}"), Launch::Store));
        assert!(matches!(
            bare_launch_from_settings(r#"{"launcher":{"roster":"removed.ai"}}"#),
            Launch::Store
        ));
    }

    #[test]
    fn notification_settings_load_with_python_and_rust_key_shapes() {
        let mut app = codeswarm::tui::App::default();
        apply_notification_preferences(
            &mut app,
            &serde_json::json!({"notifications": {"system": "always", "enabled": false}}),
        );
        assert_eq!(app.notification_policy().as_str(), "always");
        assert!(app.should_notify_system());

        apply_notification_preferences(
            &mut app,
            &serde_json::json!({"notifications": {"turn_over": true}}),
        );
        assert_eq!(app.notification_policy().as_str(), "blur");
        app.set_terminal_focused(false);
        assert!(app.should_notify_system());
    }

    #[tokio::test]
    async fn standalone_cancel_reports_a_terminal_result_and_discards_buffered_text() {
        let mut adapter = ScriptedAdapter::new(
            0,
            AgentCapabilities {
                supports_cancel: true,
                ..AgentCapabilities::default()
            },
            [AgentEvent::TurnComplete { slot: 0 }],
        );
        let (sender, receiver) = std::sync::mpsc::channel();
        let mut response_tail = "buffered response".to_owned();

        cancel_standalone_turn(&mut adapter, &sender, &mut response_tail).await;

        assert!(response_tail.is_empty());
        assert!(matches!(
            receiver.recv().expect("cancel result"),
            Err(codeswarm_adapters::AdapterError::Transport(detail))
                if detail == "standalone turn cancelled"
        ));
    }

    #[test]
    fn adapter_error_during_pending_cancel_clears_the_ui_timer() {
        let mut app = App::default();
        app.apply_event(&AgentEvent::TurnStarted { slot: 0 });
        app.request_turn_cancellation();
        assert!(app.cancellation_pending());

        assert!(finish_pending_cancellation(
            &mut app,
            "adapter cancellation timed out"
        ));
        assert!(!app.cancellation_pending());
        assert_eq!(app.status, "cancelled");
    }

    #[tokio::test]
    async fn permission_selection_routes_the_selected_option_to_the_adapter() {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        assert!(dispatch_permission_action(
            Some(&sender),
            PermissionAction::Answer {
                slot: 2,
                request_id: "request-7".into(),
                option_index: 1,
                option: "allow-once".into(),
                option_id: "allow-once".into(),
            }
        ));
        assert!(matches!(
            receiver.recv().await,
            Some(AdapterControl::Permission {
                slot: 2,
                request_id,
                answer: PermissionAnswer::Selected { option_id },
            }) if request_id == "request-7" && option_id == "allow-once"
        ));
    }

    #[tokio::test]
    async fn single_agent_roster_uses_hot_reload_host_without_self_review() {
        let host = AdapterHost::new(
            Box::new(ScriptedAdapter::new(
                0,
                AgentCapabilities::default(),
                [
                    AgentEvent::Text {
                        slot: 0,
                        text: "done".into(),
                    },
                    AgentEvent::TurnComplete { slot: 0 },
                ],
            )),
            None,
        );
        let mut relay = RelayHost::new(vec![host], 10).expect("single-agent host");
        relay.start().await.expect("start");
        let (sender, _events) = std::sync::mpsc::channel::<AdapterResult<AgentEvent>>();
        let (_control_sender, mut controls) = tokio::sync::mpsc::unbounded_channel();
        let (stopping, deferred) =
            run_relay_sequence_with_controls(&mut relay, &mut controls, &sender, "task".into(), 0)
                .await;
        assert!(!stopping);
        assert!(deferred.is_empty());
        assert_eq!(relay.dispatches().len(), 1);
    }

    #[tokio::test]
    async fn roster_sequence_advances_through_each_agent_turn() {
        let hosts = vec![
            AdapterHost::new(
                Box::new(ScriptedAdapter::new(
                    0,
                    AgentCapabilities::default(),
                    [
                        AgentEvent::Text {
                            slot: 0,
                            text: "first response".into(),
                        },
                        AgentEvent::TurnComplete { slot: 0 },
                    ],
                )),
                None,
            ),
            AdapterHost::new(
                Box::new(ScriptedAdapter::new(
                    1,
                    AgentCapabilities::default(),
                    [
                        AgentEvent::Text {
                            slot: 1,
                            text: "review response".into(),
                        },
                        AgentEvent::TurnComplete { slot: 1 },
                    ],
                )),
                None,
            ),
        ];
        let mut relay = RelayHost::new(hosts, 2).expect("two-agent relay");
        relay.start().await.expect("scripted adapters start");
        let (sender, _events) = std::sync::mpsc::channel::<AdapterResult<AgentEvent>>();
        let (_control_sender, mut controls) = tokio::sync::mpsc::unbounded_channel();
        let (_stopping, deferred) = run_relay_sequence_with_controls(
            &mut relay,
            &mut controls,
            &sender,
            "initial task".into(),
            0,
        )
        .await;

        assert!(deferred.is_empty());
        assert_eq!(
            relay
                .dispatches()
                .iter()
                .map(|(slot, _)| *slot)
                .collect::<Vec<_>>(),
            vec![0, 1]
        );
    }

    #[tokio::test]
    async fn startup_mode_control_does_not_stop_after_the_first_agent() {
        let first = YieldingAdapter {
            slot: 0,
            yielded: false,
            events: [
                AgentEvent::Text {
                    slot: 0,
                    text: "first response".into(),
                },
                AgentEvent::TurnComplete { slot: 0 },
            ]
            .into(),
        };
        let second = ScriptedAdapter::new(
            1,
            AgentCapabilities::default(),
            [
                AgentEvent::Text {
                    slot: 1,
                    text: "review response".into(),
                },
                AgentEvent::TurnComplete { slot: 1 },
            ],
        );
        let mut relay = RelayHost::new(
            vec![
                AdapterHost::new(Box::new(first), None),
                AdapterHost::new(Box::new(second), None),
            ],
            2,
        )
        .expect("relay");
        relay.start().await.expect("start");
        let (sender, _events) = std::sync::mpsc::channel::<AdapterResult<AgentEvent>>();
        let (control_sender, mut controls) = tokio::sync::mpsc::unbounded_channel();
        control_sender
            .send(AdapterControl::SetMode("full-access".into()))
            .expect("startup mode control");

        let (stopping, deferred) = run_relay_sequence_with_controls(
            &mut relay,
            &mut controls,
            &sender,
            "initial task".into(),
            0,
        )
        .await;

        assert!(!stopping);
        assert!(deferred.is_empty());
        assert_eq!(
            relay
                .dispatches()
                .iter()
                .map(|(slot, _)| *slot)
                .collect::<Vec<_>>(),
            [0, 1]
        );
    }

    #[tokio::test]
    async fn reviewer_stop_token_stops_the_roster_sequence() {
        let hosts = vec![
            AdapterHost::new(
                Box::new(ScriptedAdapter::new(
                    0,
                    AgentCapabilities::default(),
                    [
                        AgentEvent::Text {
                            slot: 0,
                            text: "done".into(),
                        },
                        AgentEvent::TurnComplete { slot: 0 },
                    ],
                )),
                None,
            ),
            AdapterHost::new(
                Box::new(ScriptedAdapter::new(
                    1,
                    AgentCapabilities::default(),
                    [
                        AgentEvent::Text {
                            slot: 1,
                            text: codeswarm_adapters::relay::STOP_TOKEN.into(),
                        },
                        AgentEvent::TurnComplete { slot: 1 },
                    ],
                )),
                None,
            ),
        ];
        let mut relay = RelayHost::new(hosts, 10).expect("two-agent relay");
        relay.start().await.expect("scripted adapters start");
        let (sender, _events) = std::sync::mpsc::channel::<AdapterResult<AgentEvent>>();
        let (_control_sender, mut controls) = tokio::sync::mpsc::unbounded_channel();
        let (stopping, deferred) = run_relay_sequence_with_controls(
            &mut relay,
            &mut controls,
            &sender,
            "initial task".into(),
            0,
        )
        .await;

        assert!(!stopping);
        assert!(deferred.is_empty());
        assert_eq!(
            relay
                .dispatches()
                .iter()
                .map(|(slot, _)| *slot)
                .collect::<Vec<_>>(),
            vec![0, 1]
        );
        assert_eq!(
            relay.run_turn("", 0).await.expect("stopped batch"),
            codeswarm_adapters::relay::RelayDecision::Complete
        );
    }

    #[test]
    fn queued_direct_prompt_without_target_is_rejected_without_panic() {
        let (sender, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let prompt = QueuedPrompt {
            id: 1,
            prompt: "private check".into(),
            target: None,
            direct: true,
        };
        assert!(!dispatch_queued_prompt(Some(&sender), &prompt));
    }

    #[test]
    fn standalone_stop_token_is_hidden_even_when_split_across_chunks() {
        let token = codeswarm_adapters::relay::STOP_TOKEN;
        let mut tail = String::new();
        let mut output = Vec::new();
        output.extend(sanitize_direct_event(
            AgentEvent::Text {
                slot: 0,
                text: format!("visible {token}").replace(token, "[CODESWARM:"),
            },
            &mut tail,
        ));
        output.extend(sanitize_direct_event(
            AgentEvent::Text {
                slot: 0,
                text: "STOP] trailing".to_string(),
            },
            &mut tail,
        ));
        output.extend(sanitize_direct_event(
            AgentEvent::TurnComplete { slot: 0 },
            &mut tail,
        ));
        let text = output
            .into_iter()
            .filter_map(|event| match event {
                AgentEvent::Text { text, .. } => Some(text),
                _ => None,
            })
            .collect::<String>();
        assert_eq!(text, "visible  trailing");
        assert!(!text.contains(token));
    }
}
