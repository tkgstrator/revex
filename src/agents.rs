//! Helper agents, started by the parent's `>> spawn` and collected with `>> wait`.
//! Each child has its own history, status and usage. Records are saved privately so a
//! restart can show what happened; unfinished work becomes interrupted and is never rerun.

use super::*;
use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::{Condvar, Mutex, MutexGuard};

/// Children running at once; more are queued.
pub(crate) const MAX_RUNNING: usize = 2;
/// Children waiting for a slot; more are refused.
pub(crate) const MAX_QUEUED: usize = 8;
const RECORD_VERSION: u64 = 1;
const PARENT_ID: &str = "main";
const EXTERNAL_OUTPUT_LIMIT: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Provider {
    Local,
    Codex,
    Claude,
}

impl Provider {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Codex => "codex",
            Self::Claude => "claude",
        }
    }

    fn parse(name: &str) -> Option<Self> {
        match name {
            "local" => Some(Self::Local),
            "codex" => Some(Self::Codex),
            "claude" => Some(Self::Claude),
            _ => None,
        }
    }

    fn index(self) -> usize {
        match self {
            Self::Local => 0,
            Self::Codex => 1,
            Self::Claude => 2,
        }
    }
}

/// One model request. `chat` in normal use; tests substitute a fake provider.
pub(crate) type ModelFn = dyn Fn(&Cfg, &[Value]) -> Result<ChatResult, String> + Send + Sync;
type ErrorSink = Arc<dyn Fn(String) + Send + Sync>;

pub(crate) fn model_fn(
    f: impl Fn(&Cfg, &[Value]) -> Result<ChatResult, String> + Send + Sync + 'static,
) -> Arc<ModelFn> {
    Arc::new(f)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Status {
    /// The parent before its first task.
    Idle,
    Queued,
    Running,
    Done,
    Failed,
    Interrupted,
}

impl Status {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Interrupted => "interrupted",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        [
            Self::Idle,
            Self::Queued,
            Self::Running,
            Self::Done,
            Self::Failed,
            Self::Interrupted,
        ]
        .into_iter()
        .find(|v| v.name() == s)
    }

    fn finished(self) -> bool {
        matches!(self, Self::Done | Self::Failed | Self::Interrupted)
    }
}

/// What `/agents` shows for one agent.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AgentInfo {
    pub(crate) id: String,
    pub(crate) provider: Provider,
    pub(crate) task: String,
    pub(crate) status: Status,
    pub(crate) model: Option<String>,
    pub(crate) cwd: String,
    /// Every model request of this agent; missing provider usage is counted as missing.
    pub(crate) usage: UsageTotal,
    /// Reported by the external helper CLI; not inferred from token prices.
    pub(crate) cost_usd: Option<f64>,
}

struct Child {
    info: AgentInfo,
    result: Option<String>,
    error: Option<String>,
    /// The parent already received the result; a repeated wait does not insert it again.
    delivered: bool,
    messages: Vec<Value>,
    /// Why the latest record save failed, reported with the result.
    unsaved: Option<String>,
}

struct State {
    parent: AgentInfo,
    children: Vec<Child>,
    queue: VecDeque<(String, Cfg, Provider)>,
    providers: [bool; 3],
    provider_cursor: usize,
    external_pids: HashMap<String, i32>,
    external_threads: Vec<std::thread::JoinHandle<()>>,
    next: u64,
    stopped: bool,
}

pub(crate) struct Agents {
    /// None keeps records in memory only (one-shot and piped runs).
    dir: Option<PathBuf>,
    model: Arc<ModelFn>,
    state: Mutex<State>,
    changed: Condvar,
    error_sink: Mutex<Option<ErrorSink>>,
}

fn id_number(id: &str) -> Option<u64> {
    let n = id.strip_prefix('a')?;
    if n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    n.parse().ok()
}

fn find<'a>(s: &'a mut State, id: &str) -> Option<&'a mut Child> {
    s.children.iter_mut().find(|c| c.info.id == id)
}

fn select_provider<'a>(s: &mut State, task: &'a str) -> Result<(Provider, &'a str), String> {
    let task = task.trim();
    if task.is_empty() {
        return Err("spawn needs a TASK".into());
    }
    if task.starts_with('@') {
        let (word, rest) = task.split_once(char::is_whitespace).unwrap_or((task, ""));
        let provider = Provider::parse(&word[1..])
            .ok_or_else(|| format!("unknown helper provider `{word}`"))?;
        if !s.providers[provider.index()] {
            return Err(format!(
                "helper provider `{}` is not enabled",
                provider.name()
            ));
        }
        let rest = rest.trim();
        if rest.is_empty() {
            return Err("spawn needs a TASK".into());
        }
        return Ok((provider, rest));
    }
    for _ in 0..3 {
        let index = s.provider_cursor % 3;
        s.provider_cursor = (index + 1) % 3;
        if s.providers[index] {
            return Ok((
                [Provider::Local, Provider::Codex, Provider::Claude][index],
                task,
            ));
        }
    }
    Err("no helper provider is enabled".into())
}

fn selection_from_file(path: &Path) -> Result<[bool; 3], String> {
    let settings = read_settings(path)?;
    let Some(value) = settings.get("helper_providers") else {
        return Ok([true, false, false]);
    };
    let csv = value
        .as_str()
        .ok_or("helper_providers in ~/.revex must be a string")?;
    let mut selected = [false; 3];
    for word in csv.split(',') {
        let name = word.trim();
        let provider =
            Provider::parse(name).ok_or_else(|| format!("unknown helper provider `{name}`"))?;
        let slot = &mut selected[provider.index()];
        if *slot {
            return Err(format!("duplicate helper provider `{name}`"));
        }
        *slot = true;
    }
    if !selected.iter().any(|enabled| *enabled) {
        return Err("select at least one helper provider".into());
    }
    Ok(selected)
}

fn kill_process_group(pid: i32) {
    // External helpers start in a new process group. A negative PID targets
    // their group, including grandchildren that kept the group after CLI exit.
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
    }
}

struct ExternalProcess {
    child: std::process::Child,
    pid: i32,
}

impl Drop for ExternalProcess {
    fn drop(&mut self) {
        kill_process_group(self.pid);
        let _ = self.child.wait();
    }
}

fn capture_bounded(mut stream: impl Read) -> Result<String, String> {
    let mut output = Vec::new();
    let mut buf = [0_u8; 8192];
    loop {
        let size = stream.read(&mut buf).map_err(|e| e.to_string())?;
        if size == 0 {
            break;
        }
        output.extend_from_slice(&buf[..size]);
        if output.len() > EXTERNAL_OUTPUT_LIMIT {
            output.drain(..output.len() - EXTERNAL_OUTPUT_LIMIT);
        }
    }
    Ok(String::from_utf8_lossy(&output).into_owned())
}

fn reported_usage(v: &Value) -> Option<Usage> {
    let usage = &v["usage"];
    let input = usage["input_tokens"].as_u64();
    let output = usage["output_tokens"].as_u64();
    (input.is_some() || output.is_some()).then_some(Usage { input, output })
}

fn parse_external_output(
    provider: Provider,
    output: &str,
) -> Result<(String, Option<Usage>, Option<f64>), String> {
    match provider {
        Provider::Codex => {
            let mut answer = None;
            let mut usage = None;
            let mut completed = false;
            for line in output.lines() {
                let Ok(v) = serde_json::from_str::<Value>(line) else {
                    continue;
                };
                match v["type"].as_str() {
                    Some("item.completed") if v["item"]["type"] == "agent_message" => {
                        if let Some(text) = v["item"]["text"].as_str() {
                            answer = Some(text.to_string());
                        }
                    }
                    Some("turn.completed") => {
                        completed = true;
                        usage = reported_usage(&v);
                    }
                    _ => {}
                }
            }
            if !completed {
                return Err("codex helper ended without a completed turn".into());
            }
            Ok((
                answer.ok_or("codex helper returned no final answer")?,
                usage,
                None,
            ))
        }
        Provider::Claude => {
            let v: Value = serde_json::from_str(output)
                .map_err(|e| format!("claude helper returned invalid JSON: {e}"))?;
            if v["type"] != "result" || v["subtype"].as_str().is_some_and(|s| s != "success") {
                return Err("claude helper did not report success".into());
            }
            let answer = v["result"]
                .as_str()
                .ok_or("claude helper returned no final answer")?;
            let cost = v["total_cost_usd"]
                .as_f64()
                .filter(|value| value.is_finite() && *value >= 0.0);
            Ok((answer.into(), reported_usage(&v), cost))
        }
        Provider::Local => Err("local helper cannot use external runner".into()),
    }
}

fn external_command(provider: Provider, cwd: &Path, program: &Path) -> Result<Command, String> {
    let mut command = Command::new(program);
    match provider {
        Provider::Codex => {
            command
                .args([
                    "exec",
                    "--dangerously-bypass-approvals-and-sandbox",
                    "--skip-git-repo-check",
                    "--json",
                    "-C",
                ])
                .arg(cwd)
                .arg("-");
        }
        Provider::Claude => {
            command.args([
                "-p",
                "--dangerously-skip-permissions",
                "--output-format",
                "json",
            ]);
        }
        Provider::Local => return Err("local helper cannot use external runner".into()),
    }
    command
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // This credential belongs to Revex's local API endpoint. External CLIs
    // inherit their own authentication environment unchanged.
    command.env_remove("REVEX_API_KEY");
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    Ok(command)
}

impl Agents {
    pub(crate) fn memory(model: Arc<ModelFn>) -> Arc<Self> {
        Self::with(None, model, None, vec![])
    }

    /// Load the records in `dir`. Records left queued or running by an earlier process are
    /// marked interrupted and saved; they are not started again. A corrupt record is an error.
    pub(crate) fn open(dir: PathBuf, model: Arc<ModelFn>) -> Result<Arc<Self>, String> {
        let mut parent = load_parent(&dir)?;
        if let Some(info) = parent.as_mut()
            && !info.status.finished()
            && info.status != Status::Idle
        {
            info.status = Status::Interrupted;
            save_parent(&dir, info)?;
        }
        let mut children = load_records(&dir)?;
        for c in children.iter_mut().filter(|c| !c.info.status.finished()) {
            c.info.status = Status::Interrupted;
            c.error = Some(
                "interrupted: revex stopped before this agent finished; it was not rerun".into(),
            );
            save_record(&dir, c)?;
        }
        Ok(Self::with(Some(dir), model, parent, children))
    }

    fn with(
        dir: Option<PathBuf>,
        model: Arc<ModelFn>,
        parent: Option<AgentInfo>,
        children: Vec<Child>,
    ) -> Arc<Self> {
        let next = children
            .iter()
            .filter_map(|c| id_number(&c.info.id))
            .max()
            .unwrap_or(0)
            + 1;
        let parent = parent.unwrap_or_else(|| AgentInfo {
            id: PARENT_ID.into(),
            provider: Provider::Local,
            task: String::new(),
            status: Status::Idle,
            model: None,
            cwd: String::new(),
            usage: UsageTotal::default(),
            cost_usd: None,
        });
        Arc::new(Self {
            dir,
            model,
            state: Mutex::new(State {
                parent,
                children,
                queue: VecDeque::new(),
                providers: [true, false, false],
                provider_cursor: 0,
                external_pids: HashMap::new(),
                external_threads: Vec::new(),
                next,
                stopped: false,
            }),
            changed: Condvar::new(),
            error_sink: Mutex::new(None),
        })
    }

    pub(crate) fn set_error_sink(&self, sink: impl Fn(String) + Send + Sync + 'static) {
        *self.error_sink.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(sink));
    }

    pub(crate) fn set_providers(&self, selected: [bool; 3]) -> Result<(), String> {
        if !selected.iter().any(|enabled| *enabled) {
            return Err("select at least one helper provider".into());
        }
        let mut state = self.lock();
        state.providers = selected;
        state.provider_cursor = 0;
        Ok(())
    }

    pub(crate) fn provider_enabled(&self, provider: Provider) -> bool {
        self.lock().providers[provider.index()]
    }

    /// Noninteractive sessions keep running when home helper selection is invalid.
    /// Invalid settings can never opt into an external, editable helper.
    pub(crate) fn set_providers_from_home_file(&self, home: Option<&Path>) -> Option<String> {
        let (selected, warning) = match home.map(selection_from_file) {
            None => ([true, false, false], None),
            Some(Ok(selected)) => (selected, None),
            Some(Err(error)) => ([true, false, false], Some(error)),
        };
        // Both a parsed nonempty selection and the fallback satisfy set_providers.
        self.set_providers(selected)
            .expect("nonempty helper selection");
        warning
    }

    fn report_error(&self, message: String) {
        let sink = self.error_sink.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(sink) = sink.as_ref() {
            sink(message);
        } else {
            eprintln!("{message}");
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn model(&self) -> Arc<ModelFn> {
        self.model.clone()
    }

    /// Parent first, then children in spawn order. Safe to call from any thread at any time.
    pub(crate) fn snapshot(&self) -> Vec<AgentInfo> {
        let s = self.lock();
        std::iter::once(s.parent.clone())
            .chain(s.children.iter().map(|c| c.info.clone()))
            .collect()
    }

    fn save_parent(&self, info: &AgentInfo) -> Result<(), String> {
        match &self.dir {
            Some(dir) => save_parent(dir, info),
            None => Ok(()),
        }
    }

    pub(crate) fn parent_started(&self, cfg: &Cfg, task: &str) -> Result<(), String> {
        let mut s = self.lock();
        s.parent.task = task.into();
        s.parent.status = Status::Running;
        s.parent.model = cfg.model.clone();
        s.parent.cwd = cwd_identity(&cfg.cwd);
        self.save_parent(&s.parent).map_err(|e| {
            s.parent.status = Status::Failed;
            format!("parent record not saved: {e}")
        })
    }

    pub(crate) fn parent_finished(&self, ok: bool) -> Result<(), String> {
        let mut g = self.lock();
        if g.parent.status == Status::Running {
            g.parent.status = if ok { Status::Done } else { Status::Failed };
        }
        self.save_parent(&g.parent)
            .map_err(|e| format!("parent record not saved: {e}"))
    }

    /// An interactive session was closed while the parent was still in a turn.
    /// A late model response cannot turn this into a completed or failed run.
    pub(crate) fn parent_interrupted(&self) -> Result<(), String> {
        let mut g = self.lock();
        if g.parent.status == Status::Running {
            g.parent.status = Status::Interrupted;
        }
        self.save_parent(&g.parent)
            .map_err(|e| format!("parent record not saved: {e}"))
    }

    /// Clear one completed interactive session. Move the agent directory aside
    /// before saving the new parent history, so a history-save failure can put
    /// every agent record back without exposing a partial reset on restart.
    pub(crate) fn clear_with(
        &self,
        save_history: impl FnOnce() -> Result<(), String>,
    ) -> Result<(), String> {
        let mut s = self.lock();
        if s.stopped
            || s.parent.status == Status::Running
            || s.children.iter().any(|c| !c.info.status.finished())
        {
            return Err("helper agents are still active; wait for them before /clear".into());
        }
        let backup = if let Some(dir) = &self.dir {
            match std::fs::read_dir(dir) {
                Ok(entries) => {
                    for entry in entries {
                        let entry = entry.map_err(|e| format!("{}: {e}", dir.display()))?;
                        let name = entry.file_name();
                        let name = name.to_string_lossy();
                        let known = name == "main.json"
                            || name.strip_suffix(".json").and_then(id_number).is_some();
                        if !known {
                            return Err(format!(
                                "agent state contains an unrecognized file ({name}); /clear refused"
                            ));
                        }
                    }
                    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
                    let moved = dir.with_extension(format!("clear-{}-{seq}", std::process::id()));
                    std::fs::rename(dir, &moved).map_err(|e| format!("{}: {e}", dir.display()))?;
                    Some((dir.clone(), moved))
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => return Err(format!("{}: {e}", dir.display())),
            }
        } else {
            None
        };
        if let Err(e) = save_history() {
            if let Some((dir, moved)) = backup {
                std::fs::rename(&moved, &dir).map_err(|restore| {
                    format!("{e}; agent records could not be restored: {restore}")
                })?;
            }
            return Err(e);
        }
        s.parent.task.clear();
        s.parent.status = Status::Idle;
        s.parent.model = None;
        s.parent.cwd.clear();
        s.parent.usage = UsageTotal::default();
        s.children.clear();
        s.queue.clear();
        s.next = 1;
        s.provider_cursor = 0;
        if let Some((_, moved)) = backup
            && let Err(e) = std::fs::remove_dir_all(&moved)
        {
            self.report_error(format!(
                "warning: session cleared but old agent records remain at {}: {e}",
                moved.display()
            ));
        }
        Ok(())
    }

    /// Record one model request for the parent (`None`) or a child.
    pub(crate) fn add_usage(&self, id: Option<&str>, u: Option<Usage>) {
        let mut g = self.lock();
        let s = &mut *g;
        let total = match id {
            None => &mut s.parent.usage,
            Some(id) => match find(s, id) {
                Some(c) => &mut c.info.usage,
                None => return,
            },
        };
        match u {
            Some(u) => total.add(u),
            None => total.add_unreported(),
        }
        if id.is_none()
            && let Err(e) = self.save_parent(&s.parent)
        {
            self.report_error(format!("parent usage not saved: {e}"));
        }
    }

    fn stopped(&self) -> bool {
        self.lock().stopped
    }

    fn save(&self, c: &Child) -> Result<(), String> {
        match &self.dir {
            Some(d) => save_record(d, c),
            None => Ok(()),
        }
    }

    /// Start a child on `task` in the parent's cwd, or queue it when both slots are busy.
    pub(crate) fn spawn(self: &Arc<Self>, cfg: &Cfg, task: &str) -> String {
        let mut g = self.lock();
        if g.stopped {
            return "error: the session is ending; no agent was started".into();
        }
        if g.queue.len() >= MAX_QUEUED {
            return format!(
                "error: {MAX_QUEUED} agents are already queued; wait for some before spawning more"
            );
        }
        let (provider, task) = match select_provider(&mut g, task) {
            Ok(selected) => selected,
            Err(e) => return format!("error: {e}"),
        };
        match self.start_selected(&mut g, cfg, provider, task) {
            Ok((id, Status::Running)) => format!("{id} started"),
            Ok((id, Status::Queued)) => format!(
                "{id} queued: {MAX_RUNNING} agents are running; it starts when one finishes"
            ),
            Ok((id, _)) => format!("{id} failed to start"),
            Err(e) => format!("error: {e}"),
        }
    }

    /// Run a specifically requested external provider and return its answer unchanged.
    /// The child still uses the ordinary records, queue, and shutdown path.
    pub(crate) fn invoke(
        self: &Arc<Self>,
        cfg: &Cfg,
        provider: Provider,
        task: &str,
    ) -> Result<String, String> {
        if provider == Provider::Local {
            return Err("direct invocation requires an external helper".into());
        }
        let id = {
            let mut g = self.lock();
            if !g.providers[provider.index()] {
                return Err(format!(
                    "helper provider `{}` is not enabled",
                    provider.name()
                ));
            }
            self.start_selected(&mut g, cfg, provider, task)?.0
        };
        let mut g = self.lock();
        while !g.stopped
            && g.children
                .iter()
                .any(|c| c.info.id == id && !c.info.status.finished())
        {
            g = self.changed.wait(g).unwrap_or_else(|e| e.into_inner());
        }
        let c = find(&mut g, &id).ok_or("helper record missing")?;
        if !c.info.status.finished() {
            return Err("interrupted: parent session ended".into());
        }
        c.delivered = true;
        let result = if c.info.status == Status::Done {
            c.result.clone().ok_or("helper returned no result".into())
        } else {
            Err(c.error.clone().unwrap_or_else(|| "helper failed".into()))
        };
        self.save(c)
            .map_err(|e| format!("agent record not saved: {e}"))?;
        result
    }

    fn start_selected(
        self: &Arc<Self>,
        g: &mut State,
        cfg: &Cfg,
        provider: Provider,
        task: &str,
    ) -> Result<(String, Status), String> {
        if g.stopped {
            return Err("the session is ending; no agent was started".into());
        }
        if g.queue.len() >= MAX_QUEUED {
            return Err(format!(
                "{MAX_QUEUED} agents are already queued; wait for some before spawning more"
            ));
        }
        let id = format!("a{}", g.next);
        g.next += 1;
        let child_cfg = Cfg {
            quiet: true,
            history: None,
            strict_save: false,
            frontier: None,
            route: Route::Off,
            yes: false,
            // set to Role::Child when the job starts
            role: Role::Parent(None),
            ..cfg.clone()
        };
        let c = Child {
            info: AgentInfo {
                id: id.clone(),
                provider,
                task: task.into(),
                status: Status::Queued,
                model: (provider == Provider::Local)
                    .then(|| cfg.model.clone())
                    .flatten(),
                cwd: cwd_identity(&cfg.cwd),
                usage: UsageTotal::default(),
                cost_usd: None,
            },
            result: None,
            error: None,
            delivered: false,
            messages: vec![
                json!({"role": "system", "content": CHILD_SYSTEM}),
                json!({"role": "user", "content": task}),
            ],
            unsaved: None,
        };
        if let Err(e) = self.save(&c) {
            return Err(format!(
                "agent record not saved ({e}); no agent was started"
            ));
        }
        g.children.push(c);
        g.queue.push_back((id.clone(), child_cfg, provider));
        self.pump(g);
        let status = g.children.last().map(|c| c.info.status);
        match status {
            Some(Status::Running | Status::Queued) => Ok((id, status.unwrap())),
            _ => Err(format!(
                "{id} failed to start: {}",
                g.children
                    .last()
                    .and_then(|c| c.error.clone())
                    .unwrap_or_default()
            )),
        }
    }

    /// Start queued jobs while slots are free.
    fn pump(self: &Arc<Self>, s: &mut State) {
        while !s.stopped
            && s.children
                .iter()
                .filter(|c| c.info.status == Status::Running)
                .count()
                < MAX_RUNNING
        {
            let Some((id, cfg, provider)) = s.queue.pop_front() else {
                return;
            };
            let me = self.clone();
            let tid = id.clone();
            let started = std::thread::Builder::new()
                .name(format!("revex-{id}"))
                .spawn(move || me.run_child(tid, cfg, provider));
            let Some(c) = find(s, &id) else { continue };
            let mut external_handle = None;
            match started {
                Ok(handle) => {
                    c.info.status = Status::Running;
                    if provider != Provider::Local {
                        external_handle = Some(handle);
                    }
                }
                Err(e) => {
                    c.info.status = Status::Failed;
                    c.error = Some(format!("cannot start thread: {e}"));
                }
            }
            c.unsaved = self.save(c).err();
            if let Some(handle) = external_handle {
                s.external_threads.push(handle);
            }
        }
    }

    fn run_child(self: Arc<Self>, id: String, mut cfg: Cfg, provider: Provider) {
        if provider != Provider::Local {
            let task = self
                .lock()
                .children
                .iter()
                .find(|c| c.info.id == id)
                .map(|c| c.info.task.clone())
                .unwrap_or_default();
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                self.run_external(&id, provider, &cfg.cwd, &task)
            }))
            .unwrap_or_else(|_| Err("external helper crashed".into()));
            self.lock().external_pids.remove(&id);
            self.finish(&id, result);
            return;
        }
        cfg.role = Role::Child(self.clone(), id.clone());
        let mut msgs = find(&mut self.lock(), &id)
            .map(|c| c.messages.clone())
            .unwrap_or_default();
        let model = self.model();
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            steps(
                &cfg,
                &mut msgs,
                // the manager's record is the total that counts; see `add_usage`
                &mut UsageTotal::default(),
                &*model,
                &|| self.stopped(),
                &mut |m: &[Value]| self.save_messages(&id, m),
            )
        }))
        .unwrap_or_else(|_| Err("helper agent crashed".into()));
        self.finish(&id, r);
    }

    fn run_external(
        &self,
        id: &str,
        provider: Provider,
        cwd: &Path,
        task: &str,
    ) -> Result<String, String> {
        let program = match provider {
            Provider::Codex => Path::new("codex"),
            Provider::Claude => Path::new("claude"),
            Provider::Local => return Err("local helper cannot use external runner".into()),
        };
        self.run_external_program(id, provider, cwd, task, program)
    }

    fn run_external_program(
        &self,
        id: &str,
        provider: Provider,
        cwd: &Path,
        task: &str,
        program: &Path,
    ) -> Result<String, String> {
        let result = self.run_external_program_inner(id, provider, cwd, task, program);
        self.lock().external_pids.remove(id);
        result
    }

    fn run_external_program_inner(
        &self,
        id: &str,
        provider: Provider,
        cwd: &Path,
        task: &str,
        program: &Path,
    ) -> Result<String, String> {
        let mut command = external_command(provider, cwd, program)?;
        let child = command
            .spawn()
            .map_err(|e| format!("cannot launch {} helper: {e}", provider.name()))?;
        let pid = child.id() as i32;
        let mut process = ExternalProcess { child, pid };
        {
            let mut state = self.lock();
            if state.stopped {
                return Err("interrupted: parent session ended".into());
            }
            state.external_pids.insert(id.into(), pid);
        }
        let stdout = process.child.stdout.take().ok_or("missing helper stdout")?;
        let stderr = process.child.stderr.take().ok_or("missing helper stderr")?;
        let out_reader = std::thread::spawn(move || capture_bounded(stdout));
        let err_reader = std::thread::spawn(move || capture_bounded(stderr));
        let mut stdin = process.child.stdin.take().ok_or("missing helper stdin")?;
        let input = task.as_bytes().to_vec();
        let writer = std::thread::spawn(move || stdin.write_all(&input));
        let status = process
            .child
            .wait()
            .map_err(|e| format!("{} helper wait failed: {e}", provider.name()))?;
        // Descendants can outlive the CLI leader and keep its pipes open.
        // Close the entire owned group before joining pipe readers.
        kill_process_group(pid);
        let input_result = writer.join().map_err(|_| "helper stdin writer crashed")?;
        let output = out_reader
            .join()
            .map_err(|_| "helper stdout reader crashed")??;
        let error = err_reader
            .join()
            .map_err(|_| "helper stderr reader crashed")??;
        let parsed = parse_external_output(provider, &output);
        self.add_usage(
            Some(id),
            parsed.as_ref().ok().and_then(|(_, usage, _)| *usage),
        );
        if status.success()
            && let Ok((_, _, Some(cost))) = &parsed
        {
            let mut state = self.lock();
            if !state.stopped
                && let Some(child) = find(&mut state, id)
            {
                child.info.cost_usd = Some(*cost);
                if let Err(e) = self.save(child) {
                    self.report_error(format!("helper cost not saved: {e}"));
                }
            }
        }
        if !self.stopped() && status.success() {
            if let Err(e) = input_result {
                return Err(format!("{} helper input failed: {e}", provider.name()));
            }
            parsed.map(|(answer, _, _)| answer)
        } else if self.stopped() {
            Err("interrupted: parent session ended".into())
        } else {
            Err(format!(
                "{} helper exited with {}: {}",
                provider.name(),
                status,
                truncate(error.trim().to_string())
            ))
        }
    }

    fn save_messages(&self, id: &str, m: &[Value]) -> Result<(), String> {
        let mut g = self.lock();
        // After shutdown another process may own these records; stop writing them.
        if g.stopped {
            return Err("interrupted".into());
        }
        let Some(c) = find(&mut g, id) else {
            return Ok(());
        };
        c.messages = m.to_vec();
        self.save(c)
            .map_err(|e| format!("agent record not saved: {e}"))
    }

    fn finish(self: &Arc<Self>, id: &str, r: Result<String, String>) {
        let mut g = self.lock();
        // after shutdown the record already says interrupted and must stay that way
        if let Some(c) = find(&mut g, id)
            && c.info.status == Status::Running
        {
            match r {
                Ok(text) => {
                    c.info.status = Status::Done;
                    c.result = Some(text);
                }
                Err(e) => {
                    c.info.status = Status::Failed;
                    c.error = Some(e);
                }
            }
            c.unsaved = self.save(c).err();
        }
        self.pump(&mut g);
        drop(g);
        self.changed.notify_all();
    }

    /// Block until each listed child has finished, then return each result once.
    pub(crate) fn wait(&self, ids: &[String]) -> String {
        if ids.is_empty() {
            return "error: wait needs at least one agent ID".into();
        }
        let mut want: Vec<&str> = vec![];
        for id in ids {
            if !want.contains(&id.as_str()) {
                want.push(id);
            }
        }
        let mut g = self.lock();
        while !g.stopped
            && want.iter().any(|id| {
                g.children
                    .iter()
                    .any(|c| c.info.id == *id && !c.info.status.finished())
            })
        {
            g = self.changed.wait(g).unwrap_or_else(|e| e.into_inner());
        }
        let mut parts = vec![];
        for id in want {
            let Some(c) = find(&mut g, id) else {
                parts.push(format!("[{id}] error: no such agent"));
                continue;
            };
            let status = c.info.status;
            if c.delivered {
                parts.push(format!(
                    "[{id}] {}: result already returned by an earlier wait",
                    status.name()
                ));
                continue;
            }
            if !status.finished() {
                parts.push(format!("[{id}] {}: not finished", status.name()));
                continue;
            }
            c.delivered = true;
            let body = match status {
                Status::Done => c.result.clone().unwrap_or_default(),
                _ => c.error.clone().unwrap_or_else(|| "unknown error".into()),
            };
            let mut note = c.unsaved.clone();
            if let Err(e) = self.save(c) {
                note = Some(e);
            }
            let note = note
                .map(|e| format!("\n(note: agent record not saved: {e})"))
                .unwrap_or_default();
            parts.push(format!(
                "[{id}] {}\n{}{note}",
                status.name(),
                truncate(body)
            ));
        }
        parts.join("\n\n")
    }

    /// End the session: drop the queue and record unfinished children as interrupted.
    /// Running threads stop before their next model request and write nothing more.
    pub(crate) fn shutdown(&self) {
        let mut g = self.lock();
        g.stopped = true;
        g.queue.clear();
        for pid in g.external_pids.values() {
            kill_process_group(*pid);
        }
        let threads = std::mem::take(&mut g.external_threads);
        for c in g.children.iter_mut().filter(|c| !c.info.status.finished()) {
            c.info.status = Status::Interrupted;
            c.error =
                Some("interrupted: the parent session ended before this agent finished".into());
            if let Err(e) = self.save(c) {
                self.report_error(format!("agent record not saved: {e}"));
            }
        }
        drop(g);
        self.changed.notify_all();
        for thread in threads {
            if thread.join().is_err() {
                self.report_error("external helper thread crashed during shutdown".into());
            }
        }
    }
}

fn save_record(dir: &Path, c: &Child) -> Result<(), String> {
    let u = &c.info.usage;
    let body = json!({
        "version": RECORD_VERSION,
        "id": c.info.id,
        "provider": c.info.provider.name(),
        "task": c.info.task,
        "cwd": c.info.cwd,
        "model": c.info.model,
        "status": c.info.status.name(),
        "result": c.result,
        "error": c.error,
        "delivered": c.delivered,
        "usage": {
            "input": u.input, "output": u.output, "requests": u.requests,
            "input_missing": u.input_missing, "output_missing": u.output_missing
        },
        "cost_usd": c.info.cost_usd,
        "messages": c.messages,
    });
    let path = dir.join(format!("{}.json", c.info.id));
    write_private(&path, &body, false).map_err(|e| format!("{}: {e}", path.display()))
}

fn save_parent(dir: &Path, info: &AgentInfo) -> Result<(), String> {
    let u = &info.usage;
    let body = json!({
        "version": RECORD_VERSION,
        "id": PARENT_ID,
        "provider": info.provider.name(),
        "task": info.task,
        "cwd": info.cwd,
        "model": info.model,
        "status": info.status.name(),
        "usage": {
            "input": u.input, "output": u.output, "requests": u.requests,
            "input_missing": u.input_missing, "output_missing": u.output_missing
        },
    });
    let path = dir.join("main.json");
    write_private(&path, &body, false).map_err(|e| format!("{}: {e}", path.display()))
}

fn load_parent(dir: &Path) -> Result<Option<AgentInfo>, String> {
    let path = dir.join("main.json");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let parse = || -> Result<AgentInfo, String> {
        let v: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
        if v["version"].as_u64() != Some(RECORD_VERSION) || v["id"].as_str() != Some(PARENT_ID) {
            return Err("unsupported version or wrong ID".into());
        }
        let string = |key: &str| {
            v[key]
                .as_str()
                .map(String::from)
                .ok_or_else(|| format!("{key} is not a string"))
        };
        let n = |key: &str| {
            v["usage"][key]
                .as_u64()
                .ok_or_else(|| format!("usage.{key} is not a number"))
        };
        let n32 =
            |key: &str| u32::try_from(n(key)?).map_err(|_| format!("usage.{key} is too large"));
        let model = match &v["model"] {
            Value::Null => None,
            Value::String(s) => Some(s.clone()),
            _ => return Err("model is not a string".into()),
        };
        let status = v["status"]
            .as_str()
            .and_then(Status::parse)
            .ok_or("unknown status")?;
        Ok(AgentInfo {
            id: PARENT_ID.into(),
            provider: match v.get("provider") {
                None => Provider::Local,
                Some(Value::String(name)) => Provider::parse(name).ok_or("unknown provider")?,
                _ => return Err("provider is not a string".into()),
            },
            task: string("task")?,
            status,
            model,
            cwd: string("cwd")?,
            usage: UsageTotal {
                input: n("input")?,
                output: n("output")?,
                requests: n32("requests")?,
                input_missing: n32("input_missing")?,
                output_missing: n32("output_missing")?,
            },
            cost_usd: None,
        })
    };
    parse()
        .map(Some)
        .map_err(|e| format!("{}: corrupt ({e})", path.display()))
}

fn load_records(dir: &Path) -> Result<Vec<Child>, String> {
    let entries = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(format!("{}: {e}", dir.display())),
    };
    let mut out = vec![];
    for e in entries {
        let e = e.map_err(|e| format!("{}: {e}", dir.display()))?;
        let name = e.file_name();
        let Some(id) = name.to_str().and_then(|n| n.strip_suffix(".json")) else {
            continue;
        };
        if id_number(id).is_none() {
            continue;
        }
        let path = e.path();
        let text =
            std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        out.push(
            parse_record(&text, id)
                .map_err(|why| format!("{}: corrupt ({why})", path.display()))?,
        );
    }
    out.sort_by_key(|c| id_number(&c.info.id));
    Ok(out)
}

fn parse_record(text: &str, id: &str) -> Result<Child, String> {
    let v: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
    if v["version"].as_u64() != Some(RECORD_VERSION) {
        return Err(format!("unsupported version {}", v["version"]));
    }
    if v["id"].as_str() != Some(id) {
        return Err("id does not match the file name".into());
    }
    let text_of = |k: &str| -> Result<String, String> {
        v[k].as_str()
            .map(String::from)
            .ok_or_else(|| format!("{k} is not a string"))
    };
    let opt = |k: &str| -> Result<Option<String>, String> {
        match &v[k] {
            Value::Null => Ok(None),
            Value::String(s) => Ok(Some(s.clone())),
            _ => Err(format!("{k} is not a string")),
        }
    };
    let n = |k: &str| -> Result<u64, String> {
        v["usage"][k]
            .as_u64()
            .ok_or_else(|| format!("usage.{k} is not a number"))
    };
    let n32 = |k: &str| -> Result<u32, String> {
        u32::try_from(n(k)?).map_err(|_| format!("usage.{k} is too large"))
    };
    let status = v["status"]
        .as_str()
        .and_then(Status::parse)
        .ok_or("unknown status")?;
    let Some(Value::Array(messages)) = v.get("messages").cloned() else {
        return Err("messages is not an array".into());
    };
    Ok(Child {
        info: AgentInfo {
            id: id.into(),
            provider: match v.get("provider") {
                None => Provider::Local,
                Some(Value::String(name)) => Provider::parse(name).ok_or("unknown provider")?,
                _ => return Err("provider is not a string".into()),
            },
            task: text_of("task")?,
            status,
            model: opt("model")?,
            cwd: text_of("cwd")?,
            usage: UsageTotal {
                input: n("input")?,
                output: n("output")?,
                requests: n32("requests")?,
                input_missing: n32("input_missing")?,
                output_missing: n32("output_missing")?,
            },
            cost_usd: match &v["cost_usd"] {
                Value::Null => None,
                Value::Number(value) => value
                    .as_f64()
                    .filter(|cost| cost.is_finite() && *cost >= 0.0),
                _ => return Err("cost_usd is not a number".into()),
            },
        },
        result: opt("result")?,
        error: opt("error")?,
        delivered: v["delivered"]
            .as_bool()
            .ok_or("delivered is not a boolean")?,
        messages,
        unsaved: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::AtomicUsize;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("revex-agents-{}-{name}", std::process::id()));
        std::fs::remove_dir_all(&d).ok();
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn cfg(cwd: &Path, m: &Arc<Agents>) -> Cfg {
        Cfg {
            temperature: None,
            max_tokens: None,
            quiet: true,
            frontier: None,
            route: Route::Off,
            api: Api::Chat,
            base: String::new(),
            key: String::new(),
            model: Some("m".into()),
            decision_model: "clef".into(),
            reasoning: None,
            yes: false,
            history: None,
            strict_save: false,
            cwd: cwd.to_path_buf(),
            role: Role::Parent(Some(m.clone())),
            ui: None,
        }
    }

    #[test]
    fn selected_providers_round_robin_and_explicit_override() {
        let m = Agents::memory(model_fn(|_, _| reply("done", None, None)));
        m.set_providers([true, true, true]).unwrap();
        let mut state = m.lock();
        assert_eq!(
            select_provider(&mut state, "one"),
            Ok((Provider::Local, "one"))
        );
        assert_eq!(
            select_provider(&mut state, "two"),
            Ok((Provider::Codex, "two"))
        );
        assert_eq!(
            select_provider(&mut state, "@local three"),
            Ok((Provider::Local, "three"))
        );
        assert_eq!(
            select_provider(&mut state, "@claude four"),
            Ok((Provider::Claude, "four"))
        );
        assert!(select_provider(&mut state, "@other no").is_err());
        drop(state);
        m.set_providers([true, false, false]).unwrap();
        assert!(select_provider(&mut m.lock(), "@codex no").is_err());
    }

    #[test]
    fn legacy_record_provider_defaults_to_local() {
        let m = Agents::memory(model_fn(|_, _| reply("done", None, None)));
        let c = cfg(Path::new("/tmp"), &m);
        m.spawn(&c, "old");
        let value = json!({
            "version": 1, "id": "a1", "task": "old", "cwd": "/tmp",
            "model": "m", "status": "done", "result": "ok", "error": null,
            "delivered": false, "usage": {"input": 0, "output": 0, "requests": 0,
            "input_missing": 0, "output_missing": 0}, "messages": []
        });
        let loaded = parse_record(&value.to_string(), "a1").unwrap();
        assert_eq!(loaded.info.provider, Provider::Local);
        assert_eq!(loaded.info.cost_usd, None);
    }

    #[test]
    fn fake_external_program_gets_task_on_stdin_and_expected_flags() {
        let dir = tmp("fake-external");
        let executable = dir.join("fake-helper");
        let args = dir.join("args");
        let input = dir.join("input");
        let agents = Agents::memory(model_fn(|_, _| reply("unused", None, None)));
        for provider in [Provider::Codex, Provider::Claude] {
            let answer = if provider == Provider::Codex {
                "printf '%s\\n' '{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"fake-answer\"}}' '{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":7,\"output_tokens\":3}}'"
            } else {
                "printf '%s\\n' '{\"type\":\"result\",\"result\":\"fake-answer\",\"usage\":{\"input_tokens\":7,\"output_tokens\":3}}'"
            };
            std::fs::write(
                &executable,
                format!(
                    "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\ncat > '{}'\n{}\n",
                    args.display(),
                    input.display(),
                    answer
                ),
            )
            .unwrap();
            std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
            let command = external_command(provider, &dir, &executable).unwrap();
            assert!(
                command
                    .get_envs()
                    .any(|(key, value)| key == "REVEX_API_KEY" && value.is_none())
            );
            let output = agents
                .run_external_program("fake", provider, &dir, "private task", &executable)
                .unwrap();
            assert_eq!(output, "fake-answer");
            assert_eq!(std::fs::read_to_string(&input).unwrap(), "private task");
            let passed = std::fs::read_to_string(&args).unwrap();
            assert!(!passed.contains("private task"));
            if provider == Provider::Codex {
                assert!(passed.contains("--dangerously-bypass-approvals-and-sandbox"));
                assert!(passed.contains("--skip-git-repo-check"));
                assert!(passed.contains(&dir.display().to_string()));
                assert!(passed.contains("--json"));
            } else {
                assert!(passed.contains("--dangerously-skip-permissions"));
                assert!(passed.contains("--output-format"));
            }
        }
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn external_json_counts_only_reported_usage() {
        let codex = concat!(
            "{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"first\"}}\n",
            "{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"last\"}}\n",
            "{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":11,\"output_tokens\":4}}\n"
        );
        let (answer, usage, cost) = parse_external_output(Provider::Codex, codex).unwrap();
        assert_eq!(answer, "last");
        assert_eq!(usage.unwrap().input, Some(11));
        assert_eq!(usage.unwrap().output, Some(4));
        assert_eq!(cost, None);
        let (answer, usage, cost) = parse_external_output(
            Provider::Claude,
            r#"{"type":"result","result":"done","usage":{},"total_cost_usd":0.125}"#,
        )
        .unwrap();
        assert_eq!(answer, "done");
        assert_eq!(usage, None);
        assert_eq!(cost, Some(0.125));
        let (_, _, cost) = parse_external_output(
            Provider::Claude,
            r#"{"type":"result","result":"done","total_cost_usd":-1}"#,
        )
        .unwrap();
        assert_eq!(cost, None);
    }

    #[test]
    fn persisted_claude_cost_survives_record_roundtrip() {
        let value = json!({
            "version": 1, "id": "a1", "provider": "claude", "task": "inspect", "cwd": "/tmp",
            "model": null, "status": "done", "result": "ok", "error": null,
            "delivered": false, "cost_usd": 0.125,
            "usage": {"input": 7, "output": 3, "requests": 1,
                      "input_missing": 0, "output_missing": 0}, "messages": []
        });
        let child = parse_record(&value.to_string(), "a1").unwrap();
        let dir = tmp("cost-roundtrip");
        save_record(&dir, &child).unwrap();
        let saved: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("a1.json")).unwrap()).unwrap();
        assert_eq!(saved["cost_usd"].as_f64(), Some(0.125));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn fake_external_launch_failure_has_no_local_fallback() {
        let dir = tmp("fake-missing");
        let agents = Agents::memory(model_fn(|_, _| panic!("local fallback invoked")));
        let error = agents
            .run_external_program("fake", Provider::Codex, &dir, "task", &dir.join("missing"))
            .unwrap_err();
        assert!(error.contains("cannot launch codex helper"));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn helper_selection_reads_only_the_home_file() {
        let dir = tmp("home-selection");
        let home = dir.join("home-settings");
        std::fs::write(&home, r#"{"helper_providers":"local,claude"}"#).unwrap();
        std::fs::write(dir.join(".revex"), r#"{"helper_providers":"codex"}"#).unwrap();
        assert_eq!(selection_from_file(&home).unwrap(), [true, false, true]);
        std::fs::write(&home, "{}").unwrap();
        assert_eq!(selection_from_file(&home).unwrap(), [true, false, false]);
        std::fs::write(&home, r#"{"helper_providers":"codex,codex"}"#).unwrap();
        assert!(selection_from_file(&home).is_err());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn malformed_home_helpers_allow_ordinary_turn_and_disable_external_spawn() {
        let dir = tmp("malformed-home-helpers");
        let home = dir.join("home-settings");
        std::fs::write(&home, r#"{"helper_providers":"codex,codex"}"#).unwrap();
        let agents = Agents::memory(model_fn(|_, _| reply("ordinary answer", None, None)));
        let warning = agents.set_providers_from_home_file(Some(&home));
        assert!(warning.is_some());
        let config = cfg(&dir, &agents);
        let mut messages = vec![json!({"role":"system", "content":SYSTEM})];
        assert_eq!(
            agent(&config, &mut messages, "ordinary task")
                .unwrap()
                .text(),
            "ordinary answer"
        );
        assert!(agents.spawn(&config, "@codex task").contains("not enabled"));
        assert_eq!(agents.lock().providers, [true, false, false]);
        agents.shutdown();
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn shutdown_kills_fake_external_process_group_and_reaps_child() {
        let dir = tmp("external-shutdown");
        let executable = dir.join("fake-helper");
        let grandchild = dir.join("grandchild");
        std::fs::write(
            &executable,
            format!(
                "#!/bin/sh\nsleep 30 &\necho $! > '{}'\nwait\n",
                grandchild.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let agents = Agents::memory(model_fn(|_, _| reply("unused", None, None)));
        let worker = agents.clone();
        let cwd = dir.clone();
        let handle = std::thread::spawn(move || {
            worker.run_external_program("fake", Provider::Claude, &cwd, "task", &executable)
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while !grandchild.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(grandchild.exists());
        let start = std::time::Instant::now();
        agents.shutdown();
        let result = handle.join().unwrap();
        assert!(result.unwrap_err().contains("interrupted"));
        assert!(start.elapsed() < std::time::Duration::from_secs(2));
        assert!(agents.lock().external_pids.is_empty());
        std::fs::remove_dir_all(dir).ok();
    }

    fn reply(text: &str, input: Option<u64>, output: Option<u64>) -> Result<ChatResult, String> {
        Ok(ChatResult {
            text: text.into(),
            usage: Usage { input, output },
        })
    }

    fn is_child(msgs: &[Value]) -> bool {
        msgs[0]["content"] == CHILD_SYSTEM
    }

    fn last(msgs: &[Value]) -> String {
        msgs.last().unwrap()["content"]
            .as_str()
            .unwrap()
            .to_string()
    }

    /// Blocks fake children until opened.
    #[derive(Clone, Default)]
    struct Gate(Arc<(Mutex<bool>, Condvar)>);

    impl Gate {
        fn open(&self) {
            *self.0.0.lock().unwrap() = true;
            self.0.1.notify_all();
        }
        fn pass(&self) {
            let mut open = self.0.0.lock().unwrap();
            while !*open {
                open = self.0.1.wait(open).unwrap();
            }
        }
    }

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parse_delegation_verbs() {
        let c = parse(">> spawn find the parser\n>> wait a1  a2\n>> search fn\\s+main src");
        assert!(matches!(&c[0], Call::Spawn(t) if t == "find the parser"));
        assert!(matches!(&c[1], Call::Wait(v) if *v == ids(&["a1", "a2"])));
        assert!(
            matches!(&c[2], Call::Search(p, paths) if p == "fn\\s+main" && *paths == ids(&["src"]))
        );
    }

    #[test]
    fn unterminated_edits_never_apply_even_with_yes() {
        let dir = tmp("missing-eof");
        let m = Agents::memory(model_fn(|_, _| reply("unused", None, None)));
        let mut c = cfg(&dir, &m);
        c.yes = true; // also the MCP default
        for text in [
            ">> write file.txt <<EOF\nlate",
            ">> replace file.txt <<EOF\nold\n===\nlate",
        ] {
            let calls = parse(text);
            assert_eq!(calls.len(), 1);
            assert!(run(&c, &calls[0]).contains("missing EOF"));
        }
        assert!(!dir.join("file.txt").exists());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn clear_refuses_active_child_then_resets_history_agents_and_usage() {
        let root = tmp("clear-session");
        let agent_dir = root.join("agents");
        let history = history_for(&root, &root);
        let gate = Gate::default();
        let child_gate = gate.clone();
        let model = model_fn(move |_, _| {
            child_gate.pass();
            reply("finished", Some(3), Some(1))
        });
        let m = Agents::open(agent_dir.clone(), model.clone()).unwrap();
        let mut c = cfg(&root, &m);
        c.history = Some(history.clone());
        let mut msgs = fresh_history();
        msgs.push(json!({"role":"user","content":"old task"}));
        save_history(&history, &msgs).unwrap();
        m.parent_started(&c, "old task").unwrap();
        m.add_usage(
            None,
            Some(Usage {
                input: Some(10),
                output: Some(2),
            }),
        );
        m.parent_finished(true).unwrap();
        assert_eq!(m.spawn(&c, "long child"), "a1 started");
        let before = msgs.clone();
        assert!(
            clear_history(&c, &mut msgs)
                .unwrap_err()
                .contains("still active")
        );
        assert_eq!(msgs, before);
        assert_eq!(load_history(&history).unwrap(), before);
        gate.open();
        assert!(m.wait(&ids(&["a1"])).contains("finished"));
        clear_history(&c, &mut msgs).unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(load_history(&history).unwrap().len(), 1);
        assert_eq!(m.snapshot().len(), 1);
        assert_eq!(m.snapshot()[0].status, Status::Idle);
        assert_eq!(m.snapshot()[0].usage, UsageTotal::default());
        drop(m);
        let reopened = Agents::open(agent_dir, model).unwrap();
        assert_eq!(reopened.snapshot().len(), 1);
        assert_eq!(reopened.snapshot()[0].status, Status::Idle);
        assert_eq!(reopened.snapshot()[0].usage, UsageTotal::default());
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn spawn_and_wait_returns_result_once_with_usage() {
        let m = Agents::memory(model_fn(|_, msgs| {
            reply(
                &format!("answer to {}", msgs[1]["content"].as_str().unwrap()),
                Some(7),
                Some(2),
            )
        }));
        let c = cfg(&std::env::temp_dir(), &m);
        assert_eq!(m.spawn(&c, "  q1 "), "a1 started");
        let out = m.wait(&ids(&["a1", "a1"]));
        assert_eq!(out, "[a1] done\nanswer to q1");
        let again = m.wait(&ids(&["a1"]));
        assert!(again.contains("already returned"), "{again}");
        assert!(!again.contains("answer to"));
        let a = &m.snapshot()[1];
        assert_eq!(
            (a.id.as_str(), a.status, a.task.as_str()),
            ("a1", Status::Done, "q1")
        );
        assert_eq!((a.usage.input, a.usage.output, a.usage.requests), (7, 2, 1));
        assert!(a.usage.complete());
        // the parent made no request, so its usage is separate and empty
        assert_eq!(m.snapshot()[0].usage, UsageTotal::default());
    }

    #[test]
    fn parent_running_record_becomes_interrupted_on_restart() {
        let dir = tmp("parent-restart");
        let model = model_fn(|_, _| reply("unused", None, None));
        let m = Agents::open(dir.clone(), model.clone()).unwrap();
        let c = cfg(&dir, &m);
        m.parent_started(&c, "unfinished task").unwrap();
        m.add_usage(
            None,
            Some(Usage {
                input: Some(9),
                output: None,
            }),
        );
        drop(m);

        let reopened = Agents::open(dir.clone(), model.clone()).unwrap();
        let parent = &reopened.snapshot()[0];
        assert_eq!(parent.status, Status::Interrupted);
        assert_eq!(parent.task, "unfinished task");
        assert_eq!(parent.model.as_deref(), Some("m"));
        assert_eq!((parent.usage.input, parent.usage.output_missing), (9, 1));
        reopened.parent_finished(true).unwrap(); // late completion cannot overwrite it
        drop(reopened);
        assert_eq!(
            Agents::open(dir.clone(), model.clone()).unwrap().snapshot()[0].status,
            Status::Interrupted
        );

        std::fs::write(dir.join("main.json"), "{bad json").unwrap();
        assert!(
            Agents::open(dir.clone(), model)
                .err()
                .unwrap()
                .contains("corrupt")
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn parent_spawns_waits_and_keeps_usage_separate() {
        let m = Agents::memory(model_fn(|_, msgs| {
            if is_child(msgs) {
                return reply("child found it", Some(3), None);
            }
            let l = last(msgs);
            if l == "task" {
                reply(">> spawn look for it", Some(10), Some(1))
            } else if l.contains("a1 started") {
                reply(">> wait a1", None, None)
            } else if l.contains("[a1] done\nchild found it") {
                reply("final answer", Some(20), Some(4))
            } else {
                Err(format!("unexpected: {l}"))
            }
        }));
        let c = cfg(&std::env::temp_dir(), &m);
        let mut msgs = fresh_history();
        let r = agent(&c, &mut msgs, "task").unwrap();
        assert_eq!(r.text(), "final answer");
        let snap = m.snapshot();
        let (p, a1) = (&snap[0], &snap[1]);
        assert_eq!((p.id.as_str(), p.status), ("main", Status::Done));
        assert_eq!(
            (p.usage.input, p.usage.output, p.usage.requests),
            (30, 5, 3)
        );
        assert_eq!((p.usage.input_missing, p.usage.output_missing), (1, 1));
        assert!(!p.usage.complete());
        assert_eq!(
            (a1.usage.input, a1.usage.requests, a1.usage.output_missing),
            (3, 1, 1)
        );
        // the child's history is its own: not part of the parent's messages
        assert!(!msgs.iter().any(|m| m["content"] == CHILD_SYSTEM));
    }

    #[test]
    fn child_tools_are_read_only() {
        let dir = tmp("readonly");
        std::fs::write(dir.join("f.txt"), "hello\n").unwrap();
        let m = Agents::memory(model_fn(|_, _| reply("x", None, None)));
        let mut c = cfg(&dir, &m);
        c.yes = true; // even with confirmation off
        c.role = Role::Child(m.clone(), "a1".into());
        for call in [
            ">> sh touch made",
            ">> touch made",
            ">> write made <<EOF\nhi\nEOF",
            ">> replace f.txt <<EOF\nhello\n===\nbye\nEOF",
            ">> spawn more",
            ">> wait a1",
        ] {
            let parsed = parse(call);
            let out = run(&c, &parsed[0]);
            assert!(
                out.starts_with("error:") && out.contains("helper agent"),
                "{call}: {out}"
            );
        }
        assert!(!dir.join("made").exists());
        assert_eq!(
            std::fs::read_to_string(dir.join("f.txt")).unwrap(),
            "hello\n"
        );
        assert_eq!(run(&c, &parse(">> read f.txt")[0]), "1\thello");
        if Command::new("rg").arg("--version").output().is_ok() {
            let out = run(&c, &parse(">> search hel+o")[0]);
            assert!(out.contains("f.txt:1:hello"), "{out}");
            // model text after the verb is never parsed as an rg option
            assert_eq!(run(&c, &parse(">> search --pre=sh")[0]), "no matches");
            assert!(!dir.join("made").exists());
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn delegation_needs_a_parent_manager() {
        let m = Agents::memory(model_fn(|_, _| reply("x", None, None)));
        let mut c = cfg(&std::env::temp_dir(), &m);
        c.role = Role::Parent(None);
        assert!(run(&c, &Call::Spawn("x".into())).starts_with("error:"));
        assert!(run(&c, &Call::Wait(ids(&["a1"]))).starts_with("error:"));
        assert_eq!(m.snapshot().len(), 1);
    }

    #[test]
    fn two_run_at_once_and_the_rest_queue() {
        let gate = Gate::default();
        let (now, peak) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let (g, n, p) = (gate.clone(), now.clone(), peak.clone());
        let m = Agents::memory(model_fn(move |_, msgs| {
            let cur = n.fetch_add(1, Ordering::SeqCst) + 1;
            p.fetch_max(cur, Ordering::SeqCst);
            g.pass();
            n.fetch_sub(1, Ordering::SeqCst);
            reply(&last(msgs), Some(1), Some(1))
        }));
        let c = cfg(&std::env::temp_dir(), &m);
        assert_eq!(m.spawn(&c, "one"), "a1 started");
        assert_eq!(m.spawn(&c, "two"), "a2 started");
        assert!(m.spawn(&c, "three").starts_with("a3 queued"));
        let st: Vec<_> = m.snapshot()[1..].iter().map(|a| a.status).collect();
        assert_eq!(st, [Status::Running, Status::Running, Status::Queued]);
        gate.open();
        let out = m.wait(&ids(&["a3", "a1", "a2"]));
        assert!(
            out.contains("[a3] done\nthree") && out.contains("[a1] done\none"),
            "{out}"
        );
        assert!(peak.load(Ordering::SeqCst) <= MAX_RUNNING);
    }

    #[test]
    fn queue_limit_is_explicit() {
        let gate = Gate::default();
        let g = gate.clone();
        let m = Agents::memory(model_fn(move |_, _| {
            g.pass();
            reply("ok", None, None)
        }));
        let c = cfg(&std::env::temp_dir(), &m);
        for _ in 0..MAX_RUNNING + MAX_QUEUED {
            assert!(!m.spawn(&c, "t").starts_with("error"));
        }
        let full = m.spawn(&c, "t");
        assert!(
            full.starts_with("error:") && full.contains("queued"),
            "{full}"
        );
        m.shutdown();
        gate.open();
        assert!(
            m.snapshot()[1..]
                .iter()
                .all(|a| a.status == Status::Interrupted)
        );
        assert!(m.spawn(&c, "late").starts_with("error:"));
    }

    #[test]
    fn failures_and_bad_ids_are_reported() {
        let m = Agents::memory(model_fn(|_, _| Err("connection refused".into())));
        let c = cfg(&std::env::temp_dir(), &m);
        assert!(m.spawn(&c, " ").starts_with("error:"));
        m.spawn(&c, "t");
        let out = m.wait(&ids(&["a1", "zz"]));
        assert!(out.contains("[a1] failed\nconnection refused"), "{out}");
        assert!(out.contains("[zz] error: no such agent"));
        assert!(m.wait(&[]).starts_with("error:"));
        let u = m.snapshot()[1].usage;
        assert_eq!((u.requests, u.input_missing, u.input), (1, 1, 0));
    }

    #[test]
    fn records_survive_restart_and_unfinished_become_interrupted() {
        let dir = tmp("restart");
        let calls = Arc::new(AtomicUsize::new(0));
        let gate = Gate::default();
        let (g, n) = (gate.clone(), calls.clone());
        let model = model_fn(move |_, msgs| {
            n.fetch_add(1, Ordering::SeqCst);
            if last(msgs) != "quick" {
                g.pass();
            }
            reply("result", Some(4), Some(2))
        });
        let a = Agents::open(dir.clone(), model.clone()).unwrap();
        let c = cfg(&dir, &a);
        a.spawn(&c, "quick");
        assert!(a.wait(&ids(&["a1"])).contains("result"));
        a.spawn(&c, "slow");
        a.spawn(&c, "slow");
        a.spawn(&c, "queued");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&dir), 0o700);
            assert_eq!(mode(&dir.join("a1.json")), 0o600);
        }
        // a new process opening the records of one that never shut down (e.g. killed)
        let restarted = Arc::new(AtomicUsize::new(0));
        let r = restarted.clone();
        let b = Agents::open(
            dir.clone(),
            model_fn(move |_, _| {
                r.fetch_add(1, Ordering::SeqCst);
                reply("new", None, None)
            }),
        )
        .unwrap();
        let snap = b.snapshot();
        let st: Vec<_> = snap[1..].iter().map(|x| x.status).collect();
        assert_eq!(
            st,
            [
                Status::Done,
                Status::Interrupted,
                Status::Interrupted,
                Status::Interrupted
            ]
        );
        assert_eq!((snap[1].usage.input, snap[1].usage.requests), (4, 1));
        assert_eq!(snap[1].cwd, cwd_identity(&dir));
        assert!(b.wait(&ids(&["a1"])).contains("already returned"));
        let w = b.wait(&ids(&["a4"]));
        assert!(
            w.contains("[a4] interrupted") && w.contains("not rerun"),
            "{w}"
        );
        assert_eq!(
            restarted.load(Ordering::SeqCst),
            0,
            "restart must not rerun work"
        );
        // new IDs continue after the stored ones
        assert_eq!(b.spawn(&cfg(&dir, &b), "fresh"), "a5 started");
        assert!(b.wait(&ids(&["a5"])).contains("[a5] done\nnew"));
        assert_eq!(restarted.load(Ordering::SeqCst), 1);
        // a stopped instance's threads finish without recording success
        a.shutdown();
        gate.open();
        let st =
            |m: &Agents| -> Vec<Status> { m.snapshot()[1..].iter().map(|x| x.status).collect() };
        assert_eq!(st(&a)[1..], [Status::Interrupted; 3]);
        let c2 = Agents::open(dir.clone(), model).unwrap();
        assert_eq!(st(&c2)[1..4], [Status::Interrupted; 3]);
        assert_eq!(st(&c2)[4], Status::Done);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn corrupt_record_is_reported_not_overwritten() {
        let dir = tmp("corrupt");
        std::fs::write(dir.join("a1.json"), "{bad").unwrap();
        let e = Agents::open(dir.clone(), model_fn(|_, _| reply("x", None, None)))
            .err()
            .unwrap();
        assert!(e.contains("a1.json") && e.contains("corrupt"), "{e}");
        assert_eq!(
            std::fs::read_to_string(dir.join("a1.json")).unwrap(),
            "{bad"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn tools_use_the_agent_cwd_not_the_process_cwd() {
        let (x, y) = (tmp("cwd-x"), tmp("cwd-y"));
        std::fs::write(x.join("f.txt"), "in x").unwrap();
        std::fs::write(y.join("f.txt"), "in y").unwrap();
        let process_cwd = std::env::current_dir().unwrap();
        let m = Agents::memory(model_fn(|_, msgs| {
            if msgs.len() == 2 {
                reply(">> read f.txt", None, None)
            } else {
                reply(&last(msgs), None, None)
            }
        }));
        let mut cx = cfg(&x, &m);
        cx.yes = true;
        assert_eq!(run(&cx, &parse(">> read f.txt")[0]), "1\tin x");
        assert_eq!(run(&cfg(&y, &m), &parse(">> read f.txt")[0]), "1\tin y");
        #[cfg(unix)]
        {
            let out = run(&cx, &parse(">> sh cat f.txt")[0]);
            assert_eq!(out, "exit status: 0\nin x");
            run(&cx, &parse(">> write g.txt <<EOF\nnew\nEOF")[0]);
            assert_eq!(std::fs::read_to_string(x.join("g.txt")).unwrap(), "new\n");
        }
        // children inherit the parent's cwd
        m.spawn(&cfg(&y, &m), "read it");
        assert!(m.wait(&ids(&["a1"])).contains("in y"));
        assert_eq!(std::env::current_dir().unwrap(), process_cwd);
        std::fs::remove_dir_all(&x).ok();
        std::fs::remove_dir_all(&y).ok();
    }
}
