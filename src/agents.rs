//! Read-only child agents, started by the parent's `>> spawn` and collected with `>> wait`.
//! Each child has its own history, status and usage. Records are saved privately so a
//! restart can show what happened; unfinished work becomes interrupted and is never rerun.

use super::*;
use std::collections::VecDeque;
use std::sync::{Condvar, Mutex, MutexGuard};

/// Children running at once; more are queued.
pub(crate) const MAX_RUNNING: usize = 2;
/// Children waiting for a slot; more are refused.
pub(crate) const MAX_QUEUED: usize = 8;
const RECORD_VERSION: u64 = 1;
const PARENT_ID: &str = "main";

/// One model request. `chat` in normal use; tests substitute a fake provider.
pub(crate) type ModelFn = dyn Fn(&Cfg, &[Value]) -> Result<ChatResult, String> + Send + Sync;

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
#[derive(Clone, Debug)]
pub(crate) struct AgentInfo {
    pub(crate) id: String,
    pub(crate) task: String,
    pub(crate) status: Status,
    pub(crate) model: Option<String>,
    pub(crate) cwd: String,
    /// Every model request of this agent; missing provider usage is counted as missing.
    pub(crate) usage: UsageTotal,
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
    queue: VecDeque<(String, Cfg)>,
    next: u64,
    stopped: bool,
}

pub(crate) struct Agents {
    /// None keeps records in memory only (one-shot and piped runs).
    dir: Option<PathBuf>,
    model: Arc<ModelFn>,
    state: Mutex<State>,
    changed: Condvar,
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
            task: String::new(),
            status: Status::Idle,
            model: None,
            cwd: String::new(),
            usage: UsageTotal::default(),
        });
        Arc::new(Self {
            dir,
            model,
            state: Mutex::new(State {
                parent,
                children,
                queue: VecDeque::new(),
                next,
                stopped: false,
            }),
            changed: Condvar::new(),
        })
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
        if let Some((_, moved)) = backup
            && let Err(e) = std::fs::remove_dir_all(&moved)
        {
            eprintln!(
                "warning: session cleared but old agent records remain at {}: {e}",
                moved.display()
            );
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
            eprintln!("parent usage not saved: {e}");
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
        let task = task.trim();
        if task.is_empty() {
            return "error: spawn needs a TASK".into();
        }
        let mut g = self.lock();
        if g.stopped {
            return "error: the session is ending; no agent was started".into();
        }
        if g.queue.len() >= MAX_QUEUED {
            return format!(
                "error: {MAX_QUEUED} agents are already queued; wait for some before spawning more"
            );
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
                task: task.into(),
                status: Status::Queued,
                model: cfg.model.clone(),
                cwd: cwd_identity(&cfg.cwd),
                usage: UsageTotal::default(),
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
            return format!("error: agent record not saved ({e}); no agent was started");
        }
        g.children.push(c);
        g.queue.push_back((id.clone(), child_cfg));
        self.pump(&mut g);
        let status = g.children.last().map(|c| c.info.status);
        match status {
            Some(Status::Running) => format!("{id} started"),
            Some(Status::Queued) => {
                format!(
                    "{id} queued: {MAX_RUNNING} agents are running; it starts when one finishes"
                )
            }
            _ => format!(
                "{id} failed to start: {}",
                g.children
                    .last()
                    .and_then(|c| c.error.clone())
                    .unwrap_or_default()
            ),
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
            let Some((id, cfg)) = s.queue.pop_front() else {
                return;
            };
            let me = self.clone();
            let tid = id.clone();
            let started = std::thread::Builder::new()
                .name(format!("revex-{id}"))
                .spawn(move || me.run_child(tid, cfg));
            let Some(c) = find(s, &id) else { continue };
            match started {
                Ok(_) => c.info.status = Status::Running,
                Err(e) => {
                    c.info.status = Status::Failed;
                    c.error = Some(format!("cannot start thread: {e}"));
                }
            }
            c.unsaved = self.save(c).err();
        }
    }

    fn run_child(self: Arc<Self>, id: String, mut cfg: Cfg) {
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
        for c in g.children.iter_mut().filter(|c| !c.info.status.finished()) {
            c.info.status = Status::Interrupted;
            c.error =
                Some("interrupted: the parent session ended before this agent finished".into());
            if let Err(e) = self.save(c) {
                eprintln!("agent record not saved: {e}");
            }
        }
        drop(g);
        self.changed.notify_all();
    }
}

fn save_record(dir: &Path, c: &Child) -> Result<(), String> {
    let u = &c.info.usage;
    let body = json!({
        "version": RECORD_VERSION,
        "id": c.info.id,
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
            yes: false,
            history: None,
            strict_save: false,
            cwd: cwd.to_path_buf(),
            role: Role::Parent(Some(m.clone())),
            ui: None,
        }
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
