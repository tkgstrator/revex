//! Terminal coordinator. Only this thread interprets input and answers approvals;
//! model and helper threads communicate through events and never read stdin.

use super::*;
use std::collections::VecDeque;
use std::io::BufRead;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
#[cfg(unix)]
use std::sync::{Condvar, Mutex};
use std::time::Duration;

#[derive(Clone)]
pub(crate) struct UiHandle {
    events: Sender<Event>,
    cancelled: Arc<AtomicBool>,
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
        let id = match &cfg.role {
            Role::Parent(_) => "main",
            Role::Child(_, id) => id,
        };
        let _ = self
            .events
            .send(Event::Progress(format!("[{id}] {message}")));
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
    Input(String),
    Eof,
    Progress(String),
    Approval {
        what: String,
        preview: String,
        answer: Sender<bool>,
    },
    Finished(Result<AgentAnswer, String>, Vec<Value>),
}

static SIGINT_REQUESTED: AtomicBool = AtomicBool::new(false);

fn banner() -> Vec<String> {
    let title = format!(
        "Revex {}  |  Local AI Coding Agent",
        env!("CARGO_PKG_VERSION")
    );
    let edge = format!("+{}+", "-".repeat(title.len() + 2));
    vec![edge.clone(), format!("| {title} |"), edge]
}

fn safe_field(value: &str, limit: usize) -> String {
    let clean: String = value
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect();
    short(&clean, limit)
}

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

fn history_for_welcome(cwd: &Path) -> String {
    let result = state_dir()
        .map(|dir| history_for(&dir, cwd))
        .and_then(|h| load_history(&h));
    match result {
        Ok(msgs) => history_label(&msgs),
        Err(_) => "saved history needs attention".into(),
    }
}

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
struct EchoGuard(Option<libc::termios>);

#[cfg(unix)]
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
impl Drop for EchoGuard {
    fn drop(&mut self) {
        if let Some(old) = &self.0 {
            unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, old) };
        }
    }
}

#[cfg(not(unix))]
struct EchoGuard;

#[cfg(not(unix))]
impl EchoGuard {
    fn hide() -> Result<Self, String> {
        Err("hidden terminal input is unavailable".into())
    }
}

fn first_run_with(
    path: &Path,
    mut ask: impl FnMut(&str, bool) -> Result<Option<String>, String>,
) -> Result<Option<()>, String> {
    let base = loop {
        let Some(raw) = ask("  1/3  API base URL (or /exit): ", false)? else {
            return Ok(None);
        };
        if raw == "/exit" || raw == "/quit" {
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
    if key == "/exit" || key == "/quit" {
        return Ok(None);
    }
    let Some(model) = ask(
        "  3/3  Model ID (optional; Enter for server default): ",
        false,
    )?
    else {
        return Ok(None);
    };
    if model == "/exit" || model == "/quit" {
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
}

impl Session {
    fn new(mut cfg: Cfg, msgs: Vec<Value>, agents: Arc<Agents>, events: Sender<Event>) -> Self {
        let cancelled = Arc::new(AtomicBool::new(false));
        cfg.quiet = true; // streamed text is emitted by the coordinator with an agent label
        cfg.ui = Some(UiHandle {
            events: events.clone(),
            cancelled: cancelled.clone(),
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
        out(&format!("[main] started: {}", short(&task, 100)));
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
        if matches!(line, "/exit" | "/quit") {
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
        match result {
            Ok(answer) => out(&format!("[main] {}", answer.text())),
            Err(e) => {
                out(&format!("[main] error: {e}"));
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
        "{} | {} | model={} | task={} | tokens in={} out={} ({})",
        info.id,
        info.status.name(),
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
        for line in body.lines().take(limit) {
            lines.push(format!("{prefix} {}", short(line, 160)));
        }
        if body.lines().count() > limit {
            lines.push(format!("{prefix} ... (more lines omitted)"));
        }
    };
    if let Some(old) = old {
        add("-", old, 8);
        add("+", new, 8);
    } else {
        add("+", new, 16);
    }
    let joined = lines.join("\n");
    if joined.len() <= 1200 {
        return joined;
    }
    let mut end = 1200;
    while !joined.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n... (preview truncated)", &joined[..end])
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
            api: Api::Chat,
            base,
            key: String::new(),
            model: Some("fake-model".into()),
            yes: false,
            history: None,
            strict_save: false,
            cwd,
            role: Role::Parent(Some(agents.clone())),
            ui: None,
        }
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
            "main | running | model=fake-model | task=edit a file | tokens in=? out=? (unavailable)",
        );
        tx.send(Event::Input("/status".into())).unwrap();
        wait_for(&output, "key=not set main=running");
        release_tx.send(()).unwrap();
        wait_for(&output, "approval: write edited.txt");
        assert!(!cwd.join("edited.txt").exists());
        tx.send(Event::Input("/agents".into())).unwrap();
        wait_for_count(
            &output,
            "main | running | model=fake-model | task=edit a file",
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
                let is_child = messages[0]["content"] == CHILD_SYSTEM;
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
            "a1 | running | model=fake-model | task=inspect file | tokens in=? out=? (unavailable)",
        );
        *gate.0.lock().unwrap() = true;
        gate.1.notify_all();
        wait_for(&output, "[main] done");
        tx.send(Event::Input("/agents".into())).unwrap();
        wait_for(
            &output,
            "a1 | done | model=fake-model | task=inspect file | tokens in=3 out=1 (complete)",
        );
        wait_for(
            &output,
            "main | done | model=fake-model | task=investigate | tokens in=15 out=4 (complete)",
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
