//! Terminal coordinator. Only this thread interprets input and answers approvals;
//! model and helper threads communicate through events and never read stdin.

use super::*;
use crossterm::cursor::{Hide, Show};
use crossterm::event::{
    self as terminal_event, Event as TerminalEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use std::collections::VecDeque;
#[cfg(test)]
use std::io::BufRead;
use std::io::Stdout;
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(test)]
use std::sync::mpsc::Receiver;
use std::sync::mpsc::{self, Sender};
#[cfg(unix)]
use std::sync::{Condvar, Mutex};
use std::time::Duration;

#[derive(Clone)]
pub(crate) struct UiHandle {
    events: Sender<Event>,
    cancelled: Arc<AtomicBool>,
    suppress_routine: bool,
    #[cfg(unix)]
    shell: Arc<ShellTracker>,
}

#[cfg(unix)]
#[derive(Default)]
struct ShellTracker {
    active: Mutex<Option<libc::pid_t>>,
    changed: Condvar,
}

impl UiHandle {
    pub(crate) fn cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    #[cfg(unix)]
    pub(crate) fn start_shell(&self, command: &mut Command) -> Result<std::process::Child, String> {
        let mut active = self.shell.active.lock().unwrap_or_else(|e| e.into_inner());
        if self.cancelled() {
            return Err("interrupted".into());
        }
        let child = command.spawn().map_err(|e| format!("error: {e}"))?;
        *active = Some(child.id() as libc::pid_t);
        Ok(child)
    }

    #[cfg(unix)]
    pub(crate) fn shell_finished(&self, pid: libc::pid_t) {
        let mut active = self.shell.active.lock().unwrap_or_else(|e| e.into_inner());
        if *active == Some(pid) {
            *active = None;
            self.shell.changed.notify_all();
        }
    }

    #[cfg(unix)]
    fn stop_shell(&self) -> Result<(), String> {
        let mut active = self.shell.active.lock().unwrap_or_else(|e| e.into_inner());
        let Some(pid) = *active else { return Ok(()) };
        // Kill the whole group at once, including descendants that may keep
        // stdout open after the shell itself exits.
        unsafe { libc::kill(-pid, libc::SIGKILL) };
        let (guard, _) = self
            .shell
            .changed
            .wait_timeout_while(active, Duration::from_secs(2), |current| {
                *current == Some(pid)
            })
            .unwrap_or_else(|e| e.into_inner());
        active = guard;
        if *active == Some(pid) {
            Err("shell process could not be reaped after cancellation".into())
        } else {
            Ok(())
        }
    }

    pub(crate) fn progress(&self, cfg: &Cfg, message: &str) {
        if self.suppress_routine && matches!(cfg.role, Role::Parent(_)) {
            if message == "requesting model" {
                let _ = self.events.send(Event::ModelPhase(true));
                return;
            }
            let _ = self.events.send(Event::ModelPhase(false));
        }
        let id = match &cfg.role {
            Role::Parent(_) => "main",
            Role::Child(_, id) => id,
        };
        let display = if self.suppress_routine && id == "main" {
            message.to_string()
        } else {
            format!("[{id}] {message}")
        };
        let _ = self.events.send(Event::Progress(display));
    }

    pub(crate) fn reasoning_chunk(&self, chunk: &str) {
        if !self.cancelled() && !chunk.is_empty() {
            let _ = self.events.send(Event::ReasoningChunk(chunk.into()));
        }
    }

    pub(crate) fn approve(&self, what: &str, preview: &str) -> bool {
        if self.cancelled() {
            return false;
        }
        let (tx, rx) = mpsc::channel();
        if self
            .events
            .send(Event::Approval {
                what: what.into(),
                preview: preview.into(),
                answer: tx,
            })
            .is_err()
        {
            return false;
        }
        rx.recv().unwrap_or(false) && !self.cancelled()
    }
}

enum Event {
    #[cfg(test)]
    Input(String),
    #[cfg(test)]
    #[allow(dead_code)]
    Eof,
    Progress(String),
    ModelPhase(bool),
    ReasoningChunk(String),
    Approval {
        what: String,
        preview: String,
        answer: Sender<bool>,
    },
    Finished(Result<AgentAnswer, String>, Vec<Value>),
    ModelsFetched {
        request: u64,
        result: Result<Vec<String>, String>,
    },
}

static SIGINT_REQUESTED: AtomicBool = AtomicBool::new(false);
static TERMINATE_REQUESTED: AtomicBool = AtomicBool::new(false);
static KEYBOARD_ENHANCEMENT_ACTIVE: AtomicBool = AtomicBool::new(false);

fn restore_keyboard_enhancement() {
    if KEYBOARD_ENHANCEMENT_ACTIVE.swap(false, Ordering::AcqRel) {
        let _ = execute!(std::io::stdout(), PopKeyboardEnhancementFlags);
    }
}

struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalGuard {
    fn enter() -> Result<Self, String> {
        install_panic_restore();
        enable_raw_mode().map_err(|e| e.to_string())?;
        let mut output = std::io::stdout();
        if let Err(e) = execute!(output, EnterAlternateScreen, Hide) {
            let _ = disable_raw_mode();
            return Err(e.to_string());
        }
        // Compatible terminals report modified Enter as CSI-u. Legacy terminals
        // ignore this request and continue to send the same CR for both keys.
        if execute!(
            output,
            PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES
            )
        )
        .is_ok()
        {
            KEYBOARD_ENHANCEMENT_ACTIVE.store(true, Ordering::Release);
        }
        let backend = CrosstermBackend::new(output);
        match Terminal::new(backend) {
            Ok(terminal) => Ok(Self { terminal }),
            Err(e) => {
                restore_keyboard_enhancement();
                let _ = execute!(std::io::stdout(), Show, LeaveAlternateScreen);
                let _ = disable_raw_mode();
                Err(e.to_string())
            }
        }
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = self.terminal.show_cursor();
        restore_keyboard_enhancement();
        let _ = execute!(std::io::stdout(), Show, LeaveAlternateScreen);
        let _ = disable_raw_mode();
    }
}

fn install_panic_restore() {
    static INSTALLED: std::sync::Once = std::sync::Once::new();
    INSTALLED.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore_keyboard_enhancement();
            let _ = disable_raw_mode();
            let _ = execute!(std::io::stdout(), Show, LeaveAlternateScreen);
            previous(info);
        }));
    });
}

fn preset_url(provider: usize) -> &'static str {
    match provider {
        0 => "http://127.0.0.1:11434/v1",
        1 => "http://127.0.0.1:1234/v1",
        2 => "http://127.0.0.1:8000/v1",
        _ => "",
    }
}

const REASONING_CHOICES: [&str; 6] = ["", "low", "medium", "high", "xhigh", "max"];
const HELPER_NAMES: [&str; 3] = ["local", "codex", "claude"];

fn parse_helper_selection(csv: &str) -> Result<[bool; 3], String> {
    let mut selected = [false; 3];
    for name in csv.split(',') {
        let name = name.trim();
        let Some(index) = HELPER_NAMES.iter().position(|known| *known == name) else {
            return Err(format!("unknown helper provider `{name}`"));
        };
        if selected[index] {
            return Err(format!("duplicate helper provider `{name}`"));
        }
        selected[index] = true;
    }
    if !selected.iter().any(|value| *value) {
        return Err("select at least one helper provider".into());
    }
    Ok(selected)
}

fn helper_selection_csv(selected: [bool; 3]) -> Result<String, String> {
    if !selected.iter().any(|value| *value) {
        return Err("select at least one helper provider".into());
    }
    Ok(HELPER_NAMES
        .iter()
        .zip(selected)
        .filter_map(|(name, enabled)| enabled.then_some(*name))
        .collect::<Vec<_>>()
        .join(","))
}

fn stored_helper_selection() -> Result<[bool; 3], String> {
    if home_settings_file().is_none() {
        return Ok([true, false, false]);
    }
    let settings = load_home_config()?;
    match settings.get("helper_providers") {
        None => Ok([true, false, false]),
        Some(Value::String(csv)) => parse_helper_selection(csv),
        Some(_) => Err("helper_providers in ~/.revex must be a comma-separated string".into()),
    }
}

fn cli_available(name: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        let candidate = dir.join(name);
        let Ok(metadata) = std::fs::metadata(candidate) else {
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
    })
}

fn effective_reasoning(cfg: &Cfg) -> Option<String> {
    if let Some(explicit) = &cfg.reasoning {
        return (!explicit.is_empty()).then(|| explicit.clone());
    }
    let extra: Value = serde_json::from_str(&std::env::var("REVEX_EXTRA").ok()?).ok()?;
    let value = match cfg.api {
        Api::Chat => &extra["reasoning_effort"],
        Api::Responses => &extra["reasoning"]["effort"],
    };
    value.as_str().map(str::to_string)
}

fn setup_view(view: &mut tui::View, cfg: &Cfg) {
    view.page = tui::Page::Setup;
    view.overlay = tui::Overlay::None;
    view.setup_provider = match cfg.base.as_str() {
        "" => 0,
        "http://127.0.0.1:11434/v1" => 0,
        "http://127.0.0.1:1234/v1" => 1,
        "http://127.0.0.1:8000/v1" => 2,
        _ => 3,
    };
    view.setup_provider_committed = (!cfg.base.is_empty()).then_some(view.setup_provider);
    view.setup_tab = 0;
    view.setup_tab_focus = true;
    view.setup_editing = false;
    view.setup_tab_fields = [0, 3, 5];
    view.setup_url = if cfg.base.is_empty() {
        preset_url(0).into()
    } else {
        cfg.base.clone()
    };
    view.setup_key.clear();
    view.setup_clear_key = false;
    view.setup_model = cfg.model.clone().unwrap_or_default();
    view.setup_reasoning = cfg.reasoning.clone().unwrap_or_default();
    view.setup_reasoning_selected = REASONING_CHOICES
        .iter()
        .position(|v| *v == view.setup_reasoning)
        .unwrap_or(0);
    view.setup_reasoning_manual = !view.setup_reasoning.is_empty()
        && !REASONING_CHOICES.contains(&view.setup_reasoning.as_str());
    view.setup_decision_model_selected = ["self", "clef", "clef-flash"]
        .iter()
        .position(|value| *value == cfg.decision_model)
        .unwrap_or(1);
    view.setup_response_language_selected = match cfg.response_language {
        ResponseLanguage::English => 0,
        ResponseLanguage::Japanese => 1,
    };
    let helper_status = match stored_helper_selection() {
        Ok(selected) => {
            view.setup_helpers = selected;
            None
        }
        Err(e) => {
            view.setup_helpers = [true, false, false];
            Some(format!("Helper settings need repair: {e}"))
        }
    };
    view.setup_helper_selected = 0;
    view.setup_approval_mode_selected = match cfg.approval_mode {
        ApprovalMode::Manual => 0,
        ApprovalMode::Auto => 1,
        ApprovalMode::Bypass => 2,
        ApprovalMode::Plan => 3,
    };
    view.setup_approval_mode_dirty = false;
    view.setup_helper_available = [true, cli_available("codex"), cli_available("claude")];
    view.setup_models.clear();
    view.setup_model_selected = 0;
    view.setup_manual = !view.setup_model.is_empty();
    view.setup_field = 0;
    view.setup_status =
        helper_status.unwrap_or_else(|| "Choose a provider. Ctrl+S saves from any tab.".into());
    view.cursor = 0;
}

fn fetch_models(tx: &Sender<Event>, request: u64, base: String, key: String) {
    let events = tx.clone();
    std::thread::spawn(move || {
        let result = query_models(&base, &key, Duration::from_secs(2)).map_err(str::to_string);
        let _ = events.send(Event::ModelsFetched { request, result });
    });
}

fn seed_transcript(msgs: &[Value], view: &mut tui::View) {
    for message in msgs.iter().skip(1) {
        let content = message["content"].as_str().unwrap_or("");
        match message["role"].as_str().unwrap_or("") {
            "user" if !content.starts_with("<out>") => view
                .transcript
                .push(format!("You  {}", terminal_safe(content))),
            "assistant" if !content.trim_start().starts_with(">>") => view
                .transcript
                .push(format!("Revex  {}", terminal_safe(content))),
            _ => {}
        }
    }
}

fn activate_chat(
    cfg: &mut Cfg,
    view: &mut tui::View,
    session: &mut Option<Session>,
    tx: &Sender<Event>,
) -> Result<(), String> {
    let cwd = cfg
        .cwd
        .canonicalize()
        .map_err(|e| format!("workspace {}: {e}", cfg.cwd.display()))?;
    let history = history_for(&state_dir()?, &cwd);
    let prompt_history = PromptHistory::load(&history)?;
    let agents = Agents::open(agents_dir(&history), model_fn(chat))?;
    let helper_selection = stored_helper_selection()?;
    agents.set_providers(helper_selection)?;
    let warning_events = tx.clone();
    agents.set_error_sink(move |message| {
        let _ = warning_events.send(Event::Progress(message));
    });
    let msgs = load_history(&history)?;
    cfg.cwd = cwd.clone();
    cfg.history = Some(history);
    cfg.strict_save = true;
    cfg.role = Role::Parent(Some(agents.clone()));
    view.cwd = cwd.display().to_string();
    view.model = cfg.model.clone().unwrap_or_else(|| "server default".into());
    view.reasoning = effective_reasoning(cfg);
    view.connection = "checking API".into();
    view.page = tui::Page::Chat;
    view.overlay = tui::Overlay::None;
    view.transcript.clear();
    seed_transcript(&msgs, view);
    let mut active = Session::new(cfg.clone(), msgs, agents, tx.clone());
    active.prompt_history = Some(prompt_history);
    active.helper_selection = helper_selection;
    active.tui = true;
    if let Some(ui) = active.cfg.ui.as_mut() {
        ui.suppress_routine = true;
    }
    *session = Some(active);
    Ok(())
}

fn append(view: &mut tui::View, message: &str) {
    let clean = terminal_safe(message);
    view.transcript.push(clean);
    if view.transcript.len() > 2000 {
        view.transcript.drain(..view.transcript.len() - 2000);
    }
    view.scroll = 0;
}

const MAX_THINKING_BYTES: usize = 16 * 1024;

fn append_reasoning(view: &mut tui::View, chunk: &str) {
    view.thinking_text.push_str(&terminal_safe(chunk));
    if view.thinking_text.len() > MAX_THINKING_BYTES {
        let mut start = view.thinking_text.len() - MAX_THINKING_BYTES;
        while !view.thinking_text.is_char_boundary(start) {
            start += 1;
        }
        view.thinking_text.drain(..start);
    }
}

fn byte_at(value: &str, scalar: usize) -> usize {
    value
        .char_indices()
        .nth(scalar)
        .map(|(byte, _)| byte)
        .unwrap_or(value.len())
}

fn edit_buffer(value: &mut String, cursor: &mut usize, key: KeyEvent) -> bool {
    match key.code {
        KeyCode::Char(ch)
            if !key.modifiers.contains(KeyModifiers::CONTROL)
                && !key.modifiers.contains(KeyModifiers::ALT) =>
        {
            value.insert(byte_at(value, *cursor), ch);
            *cursor += 1;
        }
        KeyCode::Backspace if *cursor > 0 => {
            let from = byte_at(value, *cursor - 1);
            let to = byte_at(value, *cursor);
            value.replace_range(from..to, "");
            *cursor -= 1;
        }
        KeyCode::Delete if *cursor < value.chars().count() => {
            let from = byte_at(value, *cursor);
            let to = byte_at(value, *cursor + 1);
            value.replace_range(from..to, "");
        }
        KeyCode::Left => *cursor = cursor.saturating_sub(1),
        KeyCode::Right => *cursor = (*cursor + 1).min(value.chars().count()),
        KeyCode::Home => *cursor = 0,
        KeyCode::End => *cursor = value.chars().count(),
        _ => return false,
    }
    true
}

fn setup_auth_key(cfg: &Cfg, view: &tui::View) -> String {
    if view.setup_clear_key {
        String::new()
    } else if !view.setup_key.is_empty() {
        view.setup_key.clone()
    } else if view.setup_url.trim().trim_end_matches('/') == cfg.base.trim_end_matches('/') {
        cfg.key.clone()
    } else {
        String::new()
    }
}

fn change_setup_url(view: &mut tui::View, url: String) {
    if view.setup_url != url {
        if !view.setup_key.is_empty() {
            view.setup_key.clear();
            view.setup_status = "URL changed; re-enter the API key for this endpoint.".into();
        }
        view.setup_url = url;
    }
}

fn invalidate_setup_models(view: &mut tui::View, request: &mut u64) {
    *request += 1;
    view.setup_models.clear();
    view.setup_model_selected = 0;
    view.setup_manual = !view.setup_model.is_empty();
}

fn guard_endpoint_change(
    current: &str,
    requested: &str,
    env_key_present: bool,
) -> Result<(), String> {
    if env_key_present && !current.is_empty() && requested != current.trim_end_matches('/') {
        return Err("REVEX_API_KEY is set and would be sent to the new URL on restart. Unset REVEX_API_KEY before changing the API URL.".into());
    }
    Ok(())
}

fn apply_setup_key(
    cfg: &mut Cfg,
    settings: &mut Map<String, Value>,
    view: &tui::View,
    base: &str,
) -> Result<(), String> {
    let stored_base = settings
        .get("base_url")
        .and_then(Value::as_str)
        .unwrap_or(&cfg.base)
        .trim_end_matches('/');
    let stored_key = settings
        .get("api_key")
        .and_then(Value::as_str)
        .is_some_and(|key| !key.is_empty());
    if base != stored_base && stored_key && view.setup_key.is_empty() && !view.setup_clear_key {
        return Err(
            "The saved API key belongs to the previous URL; enter a replacement or press Ctrl+K in API key to clear it".into(),
        );
    }
    if view.setup_clear_key {
        settings.insert("api_key".into(), Value::Null);
        cfg.key.clear();
    } else if !view.setup_key.is_empty() {
        settings.insert("api_key".into(), Value::String(view.setup_key.clone()));
        cfg.key = view.setup_key.clone();
    } else if base != cfg.base.trim_end_matches('/') {
        cfg.key.clear();
    }
    Ok(())
}

fn save_project_model_settings(
    path: &Path,
    model: Option<&str>,
    effort: &str,
    decision_model: &str,
    response_language: &str,
) -> Result<(), String> {
    let mut settings = read_settings(path)?;
    settings.retain(|key, _| PROJECT_KEYS.contains(&key.as_str()));
    if let Some(model) = model {
        settings.insert("model".into(), Value::String(model.into()));
    } else {
        settings.remove("model");
    }
    settings.insert("reasoning_effort".into(), Value::String(effort.into()));
    settings.insert(
        "decision_model".into(),
        Value::String(decision_model.into()),
    );
    settings.insert(
        "response_language".into(),
        Value::String(response_language.into()),
    );
    write_settings_file(path, &settings)
}

fn start_model_check(view: &mut tui::View, cfg: &Cfg, tx: &Sender<Event>, request: &mut u64) {
    *request += 1;
    view.setup_status = "Checking /models...".into();
    let key = setup_auth_key(cfg, view);
    fetch_models(
        tx,
        *request,
        view.setup_url.trim_end_matches('/').to_string(),
        key,
    );
}

fn selected_decision_model(view: &tui::View) -> Result<&'static str, String> {
    let selected = ["self", "clef", "clef-flash"]
        .get(view.setup_decision_model_selected)
        .copied()
        .ok_or("Choose a valid decision model")?;
    if selected == "self" && view.setup_model.trim().is_empty() {
        return Err("decision_model=self needs a configured chat model ID".into());
    }
    Ok(selected)
}

fn save_setup(
    cfg: &mut Cfg,
    view: &mut tui::View,
    session: &mut Option<Session>,
    tx: &Sender<Event>,
    request: &mut u64,
) -> Result<(), String> {
    let decision_model = selected_decision_model(view)?;
    let response_language = match view.setup_response_language_selected {
        0 => ResponseLanguage::English,
        1 => ResponseLanguage::Japanese,
        _ => return Err("Choose English or Japanese for response language".into()),
    };
    let approval_mode = match view.setup_approval_mode_selected {
        0 => ApprovalMode::Manual,
        1 => ApprovalMode::Auto,
        2 => ApprovalMode::Bypass,
        3 => ApprovalMode::Plan,
        _ => return Err("Choose a valid approval mode".into()),
    };
    if view.setup_provider_committed.is_none() {
        return Err("Choose a model server provider with Enter".into());
    }
    if !view.setup_helpers.iter().any(|selected| *selected) {
        return Err("Select at least one helper provider with Space".into());
    }
    let base = view.setup_url.trim().trim_end_matches('/');
    let host = base
        .strip_prefix("http://")
        .or_else(|| base.strip_prefix("https://"));
    if !host.is_some_and(|h| {
        !h.is_empty() && !h.chars().any(|ch| ch.is_control() || ch.is_whitespace())
    }) {
        return Err("Enter a full http:// or https:// API URL".into());
    }
    guard_endpoint_change(&cfg.base, base, std::env::var_os("REVEX_API_KEY").is_some())?;
    let mut settings = load_home_config()?;
    let project_path = cfg.cwd.join(".revex");
    let project_has_settings = project_path.is_file();
    apply_setup_key(cfg, &mut settings, view, base)?;
    settings.insert("base_url".into(), Value::String(base.into()));
    settings.insert(
        "helper_providers".into(),
        Value::String(helper_selection_csv(view.setup_helpers)?),
    );
    if view.setup_approval_mode_dirty {
        settings.insert(
            "approval_mode".into(),
            Value::String(approval_mode_name(approval_mode).into()),
        );
    }
    cfg.model = (!view.setup_model.trim().is_empty()).then(|| view.setup_model.trim().into());
    let effort = view.setup_reasoning.trim();
    cfg.reasoning = Some(effort.into());
    cfg.decision_model = decision_model.into();
    cfg.response_language = response_language;
    if !project_has_settings {
        if let Some(model) = &cfg.model {
            settings.insert("model".into(), Value::String(model.clone()));
        } else {
            settings.remove("model");
        }
        settings.insert("reasoning_effort".into(), Value::String(effort.into()));
        settings.insert(
            "decision_model".into(),
            Value::String(cfg.decision_model.clone()),
        );
        settings.insert(
            "response_language".into(),
            Value::String(response_language_name(cfg.response_language).into()),
        );
    }
    save_home_config(&settings)?;
    if view.setup_approval_mode_dirty {
        cfg.approval_mode = approval_mode;
        cfg.yes = false; // an explicit UI selection replaces the startup --yes override
    }
    if project_has_settings {
        save_project_model_settings(
            &project_path,
            cfg.model.as_deref(),
            effort,
            &cfg.decision_model,
            response_language_name(cfg.response_language),
        )?;
    }
    cfg.base = base.into();
    view.setup_key.clear();
    view.setup_clear_key = false;
    view.model = cfg.model.clone().unwrap_or_else(|| "server default".into());
    view.reasoning = effective_reasoning(cfg);
    view.approval_mode = approval_mode_name(cfg.approval_mode).into();
    view.connection = "checking API".into();
    if let Some(session) = session.as_mut() {
        session.agents.set_providers(view.setup_helpers)?;
        session.helper_selection = view.setup_helpers;
        session.cfg.base = cfg.base.clone();
        session.cfg.key = cfg.key.clone();
        session.cfg.model = cfg.model.clone();
        session.cfg.reasoning = cfg.reasoning.clone();
        session.cfg.decision_model = cfg.decision_model.clone();
        session.cfg.response_language = cfg.response_language;
        if view.setup_approval_mode_dirty {
            session.cfg.approval_mode = cfg.approval_mode;
            session.cfg.yes = false;
        }
        view.page = tui::Page::Chat;
        view.overlay = tui::Overlay::None;
        append(
            view,
            "Settings saved. New requests will use the updated provider.",
        );
    } else {
        activate_chat(cfg, view, session, tx)?;
    }
    *request += 1;
    fetch_models(tx, *request, cfg.base.clone(), cfg.key.clone());
    Ok(())
}

fn set_approval_mode(
    mode: ApprovalMode,
    cfg: &mut Cfg,
    view: &mut tui::View,
    session: &mut Session,
) {
    let value = approval_mode_name(mode);
    cfg.approval_mode = mode;
    cfg.yes = false;
    session.cfg.approval_mode = mode;
    session.cfg.yes = false;
    view.approval_mode = value.into();
    let result = (|| {
        let mut settings = load_home_config()?;
        settings.insert("approval_mode".into(), Value::String(value.into()));
        save_home_config(&settings)
    })();
    match result {
        Ok(()) => append(view, &format!("Approval mode: {value} (saved)")),
        Err(e) => append(
            view,
            &format!("Approval mode: {value} for this session; not saved: {e}"),
        ),
    }
    if session.busy {
        append(view, "The new approval mode applies to the next task.");
    }
}

fn submit_chat(
    line: String,
    cfg: &mut Cfg,
    view: &mut tui::View,
    session: &mut Session,
    request: &mut u64,
    exit: &mut bool,
) {
    let line = line.trim();
    if line.is_empty() {
        return;
    }
    if let Some(history) = session.prompt_history.as_mut()
        && let Err(e) = history.record(line)
    {
        append(view, &format!("Input history not saved: {e}"));
    }
    match line {
        "/exit" | "exit" => {
            session.close(&mut |message| append(view, message));
            *exit = true;
        }
        "/agents" => view.overlay = tui::Overlay::Agents,
        "/status" => {
            let parent = &session.agents.snapshot()[0];
            view.status = format!(
                "{} | {} saved messages | {} queued",
                parent.status.name(),
                session.msgs.as_ref().map_or(0, Vec::len),
                session.queued.len()
            );
            view.overlay = tui::Overlay::Status;
        }
        "/config" => {
            if session.busy
                || session
                    .agents
                    .snapshot()
                    .iter()
                    .skip(1)
                    .any(|a| matches!(a.status, agents::Status::Running | agents::Status::Queued))
            {
                append(
                    view,
                    "Finish the active task and helpers before changing settings.",
                );
            } else {
                setup_view(view, cfg);
                *request += 1; // invalidate any pending connection check
            }
        }
        "/clear" => {
            if session.busy {
                append(view, "Finish the current task before /clear.");
            } else if let Some(msgs) = session.msgs.as_mut() {
                match clear_history(&session.cfg, msgs) {
                    Ok(()) => {
                        view.transcript.clear();
                        view.turn_started = None;
                        view.thinking_started = None;
                        view.last_turn_elapsed = None;
                        view.last_turn_rate = None;
                        append(view, "Session history and agent records cleared.");
                    }
                    Err(e) => append(view, &format!("History not cleared: {e}")),
                }
            }
        }
        "/help" => append(
            view,
            "/agents  /status  /config  /model [ID]  /approval [manual|auto|bypass|plan]  /clear  /exit or exit  Ctrl+D exit  Alt+Enter newline  Ctrl+R retry models",
        ),
        _ if line.starts_with("/model") => {
            let id = line.strip_prefix("/model").unwrap_or("").trim();
            if session.busy
                || session
                    .agents
                    .snapshot()
                    .iter()
                    .skip(1)
                    .any(|a| matches!(a.status, agents::Status::Running | agents::Status::Queued))
            {
                append(
                    view,
                    "Finish the active task and helpers before changing models.",
                );
            } else if id.is_empty() {
                setup_view(view, cfg);
                view.setup_tab = 1;
                view.setup_tab_focus = false;
                view.setup_editing = false;
                view.setup_field = 3;
                view.setup_manual = false;
                start_model_check(view, cfg, &session.events, request);
            } else {
                setup_view(view, cfg);
                view.setup_tab = 1;
                view.setup_tab_focus = false;
                view.setup_editing = false;
                view.setup_model = id.into();
                view.setup_manual = true;
                view.setup_field = 4;
                view.setup_status = "Choose reasoning effort, then confirm model settings.".into();
                *request += 1;
            }
        }
        "/approval" => append(
            view,
            &format!("Approval mode: {}", approval_mode_name(cfg.approval_mode)),
        ),
        _ if line.starts_with("/approval ") => {
            let value = line.trim_start_matches("/approval ").trim();
            if let Some(mode) = parse_approval_mode(value) {
                set_approval_mode(mode, cfg, view, session);
            } else {
                append(view, "Approval mode must be manual, auto, bypass or plan.");
            }
        }
        _ if line.starts_with("/route") => {
            let value = line.strip_prefix("/route").unwrap_or("").trim();
            if value.is_empty() {
                append(view, &format!("Route: {}", route_name(cfg.route)));
            } else if let Some(route) = parse_route(value) {
                cfg.route = route;
                session.cfg.route = route;
                match save_setting("route", value) {
                    Ok(_) => append(view, &format!("Route set to {value}")),
                    Err(e) => append(
                        view,
                        &format!("Route changed for this session; not saved: {e}"),
                    ),
                }
            } else {
                append(view, "Route must be off, auto, smart, local or remote.");
            }
        }
        _ if line.starts_with('/') => {
            append(view, "Unknown command. Type /help for available commands.")
        }
        task => {
            append(view, &format!("You  {task}"));
            if session.busy {
                session.queued.push_back(task.into());
                append(view, &format!("Task queued ({})", session.queued.len()));
            } else {
                begin_turn(view, session);
                session.start(task.into(), &mut |message| append(view, message));
            }
        }
    }
}

fn begin_turn(view: &mut tui::View, session: &Session) {
    view.thinking_text.clear();
    view.thinking_started = None;
    view.turn_usage_before = session.agents.snapshot()[0].usage;
    view.turn_started = Some(std::time::Instant::now());
    view.last_turn_elapsed = None;
    view.last_turn_rate = None;
}

fn complete_turn(view: &mut tui::View, session: &Session) {
    let Some(started) = view.turn_started.take() else {
        return;
    };
    let elapsed = started.elapsed();
    let after = session.agents.snapshot()[0].usage;
    let before = view.turn_usage_before;
    let requests = after.requests.saturating_sub(before.requests);
    let missing = after.output_missing.saturating_sub(before.output_missing);
    view.last_turn_rate = (requests > 0 && missing == 0 && !elapsed.is_zero())
        .then(|| after.output.saturating_sub(before.output) as f64 / elapsed.as_secs_f64());
    view.last_turn_elapsed = Some(elapsed);
}

fn handle_setup_key(
    key: KeyEvent,
    cfg: &mut Cfg,
    view: &mut tui::View,
    session: &mut Option<Session>,
    tx: &Sender<Event>,
    request: &mut u64,
    exit: &mut bool,
) -> Result<(), String> {
    if key.code == KeyCode::Char('s') && key.modifiers.contains(KeyModifiers::CONTROL) {
        if let Err(e) = save_setup(cfg, view, session, tx, request) {
            view.setup_status = format!("Cannot save: {e}");
        }
        return Ok(());
    }
    if key.code == KeyCode::Esc {
        if view.setup_editing {
            view.setup_editing = false;
        } else if !view.setup_tab_focus {
            view.setup_tab_fields[view.setup_tab] = view.setup_field;
            view.setup_tab_focus = true;
        } else if session.is_some() {
            view.page = tui::Page::Chat;
            view.setup_key.clear();
        } else {
            *exit = true;
        }
        return Ok(());
    }
    if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
        if !view.setup_tab_focus {
            view.setup_tab_fields[view.setup_tab] = view.setup_field;
        }
        view.setup_editing = false;
        view.setup_tab = if key.code == KeyCode::Tab {
            (view.setup_tab + 1) % 3
        } else {
            (view.setup_tab + 2) % 3
        };
        view.setup_tab_focus = true;
        return Ok(());
    }
    if view.setup_tab_focus {
        match key.code {
            KeyCode::Left | KeyCode::Up => view.setup_tab = (view.setup_tab + 2) % 3,
            KeyCode::Right | KeyCode::Down => view.setup_tab = (view.setup_tab + 1) % 3,
            KeyCode::Enter => {
                view.setup_tab_focus = false;
                view.setup_editing = false;
                view.setup_field = view.setup_tab_fields[view.setup_tab];
                view.cursor = match view.setup_field {
                    1 => view.setup_url.chars().count(),
                    2 => view.setup_key.chars().count(),
                    3 => view.setup_model.chars().count(),
                    4 => view.setup_reasoning.chars().count(),
                    _ => 0,
                };
                if view.setup_field == 3 && view.setup_models.is_empty() {
                    start_model_check(view, cfg, tx, request);
                }
            }
            _ => {}
        }
        return Ok(());
    }
    if !view.setup_editing {
        let fields: &[usize] = match view.setup_tab {
            0 => &[0, 1, 2],
            1 => &[3, 4, 7, 8],
            _ => &[5, 9, 6],
        };
        let index = fields
            .iter()
            .position(|field| *field == view.setup_field)
            .unwrap_or(0);
        match key.code {
            KeyCode::Up => view.setup_field = fields[index.saturating_sub(1)],
            KeyCode::Down => view.setup_field = fields[(index + 1).min(fields.len() - 1)],
            KeyCode::Enter if view.setup_field == 6 => {
                if let Err(e) = save_setup(cfg, view, session, tx, request) {
                    view.setup_status = format!("Cannot save: {e}");
                }
            }
            KeyCode::Enter => {
                view.setup_editing = true;
                view.cursor = match view.setup_field {
                    1 => view.setup_url.chars().count(),
                    2 => view.setup_key.chars().count(),
                    3 => view.setup_model.chars().count(),
                    4 => view.setup_reasoning.chars().count(),
                    _ => 0,
                };
            }
            _ => {}
        }
        if view.page == tui::Page::Setup {
            view.setup_tab_fields[view.setup_tab] = view.setup_field;
        }
        return Ok(());
    }
    match view.setup_field {
        0 => match key.code {
            KeyCode::Up => {
                view.setup_provider = (view.setup_provider + 3) % 4;
            }
            KeyCode::Down => {
                view.setup_provider = (view.setup_provider + 1) % 4;
            }
            KeyCode::Char('1'..='4') => {
                if let KeyCode::Char(ch) = key.code {
                    view.setup_provider = ch.to_digit(10).unwrap() as usize - 1;
                }
            }
            KeyCode::Enter => {
                let changed = view.setup_provider_committed != Some(view.setup_provider);
                view.setup_provider_committed = Some(view.setup_provider);
                if changed {
                    let previous = view.setup_url.clone();
                    change_setup_url(view, preset_url(view.setup_provider).into());
                    if view.setup_url != previous {
                        invalidate_setup_models(view, request);
                    }
                }
                view.setup_editing = false;
            }
            _ => {}
        },
        1 | 2 => {
            if key.code == KeyCode::Enter {
                view.setup_editing = false;
            } else if view.setup_field == 1 {
                let previous = view.setup_url.clone();
                edit_buffer(&mut view.setup_url, &mut view.cursor, key);
                if view.setup_url != previous {
                    invalidate_setup_models(view, request);
                    if !view.setup_key.is_empty() {
                        view.setup_key.clear();
                        view.setup_status =
                            "URL changed; re-enter the API key for this endpoint.".into();
                    }
                }
            } else {
                if key.code == KeyCode::Char('k') && key.modifiers.contains(KeyModifiers::CONTROL) {
                    view.setup_key.clear();
                    view.setup_clear_key = true;
                    view.cursor = 0;
                    view.setup_status = "Saved API key will be cleared on Save.".into();
                    invalidate_setup_models(view, request);
                    return Ok(());
                }
                let previous = view.setup_key.clone();
                edit_buffer(&mut view.setup_key, &mut view.cursor, key);
                if view.setup_key != previous {
                    view.setup_clear_key = false;
                    invalidate_setup_models(view, request);
                }
            }
        }
        3 => match key.code {
            KeyCode::Enter => {
                view.setup_editing = false;
            }
            KeyCode::Up | KeyCode::Down if !view.setup_models.is_empty() => {
                if view.setup_manual {
                    view.setup_manual = false;
                    view.setup_model_selected = view
                        .setup_models
                        .iter()
                        .position(|model| model == &view.setup_model)
                        .unwrap_or(0);
                } else if key.code == KeyCode::Up {
                    view.setup_model_selected = view.setup_model_selected.saturating_sub(1);
                } else {
                    view.setup_model_selected =
                        (view.setup_model_selected + 1).min(view.setup_models.len() - 1);
                }
                view.setup_model = view.setup_models[view.setup_model_selected].clone();
            }
            KeyCode::Left | KeyCode::Right if !view.setup_manual => {}
            KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                start_model_check(view, cfg, tx, request)
            }
            _ => {
                if !view.setup_manual {
                    view.setup_manual = true;
                    view.setup_model.clear();
                    view.cursor = 0;
                }
                edit_buffer(&mut view.setup_model, &mut view.cursor, key);
            }
        },
        4 => match key.code {
            KeyCode::Enter => {
                view.setup_editing = false;
            }
            KeyCode::Up | KeyCode::Down => {
                if view.setup_reasoning_manual {
                    view.setup_reasoning_manual = false;
                } else if key.code == KeyCode::Up {
                    view.setup_reasoning_selected =
                        (view.setup_reasoning_selected + REASONING_CHOICES.len() - 1)
                            % REASONING_CHOICES.len();
                } else {
                    view.setup_reasoning_selected =
                        (view.setup_reasoning_selected + 1) % REASONING_CHOICES.len();
                }
                view.setup_reasoning = REASONING_CHOICES[view.setup_reasoning_selected].into();
            }
            KeyCode::Left | KeyCode::Right if !view.setup_reasoning_manual => {}
            KeyCode::Char('1'..='6') if !view.setup_reasoning_manual => {
                if let KeyCode::Char(ch) = key.code {
                    view.setup_reasoning_selected = ch.to_digit(10).unwrap() as usize - 1;
                    view.setup_reasoning = REASONING_CHOICES[view.setup_reasoning_selected].into();
                }
            }
            _ => {
                if !view.setup_reasoning_manual {
                    view.setup_reasoning_manual = true;
                    view.setup_reasoning.clear();
                    view.cursor = 0;
                }
                edit_buffer(&mut view.setup_reasoning, &mut view.cursor, key);
            }
        },
        7 => match key.code {
            KeyCode::Up => {
                view.setup_decision_model_selected = (view.setup_decision_model_selected + 2) % 3;
            }
            KeyCode::Down => {
                view.setup_decision_model_selected = (view.setup_decision_model_selected + 1) % 3;
            }
            KeyCode::Enter => {
                view.setup_editing = false;
            }
            _ => {}
        },
        8 => match key.code {
            KeyCode::Up | KeyCode::Down => {
                view.setup_response_language_selected = 1 - view.setup_response_language_selected;
            }
            KeyCode::Enter => {
                view.setup_editing = false;
            }
            _ => {}
        },
        5 => match key.code {
            KeyCode::Up => view.setup_helper_selected = (view.setup_helper_selected + 2) % 3,
            KeyCode::Down => view.setup_helper_selected = (view.setup_helper_selected + 1) % 3,
            KeyCode::Char(' ') => {
                let selected = view.setup_helper_selected;
                view.setup_helpers[selected] = !view.setup_helpers[selected];
            }
            KeyCode::Char('1'..='3') => {
                if let KeyCode::Char(ch) = key.code {
                    let selected = ch.to_digit(10).unwrap() as usize - 1;
                    view.setup_helper_selected = selected;
                    view.setup_helpers[selected] = !view.setup_helpers[selected];
                }
            }
            KeyCode::Enter => {
                view.setup_editing = false;
            }
            _ => {}
        },
        9 => match key.code {
            KeyCode::Up => {
                view.setup_approval_mode_selected = (view.setup_approval_mode_selected + 3) % 4;
                view.setup_approval_mode_dirty = true;
            }
            KeyCode::Down => {
                view.setup_approval_mode_selected = (view.setup_approval_mode_selected + 1) % 4;
                view.setup_approval_mode_dirty = true;
            }
            KeyCode::Enter => view.setup_editing = false,
            _ => {}
        },
        _ => match key.code {
            KeyCode::Enter => {
                if let Err(e) = save_setup(cfg, view, session, tx, request) {
                    view.setup_status = format!("Cannot save: {e}");
                }
            }
            KeyCode::Up => view.setup_field = 5,
            KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                start_model_check(view, cfg, tx, request)
            }
            _ => {}
        },
    }
    if view.page == tui::Page::Setup {
        view.setup_tab_fields[view.setup_tab] = view.setup_field;
    }
    Ok(())
}

#[cfg(unix)]
extern "C" fn terminate_handler(_: libc::c_int) {
    TERMINATE_REQUESTED.store(true, Ordering::Release);
}

fn next_page(
    cfg: &mut Cfg,
    view: &mut tui::View,
    session: &mut Option<Session>,
    tx: &Sender<Event>,
    request: &mut u64,
) -> Result<(), String> {
    if cfg.base.is_empty() {
        setup_view(view, cfg);
    } else {
        activate_chat(cfg, view, session, tx)?;
        *request += 1;
        fetch_models(tx, *request, cfg.base.clone(), cfg.key.clone());
    }
    Ok(())
}

/// The marker stores only the canonical workspace identity. Settings and
/// credentials remain in their existing files, and another cwd gets its own
/// confirmation.
fn workspace_marker_path(state: &Path, cwd: &Path) -> PathBuf {
    let history = history_for(state, cwd);
    let name = history
        .path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy();
    history.path.with_file_name(format!("accepted-{name}"))
}

fn workspace_accepted(path: &Path, cwd: &Path) -> Result<bool, String> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|e| format!("{}: invalid workspace marker ({e})", path.display()))?;
    if value["version"] != 1 || value["cwd"].as_str() != Some(&history_for(Path::new("."), cwd).cwd)
    {
        return Err(format!(
            "{}: workspace marker does not match this directory",
            path.display()
        ));
    }
    Ok(value["accepted"] == true)
}

fn accept_workspace(path: &Path, cwd: &Path) -> Result<(), String> {
    let identity = history_for(Path::new("."), cwd).cwd;
    write_private(
        path,
        &json!({"version": 1, "cwd": identity, "accepted": true}),
        false,
    )
    .map_err(|e| format!("cannot remember workspace confirmation: {e}"))
}

#[allow(clippy::too_many_arguments)]
fn handle_workspace_key(
    key: KeyEvent,
    cfg: &mut Cfg,
    view: &mut tui::View,
    session: &mut Option<Session>,
    tx: &Sender<Event>,
    marker: &Path,
    request: &mut u64,
    exit: &mut bool,
) -> Result<(), String> {
    match key.code {
        KeyCode::Esc => *exit = true,
        KeyCode::Up | KeyCode::Down | KeyCode::Left | KeyCode::Right | KeyCode::Tab => {
            view.workspace_selected = 1 - view.workspace_selected
        }
        KeyCode::Char('y' | 'Y') => view.workspace_selected = 0,
        KeyCode::Char('n' | 'N') => view.workspace_selected = 1,
        KeyCode::Enter => {
            if view.workspace_selected == 0 {
                accept_workspace(marker, &cfg.cwd)?;
                next_page(cfg, view, session, tx, request)?;
            } else {
                *exit = true;
            }
        }
        _ => {}
    }
    Ok(())
}

fn handle_chat_key(
    key: KeyEvent,
    cfg: &mut Cfg,
    view: &mut tui::View,
    session: &mut Option<Session>,
    request: &mut u64,
    exit: &mut bool,
) {
    let Some(session) = session.as_mut() else {
        return;
    };
    if view.overlay == tui::Overlay::Approval {
        match key.code {
            KeyCode::Up => view.approval_scroll = view.approval_scroll.saturating_sub(1),
            KeyCode::Down => view.approval_scroll = view.approval_scroll.saturating_add(1),
            KeyCode::PageUp => view.approval_scroll = view.approval_scroll.saturating_sub(5),
            KeyCode::PageDown => view.approval_scroll = view.approval_scroll.saturating_add(5),
            KeyCode::Left | KeyCode::Right | KeyCode::Tab | KeyCode::BackTab => {
                view.approval_allow = !view.approval_allow
            }
            KeyCode::Esc | KeyCode::Enter => {
                let allow = key.code == KeyCode::Enter && view.approval_allow;
                if let Some(answer) = session.pending.take() {
                    let _ = answer.send(allow);
                }
                append(view, if allow { "Approved." } else { "Denied." });
                view.overlay = tui::Overlay::None;
            }
            _ => {}
        }
        return;
    }
    if view.overlay != tui::Overlay::None {
        if key.code == KeyCode::Esc {
            view.overlay = tui::Overlay::None;
        }
        return;
    }
    if key.code == KeyCode::BackTab
        || (key.code == KeyCode::Tab && key.modifiers.contains(KeyModifiers::SHIFT))
    {
        if session.pending.is_none() {
            let next = match cfg.approval_mode {
                ApprovalMode::Manual => ApprovalMode::Auto,
                ApprovalMode::Auto => ApprovalMode::Bypass,
                ApprovalMode::Bypass => ApprovalMode::Plan,
                ApprovalMode::Plan => ApprovalMode::Manual,
            };
            set_approval_mode(next, cfg, view, session);
        }
        return;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('d') {
        session.close(&mut |message| append(view, message));
        *exit = true;
        return;
    }
    if view.slash_suggestions_open {
        let matches = tui::slash_matches(&view.input);
        if !matches.is_empty() {
            match key.code {
                KeyCode::Up => {
                    view.slash_suggestion_selected =
                        view.slash_suggestion_selected.saturating_sub(1);
                    return;
                }
                KeyCode::Down => {
                    view.slash_suggestion_selected =
                        (view.slash_suggestion_selected + 1).min(matches.len() - 1);
                    return;
                }
                KeyCode::Tab | KeyCode::Enter if key.modifiers.is_empty() => {
                    let command = matches[view.slash_suggestion_selected.min(matches.len() - 1)];
                    view.input = command.name.into();
                    if command.takes_argument {
                        view.input.push(' ');
                    }
                    view.cursor = view.input.chars().count();
                    view.slash_suggestions_open = false;
                    return;
                }
                KeyCode::Esc => {
                    view.slash_suggestions_open = false;
                    return;
                }
                _ => {}
            }
        }
    }
    match key.code {
        KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            view.input.insert(byte_at(&view.input, view.cursor), '\n');
            view.cursor += 1;
            view.slash_suggestions_open = false;
        }
        KeyCode::Enter
            if key
                .modifiers
                .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) =>
        {
            view.input.insert(byte_at(&view.input, view.cursor), '\n');
            view.cursor += 1;
            view.slash_suggestions_open = false;
        }
        KeyCode::Enter => {
            let line = std::mem::take(&mut view.input);
            view.cursor = 0;
            view.slash_suggestions_open = false;
            submit_chat(line, cfg, view, session, request, exit);
        }
        KeyCode::PageUp => view.scroll = view.scroll.saturating_add(8),
        KeyCode::PageDown => view.scroll = view.scroll.saturating_sub(8),
        KeyCode::Up => {
            if let Some(history) = session.prompt_history.as_mut()
                && let Some(input) = history.up(&view.input)
            {
                view.cursor = input.chars().count();
                view.input = input;
            }
        }
        KeyCode::Down => {
            if let Some(history) = session.prompt_history.as_mut()
                && let Some(input) = history.down()
            {
                view.cursor = input.chars().count();
                view.input = input;
            }
        }
        KeyCode::Esc => {
            view.input.clear();
            view.cursor = 0;
            view.slash_suggestions_open = false;
        }
        _ => {
            let previous = view.input.clone();
            edit_buffer(&mut view.input, &mut view.cursor, key);
            if view.input != previous {
                let matches = tui::slash_matches(&view.input);
                view.slash_suggestions_open =
                    !matches.is_empty() && !(matches.len() == 1 && matches[0].name == view.input);
                view.slash_suggestion_selected = 0;
            }
        }
    }
}

fn handle_worker_event(
    event: Event,
    view: &mut tui::View,
    session: &mut Option<Session>,
    request: u64,
) {
    match event {
        Event::Progress(message) => append(view, &message),
        Event::ModelPhase(active) => {
            let in_flight = active
                && session
                    .as_ref()
                    .is_some_and(|session| session.busy && !session.interrupted && !session.closed);
            if in_flight && !view.model_in_flight {
                view.thinking_started = Some(std::time::Instant::now());
            } else if !in_flight {
                view.thinking_started = None;
            }
            view.model_in_flight = in_flight;
            view.thinking_text.clear();
        }
        Event::ReasoningChunk(chunk) => {
            if view.model_in_flight
                && session
                    .as_ref()
                    .is_some_and(|session| session.busy && !session.interrupted && !session.closed)
            {
                append_reasoning(view, &chunk);
            }
        }
        Event::Approval {
            what,
            preview,
            answer,
        } => {
            if let Some(session) = session.as_mut() {
                if session.closed || session.pending.is_some() {
                    let _ = answer.send(false);
                } else {
                    session.pending = Some(answer);
                    view.page = tui::Page::Chat;
                    view.approval_what = what;
                    view.approval_preview = preview;
                    view.approval_scroll = 0;
                    view.approval_allow = false;
                    view.overlay = tui::Overlay::Approval;
                    // An approval can only be made by a key pressed after the modal appears.
                    while terminal_event::poll(Duration::ZERO).unwrap_or(false) {
                        let _ = terminal_event::read();
                    }
                }
            } else {
                let _ = answer.send(false);
            }
        }
        Event::Finished(result, msgs) => {
            view.model_in_flight = false;
            view.thinking_started = None;
            view.thinking_text.clear();
            if let Some(session) = session.as_mut() {
                complete_turn(view, session);
                session.finished(result, msgs, &mut |message| append(view, message));
                if session.busy {
                    begin_turn(view, session);
                }
            }
        }
        Event::ModelsFetched {
            request: result_request,
            result,
        } if result_request == request => match result {
            Ok(models) => {
                view.connection = "connected".into();
                view.setup_status = if models.is_empty() {
                    "Connected; no model IDs returned. Enter one manually or use server default."
                        .into()
                } else {
                    format!("Connected; {} models available.", models.len())
                };
                if view.page == tui::Page::Setup {
                    view.setup_models = models;
                    if !view.setup_manual && !view.setup_models.is_empty() {
                        if let Some(index) = view
                            .setup_models
                            .iter()
                            .position(|model| model == &view.setup_model)
                        {
                            view.setup_model_selected = index;
                        } else if view.setup_model.is_empty() {
                            view.setup_model_selected = 0;
                        } else {
                            view.setup_manual = true;
                        }
                        if !view.setup_manual {
                            view.setup_model = view.setup_models[view.setup_model_selected].clone();
                        }
                    }
                }
            }
            Err(e) => {
                view.connection = "unreachable".into();
                view.setup_status =
                    format!("Connection failed: {e}. Edit URL/key or press Ctrl+R to retry.");
                if view.page == tui::Page::Chat {
                    append(
                        view,
                        "API is unreachable. Use /config to change the URL or key.",
                    );
                }
            }
        },
        _ => {}
    }
}

/// Owns the terminal until the session ends. Model and network work stays on
/// worker threads; only this loop reads keys and answers approvals.
pub(crate) fn run_tui(mut cfg: Cfg) -> Result<(), String> {
    let launch = cfg.cwd.canonicalize().map_err(|e| e.to_string())?;
    cfg.cwd = launch.clone();
    let workspace_marker = workspace_marker_path(&state_dir()?, &launch);
    let previously_accepted = workspace_accepted(&workspace_marker, &launch)?;
    let (tx, rx) = mpsc::channel();
    let mut view = tui::View {
        cwd: launch.display().to_string(),
        model: cfg.model.clone().unwrap_or_else(|| "server default".into()),
        approval_mode: approval_mode_name(cfg.approval_mode).into(),
        connection: if cfg.base.is_empty() {
            "setup required".into()
        } else {
            "checking API".into()
        },
        ..tui::View::default()
    };
    let mut session: Option<Session> = None;
    let mut request = 0;
    view.page = tui::Page::Workspace;
    view.workspace_selected = 0;
    view.status = "Confirm the workspace used for tools and saved history".into();
    SIGINT_REQUESTED.store(false, Ordering::Release);
    TERMINATE_REQUESTED.store(false, Ordering::Release);
    #[cfg(unix)]
    unsafe {
        libc::signal(
            libc::SIGINT,
            sigint_handler as *const () as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGTERM,
            terminate_handler as *const () as libc::sighandler_t,
        );
    }
    let mut terminal = TerminalGuard::enter()?;
    if previously_accepted {
        next_page(&mut cfg, &mut view, &mut session, &tx, &mut request)?;
    }
    let mut exit = false;
    let mut dirty = true;
    let mut last_activity_tick = std::time::Instant::now();
    let mut last_snapshot = Vec::new();
    let mut terminal_error = None;
    while !exit {
        if TERMINATE_REQUESTED.swap(false, Ordering::AcqRel) {
            if let Some(session) = session.as_mut() {
                session.close(&mut |message| append(&mut view, message));
            }
            break;
        }
        if SIGINT_REQUESTED.swap(false, Ordering::AcqRel) {
            view.model_in_flight = false;
            view.thinking_started = None;
            view.thinking_text.clear();
            if let Some(session) = session.as_mut() {
                session.interrupt_turn(&mut |message| append(&mut view, message));
                view.overlay = tui::Overlay::None;
                dirty = true;
            } else {
                break;
            }
        }
        while let Ok(event) = rx.try_recv() {
            handle_worker_event(event, &mut view, &mut session, request);
            dirty = true;
        }
        if session.as_ref().is_some_and(|s| s.closed) {
            break;
        }
        if let Some(session) = session.as_ref() {
            let main = &session.agents.snapshot()[0];
            let status = format!("{}  |  {} queued", main.status.name(), session.queued.len());
            if view.status != status {
                view.status = status;
                dirty = true;
            }
        }
        let snapshot = session
            .as_ref()
            .map(|s| s.agents.snapshot())
            .unwrap_or_default();
        if snapshot != last_snapshot {
            last_snapshot = snapshot.clone();
            dirty = true;
        }
        if (view.model_in_flight || view.turn_started.is_some())
            && last_activity_tick.elapsed() >= Duration::from_millis(200)
        {
            view.activity_tick = view.activity_tick.wrapping_add(1);
            last_activity_tick = std::time::Instant::now();
            dirty = true;
        }
        if dirty {
            if let Err(e) = terminal
                .terminal
                .draw(|frame| tui::render(frame, &view, &snapshot))
            {
                terminal_error = Some(e.to_string());
                break;
            }
            dirty = false;
        }
        let ready = match terminal_event::poll(Duration::from_millis(50)) {
            Ok(ready) => ready,
            Err(e) => {
                terminal_error = Some(e.to_string());
                break;
            }
        };
        if !ready {
            continue;
        }
        let input = match terminal_event::read() {
            Ok(input) => input,
            Err(e) => {
                terminal_error = Some(e.to_string());
                break;
            }
        };
        if matches!(input, TerminalEvent::Resize(_, _)) {
            dirty = true;
            continue;
        }
        let TerminalEvent::Key(key) = input else {
            continue;
        };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        dirty = true;
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            view.model_in_flight = false;
            view.thinking_started = None;
            view.thinking_text.clear();
            if let Some(session) = session.as_mut() {
                session.interrupt_turn(&mut |message| append(&mut view, message));
                view.overlay = tui::Overlay::None;
            } else {
                exit = true;
            }
            continue;
        }
        if view.overlay == tui::Overlay::Approval {
            handle_chat_key(
                key,
                &mut cfg,
                &mut view,
                &mut session,
                &mut request,
                &mut exit,
            );
            continue;
        }
        match view.page {
            tui::Page::Workspace => {
                if let Err(e) = handle_workspace_key(
                    key,
                    &mut cfg,
                    &mut view,
                    &mut session,
                    &tx,
                    &workspace_marker,
                    &mut request,
                    &mut exit,
                ) {
                    view.status = e;
                }
            }
            tui::Page::Setup => {
                if let Err(e) = handle_setup_key(
                    key,
                    &mut cfg,
                    &mut view,
                    &mut session,
                    &tx,
                    &mut request,
                    &mut exit,
                ) {
                    view.setup_status = e;
                }
            }
            tui::Page::Chat => handle_chat_key(
                key,
                &mut cfg,
                &mut view,
                &mut session,
                &mut request,
                &mut exit,
            ),
        }
    }
    if let Some(session) = session.as_mut()
        && !session.closed
    {
        session.close(&mut |_| {});
    }
    drop(terminal);
    terminal_error.map_or(Ok(()), Err)
}

#[cfg(test)]
fn banner() -> Vec<String> {
    let title = format!(
        "Revex {}  |  Local AI Coding Agent",
        env!("CARGO_PKG_VERSION")
    );
    let edge = format!("+{}+", "-".repeat(title.len() + 2));
    vec![edge.clone(), format!("| {title} |"), edge]
}

#[cfg(test)]
fn safe_field(value: &str, limit: usize) -> String {
    let clean: String = value
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect();
    short(&clean, limit)
}

#[cfg(test)]
fn history_label(msgs: &[Value]) -> String {
    let turns = msgs
        .iter()
        .filter(|m| {
            m["role"] == "user" && !m["content"].as_str().unwrap_or("").starts_with("<out>")
        })
        .count();
    if turns == 0 {
        "new session".into()
    } else {
        format!(
            "resumed {turns} user turn(s), {} saved messages",
            msgs.len().saturating_sub(1)
        )
    }
}

#[cfg(test)]
#[allow(dead_code)]
fn history_for_welcome(cwd: &Path) -> String {
    let result = state_dir()
        .map(|dir| history_for(&dir, cwd))
        .and_then(|h| load_history(&h));
    match result {
        Ok(msgs) => history_label(&msgs),
        Err(_) => "saved history needs attention".into(),
    }
}

#[cfg(test)]
fn setup_welcome(cwd: &Path, history: &str) -> Vec<String> {
    let mut lines = banner();
    lines.extend([
        format!("Project  {}", safe_field(&cwd.display().to_string(), 100)),
        format!("History  {}", safe_field(history, 100)),
        "API      setup required".into(),
        "Model    not selected".into(),
        "Commands after setup  /agents  /status  /model  /help  /exit".into(),
        String::new(),
        "Setup  Connect a running OpenAI-compatible API to continue.".into(),
        "       Example: http://127.0.0.1:1234/v1 (only if your server uses it).".into(),
    ]);
    lines
}

#[cfg(test)]
#[allow(dead_code)]
fn connection_status(base: &str, key: &str) -> String {
    match ureq::get(&format!("{base}/models"))
        .set("Authorization", &format!("Bearer {key}"))
        .timeout(Duration::from_secs(1))
        .call()
    {
        Ok(_) => "connected".into(),
        Err(ureq::Error::Status(401 | 403, _)) => "reachable (authorization needed)".into(),
        Err(ureq::Error::Status(404, _)) => "reachable (model list unavailable)".into(),
        Err(ureq::Error::Status(code, _)) => format!("reachable (HTTP {code})"),
        Err(_) => "unreachable".into(),
    }
}

#[cfg(test)]
fn welcome(cfg: &Cfg, msgs: &[Value], connection: &str) -> Vec<String> {
    let mut lines = banner();
    lines.extend([
        format!(
            "Project  {}",
            safe_field(&cfg.cwd.display().to_string(), 100)
        ),
        format!("History  {}", history_label(msgs)),
        format!("API      {}  |  {}", connection, safe_field(&cfg.base, 100)),
        format!(
            "Model    {}",
            safe_field(cfg.model.as_deref().unwrap_or("server default"), 100)
        ),
        "Commands /agents  /status  /model ID  /help  /clear  /exit".into(),
        "Settings Type s below to edit the API URL, key, or model.".into(),
        String::new(),
        "Enter a task, press Enter for the prompt, or type s for settings.".into(),
    ]);
    lines
}

/// The first interactive launch stores the API endpoint in the user's private
/// settings file. A blank optional field leaves any existing value alone.
#[cfg(test)]
#[allow(dead_code)]
pub(crate) fn first_run_setup() -> Result<Option<()>, String> {
    let path = home_file().ok_or("HOME is not set; cannot save ~/.revex")?;
    let cwd = std::env::current_dir()
        .and_then(|d| d.canonicalize())
        .map_err(|e| e.to_string())?;
    for line in setup_welcome(&cwd, &history_for_welcome(&cwd)) {
        eprintln!("{line}");
    }
    let result = first_run_with(&path, ask_terminal)?;
    if result.is_some() {
        eprintln!(
            "setup saved to {}",
            safe_field(&path.display().to_string(), 100)
        );
    } else {
        eprintln!("setup cancelled");
    }
    Ok(result)
}

#[cfg(test)]
#[allow(dead_code)]
fn ask_terminal(prompt: &str, secret: bool) -> Result<Option<String>, String> {
    use std::io::Write;
    let hidden = if secret {
        match EchoGuard::hide() {
            Ok(guard) => Some(guard),
            Err(_) => {
                eprintln!(
                    "hidden key input is unavailable; skipping it. Set REVEX_API_KEY instead."
                );
                return Ok(Some(String::new()));
            }
        }
    } else {
        None
    };
    eprint!("{prompt}");
    std::io::stderr().flush().map_err(|e| e.to_string())?;
    let mut line = String::new();
    let n = std::io::stdin()
        .read_line(&mut line)
        .map_err(|e| e.to_string())?;
    drop(hidden);
    if secret {
        eprintln!();
    }
    Ok((n > 0).then(|| line.trim().to_string()))
}

#[cfg(test)]
#[allow(dead_code)]
fn edit_settings(cfg: &mut Cfg) -> Result<(), String> {
    let path = home_file().ok_or("HOME is not set; cannot save ~/.revex")?;
    eprintln!();
    eprintln!("Settings  Enter a new value or press Enter to keep the current value.");
    eprintln!("Current API    {}", safe_field(&cfg.base, 100));
    eprintln!(
        "Current model  {}",
        safe_field(cfg.model.as_deref().unwrap_or("server default"), 100)
    );
    eprintln!("The API key is never displayed. A blank key keeps the current value.");
    if !edit_settings_with(cfg, &path, ask_terminal)? {
        eprintln!("settings unchanged");
        return Ok(());
    }
    eprintln!(
        "settings saved to {}",
        safe_field(&path.display().to_string(), 100)
    );
    if ["REVEX_BASE_URL", "REVEX_API_KEY", "REVEX_MODEL"]
        .iter()
        .any(|key| std::env::var_os(key).is_some())
    {
        eprintln!(
            "note: REVEX_* environment settings may override saved values on the next launch"
        );
    }
    Ok(())
}

#[cfg(test)]
fn edit_settings_with(
    cfg: &mut Cfg,
    path: &Path,
    mut ask: impl FnMut(&str, bool) -> Result<Option<String>, String>,
) -> Result<bool, String> {
    let previous_base = cfg.base.clone();
    let mut field = 0;
    let mut changed_key = false;
    let mut changed_model = false;
    let updated = first_run_with(path, |prompt, secret| {
        field += 1;
        let response = ask(prompt, secret)?;
        if field == 2 && response.as_ref().is_some_and(|value| !value.is_empty()) {
            changed_key = true;
        }
        if field == 3 && response.as_ref().is_some_and(|value| !value.is_empty()) {
            changed_model = true;
        }
        if field == 1 && response.as_deref() == Some("") {
            Ok(Some(previous_base.clone()))
        } else {
            Ok(response)
        }
    })?;
    if updated.is_none() {
        return Ok(false);
    }
    let settings = read_settings(path)?;
    cfg.base = settings["base_url"]
        .as_str()
        .unwrap_or(&cfg.base)
        .to_string();
    if changed_key && let Some(key) = settings["api_key"].as_str() {
        cfg.key = key.to_string();
    }
    if changed_model && let Some(model) = settings["model"].as_str() {
        cfg.model = Some(model.to_string());
    }
    Ok(true)
}

#[cfg(unix)]
#[cfg(test)]
#[allow(dead_code)]
struct EchoGuard(Option<libc::termios>);

#[cfg(unix)]
#[cfg(test)]
#[allow(dead_code)]
impl EchoGuard {
    fn hide() -> Result<Self, String> {
        let mut old = std::mem::MaybeUninit::<libc::termios>::uninit();
        if unsafe { libc::tcgetattr(libc::STDIN_FILENO, old.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let old = unsafe { old.assume_init() };
        let mut hidden = old;
        hidden.c_lflag &= !libc::ECHO;
        if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &hidden) } != 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        Ok(Self(Some(old)))
    }
}

#[cfg(unix)]
#[cfg(test)]
impl Drop for EchoGuard {
    fn drop(&mut self) {
        if let Some(old) = &self.0 {
            unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, old) };
        }
    }
}

#[cfg(not(unix))]
#[cfg(test)]
#[allow(dead_code)]
struct EchoGuard;

#[cfg(not(unix))]
#[cfg(test)]
#[allow(dead_code)]
impl EchoGuard {
    fn hide() -> Result<Self, String> {
        Err("hidden terminal input is unavailable".into())
    }
}

#[cfg(test)]
fn first_run_with(
    path: &Path,
    mut ask: impl FnMut(&str, bool) -> Result<Option<String>, String>,
) -> Result<Option<()>, String> {
    let base = loop {
        let Some(raw) = ask("  1/3  API base URL (or /exit): ", false)? else {
            return Ok(None);
        };
        if raw == "/exit" {
            return Ok(None);
        }
        let value = raw.trim().trim_end_matches('/');
        let host = value
            .strip_prefix("http://")
            .or_else(|| value.strip_prefix("https://"));
        if host.is_some_and(|s| {
            !s.is_empty() && !s.chars().any(|ch| ch.is_whitespace() || ch.is_control())
        }) {
            break value.to_string();
        }
        eprintln!("enter a full http:// or https:// API base URL");
    };
    let Some(key) = ask("  2/3  API key (optional, hidden; Enter to skip): ", true)? else {
        return Ok(None);
    };
    if key == "/exit" {
        return Ok(None);
    }
    let Some(model) = ask(
        "  3/3  Model ID (optional; Enter for server default): ",
        false,
    )?
    else {
        return Ok(None);
    };
    if model == "/exit" {
        return Ok(None);
    }
    let mut settings = read_settings(path)?;
    settings.insert("base_url".into(), Value::String(base));
    if !key.trim().is_empty() {
        settings.insert("api_key".into(), Value::String(key.trim().into()));
    }
    if !model.trim().is_empty() {
        settings.insert("model".into(), Value::String(model.trim().into()));
    }
    write_settings_file(path, &settings)?;
    Ok(Some(()))
}

#[cfg(unix)]
extern "C" fn sigint_handler(_: libc::c_int) {
    SIGINT_REQUESTED.store(true, Ordering::Release);
}

/// Run the terminal session. The input reader sends lines; it makes no approval
/// decisions, and an in-flight model call cannot hold up `/agents` or `/status`.
#[cfg(test)]
#[allow(dead_code)]
pub(crate) fn run(mut cfg: Cfg, msgs: Vec<Value>, agents: Arc<Agents>, show_welcome: bool) {
    let connection = connection_status(&cfg.base, &cfg.key);
    let mut first_input = None;
    if show_welcome {
        for line in welcome(&cfg, &msgs, &connection) {
            eprintln!("{line}");
        }
        match ask_terminal("start > ", false) {
            Ok(Some(choice)) if choice.eq_ignore_ascii_case("s") || choice == "/config" => {
                if let Err(e) = edit_settings(&mut cfg) {
                    eprintln!("settings not changed: {e}");
                }
                let connection = connection_status(&cfg.base, &cfg.key);
                eprintln!("API      {}  |  {}", connection, safe_field(&cfg.base, 100));
                eprintln!(
                    "Model    {}",
                    safe_field(cfg.model.as_deref().unwrap_or("server default"), 100)
                );
                eprintln!("Enter a task to begin.");
            }
            Ok(Some(choice)) if !choice.is_empty() => first_input = Some(choice),
            Ok(Some(_)) => {}
            Ok(None) => {
                agents.shutdown();
                return;
            }
            Err(e) => {
                eprintln!("input error: {e}");
                agents.shutdown();
                return;
            }
        }
    } else {
        eprintln!("API      {}  |  {}", connection, safe_field(&cfg.base, 100));
        eprintln!(
            "Model    {}",
            safe_field(cfg.model.as_deref().unwrap_or("server default"), 100)
        );
        eprintln!("Setup complete. Enter a task to begin.");
    }
    SIGINT_REQUESTED.store(false, Ordering::Release);
    #[cfg(unix)]
    unsafe {
        libc::signal(
            libc::SIGINT,
            sigint_handler as *const () as libc::sighandler_t,
        );
    }
    let (tx, rx) = mpsc::channel();
    if let Some(input) = first_input {
        let _ = tx.send(Event::Input(input));
    }
    let input_tx = tx.clone();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            match line {
                Ok(s) => {
                    if input_tx.send(Event::Input(s)).is_err() {
                        return;
                    }
                }
                Err(_) => break,
            }
        }
        let _ = input_tx.send(Event::Eof);
    });
    run_loop(cfg, msgs, agents, tx, rx, &SIGINT_REQUESTED, &mut |s| {
        let safe = terminal_safe(s);
        if s == "> " {
            eprint!("{safe}");
        } else {
            eprintln!("{safe}");
        }
    });
}

fn terminal_safe(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch == '\n' || ch == '\t' || !ch.is_control() {
                ch
            } else {
                ' '
            }
        })
        .collect()
}

const MAX_PROMPT_HISTORY: usize = 100;
const MAX_SAVED_PROMPT_BYTES: usize = 16 * 1024;

struct PromptHistory {
    path: PathBuf,
    cwd: String,
    entries: Vec<String>,
    position: Option<usize>,
    draft: String,
}

impl PromptHistory {
    fn path_for(history: &History) -> PathBuf {
        let name = history
            .path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy();
        history.path.with_file_name(format!("input-{name}"))
    }

    fn load(history: &History) -> Result<Self, String> {
        let path = Self::path_for(history);
        let entries = match std::fs::read(&path) {
            Ok(bytes) => {
                let value: Value = serde_json::from_slice(&bytes)
                    .map_err(|e| format!("{}: {e}", path.display()))?;
                if value["version"].as_u64() != Some(1)
                    || value["cwd"].as_str() != Some(&history.cwd)
                {
                    return Err(format!(
                        "{}: input history version or workspace mismatch",
                        path.display()
                    ));
                }
                let entries = value["entries"]
                    .as_array()
                    .ok_or_else(|| format!("{}: invalid input history", path.display()))?;
                if entries.len() > MAX_PROMPT_HISTORY
                    || entries
                        .iter()
                        .any(|e| e.as_str().is_none_or(|s| s.len() > MAX_SAVED_PROMPT_BYTES))
                {
                    return Err(format!("{}: invalid input history entries", path.display()));
                }
                entries
                    .iter()
                    .map(|e| e.as_str().unwrap().to_string())
                    .collect()
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        Ok(Self {
            path,
            cwd: history.cwd.clone(),
            entries,
            position: None,
            draft: String::new(),
        })
    }

    fn save(&self) -> Result<(), String> {
        write_private(
            &self.path,
            &json!({"version":1,"cwd":self.cwd,"entries":self.entries}),
            false,
        )
        .map_err(|e| format!("{}: {e}", self.path.display()))
    }

    fn record(&mut self, input: &str) -> Result<(), String> {
        self.position = None;
        self.draft.clear();
        if input.len() > MAX_SAVED_PROMPT_BYTES {
            return Err("input exceeds 16 KiB prompt-history limit".into());
        }
        self.entries.push(input.into());
        if self.entries.len() > MAX_PROMPT_HISTORY {
            self.entries.remove(0);
        }
        self.save()?;
        Ok(())
    }

    fn up(&mut self, current: &str) -> Option<String> {
        if self.entries.is_empty() {
            return None;
        }
        let index = match self.position {
            Some(index) => index.saturating_sub(1),
            None => {
                self.draft = current.into();
                self.entries.len() - 1
            }
        };
        self.position = Some(index);
        Some(self.entries[index].clone())
    }

    fn down(&mut self) -> Option<String> {
        let index = self.position?;
        if index + 1 < self.entries.len() {
            self.position = Some(index + 1);
            Some(self.entries[index + 1].clone())
        } else {
            self.position = None;
            Some(self.draft.clone())
        }
    }
}

struct Session {
    cfg: Cfg,
    msgs: Option<Vec<Value>>,
    agents: Arc<Agents>,
    events: Sender<Event>,
    cancelled: Arc<AtomicBool>,
    busy: bool,
    pending: Option<Sender<bool>>,
    queued: VecDeque<String>,
    closed: bool,
    interrupted: bool,
    prompt_history: Option<PromptHistory>,
    helper_selection: [bool; 3],
    tui: bool,
}

impl Session {
    fn new(mut cfg: Cfg, msgs: Vec<Value>, agents: Arc<Agents>, events: Sender<Event>) -> Self {
        let cancelled = Arc::new(AtomicBool::new(false));
        cfg.quiet = true; // streamed text is emitted by the coordinator with an agent label
        cfg.ui = Some(UiHandle {
            events: events.clone(),
            cancelled: cancelled.clone(),
            suppress_routine: false,
            #[cfg(unix)]
            shell: Arc::new(ShellTracker::default()),
        });
        Self {
            cfg,
            msgs: Some(msgs),
            agents,
            events,
            cancelled,
            busy: false,
            pending: None,
            queued: VecDeque::new(),
            closed: false,
            interrupted: false,
            prompt_history: None,
            helper_selection: [true, false, false],
            tui: false,
        }
    }

    fn interrupt_turn(&mut self, out: &mut dyn FnMut(&str)) {
        if !self.busy {
            out("No task is running. Ctrl+D exits Revex.");
            return;
        }
        self.cancelled.store(true, Ordering::Release);
        #[cfg(unix)]
        if let Some(ui) = &self.cfg.ui
            && let Err(e) = ui.stop_shell()
        {
            out(&format!("error: {e}"));
        }
        if let Some(answer) = self.pending.take() {
            let _ = answer.send(false);
        }
        if let Err(e) = self.agents.parent_interrupted() {
            out(&format!("error: {e}"));
        }
        self.agents.shutdown();
        self.queued.clear();
        self.interrupted = true;
        out("Interrupt requested; waiting for the current model request to stop.");
    }

    fn reset_after_interrupt(&mut self, out: &mut dyn FnMut(&str)) {
        let Some(history) = &self.cfg.history else {
            return;
        };
        match Agents::open(agents_dir(history), model_fn(chat)) {
            Ok(agents) => {
                if let Err(e) = agents.set_providers(self.helper_selection) {
                    out(&format!(
                        "cannot resume helper settings after interruption: {e}"
                    ));
                    self.closed = true;
                    return;
                }
                let warning_events = self.events.clone();
                agents.set_error_sink(move |message| {
                    let _ = warning_events.send(Event::Progress(message));
                });
                self.agents = agents.clone();
                self.cfg.role = Role::Parent(Some(agents));
                self.cancelled = Arc::new(AtomicBool::new(false));
                self.cfg.ui = Some(UiHandle {
                    events: self.events.clone(),
                    cancelled: self.cancelled.clone(),
                    suppress_routine: self.tui,
                    #[cfg(unix)]
                    shell: Arc::new(ShellTracker::default()),
                });
                out("Task interrupted. Ready for another task.");
            }
            Err(e) => {
                out(&format!("cannot resume session after interruption: {e}"));
                self.closed = true;
            }
        }
    }

    fn close(&mut self, out: &mut dyn FnMut(&str)) {
        self.cancelled.store(true, Ordering::Release);
        #[cfg(unix)]
        if let Some(ui) = &self.cfg.ui
            && let Err(e) = ui.stop_shell()
        {
            out(&format!("error: {e}"));
        }
        if let Some(answer) = self.pending.take() {
            let _ = answer.send(false);
        }
        if let Err(e) = self.agents.parent_interrupted() {
            out(&format!("error: {e}"));
        }
        self.agents.shutdown();
        self.closed = true;
    }

    fn start(&mut self, task: String, out: &mut dyn FnMut(&str)) {
        let Some(mut msgs) = self.msgs.take() else {
            return;
        };
        let cfg = self.cfg.clone();
        let events = self.events.clone();
        self.busy = true;
        if !self.tui {
            out(&format!("[main] started: {}", short(&task, 100)));
        }
        let started = std::thread::Builder::new()
            .name("revex-main".into())
            .spawn(move || {
                let result = agent(&cfg, &mut msgs, &task);
                let _ = events.send(Event::Finished(result, msgs));
            });
        if let Err(e) = started {
            self.busy = false;
            // The closure, including msgs, is returned in the spawn error.
            // A thread spawn failure is fatal for this interactive session.
            out(&format!("error: cannot start parent worker: {e}"));
            self.close(out);
        }
    }

    fn line(&mut self, line: &str, out: &mut dyn FnMut(&str)) {
        let line = line.trim();
        if line == "/exit" {
            self.close(out);
            return;
        }
        if line == "/agents" {
            for info in self.agents.snapshot() {
                out(&agent_line(&info));
            }
            return;
        }
        if line == "/status" {
            let parent = &self.agents.snapshot()[0];
            out(&format!(
                "api={} base={} model={} route={} frontier={} key={} main={} queued_tasks={} cwd={}",
                if self.cfg.api == Api::Chat {
                    "chat"
                } else {
                    "responses"
                },
                self.cfg.base,
                self.cfg.model.as_deref().unwrap_or("(server default)"),
                route_name(self.cfg.route),
                self.cfg.frontier.as_deref().unwrap_or("(none)"),
                if self.cfg.key.is_empty() {
                    "not set"
                } else {
                    "set"
                },
                parent.status.name(),
                self.queued.len(),
                self.cfg.cwd.display()
            ));
            return;
        }
        if line == "/help" {
            out(HELP);
            return;
        }
        if let Some(answer) = self.pending.as_ref() {
            match line.to_ascii_lowercase().as_str() {
                "y" | "yes" => {
                    let _ = answer.send(true);
                    self.pending = None;
                    out("approved");
                }
                "" | "n" | "no" => {
                    let _ = answer.send(false);
                    self.pending = None;
                    out("denied");
                }
                _ => {
                    self.queued.push_back(line.into());
                    out("queued input; answer the pending approval with y or n");
                }
            }
            return;
        }
        if line.is_empty() {
            return;
        }
        if self.busy {
            self.queued.push_back(line.into());
            out(&format!("[main] queued input ({})", self.queued.len()));
            return;
        }
        if line.starts_with('/') {
            if let Some(msgs) = self.msgs.as_mut() {
                // Existing settings/history commands are only run when the worker
                // has returned ownership of the parent history.
                command(&mut self.cfg, msgs, line);
            }
        } else {
            self.start(line.into(), out);
        }
    }

    fn finished(
        &mut self,
        result: Result<AgentAnswer, String>,
        msgs: Vec<Value>,
        out: &mut dyn FnMut(&str),
    ) {
        self.busy = false;
        self.msgs = Some(msgs);
        if self.interrupted {
            self.interrupted = false;
            self.reset_after_interrupt(out);
            return;
        }
        match result {
            Ok(answer) => out(&format!(
                "{}{}",
                if self.tui { "Revex  " } else { "[main] " },
                answer.text()
            )),
            Err(e) => {
                out(&format!(
                    "{}error: {e}",
                    if self.tui { "" } else { "[main] " }
                ));
                if e.starts_with("history not saved:") {
                    self.close(out);
                    return;
                }
            }
        }
        while !self.busy && !self.closed {
            let Some(line) = self.queued.pop_front() else {
                break;
            };
            self.line(&line, out);
        }
    }
}

#[cfg(test)]
fn run_loop(
    cfg: Cfg,
    msgs: Vec<Value>,
    agents: Arc<Agents>,
    tx: Sender<Event>,
    rx: Receiver<Event>,
    interrupt: &AtomicBool,
    out: &mut dyn FnMut(&str),
) {
    let mut session = Session::new(cfg, msgs, agents, tx);
    out("> ");
    while !session.closed {
        if interrupt.swap(false, Ordering::AcqRel) {
            out("interrupted");
            session.close(out);
            break;
        }
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::Input(line)) => session.line(&line, out),
            Ok(Event::Eof) => session.close(out),
            Ok(Event::Progress(message)) => out(&message),
            Ok(Event::ModelPhase(_)) | Ok(Event::ReasoningChunk(_)) => {}
            Ok(Event::Approval {
                what,
                preview,
                answer,
            }) => {
                if session.closed || session.pending.is_some() {
                    let _ = answer.send(false);
                } else {
                    out(&format!("[main] approval: {what}"));
                    if !preview.is_empty() {
                        out(&preview);
                    }
                    out("approve? [y/N] ");
                    session.pending = Some(answer);
                }
            }
            Ok(Event::Finished(result, msgs)) => session.finished(result, msgs, out),
            Ok(Event::ModelsFetched { .. }) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => session.close(out),
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
        }
        if !session.closed && session.pending.is_none() {
            out("> ");
        }
    }
}

fn short(s: &str, limit: usize) -> String {
    if s.chars().count() <= limit {
        return s.into();
    }
    format!("{}...", s.chars().take(limit).collect::<String>())
}

fn agent_line(info: &agents::AgentInfo) -> String {
    let u = info.usage;
    let count = |reported: u64, missing: u32| {
        if u.requests == missing {
            "?".into()
        } else {
            reported.to_string()
        }
    };
    let completeness =
        if u.requests == 0 || (u.input_missing == u.requests && u.output_missing == u.requests) {
            "unavailable"
        } else if u.complete() {
            "complete"
        } else {
            "partial"
        };
    format!(
        "{} | {} | provider={} | model={} | task={} | tokens in={} out={} ({})",
        info.id,
        info.status.name(),
        info.provider.name(),
        info.model.as_deref().unwrap_or("(server default)"),
        if info.task.is_empty() {
            "(none)".into()
        } else {
            short(&info.task, 80)
        },
        count(u.input, u.input_missing),
        count(u.output, u.output_missing),
        completeness
    )
}

pub(crate) fn call_summary(call: &Call) -> String {
    match call {
        Call::Read(p, _) | Call::Write(p, _) | Call::Replace(p, _) => {
            format!("{} {}", call.verb(), short(p, 100))
        }
        Call::Search(pattern, _) => format!("search {}", short(pattern, 100)),
        Call::Sh(cmd) => format!("sh {}", short(cmd, 100)),
        Call::Mcp(server, tool, _) => format!("mcp {} {}", short(server, 50), short(tool, 50)),
        Call::Spawn(task) => format!("spawn {}", short(task, 100)),
        Call::Wait(ids) => format!("wait {}", short(&ids.join(" "), 100)),
        Call::Invalid(message) => format!("invalid {}", short(message, 100)),
    }
}

pub(crate) fn result_summary(result: &str) -> String {
    let first = result.lines().next().unwrap_or("");
    if first == "ok"
        || first == "denied"
        || first.starts_with("error:")
        || first.starts_with("exit status:")
    {
        return short(first, 140);
    }
    format!("{} bytes: {}", result.len(), short(first, 100))
}

/// Bound both lines and bytes so a model cannot flood the approval prompt.
pub(crate) fn edit_preview(old: Option<&str>, new: &str) -> String {
    let mut lines = Vec::new();
    let mut add = |prefix: &str, body: &str, limit: usize| {
        if body.is_empty() {
            lines.push(format!("{prefix} (empty)"));
            return;
        }
        for line in body.lines().take(limit) {
            let mut end = line.len().min(100);
            while !line.is_char_boundary(end) {
                end -= 1;
            }
            let suffix = if end < line.len() { "..." } else { "" };
            lines.push(format!("{prefix} {}{suffix}", &line[..end]));
        }
        if body.lines().count() > limit {
            lines.push(format!("{prefix} ... (more lines omitted)"));
        }
    };
    if let Some(old) = old {
        add("-", old, 5);
        add("+", new, 5);
    } else {
        add("+", new, 10);
    }
    lines.join("\n")
}

pub(crate) fn write_preview(before: Option<&[u8]>, new: &str) -> String {
    let Some(bytes) = before else {
        return edit_preview(None, new);
    };
    let content = String::from_utf8_lossy(&bytes[..bytes.len().min(4096)]);
    let note = if bytes.len() > 4096 {
        " (showing first 4096 bytes)"
    } else {
        ""
    };
    format!(
        "overwrites existing file{note}\n{}",
        edit_preview(Some(&content), new)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::{Condvar, Mutex};
    use std::time::Instant;

    fn temp_dir(name: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("revex-interactive-{}-{name}", std::process::id()));
        std::fs::remove_dir_all(&d).ok();
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn cfg(cwd: PathBuf, base: String, agents: &Arc<Agents>) -> Cfg {
        Cfg {
            temperature: None,
            max_tokens: None,
            quiet: false,
            frontier: None,
            route: Route::Off,
            decision_model: "clef".into(),
            response_language: ResponseLanguage::English,
            approval_mode: ApprovalMode::Manual,
            max_steps: 100,
            api: Api::Chat,
            base,
            key: String::new(),
            model: Some("fake-model".into()),
            reasoning: None,
            yes: false,
            history: None,
            strict_save: false,
            cwd,
            role: Role::Parent(Some(agents.clone())),
            ui: None,
        }
    }

    #[test]
    fn workspace_confirmation_keeps_launch_cwd_or_exits() {
        let marker_dir = temp_dir("workspace-marker");
        let agents = Agents::memory(model_fn(|_, _| unreachable!()));
        let mut config = cfg(
            PathBuf::from("/tmp/revex-workspace"),
            String::new(),
            &agents,
        );
        let mut view = tui::View {
            page: tui::Page::Workspace,
            workspace_selected: 0,
            ..Default::default()
        };
        let (tx, _rx) = mpsc::channel();
        let mut session = None;
        let mut request = 0;
        let mut exit = false;
        let marker = workspace_marker_path(&marker_dir, &config.cwd);
        handle_workspace_key(
            KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
            &mut config,
            &mut view,
            &mut session,
            &tx,
            &marker,
            &mut request,
            &mut exit,
        )
        .unwrap();
        assert_eq!(view.workspace_selected, 1);
        handle_workspace_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut config,
            &mut view,
            &mut session,
            &tx,
            &marker,
            &mut request,
            &mut exit,
        )
        .unwrap();
        assert!(exit);
        assert!(session.is_none());
        assert!(!workspace_accepted(&marker, &config.cwd).unwrap());
        assert_eq!(config.cwd, PathBuf::from("/tmp/revex-workspace"));

        exit = false;
        view.workspace_selected = 0;
        handle_workspace_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut config,
            &mut view,
            &mut session,
            &tx,
            &marker,
            &mut request,
            &mut exit,
        )
        .unwrap();
        assert!(!exit);
        assert_eq!(view.page, tui::Page::Setup);
        assert_eq!(config.cwd, PathBuf::from("/tmp/revex-workspace"));
        assert!(workspace_accepted(&marker, &config.cwd).unwrap());
        std::fs::remove_dir_all(marker_dir).ok();
    }

    #[test]
    fn workspace_marker_is_private_and_scoped_to_canonical_directory() {
        let state = temp_dir("workspace-scopes");
        let first = state.join("first");
        let second = state.join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        let first = first.canonicalize().unwrap();
        let second = second.canonicalize().unwrap();
        let a = workspace_marker_path(&state, &first);
        let b = workspace_marker_path(&state, &second);
        assert_ne!(a, b);
        assert!(!workspace_accepted(&a, &first).unwrap());
        accept_workspace(&a, &first).unwrap();
        assert!(workspace_accepted(&a, &first).unwrap());
        assert!(!workspace_accepted(&b, &second).unwrap());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&a).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        std::fs::remove_dir_all(state).ok();
    }

    #[test]
    fn setup_provider_keys_select_omlx_and_custom_urls() {
        let agents = Agents::memory(model_fn(|_, _| unreachable!()));
        let mut config = cfg(PathBuf::from("/tmp"), String::new(), &agents);
        let mut view = tui::View::default();
        setup_view(&mut view, &config);
        let (tx, _rx) = mpsc::channel();
        let mut session = None;
        let mut request = 0;
        let mut exit = false;
        assert_eq!(view.setup_provider, 0);
        view.setup_tab_focus = false;
        view.setup_editing = true;
        handle_setup_key(
            KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE),
            &mut config,
            &mut view,
            &mut session,
            &tx,
            &mut request,
            &mut exit,
        )
        .unwrap();
        assert_eq!(view.setup_provider, 2);
        assert_eq!(view.setup_url, preset_url(0));
        handle_setup_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut config,
            &mut view,
            &mut session,
            &tx,
            &mut request,
            &mut exit,
        )
        .unwrap();
        assert_eq!(view.setup_url, "http://127.0.0.1:8000/v1");
        view.setup_field = 0;
        view.setup_editing = true;
        handle_setup_key(
            KeyEvent::new(KeyCode::Char('4'), KeyModifiers::NONE),
            &mut config,
            &mut view,
            &mut session,
            &tx,
            &mut request,
            &mut exit,
        )
        .unwrap();
        assert_eq!(view.setup_provider, 3);
        assert_eq!(view.setup_url, preset_url(2));
    }

    #[test]
    fn manual_model_id_may_start_with_r_after_discovery_failure() {
        let agents = Agents::memory(model_fn(|_, _| unreachable!()));
        let mut config = cfg(PathBuf::from("/tmp"), "http://unused".into(), &agents);
        config.model = None;
        let mut view = tui::View::default();
        setup_view(&mut view, &config);
        view.setup_tab = 1;
        view.setup_tab_focus = false;
        view.setup_editing = true;
        view.setup_field = 3;
        view.setup_status = "Connection failed".into();
        let (tx, _rx) = mpsc::channel();
        let mut session = None;
        let mut request = 0;
        let mut exit = false;
        for ch in ['r', '1'] {
            handle_setup_key(
                KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
                &mut config,
                &mut view,
                &mut session,
                &tx,
                &mut request,
                &mut exit,
            )
            .unwrap();
        }
        assert_eq!(view.setup_model, "r1");
        assert!(view.setup_manual);
        assert_eq!(request, 0, "typing r must not start model discovery");
    }

    #[test]
    fn appended_output_returns_to_latest_transcript_line() {
        let mut view = tui::View {
            scroll: 40,
            transcript: (0..100).map(|n| format!("line {n}")).collect(),
            ..Default::default()
        };
        append(&mut view, "new result");
        assert_eq!(view.scroll, 0);
        assert_eq!(view.transcript.last().unwrap(), "new result");
    }

    #[test]
    fn prompt_history_replays_draft_and_survives_restart_per_workspace() {
        let dir = temp_dir("input-history");
        let a = history_for(&dir, Path::new("/work/a"));
        let b = history_for(&dir, Path::new("/work/b"));
        let mut history = PromptHistory::load(&a).unwrap();
        history.record("first").unwrap();
        history.record("second\nline").unwrap();
        history.record("second\nline").unwrap();
        assert_eq!(history.entries, ["first", "second\nline", "second\nline"]);
        assert_eq!(history.up("draft 界"), Some("second\nline".into()));
        assert_eq!(history.up("ignored"), Some("second\nline".into()));
        assert_eq!(history.up("ignored"), Some("first".into()));
        assert_eq!(history.up("ignored"), Some("first".into()));
        assert_eq!(history.down(), Some("second\nline".into()));
        assert_eq!(history.down(), Some("second\nline".into()));
        assert_eq!(history.down(), Some("draft 界".into()));
        assert_eq!(history.down(), None);
        let mut reloaded = PromptHistory::load(&a).unwrap();
        assert_eq!(reloaded.up(""), Some("second\nline".into()));
        assert!(PromptHistory::load(&b).unwrap().entries.is_empty());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(PromptHistory::path_for(&a))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
            assert_eq!(
                std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn prompt_history_keeps_only_latest_hundred_entries_after_restart() {
        let dir = temp_dir("input-history-bound");
        let workspace = history_for(&dir, Path::new("/work/bound"));
        let mut history = PromptHistory::load(&workspace).unwrap();
        for n in 0..101 {
            history.record(&format!("entry-{n}")).unwrap();
        }
        history.record("entry-100").unwrap();
        assert_eq!(history.entries.len(), 100);
        assert_eq!(history.entries.first().unwrap(), "entry-2");
        assert_eq!(history.entries.last().unwrap(), "entry-100");
        assert_eq!(history.entries[98], "entry-100");
        let restored = PromptHistory::load(&workspace).unwrap();
        assert_eq!(restored.entries, history.entries);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn chat_history_keys_restore_draft_and_do_not_navigate_overlays() {
        let dir = temp_dir("input-history-keys");
        let history_file = history_for(&dir, Path::new("/work/keys"));
        let mut prompts = PromptHistory::load(&history_file).unwrap();
        prompts.record("alpha").unwrap();
        prompts.record("界 beta").unwrap();
        let agents = Agents::memory(model_fn(|_, _| unreachable!()));
        let mut config = cfg(dir.clone(), "http://unused".into(), &agents);
        let (tx, _rx) = mpsc::channel();
        let mut active = Session::new(config.clone(), fresh_history(), agents, tx);
        active.prompt_history = Some(prompts);
        let mut session = Some(active);
        let mut view = tui::View {
            input: "draft".into(),
            cursor: 5,
            ..Default::default()
        };
        let mut request = 0;
        let mut exit = false;
        let up = KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        handle_chat_key(
            up,
            &mut config,
            &mut view,
            &mut session,
            &mut request,
            &mut exit,
        );
        assert_eq!(view.input, "界 beta");
        assert_eq!(view.cursor, 6);
        handle_chat_key(
            up,
            &mut config,
            &mut view,
            &mut session,
            &mut request,
            &mut exit,
        );
        assert_eq!(view.input, "alpha");
        view.overlay = tui::Overlay::Status;
        handle_chat_key(
            down,
            &mut config,
            &mut view,
            &mut session,
            &mut request,
            &mut exit,
        );
        assert_eq!(view.input, "alpha");
        view.overlay = tui::Overlay::None;
        handle_chat_key(
            down,
            &mut config,
            &mut view,
            &mut session,
            &mut request,
            &mut exit,
        );
        assert_eq!(view.input, "界 beta");
        handle_chat_key(
            down,
            &mut config,
            &mut view,
            &mut session,
            &mut request,
            &mut exit,
        );
        assert_eq!(view.input, "draft");
        assert_eq!(view.cursor, 5);
        assert!(!exit);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn escape_resets_chat_cursor_before_editing_again() {
        let agents = Agents::memory(model_fn(|_, _| unreachable!()));
        let mut config = cfg(PathBuf::from("/tmp"), "http://unused".into(), &agents);
        let (tx, _rx) = mpsc::channel();
        let mut session = Some(Session::new(config.clone(), fresh_history(), agents, tx));
        let mut view = tui::View {
            input: "abc界".into(),
            cursor: 3,
            ..Default::default()
        };
        let mut request = 0;
        let mut exit = false;
        for key in [KeyCode::Esc, KeyCode::Char('x'), KeyCode::Backspace] {
            handle_chat_key(
                KeyEvent::new(key, KeyModifiers::NONE),
                &mut config,
                &mut view,
                &mut session,
                &mut request,
                &mut exit,
            );
            match key {
                KeyCode::Esc => assert_eq!((&*view.input, view.cursor), ("", 0)),
                KeyCode::Char('x') => assert_eq!((&*view.input, view.cursor), ("x", 1)),
                KeyCode::Backspace => assert_eq!((&*view.input, view.cursor), ("", 0)),
                _ => unreachable!(),
            }
        }
    }

    #[test]
    fn alt_enter_inserts_newline_without_submitting() {
        let agents = Agents::memory(model_fn(|_, _| unreachable!()));
        let mut config = cfg(PathBuf::from("/tmp"), "http://unused".into(), &agents);
        let (tx, _rx) = mpsc::channel();
        let mut session = Some(Session::new(config.clone(), fresh_history(), agents, tx));
        let mut view = tui::View {
            input: "firstsecond".into(),
            cursor: 5,
            ..Default::default()
        };
        let mut request = 0;
        let mut exit = false;
        handle_chat_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT),
            &mut config,
            &mut view,
            &mut session,
            &mut request,
            &mut exit,
        );
        assert_eq!(view.input, "first\nsecond");
        assert_eq!(view.cursor, 6);
        assert!(!session.as_ref().unwrap().busy);
        handle_chat_key(
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL),
            &mut config,
            &mut view,
            &mut session,
            &mut request,
            &mut exit,
        );
        assert_eq!(view.input, "first\n\nsecond");
        assert_eq!(view.cursor, 7);
    }

    #[test]
    fn shift_enter_inserts_newline_and_plain_enter_submits_multiline_prompt() {
        let agents = Agents::memory(model_fn(|_, _| unreachable!()));
        let mut config = cfg(PathBuf::from("/tmp"), "http://unused".into(), &agents);
        let (tx, _rx) = mpsc::channel();
        let mut session = Some(Session::new(config.clone(), fresh_history(), agents, tx));
        let mut view = tui::View {
            input: "firstsecond".into(),
            cursor: 5,
            ..Default::default()
        };
        let mut request = 0;
        let mut exit = false;
        handle_chat_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT),
            &mut config,
            &mut view,
            &mut session,
            &mut request,
            &mut exit,
        );
        assert_eq!(view.input, "first\nsecond");
        assert_eq!(view.cursor, 6);
        assert!(!session.as_ref().unwrap().busy);
        handle_chat_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut config,
            &mut view,
            &mut session,
            &mut request,
            &mut exit,
        );
        assert!(view.input.is_empty());
        assert_eq!(view.cursor, 0);
        assert!(
            view.transcript
                .iter()
                .any(|line| line == "You  first\nsecond")
        );
    }

    #[test]
    fn blank_api_key_preserves_same_endpoint_and_rejects_changed_endpoint() {
        let agents = Agents::memory(model_fn(|_, _| unreachable!()));
        let mut config = cfg(
            PathBuf::from("/tmp"),
            "http://old.example/v1".into(),
            &agents,
        );
        config.key = "old-secret".into();
        let mut view = tui::View {
            setup_url: "http://old.example/v1".into(),
            ..Default::default()
        };
        assert_eq!(setup_auth_key(&config, &view), "old-secret");
        view.setup_url = "http://new.example/v1".into();
        assert_eq!(setup_auth_key(&config, &view), "");
        let mut settings = Map::new();
        settings.insert("api_key".into(), Value::String("old-secret".into()));
        settings.insert(
            "base_url".into(),
            Value::String("http://old.example/v1".into()),
        );
        apply_setup_key(&mut config, &mut settings, &view, "http://old.example/v1").unwrap();
        assert_eq!(settings["api_key"], "old-secret");
        let error = apply_setup_key(&mut config, &mut settings, &view, "http://new.example/v1")
            .unwrap_err();
        assert!(error.contains("Ctrl+K"));
        assert_eq!(config.key, "old-secret");
        assert_eq!(settings["api_key"], "old-secret");
        view.setup_clear_key = true;
        apply_setup_key(&mut config, &mut settings, &view, "http://new.example/v1").unwrap();
        assert!(config.key.is_empty());
        assert!(settings["api_key"].is_null());
        view.setup_key = "new-secret".into();
        view.setup_clear_key = false;
        assert_eq!(setup_auth_key(&config, &view), "new-secret");
        change_setup_url(&mut view, "http://third.example/v1".into());
        assert!(view.setup_key.is_empty());
        assert!(view.setup_status.contains("re-enter"));
    }

    #[test]
    fn ctrl_k_marks_saved_api_key_for_clear_without_saving() {
        let agents = Agents::memory(model_fn(|_, _| unreachable!()));
        let mut config = cfg(
            PathBuf::from("/tmp"),
            "http://old.example/v1".into(),
            &agents,
        );
        let mut view = tui::View::default();
        setup_view(&mut view, &config);
        view.setup_tab_focus = false;
        view.setup_editing = true;
        view.setup_field = 2;
        view.setup_key = "draft-secret".into();
        let (tx, _rx) = mpsc::channel();
        let mut session = None;
        let mut request = 0;
        let mut exit = false;
        handle_setup_key(
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL),
            &mut config,
            &mut view,
            &mut session,
            &tx,
            &mut request,
            &mut exit,
        )
        .unwrap();
        assert!(view.setup_key.is_empty());
        assert!(view.setup_clear_key);
        assert!(setup_auth_key(&config, &view).is_empty());
    }

    #[test]
    fn environment_key_blocks_new_endpoint_that_would_reuse_it_after_restart() {
        let old = "http://old.example/v1";
        let new = "http://new.example/v1";
        let error = guard_endpoint_change(old, new, true).unwrap_err();
        assert!(error.contains("Unset REVEX_API_KEY"));
        assert!(guard_endpoint_change(old, old, true).is_ok());
        assert!(guard_endpoint_change(old, new, false).is_ok());
        assert!(guard_endpoint_change("", new, true).is_ok());
    }

    #[test]
    fn busy_model_command_cannot_hide_pending_approval() {
        let agents = Agents::memory(model_fn(|_, _| unreachable!()));
        let mut config = cfg(PathBuf::from("/tmp"), "http://fake".into(), &agents);
        let (tx, _rx) = mpsc::channel();
        let mut session = Session::new(config.clone(), fresh_history(), agents, tx);
        session.busy = true;
        let mut view = tui::View {
            page: tui::Page::Chat,
            ..Default::default()
        };
        let mut request = 0;
        let mut exit = false;
        submit_chat(
            "/model".into(),
            &mut config,
            &mut view,
            &mut session,
            &mut request,
            &mut exit,
        );
        assert_eq!(view.page, tui::Page::Chat);
        submit_chat(
            "/model new".into(),
            &mut config,
            &mut view,
            &mut session,
            &mut request,
            &mut exit,
        );
        assert_eq!(view.page, tui::Page::Chat);
        assert!(
            view.transcript
                .iter()
                .any(|line| line.contains("Finish the active task"))
        );

        let (answer, reply) = mpsc::channel();
        let mut session = Some(session);
        view.page = tui::Page::Setup; // stale setup cannot steal approval keys
        handle_worker_event(
            Event::Approval {
                what: "write file".into(),
                preview: "old\nnew".into(),
                answer,
            },
            &mut view,
            &mut session,
            request,
        );
        assert_eq!(view.page, tui::Page::Chat);
        assert_eq!(view.overlay, tui::Overlay::Approval);
        handle_chat_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut config,
            &mut view,
            &mut session,
            &mut request,
            &mut exit,
        );
        assert!(!reply.recv().unwrap());
    }

    #[test]
    fn chat_help_lists_plain_and_slash_exit() {
        let agents = Agents::memory(model_fn(|_, _| unreachable!()));
        let mut config = cfg(PathBuf::from("/tmp"), "http://fake".into(), &agents);
        let (tx, _rx) = mpsc::channel();
        let mut session = Session::new(config.clone(), fresh_history(), agents, tx);
        let mut view = tui::View {
            page: tui::Page::Chat,
            ..Default::default()
        };
        let mut request = 0;
        let mut exit = false;
        submit_chat(
            "/help".into(),
            &mut config,
            &mut view,
            &mut session,
            &mut request,
            &mut exit,
        );
        assert!(view.transcript.last().unwrap().contains("/exit or exit"));
        assert!(!view.transcript.last().unwrap().contains("/quit"));
        assert!(
            view.transcript
                .last()
                .unwrap()
                .contains("Alt+Enter newline")
        );
        assert!(!view.transcript.last().unwrap().contains("Ctrl+J newline"));
        assert!(!view.transcript.last().unwrap().contains("Shift+Enter"));
        assert!(!exit);
        submit_chat(
            "/quit".into(),
            &mut config,
            &mut view,
            &mut session,
            &mut request,
            &mut exit,
        );
        assert!(!exit);
        assert!(view.transcript.last().unwrap().contains("Unknown command"));
        submit_chat(
            "exit".into(),
            &mut config,
            &mut view,
            &mut session,
            &mut request,
            &mut exit,
        );
        assert!(exit);
    }

    #[test]
    fn provider_reasoning_stays_bounded_and_sanitized() {
        let mut view = tui::View::default();
        append_reasoning(&mut view, "\x1b[31mfirst\n");
        append_reasoning(&mut view, &"あ".repeat(MAX_THINKING_BYTES));
        assert!(view.thinking_text.len() <= MAX_THINKING_BYTES);
        assert!(!view.thinking_text.contains('\x1b'));
        assert!(view.thinking_text.ends_with('あ'));
        assert!(view.transcript.is_empty());
    }

    #[test]
    fn model_phase_timer_starts_once_and_clears_when_generation_ends() {
        let agents = Agents::memory(model_fn(|_, _| unreachable!()));
        let config = cfg(PathBuf::from("/tmp"), "http://fake".into(), &agents);
        let (tx, _rx) = mpsc::channel();
        let mut active = Session::new(config, fresh_history(), agents, tx);
        active.busy = true;
        let mut session = Some(active);
        let mut view = tui::View::default();

        handle_worker_event(Event::ModelPhase(true), &mut view, &mut session, 0);
        assert!(view.model_in_flight);
        let started = view.thinking_started.expect("model phase starts timer");
        handle_worker_event(Event::ModelPhase(true), &mut view, &mut session, 0);
        assert_eq!(view.thinking_started, Some(started));

        handle_worker_event(Event::ModelPhase(false), &mut view, &mut session, 0);
        assert!(!view.model_in_flight);
        assert!(view.thinking_started.is_none());
    }

    #[test]
    fn interrupted_turn_ignores_late_reasoning_and_model_phase() {
        let agents = Agents::memory(model_fn(|_, _| unreachable!()));
        let config = cfg(PathBuf::from("/tmp"), "http://fake".into(), &agents);
        let (tx, _rx) = mpsc::channel();
        let mut active = Session::new(config, fresh_history(), agents, tx);
        active.busy = true;
        let mut session = Some(active);
        let mut view = tui::View::default();
        handle_worker_event(Event::ModelPhase(true), &mut view, &mut session, 0);
        handle_worker_event(
            Event::ReasoningChunk("visible".into()),
            &mut view,
            &mut session,
            0,
        );
        assert_eq!(view.thinking_text, "visible");
        session.as_mut().unwrap().interrupted = true;
        view.model_in_flight = false;
        view.thinking_text.clear();
        handle_worker_event(Event::ModelPhase(true), &mut view, &mut session, 0);
        handle_worker_event(
            Event::ReasoningChunk("stale".into()),
            &mut view,
            &mut session,
            0,
        );
        assert!(!view.model_in_flight);
        assert!(view.thinking_text.is_empty());
    }

    #[test]
    fn reasoning_picker_can_be_reached_without_editing_model() {
        let agents = Agents::memory(model_fn(|_, _| unreachable!()));
        let mut config = cfg(PathBuf::from("/tmp"), "http://fake".into(), &agents);
        let mut view = tui::View::default();
        setup_view(&mut view, &config);
        view.setup_tab = 1;
        view.setup_tab_focus = false;
        view.setup_field = 3;
        let (tx, _rx) = mpsc::channel();
        let mut session = None;
        let mut request = 0;
        let mut exit = false;
        handle_setup_key(
            KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
            &mut config,
            &mut view,
            &mut session,
            &tx,
            &mut request,
            &mut exit,
        )
        .unwrap();
        assert_eq!(view.setup_field, 4);
        handle_setup_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut config,
            &mut view,
            &mut session,
            &tx,
            &mut request,
            &mut exit,
        )
        .unwrap();
        assert!(view.setup_editing);
        for _ in 0..3 {
            handle_setup_key(
                KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
                &mut config,
                &mut view,
                &mut session,
                &tx,
                &mut request,
                &mut exit,
            )
            .unwrap();
        }
        assert_eq!(view.setup_reasoning, "high");
        handle_setup_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut config,
            &mut view,
            &mut session,
            &tx,
            &mut request,
            &mut exit,
        )
        .unwrap();
        assert_eq!(view.setup_field, 4);
        assert!(!view.setup_editing);
    }

    #[test]
    fn setup_tabs_preserve_draft_and_provider_needs_enter() {
        let agents = Agents::memory(model_fn(|_, _| unreachable!()));
        let mut config = cfg(PathBuf::from("/tmp"), String::new(), &agents);
        let mut view = tui::View::default();
        setup_view(&mut view, &config);
        let (tx, _rx) = mpsc::channel();
        let mut session = None;
        let mut request = 0;
        let mut exit = false;
        macro_rules! press {
            ($code:expr) => {
                handle_setup_key(
                    KeyEvent::new($code, KeyModifiers::NONE),
                    &mut config,
                    &mut view,
                    &mut session,
                    &tx,
                    &mut request,
                    &mut exit,
                )
                .unwrap()
            };
        }
        press!(KeyCode::Right);
        assert_eq!(view.setup_tab, 1);
        press!(KeyCode::Tab);
        assert_eq!(view.setup_tab, 2);
        press!(KeyCode::BackTab);
        assert_eq!(view.setup_tab, 1);
        press!(KeyCode::BackTab);
        assert_eq!(view.setup_tab, 0);
        press!(KeyCode::Enter); // Browse Provider.
        press!(KeyCode::Enter); // Edit Provider.
        press!(KeyCode::Down); // Candidate LM Studio; no URL change yet.
        assert_eq!(view.setup_url, preset_url(0));
        assert_eq!(view.setup_provider_committed, None);
        press!(KeyCode::Enter);
        assert_eq!(view.setup_provider_committed, Some(1));
        assert_eq!(view.setup_url, preset_url(1));
    }

    #[test]
    fn left_right_inside_provider_does_not_change_tabs_or_choice() {
        let agents = Agents::memory(model_fn(|_, _| unreachable!()));
        let mut config = cfg(PathBuf::from("/tmp"), String::new(), &agents);
        let mut view = tui::View::default();
        setup_view(&mut view, &config);
        view.setup_tab_focus = false;
        view.setup_editing = true;
        let (tx, _rx) = mpsc::channel();
        let mut session = None;
        let mut request = 0;
        let mut exit = false;
        handle_setup_key(
            KeyEvent::new(KeyCode::Right, KeyModifiers::NONE),
            &mut config,
            &mut view,
            &mut session,
            &tx,
            &mut request,
            &mut exit,
        )
        .unwrap();
        assert_eq!(view.setup_tab, 0);
        assert_eq!(view.setup_provider, 0);
    }

    #[test]
    fn confirming_saved_custom_provider_keeps_existing_url() {
        let agents = Agents::memory(model_fn(|_, _| unreachable!()));
        let mut config = cfg(
            PathBuf::from("/tmp"),
            "http://custom.example/v1".into(),
            &agents,
        );
        let mut view = tui::View::default();
        setup_view(&mut view, &config);
        view.setup_tab_focus = false;
        view.setup_editing = true;
        let (tx, _rx) = mpsc::channel();
        let mut session = None;
        let mut request = 0;
        let mut exit = false;
        handle_setup_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut config,
            &mut view,
            &mut session,
            &tx,
            &mut request,
            &mut exit,
        )
        .unwrap();
        assert_eq!(view.setup_url, "http://custom.example/v1");
    }

    #[test]
    fn changing_setup_endpoint_invalidates_pending_model_list() {
        let agents = Agents::memory(model_fn(|_, _| unreachable!()));
        let mut config = cfg(
            PathBuf::from("/tmp"),
            "http://old.example/v1".into(),
            &agents,
        );
        let mut view = tui::View::default();
        setup_view(&mut view, &config);
        view.setup_tab_focus = false;
        view.setup_editing = true;
        view.setup_field = 1;
        view.cursor = view.setup_url.chars().count();
        let (tx, _rx) = mpsc::channel();
        let mut session = None;
        let mut request = 1;
        let mut exit = false;
        handle_setup_key(
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
            &mut config,
            &mut view,
            &mut session,
            &tx,
            &mut request,
            &mut exit,
        )
        .unwrap();
        assert_eq!(request, 2);
        handle_worker_event(
            Event::ModelsFetched {
                request: 1,
                result: Ok(vec!["old-model".into()]),
            },
            &mut view,
            &mut session,
            request,
        );
        assert!(view.setup_models.is_empty());
    }

    #[test]
    fn model_tab_edits_decision_model_without_changing_other_items() {
        let agents = Agents::memory(model_fn(|_, _| unreachable!()));
        let mut config = cfg(PathBuf::from("/tmp"), "http://fake".into(), &agents);
        let mut view = tui::View::default();
        setup_view(&mut view, &config);
        view.setup_tab = 1;
        view.setup_tab_focus = false;
        view.setup_field = 4;
        let (tx, _rx) = mpsc::channel();
        let mut session = None;
        let mut request = 0;
        let mut exit = false;
        for code in [KeyCode::Down, KeyCode::Enter, KeyCode::Down, KeyCode::Enter] {
            handle_setup_key(
                KeyEvent::new(code, KeyModifiers::NONE),
                &mut config,
                &mut view,
                &mut session,
                &tx,
                &mut request,
                &mut exit,
            )
            .unwrap();
        }
        assert_eq!(view.setup_decision_model_selected, 2);
        assert_eq!(view.setup_field, 7);
        assert!(!view.setup_editing);
        assert_eq!(view.setup_tab, 1);
    }

    #[test]
    fn model_tab_edits_response_language_without_changing_other_items() {
        let agents = Agents::memory(model_fn(|_, _| unreachable!()));
        let mut config = cfg(PathBuf::from("/tmp"), "http://fake".into(), &agents);
        let mut view = tui::View::default();
        setup_view(&mut view, &config);
        view.setup_tab = 1;
        view.setup_tab_focus = false;
        view.setup_field = 7;
        let (tx, _rx) = mpsc::channel();
        let mut session = None;
        let mut request = 0;
        let mut exit = false;
        for code in [KeyCode::Down, KeyCode::Enter, KeyCode::Down, KeyCode::Enter] {
            handle_setup_key(
                KeyEvent::new(code, KeyModifiers::NONE),
                &mut config,
                &mut view,
                &mut session,
                &tx,
                &mut request,
                &mut exit,
            )
            .unwrap();
        }
        assert_eq!(view.setup_response_language_selected, 1);
        assert_eq!(view.setup_field, 8);
        assert_eq!(view.setup_tab, 1);
        assert!(!view.setup_editing);
    }

    #[test]
    fn decision_model_self_requires_explicit_chat_model_id() {
        let view = tui::View {
            setup_decision_model_selected: 0,
            ..Default::default()
        };
        let error = selected_decision_model(&view).unwrap_err();
        assert_eq!(
            error,
            "decision_model=self needs a configured chat model ID"
        );
        let view = tui::View {
            setup_model: "deepseek".into(),
            ..view
        };
        assert_eq!(selected_decision_model(&view), Ok("self"));
    }

    #[test]
    fn helper_picker_toggles_multiple_and_requires_one() {
        let agents = Agents::memory(model_fn(|_, _| unreachable!()));
        let mut config = cfg(PathBuf::from("/tmp"), "http://fake".into(), &agents);
        let mut view = tui::View::default();
        setup_view(&mut view, &config);
        view.setup_tab = 2;
        view.setup_tab_focus = false;
        view.setup_field = 5;
        view.setup_editing = true;
        view.setup_helpers = [true, false, false];
        view.setup_helper_selected = 0;
        let (tx, _rx) = mpsc::channel();
        let mut session = None;
        let mut request = 0;
        let mut exit = false;
        macro_rules! key {
            ($code:expr) => {
                handle_setup_key(
                    KeyEvent::new($code, KeyModifiers::NONE),
                    &mut config,
                    &mut view,
                    &mut session,
                    &tx,
                    &mut request,
                    &mut exit,
                )
                .unwrap()
            };
        }
        key!(KeyCode::Char(' ')); // Deselect Local.
        assert!(!view.setup_helpers.iter().any(|selected| *selected));
        key!(KeyCode::Down);
        key!(KeyCode::Char(' ')); // Select Codex.
        key!(KeyCode::Down);
        key!(KeyCode::Char(' ')); // Select Claude too.
        assert_eq!(view.setup_helpers, [false, true, true]);
        key!(KeyCode::Enter);
        assert!(!view.setup_editing);
        key!(KeyCode::Down);
        assert_eq!(view.setup_field, 9);
        key!(KeyCode::Down);
        assert_eq!(view.setup_field, 6);
    }

    #[test]
    fn helper_selection_csv_roundtrips_and_rejects_invalid_values() {
        assert_eq!(
            parse_helper_selection("local").unwrap(),
            [true, false, false]
        );
        assert_eq!(
            parse_helper_selection("codex,claude").unwrap(),
            [false, true, true]
        );
        assert_eq!(
            helper_selection_csv([false, true, true]).unwrap(),
            "codex,claude"
        );
        assert_eq!(
            helper_selection_csv([true, true, true]).unwrap(),
            "local,codex,claude"
        );
        for invalid in ["", "unknown", "local,local", "local,", ",claude"] {
            assert!(parse_helper_selection(invalid).is_err(), "{invalid}");
        }
        assert!(helper_selection_csv([false; 3]).is_err());
    }

    #[test]
    fn bare_model_fetches_models_and_preselects_current_id() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut request = [0; 2048];
            let len = stream.read(&mut request).unwrap();
            assert!(String::from_utf8_lossy(&request[..len]).starts_with("GET /models "));
            let body = r#"{"data":[{"id":"m1"},{"id":"m2"}]}"#;
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            stream.flush().unwrap();
        });
        let agents = Agents::memory(model_fn(|_, _| unreachable!()));
        let mut config = cfg(PathBuf::from("/tmp"), base, &agents);
        config.model = Some("m2".into());
        let (tx, rx) = mpsc::channel();
        let mut session = Session::new(config.clone(), fresh_history(), agents, tx);
        let mut view = tui::View {
            page: tui::Page::Chat,
            ..Default::default()
        };
        let mut request = 0;
        let mut exit = false;
        submit_chat(
            "/model".into(),
            &mut config,
            &mut view,
            &mut session,
            &mut request,
            &mut exit,
        );
        assert_eq!(view.page, tui::Page::Setup);
        assert_eq!(view.setup_field, 3);
        assert_eq!(request, 1);
        let result = rx.recv_timeout(Duration::from_secs(3)).unwrap();
        let mut session = Some(session);
        handle_worker_event(result, &mut view, &mut session, request);
        assert_eq!(view.setup_models, ["m1", "m2"]);
        assert_eq!(view.setup_model_selected, 1);
        assert_eq!(view.setup_model, "m2");
        assert!(!view.setup_manual);
        server.join().unwrap();
    }

    #[test]
    fn project_model_effort_and_decision_settings_persist_together() {
        let dir = temp_dir("project-effort");
        let project = dir.join(".revex");
        std::fs::write(&project, r#"{"model":"old","route":"off"}"#).unwrap();
        save_project_model_settings(&project, Some("new"), "high", "clef-flash", "ja").unwrap();
        let saved = read_settings(&project).unwrap();
        assert_eq!(saved["model"], "new");
        assert_eq!(saved["reasoning_effort"], "high");
        assert_eq!(saved["decision_model"], "clef-flash");
        assert_eq!(saved["response_language"], "ja");
        assert_eq!(saved["route"], "off");
        save_project_model_settings(&project, None, "", "self", "en").unwrap();
        let cleared = read_settings(&project).unwrap();
        assert!(!cleared.contains_key("model"));
        assert_eq!(cleared["reasoning_effort"], "");
        assert_eq!(cleared["decision_model"], "self");
        assert_eq!(cleared["response_language"], "en");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn first_run_screen_shows_setup_fields_and_workspace() {
        let lines = setup_welcome(Path::new("/work/project"), "new session").join("\n");
        assert!(lines.contains("Revex 0.1.0  |  Local AI Coding Agent"));
        assert!(lines.contains("Project  /work/project"));
        assert!(lines.contains("History  new session"));
        assert!(lines.contains("API      setup required"));
        assert!(lines.contains("Model    not selected"));
        assert!(lines.contains("/agents  /status  /model"));
        assert!(lines.contains("Setup  Connect a running OpenAI-compatible API"));
    }

    #[test]
    fn restored_messages_still_seed_chat_transcript() {
        let mut view = tui::View::default();
        let msgs = vec![
            json!({"role": "system", "content": "instructions"}),
            json!({"role": "user", "content": "saved question"}),
            json!({"role": "assistant", "content": "saved answer"}),
            json!({"role": "user", "content": "<out>tool output</out>"}),
        ];
        seed_transcript(&msgs, &mut view);
        assert_eq!(
            view.transcript,
            ["You  saved question", "Revex  saved answer"]
        );
    }

    #[test]
    fn returning_screen_shows_session_and_sanitizes_terminal_controls() {
        let agents = Agents::memory(model_fn(|_, _| unreachable!()));
        let mut cfg = cfg(
            PathBuf::from("/work/project\nother"),
            "http://local\x1b[31m/v1".into(),
            &agents,
        );
        cfg.model = Some("model\x1b[31m".into());
        let mut msgs = fresh_history();
        msgs.push(json!({"role":"user","content":"task"}));
        msgs.push(json!({"role":"assistant","content":"done"}));
        msgs.push(json!({"role":"user","content":"<out>\nread\n</out>"}));
        let lines = welcome(&cfg, &msgs, "unreachable").join("\n");
        assert!(lines.contains("History  resumed 1 user turn(s), 3 saved messages"));
        assert!(lines.contains("API      unreachable"));
        assert!(lines.contains("Model    model"));
        assert!(lines.contains("Settings Type s"));
        assert!(lines.contains("/agents  /status  /model ID"));
        assert!(!lines.contains('\x1b'));
        assert_eq!(terminal_safe("ok\x1b[31m\nnext"), "ok [31m\nnext");
    }

    type Output = Arc<(Mutex<Vec<String>>, Condvar)>;

    fn wait_for(output: &Output, needle: &str) {
        wait_for_count(output, needle, 1);
    }

    fn wait_for_count(output: &Output, needle: &str, count: usize) {
        let deadline = Instant::now() + Duration::from_secs(5);
        let (lock, changed) = &**output;
        let mut lines = lock.lock().unwrap();
        while lines.iter().filter(|s| s.contains(needle)).count() < count {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "missing {needle:?}: {lines:?}");
            let (l, _) = changed.wait_timeout(lines, left).unwrap();
            lines = l;
        }
    }

    fn start_ui(
        cfg: Cfg,
        agents: Arc<Agents>,
    ) -> (Sender<Event>, Output, std::thread::JoinHandle<()>) {
        let (tx, rx) = mpsc::channel();
        let output: Output = Arc::new((Mutex::new(Vec::new()), Condvar::new()));
        let copy = output.clone();
        let ui_tx = tx.clone();
        let handle = std::thread::spawn(move || {
            let interrupt = AtomicBool::new(false);
            run_loop(
                cfg,
                fresh_history(),
                agents,
                ui_tx,
                rx,
                &interrupt,
                &mut |s| {
                    copy.0.lock().unwrap().push(s.into());
                    copy.1.notify_all();
                },
            );
        });
        (tx, output, handle)
    }

    fn read_request(stream: &mut TcpStream) -> Value {
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut buf = Vec::new();
        let mut part = [0; 4096];
        while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = stream.read(&mut part).unwrap();
            assert!(n > 0, "client closed before request headers");
            buf.extend_from_slice(&part[..n]);
        }
        let end = buf.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        let head = String::from_utf8_lossy(&buf[..end]);
        let length = head
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .and_then(|n| n.trim().parse::<usize>().ok())
            })
            .unwrap_or(0);
        while buf.len() - end < length {
            let n = stream.read(&mut part).unwrap();
            assert!(n > 0, "client closed before request body");
            buf.extend_from_slice(&part[..n]);
        }
        serde_json::from_slice(&buf[end..end + length]).unwrap()
    }

    fn sse(stream: &mut TcpStream, text: &str, input: u64, output: u64) {
        let delta = json!({"choices": [{"delta": {"content": text}}], "usage": null});
        let usage =
            json!({"choices": [], "usage": {"prompt_tokens": input, "completion_tokens": output}});
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {delta}\n\ndata: {usage}\n\ndata: [DONE]\n\n"
        ).unwrap();
        stream.flush().unwrap();
    }

    #[test]
    fn agents_respond_during_blocked_sse_and_approval_goes_to_parent_edit() {
        let cwd = temp_dir("sse-approval");
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (seen_tx, seen_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut first, _) = listener.accept().unwrap();
            let _ = read_request(&mut first);
            seen_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            sse(&mut first, ">> write edited.txt <<EOF\nchanged\nEOF", 12, 3);
            drop(first);
            let (mut second, _) = listener.accept().unwrap();
            let _ = read_request(&mut second);
            sse(&mut second, "done", 8, 2);
        });
        let agents = Agents::memory(model_fn(chat));
        let (tx, output, ui) = start_ui(cfg(cwd.clone(), base, &agents), agents.clone());
        tx.send(Event::Input("edit a file".into())).unwrap();
        seen_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        tx.send(Event::Input("/agents".into())).unwrap();
        wait_for(
            &output,
            "main | running | provider=local | model=fake-model | task=edit a file | tokens in=? out=? (unavailable)",
        );
        tx.send(Event::Input("/status".into())).unwrap();
        wait_for(&output, "key=not set main=running");
        release_tx.send(()).unwrap();
        wait_for(&output, "approval: write edited.txt");
        assert!(!cwd.join("edited.txt").exists());
        tx.send(Event::Input("/agents".into())).unwrap();
        wait_for_count(
            &output,
            "main | running | provider=local | model=fake-model | task=edit a file",
            2,
        );
        assert!(!cwd.join("edited.txt").exists());
        tx.send(Event::Input("y".into())).unwrap();
        wait_for(&output, "[main] done");
        assert_eq!(
            std::fs::read_to_string(cwd.join("edited.txt")).unwrap(),
            "changed\n"
        );
        tx.send(Event::Input("/agents".into())).unwrap();
        wait_for(&output, "tokens in=20 out=5 (complete)");
        tx.send(Event::Input("/exit".into())).unwrap();
        ui.join().unwrap();
        server.join().unwrap();
        std::fs::remove_dir_all(cwd).ok();
    }

    #[test]
    fn agents_show_blocked_child_and_waited_result_with_separate_usage() {
        let cwd = temp_dir("child-sse");
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let child_gate = gate.clone();
        let (child_seen_tx, child_seen_rx) = mpsc::channel();
        let saw_wait_result = Arc::new(AtomicBool::new(false));
        let saw_wait_result_server = saw_wait_result.clone();
        let server = std::thread::spawn(move || {
            let mut handlers = Vec::new();
            for _ in 0..4 {
                let (mut stream, _) = listener.accept().unwrap();
                let request = read_request(&mut stream);
                let messages = request["messages"].as_array().unwrap();
                let is_child = messages[0]["content"]
                    .as_str()
                    .is_some_and(|prompt| prompt.contains(CHILD_SYSTEM));
                let sequence = messages.len();
                let gate = child_gate.clone();
                if is_child {
                    child_seen_tx.send(()).unwrap();
                }
                if !is_child && sequence == 6 {
                    assert!(
                        messages.last().unwrap()["content"]
                            .as_str()
                            .unwrap()
                            .contains("[a1] done\nfound")
                    );
                    saw_wait_result_server.store(true, Ordering::Release);
                }
                handlers.push(std::thread::spawn(move || {
                    if is_child {
                        let (lock, changed) = &*gate;
                        let mut released = lock.lock().unwrap();
                        while !*released {
                            released = changed.wait(released).unwrap();
                        }
                        sse(&mut stream, "found", 3, 1);
                    } else {
                        match sequence {
                            2 => sse(&mut stream, ">> spawn inspect file", 4, 1),
                            4 => sse(&mut stream, ">> wait a1", 5, 2),
                            6 => sse(&mut stream, "done", 6, 1),
                            _ => panic!("unexpected parent request with {sequence} messages"),
                        }
                    }
                }));
            }
            for handler in handlers {
                handler.join().unwrap();
            }
        });
        let agents = Agents::memory(model_fn(chat));
        let (tx, output, ui) = start_ui(cfg(cwd.clone(), base, &agents), agents.clone());
        tx.send(Event::Input("investigate".into())).unwrap();
        child_seen_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        tx.send(Event::Input("/agents".into())).unwrap();
        wait_for(
            &output,
            "a1 | running | provider=local | model=fake-model | task=inspect file | tokens in=? out=? (unavailable)",
        );
        *gate.0.lock().unwrap() = true;
        gate.1.notify_all();
        wait_for(&output, "[main] done");
        tx.send(Event::Input("/agents".into())).unwrap();
        wait_for(
            &output,
            "a1 | done | provider=local | model=fake-model | task=inspect file | tokens in=3 out=1 (complete)",
        );
        wait_for(
            &output,
            "main | done | provider=local | model=fake-model | task=investigate | tokens in=15 out=4 (complete)",
        );
        tx.send(Event::Input("/exit".into())).unwrap();
        ui.join().unwrap();
        server.join().unwrap();
        assert!(saw_wait_result.load(Ordering::Acquire));
        std::fs::remove_dir_all(cwd).ok();
    }

    #[test]
    fn exit_marks_busy_parent_interrupted_and_late_reply_cannot_change_it() {
        let cwd = temp_dir("exit");
        let (seen_tx, seen_rx) = mpsc::channel();
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let worker_gate = gate.clone();
        let agents = Agents::memory(model_fn(move |_, _| {
            seen_tx.send(()).ok();
            let (lock, changed) = &*worker_gate;
            let mut released = lock.lock().unwrap();
            while !*released {
                released = changed.wait(released).unwrap();
            }
            Ok(ChatResult {
                text: ">> write edited.txt <<EOF\nlate\nEOF".into(),
                usage: Usage::default(),
            })
        }));
        let (tx, output, ui) = start_ui(
            cfg(cwd.clone(), "http://unused".into(), &agents),
            agents.clone(),
        );
        tx.send(Event::Input("edit late".into())).unwrap();
        seen_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        tx.send(Event::Input("/exit".into())).unwrap();
        ui.join().unwrap();
        assert_eq!(agents.snapshot()[0].status, agents::Status::Interrupted);
        *gate.0.lock().unwrap() = true;
        gate.1.notify_all();
        assert!(!cwd.join("edited.txt").exists());
        assert!(
            output
                .0
                .lock()
                .unwrap()
                .iter()
                .any(|s| s.contains("[main] started"))
        );
        std::fs::remove_dir_all(cwd).ok();
    }

    #[test]
    fn edit_preview_is_bounded() {
        let preview = edit_preview(Some("old"), &"x\n".repeat(1000));
        assert!(preview.contains("- old"));
        assert!(preview.contains("+ x"));
        assert!(preview.len() < 1300);
    }

    #[test]
    fn long_old_lines_cannot_hide_proposed_edit_in_approval_preview() {
        let old = (0..8)
            .map(|_| "界".repeat(200))
            .collect::<Vec<_>>()
            .join("\n");
        let preview = edit_preview(Some(&old), "PROPOSED NEW CONTENT\nsecond line");
        assert!(preview.contains("- 界"));
        assert!(preview.contains("+ PROPOSED NEW CONTENT"));
        assert!(preview.contains("+ second line"));
        assert!(preview.len() <= 1200);
    }

    #[test]
    fn write_preview_shows_existing_content_before_overwrite() {
        let dir = temp_dir("write-preview");
        let path = dir.join("file.txt");
        std::fs::write(&path, "old line\n").unwrap();
        let preview = write_preview(Some(&std::fs::read(&path).unwrap()), "new line");
        assert!(preview.contains("overwrites existing file"));
        assert!(preview.contains("- old line"));
        assert!(preview.contains("+ new line"));
        assert!(preview.len() < 1300);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn first_run_setup_saves_private_settings_and_preserves_other_keys() {
        let dir = temp_dir("setup");
        let path = dir.join(".revex");
        std::fs::write(&path, r#"{"route":"off"}"#).unwrap();
        let mut answers = VecDeque::from([
            Some("bad-url".to_string()),
            Some("http://127.0.0.1:1234/v1/".to_string()),
            Some("test-key".to_string()),
            Some("model-a".to_string()),
        ]);
        assert!(
            first_run_with(&path, |_, _| Ok(answers.pop_front().unwrap()))
                .unwrap()
                .is_some()
        );
        let saved = read_settings(&path).unwrap();
        assert_eq!(saved["base_url"], "http://127.0.0.1:1234/v1");
        assert_eq!(saved["api_key"], "test-key");
        assert_eq!(saved["model"], "model-a");
        assert_eq!(saved["route"], "off");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn first_run_cancel_or_eof_does_not_save_partial_settings() {
        let dir = temp_dir("setup-cancel");
        let path = dir.join(".revex");
        let mut answers = VecDeque::from([Some("http://localhost:1234/v1".into()), None]);
        assert!(
            first_run_with(&path, |_, _| Ok(answers.pop_front().unwrap()))
                .unwrap()
                .is_none()
        );
        assert!(!path.exists());
        assert!(
            first_run_with(&path, |_, _| Ok(Some("/exit".into())))
                .unwrap()
                .is_none()
        );
        assert!(!path.exists());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn returning_settings_option_can_change_url_and_keep_or_replace_other_fields() {
        let dir = temp_dir("settings-edit");
        let path = dir.join(".revex");
        let agents = Agents::memory(model_fn(|_, _| unreachable!()));
        let mut cfg = cfg(dir.clone(), "http://old.example/v1".into(), &agents);
        cfg.key = "old-key".into();
        cfg.model = Some("old-model".into());
        let mut keep = VecDeque::from([
            Some(String::new()),
            Some(String::new()),
            Some(String::new()),
        ]);
        assert!(edit_settings_with(&mut cfg, &path, |_, _| Ok(keep.pop_front().unwrap())).unwrap());
        assert_eq!(cfg.base, "http://old.example/v1");
        assert_eq!(cfg.key, "old-key");
        assert_eq!(cfg.model.as_deref(), Some("old-model"));

        let mut change = VecDeque::from([
            Some("http://new.example/v1".into()),
            Some("new-key".into()),
            Some("new-model".into()),
        ]);
        assert!(
            edit_settings_with(&mut cfg, &path, |_, _| Ok(change.pop_front().unwrap())).unwrap()
        );
        assert_eq!(cfg.base, "http://new.example/v1");
        assert_eq!(cfg.key, "new-key");
        assert_eq!(cfg.model.as_deref(), Some("new-model"));
        assert_eq!(
            read_settings(&path).unwrap()["base_url"],
            "http://new.example/v1"
        );

        let mut cancel = VecDeque::from([Some("/exit".into())]);
        assert!(
            !edit_settings_with(&mut cfg, &path, |_, _| Ok(cancel.pop_front().unwrap())).unwrap()
        );
        assert_eq!(cfg.base, "http://new.example/v1");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn cancelled_yes_mode_cannot_write() {
        let cwd = temp_dir("cancelled-yes");
        let (events, _) = mpsc::channel();
        let cancelled = Arc::new(AtomicBool::new(true));
        let agents = Agents::memory(model_fn(|_, _| unreachable!()));
        let mut cfg = cfg(cwd.clone(), "http://unused".into(), &agents);
        cfg.yes = true;
        cfg.ui = Some(UiHandle {
            events,
            cancelled,
            suppress_routine: false,
            #[cfg(unix)]
            shell: Arc::new(ShellTracker::default()),
        });
        assert_eq!(
            crate::run(&cfg, &Call::Write("edited.txt".into(), "late".into())),
            "interrupted"
        );
        assert!(!cwd.join("edited.txt").exists());
        std::fs::remove_dir_all(cwd).ok();
    }

    #[test]
    fn edit_approval_refuses_a_file_changed_while_user_decides() {
        let cwd = temp_dir("edit-race");
        let agents = Agents::memory(model_fn(|_, _| unreachable!()));
        for (call, file, original, changed) in [
            (
                Call::Write("write.txt".into(), "new".into()),
                "write.txt",
                Some("old"),
                "external",
            ),
            (
                Call::Replace("replace.txt".into(), "old\n===\nnew".into()),
                "replace.txt",
                Some("old"),
                "external",
            ),
            (
                Call::Write("appeared.txt".into(), "new".into()),
                "appeared.txt",
                None,
                "external",
            ),
        ] {
            let path = cwd.join(file);
            if let Some(old) = original {
                std::fs::write(&path, old).unwrap();
            }
            let (events, requests) = mpsc::channel();
            let mut config = cfg(cwd.clone(), "http://unused".into(), &agents);
            config.ui = Some(UiHandle {
                events,
                cancelled: Arc::new(AtomicBool::new(false)),
                suppress_routine: false,
                #[cfg(unix)]
                shell: Arc::new(ShellTracker::default()),
            });
            let worker = std::thread::spawn(move || crate::run(&config, &call));
            let Event::Approval {
                answer, preview, ..
            } = requests.recv_timeout(Duration::from_secs(2)).unwrap()
            else {
                panic!("edit did not request approval");
            };
            if original.is_some() {
                assert!(preview.contains("- old"));
            }
            std::fs::write(&path, changed).unwrap();
            answer.send(true).unwrap();
            assert_eq!(worker.join().unwrap(), EDIT_CHANGED);
            assert_eq!(std::fs::read_to_string(&path).unwrap(), changed);
        }
        std::fs::remove_dir_all(cwd).ok();
    }

    #[cfg(unix)]
    #[test]
    fn exit_kills_and_reaps_running_shell_group_before_returning() {
        let cwd = temp_dir("shell-exit");
        let agents = Agents::memory(model_fn(|_, _| {
            Ok(ChatResult {
                text: ">> sh echo $$ > shell.pid; sleep 1; echo late > late.txt".into(),
                usage: Usage::default(),
            })
        }));
        let mut config = cfg(cwd.clone(), "http://unused".into(), &agents);
        config.yes = true;
        let (tx, _output, ui) = start_ui(config, agents.clone());
        tx.send(Event::Input("run a shell".into())).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while !cwd.join("shell.pid").exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(cwd.join("shell.pid").exists(), "shell never started");
        tx.send(Event::Input("/exit".into())).unwrap();
        ui.join().unwrap(); // close waits for wait_with_output to reap the shell
        assert_eq!(agents.snapshot()[0].status, agents::Status::Interrupted);
        std::thread::sleep(Duration::from_millis(1100));
        assert!(
            !cwd.join("late.txt").exists(),
            "shell continued after Revex exited"
        );
        std::fs::remove_dir_all(cwd).ok();
    }
}
