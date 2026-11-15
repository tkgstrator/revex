use serde_json::{Map, Value, json};
use std::io::{BufRead, Read};
use std::path::{Path, PathBuf};
use std::process::Command;

const SYSTEM: &str = "You are a terse coding and reverse-engineering agent. To act, output lines starting with `>> `; results come back in <out>. One action per line, then stop.\n\
>> read PATH [A-B]      read file lines\n\
>> sh COMMAND           run shell (grep, rg, objdump -d, strings, r2 -qc 'pdf @ main' bin, nm, otool -tV, lldb -b)\n\
>> write PATH <<EOF     create file; body follows, ends with EOF line\n\
>> replace PATH <<EOF   body: old text, line `===`, new text, then EOF line\n\
Rules: inspect before editing; keep output small (use head, grep, ranges); when done, answer in plain text without `>>`.";

const MAX_OUT: usize = 2000;

#[derive(Clone, Copy, PartialEq)]
enum Route {
    Off,
    Auto,
    Local,
    Remote,
}

#[derive(Clone, Copy, PartialEq)]
enum Api {
    Chat,
    Responses,
}

#[derive(Clone)]
struct Cfg {
    temperature: Option<f64>,
    max_tokens: Option<u32>,
    quiet: bool,
    frontier: Option<String>,
    route: Route,
    api: Api,
    base: String,
    key: String,
    model: Option<String>,
    yes: bool,
}

fn truncate(s: String) -> String {
    if s.len() <= MAX_OUT {
        return s;
    }
    let mut end = MAX_OUT;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n...({} bytes omitted)", &s[..end], s.len() - end)
}

/// First model the server lists, used when `REVEX_MODEL` is unset.
fn list_models(base: &str, key: &str) -> Vec<String> {
    let text = ureq::get(&format!("{base}/models"))
        .set("Authorization", &format!("Bearer {key}"))
        .call()
        .ok()
        .and_then(|r| r.into_string().ok());
    let v: Value = text
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(Value::Null);
    v["data"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|m| m["id"].as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

fn default_model(base: &str, key: &str) -> Option<String> {
    list_models(base, key).into_iter().next()
}

fn chat(cfg: &Cfg, msgs: &[Value]) -> Result<String, String> {
    let (path, mut body) = match cfg.api {
        Api::Chat => (
            "chat/completions",
            json!({
                "model": cfg.model.as_deref(), "messages": msgs, "stream": true,
                "stop": ["<out>"], "cache_prompt": true
            }),
        ),
        Api::Responses => {
            let (sys, rest) = match msgs.split_first() {
                Some((f, r)) if f["role"] == "system" => (f["content"].clone(), r),
                _ => (Value::Null, msgs),
            };
            let mut b = json!({
                "model": cfg.model.as_deref(), "input": rest, "stream": true, "store": false
            });
            if !sys.is_null() {
                b["instructions"] = sys;
            }
            ("responses", b)
        }
    };
    if let Some(t) = cfg.temperature {
        body["temperature"] = t.into();
    }
    if let Some(n) = cfg.max_tokens {
        match cfg.api {
            Api::Chat => body["max_tokens"] = n.into(),
            Api::Responses => body["max_output_tokens"] = n.into(),
        }
    }
    // If no model could be found, let the server pick its own default.
    if cfg.model.is_none() {
        body.as_object_mut().map(|o| o.remove("model"));
    }
    if let Some(Ok(Value::Object(extra))) = std::env::var("REVEX_EXTRA")
        .ok()
        .map(|e| serde_json::from_str::<Value>(&e))
    {
        for (k, v) in extra {
            body[k] = v;
        }
    }
    let resp = ureq::post(&format!("{}/{path}", cfg.base))
        .set("Authorization", &format!("Bearer {}", cfg.key))
        .set("Content-Type", "application/json")
        .send_string(&body.to_string())
        .map_err(|e| e.to_string())?;
    let mut full = String::new();
    for line in std::io::BufReader::new(resp.into_reader()).lines() {
        let line = line.map_err(|e| e.to_string())?;
        if std::env::var_os("REVEX_DEBUG").is_some() {
            eprintln!("[raw] {line}");
        }
        let Some(data) = line.strip_prefix("data: ") else {
            continue;
        };
        if data == "[DONE]" {
            break;
        }
        let Ok(v) = serde_json::from_str::<Value>(data) else {
            continue;
        };
        let delta = match cfg.api {
            Api::Chat => {
                if v["choices"][0]["finish_reason"] == "content_filter"
                    || v["choices"][0]["delta"]["refusal"].is_string()
                {
                    return Err("refused by model/provider (content_filter)".into());
                }
                v["choices"][0]["delta"]["content"].as_str()
            }
            Api::Responses => match v["type"].as_str().unwrap_or("") {
                "response.output_text.delta" => v["delta"].as_str(),
                "response.refusal.delta" | "response.refusal.done" => {
                    return Err("refused by model/provider (refusal)".into());
                }
                "response.incomplete" => {
                    if v["response"]["incomplete_details"]["reason"] == "content_filter" {
                        return Err("refused by model/provider (content_filter)".into());
                    }
                    break;
                }
                "response.failed" | "error" => {
                    let m = v["response"]["error"]["message"]
                        .as_str()
                        .or(v["message"].as_str())
                        .unwrap_or("unknown error");
                    return Err(format!("provider error: {m}"));
                }
                "response.completed" => break,
                _ => None,
            },
        };
        if let Some(t) = delta {
            if !cfg.quiet {
                eprint!("{t}");
            }
            full.push_str(t);
            // The Responses API has no `stop`, so cut at the marker here.
            if cfg.api == Api::Responses
                && let Some(i) = full.find("<out>")
            {
                full.truncate(i);
                break;
            }
        }
    }
    if !cfg.quiet {
        eprintln!();
    }
    Ok(full)
}

enum Call {
    Read(String, Option<(usize, usize)>),
    Sh(String),
    Write(String, String),
    Replace(String, String),
}

fn parse(reply: &str) -> Vec<Call> {
    let mut calls = vec![];
    let mut lines = reply.lines();
    while let Some(l) = lines.next() {
        let Some(cmd) = l.trim_start().strip_prefix(">> ") else {
            continue;
        };
        let (verb, rest) = cmd.split_once(' ').unwrap_or((cmd, ""));
        match verb {
            "read" => {
                let mut it = rest.split_whitespace();
                let path = it.next().unwrap_or("").to_string();
                let range = it.next().and_then(|r| {
                    let (a, b) = r.split_once('-')?;
                    Some((a.parse().ok()?, b.parse().ok()?))
                });
                calls.push(Call::Read(path, range));
            }
            "sh" => calls.push(Call::Sh(rest.to_string())),
            "write" | "replace" => {
                let path = rest.split_whitespace().next().unwrap_or("").to_string();
                let mut body = vec![];
                for b in lines.by_ref() {
                    if b.trim() == "EOF" {
                        break;
                    }
                    body.push(b);
                }
                let body = body.join("\n");
                calls.push(if verb == "write" {
                    Call::Write(path, body)
                } else {
                    Call::Replace(path, body)
                });
            }
            _ => calls.push(Call::Sh(cmd.to_string())),
        }
    }
    calls
}

fn confirm(cfg: &Cfg, what: &str) -> bool {
    if cfg.yes {
        return true;
    }
    eprint!("run `{what}`? [y/N] ");
    let mut s = String::new();
    std::io::stdin().read_line(&mut s).ok();
    s.trim().eq_ignore_ascii_case("y")
}

fn run(cfg: &Cfg, c: &Call) -> String {
    let out = match c {
        Call::Read(p, range) => match std::fs::read_to_string(p) {
            Ok(t) => {
                let (a, b) = range.unwrap_or((1, usize::MAX));
                t.lines()
                    .enumerate()
                    .skip(a.saturating_sub(1))
                    .take(b.saturating_sub(a) + 1)
                    .map(|(i, l)| format!("{}\t{}", i + 1, l))
                    .collect::<Vec<_>>()
                    .join("\n")
            }
            Err(e) => format!("error: {e}"),
        },
        Call::Sh(cmd) => {
            if !confirm(cfg, cmd) {
                return "denied".into();
            }
            match Command::new("sh").arg("-c").arg(cmd).output() {
                Ok(o) => format!(
                    "{}{}",
                    String::from_utf8_lossy(&o.stdout),
                    String::from_utf8_lossy(&o.stderr)
                ),
                Err(e) => format!("error: {e}"),
            }
        }
        Call::Write(p, body) => {
            if !confirm(cfg, &format!("write {p}")) {
                return "denied".into();
            }
            match std::fs::write(p, format!("{body}\n")) {
                Ok(_) => "ok".into(),
                Err(e) => format!("error: {e}"),
            }
        }
        Call::Replace(p, body) => {
            let Some((old, new)) = body.split_once("\n===\n") else {
                return "error: need old, `===`, new".into();
            };
            if !confirm(cfg, &format!("replace in {p}")) {
                return "denied".into();
            }
            match std::fs::read_to_string(p) {
                Ok(t) if t.matches(old).count() == 1 => {
                    match std::fs::write(p, t.replacen(old, new, 1)) {
                        Ok(_) => "ok".into(),
                        Err(e) => format!("error: {e}"),
                    }
                }
                Ok(t) => format!(
                    "error: old text matched {} times (need 1)",
                    t.matches(old).count()
                ),
                Err(e) => format!("error: {e}"),
            }
        }
    };
    truncate(out)
}

const ROUTER: &str = "Decide who should handle the user's task. Reply with exactly one letter.\n\
F = general software work any hosted coding assistant would normally accept.\n\
L = reverse engineering of binaries, exploit or malware analysis, or anything a hosted assistant might refuse or hedge on.\n\
If unsure, answer L.";

/// Ask the local model for one letter: true means forward to the frontier command.
fn route(cfg: &Cfg, task: &str) -> bool {
    let msgs = [
        json!({"role": "system", "content": ROUTER}),
        json!({"role": "user", "content": task}),
    ];
    let mut quick = Cfg {
        max_tokens: Some(4),
        temperature: Some(0.0),
        ..cfg.clone()
    };
    quick.quiet = true;
    chat(&quick, &msgs).is_ok_and(|r| r.trim_start().starts_with('F'))
}

/// Run the frontier command with the task as its last argument.
/// Returns None when it cannot be used, so the caller falls back to the local loop.
fn forward(frontier: &str, task: &str) -> Option<String> {
    let mut parts = frontier.split_whitespace();
    let out = Command::new(parts.next()?)
        .args(parts)
        .arg(task)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| eprintln!("frontier: cannot start ({e}); handling locally"))
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !out.status.success() || text.is_empty() {
        eprintln!("frontier: failed (status {}); handling locally", out.status);
        return None;
    }
    // `claude -p --output-format json` reports refusals in `stop_reason`.
    if let Ok(v) = serde_json::from_str::<Value>(&text) {
        if v["stop_reason"] == "refusal" || v["is_error"] == true {
            eprintln!("frontier: refused; handling locally");
            return None;
        }
        if let Some(r) = v["result"].as_str() {
            return Some(r.to_string());
        }
    }
    Some(text)
}

fn agent(cfg: &Cfg, msgs: &mut Vec<Value>, task: &str) -> Result<String, String> {
    if let Some(f) = &cfg.frontier
        && matches!(cfg.route, Route::Auto | Route::Remote)
        && (cfg.route == Route::Remote || route(cfg, task))
        && let Some(r) = forward(f, task)
    {
        return Ok(r);
    }
    msgs.push(json!({"role": "user", "content": task}));
    for _ in 0..30 {
        let reply = chat(cfg, msgs)?;
        if reply.trim() == ">>" {
            continue;
        }
        msgs.push(json!({"role": "assistant", "content": reply}));
        let calls = parse(&reply);
        if calls.is_empty() {
            return Ok(reply.trim().to_string());
        }
        let outs: Vec<String> = calls.iter().map(|c| run(cfg, c)).collect();
        msgs.push(
            json!({"role": "user", "content": format!("<out>\n{}\n</out>", outs.join("\n---\n"))}),
        );
    }
    Err("step limit reached".into())
}

fn mcp(cfg: &Cfg) {
    let reply = |id: Value, r: Result<Value, (i64, &str)>| {
        let m = match r {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err((code, msg)) => {
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": msg}})
            }
        };
        println!("{m}");
    };
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let Ok(req) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(id) = req.get("id").cloned() else {
            continue;
        };
        let r = match req["method"].as_str().unwrap_or("") {
            "initialize" => Ok(json!({
                "protocolVersion": req["params"]["protocolVersion"].as_str().unwrap_or("2024-11-05"),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "revex", "version": env!("CARGO_PKG_VERSION")}
            })),
            "tools/list" => Ok(json!({"tools": [{
                "name": "delegate",
                "description": "Run a coding / reverse-engineering task on a local LLM agent (shell, file read/write). Returns its final answer.",
                "inputSchema": {"type": "object", "properties": {
                    "task": {"type": "string"},
                    "cwd": {"type": "string", "description": "working directory"}
                }, "required": ["task"]}
            }]})),
            "tools/call" => {
                let a = &req["params"]["arguments"];
                if let Some(d) = a["cwd"].as_str() {
                    std::env::set_current_dir(d).ok();
                }
                let mut msgs = vec![json!({"role": "system", "content": SYSTEM})];
                let (text, err) = match agent(cfg, &mut msgs, a["task"].as_str().unwrap_or("")) {
                    Ok(t) => (t, false),
                    Err(e) => (e, true),
                };
                Ok(json!({"content": [{"type": "text", "text": text}], "isError": err}))
            }
            "ping" => Ok(json!({})),
            _ => Err((-32601, "method not found")),
        };
        reply(id, r);
    }
}

/// Keys a project-local `.revex` may set. `frontier` runs a command and `base_url`
/// receives the API key, so a cloned repository must not control either.
const PROJECT_KEYS: [&str; 3] = ["model", "route", "api"];

fn die(msg: &str) -> ! {
    eprintln!("error: {msg}");
    std::process::exit(2);
}

fn parse_api(s: &str) -> Option<Api> {
    match s {
        "chat" => Some(Api::Chat),
        "responses" => Some(Api::Responses),
        _ => None,
    }
}

fn parse_route(s: &str) -> Option<Route> {
    match s {
        "off" => Some(Route::Off),
        "auto" => Some(Route::Auto),
        "local" => Some(Route::Local),
        "remote" => Some(Route::Remote),
        _ => None,
    }
}

fn route_name(r: Route) -> &'static str {
    match r {
        Route::Off => "off",
        Route::Auto => "auto",
        Route::Local => "local",
        Route::Remote => "remote",
    }
}

fn home_file() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".revex"))
}

/// A missing file is empty settings; an unreadable or invalid one is an error.
fn read_settings(path: &Path) -> Result<Map<String, Value>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Map::new()),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(m)) => Ok(m),
        _ => Err(format!("{}: not a JSON object", path.display())),
    }
}

/// `~/.revex` merged with `./.revex`; the project file wins key by key.
fn load_settings() -> Map<String, Value> {
    let home = home_file();
    let mut m = Map::new();
    if let Some(h) = &home {
        match read_settings(h) {
            Ok(v) => m = v,
            Err(e) => eprintln!("warning: {e}; ignored"),
        }
    }
    let project = Path::new(".revex");
    let same = home
        .as_ref()
        .and_then(|h| Some(std::fs::canonicalize(h).ok()? == std::fs::canonicalize(project).ok()?))
        .unwrap_or(false);
    if !same {
        match read_settings(project) {
            Ok(v) => {
                for (k, val) in v {
                    if PROJECT_KEYS.contains(&k.as_str()) {
                        m.insert(k, val);
                    } else {
                        eprintln!(
                            "warning: ./.revex: `{k}` is not allowed in a project file; ignored"
                        );
                    }
                }
            }
            Err(e) => eprintln!("warning: {e}; ignored"),
        }
    }
    m
}

/// Environment first, then the merged settings file.
fn setting(file: &Map<String, Value>, key: &str, env: &str) -> Option<String> {
    std::env::var(env)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| file.get(key).and_then(|v| v.as_str()).map(String::from))
}

/// Write to `./.revex` if it exists, otherwise to `~/.revex`.
fn save_setting(key: &str, value: &str) -> Result<PathBuf, String> {
    let path = if Path::new(".revex").exists() {
        PathBuf::from(".revex")
    } else {
        home_file().ok_or("HOME is not set")?
    };
    let mut m = read_settings(&path)?;
    if path == Path::new(".revex") {
        // Drop the keys a project file may not set instead of writing them back.
        m.retain(|k, _| PROJECT_KEYS.contains(&k.as_str()));
    }
    m.insert(key.into(), Value::String(value.into()));
    let text = serde_json::to_string_pretty(&Value::Object(m)).map_err(|e| e.to_string())?;
    std::fs::write(&path, text + "\n").map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(path)
}

enum Cmd {
    No,
    Done,
    Exit,
}

const HELP: &str = "/help  /clear  /model [ID]  /route [off|auto|local|remote]  /status  /exit";

fn saved(key: &str, env: &str, value: &str) {
    match save_setting(key, value) {
        Ok(p) => eprintln!("{key} = {value} (saved to {})", p.display()),
        Err(e) => eprintln!("{key} = {value} (not saved: {e})"),
    }
    if std::env::var_os(env).is_some() {
        eprintln!("note: {env} is set and overrides the saved value on the next start");
    }
}

fn command(cfg: &mut Cfg, msgs: &mut Vec<Value>, line: &str) -> Cmd {
    let Some(rest) = line.strip_prefix('/') else {
        return Cmd::No;
    };
    let (name, arg) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    let arg = arg.trim();
    match name {
        "help" => eprintln!("{HELP}"),
        "exit" | "quit" => return Cmd::Exit,
        "clear" => {
            msgs.truncate(1);
            eprintln!("history cleared");
        }
        "model" if arg.is_empty() => {
            let cur = cfg.model.clone().unwrap_or_default();
            eprintln!(
                "model: {}",
                if cur.is_empty() {
                    "(server default)"
                } else {
                    &cur
                }
            );
            for m in list_models(&cfg.base, &cfg.key) {
                eprintln!("  {} {m}", if m == cur { "*" } else { " " });
            }
        }
        "model" => {
            let known = list_models(&cfg.base, &cfg.key);
            if !known.is_empty() && !known.iter().any(|m| m == arg) {
                eprintln!("`{arg}` is not in /models; try `/model` to list them");
            } else {
                cfg.model = Some(arg.to_string());
                saved("model", "REVEX_MODEL", arg);
            }
        }
        "route" if arg.is_empty() => eprintln!("route: {}", route_name(cfg.route)),
        "route" => match parse_route(arg) {
            Some(r) => {
                cfg.route = r;
                saved("route", "REVEX_ROUTE", arg);
            }
            None => eprintln!("route must be off, auto, local or remote"),
        },
        "status" => {
            eprintln!(
                "api:      {}",
                if cfg.api == Api::Chat {
                    "chat"
                } else {
                    "responses"
                }
            );
            eprintln!("base:     {}", cfg.base);
            eprintln!(
                "model:    {}",
                cfg.model.as_deref().unwrap_or("(server default)")
            );
            eprintln!("route:    {}", route_name(cfg.route));
            eprintln!("frontier: {}", cfg.frontier.as_deref().unwrap_or("(none)"));
            eprintln!(
                "api key:  {}",
                if cfg.key.is_empty() { "not set" } else { "set" }
            );
        }
        _ => eprintln!("unknown command `/{name}`; {HELP}"),
    }
    Cmd::Done
}

fn main() {
    let file = load_settings();
    let get = |key: &str, env: &str| setting(&file, key, env);
    let api = match get("api", "REVEX_API") {
        None => Api::Chat,
        Some(v) => parse_api(&v)
            .unwrap_or_else(|| die(&format!("api must be `chat` or `responses`, got `{v}`"))),
    };
    let route = match get("route", "REVEX_ROUTE") {
        None => Route::Off,
        Some(v) => parse_route(&v).unwrap_or_else(|| {
            die(&format!(
                "route must be off, auto, local or remote, got `{v}`"
            ))
        }),
    };
    let mut cfg = Cfg {
        temperature: None,
        max_tokens: None,
        quiet: false,
        frontier: get("frontier", "REVEX_FRONTIER"),
        route,
        api,
        base: get("base_url", "REVEX_BASE_URL")
            .unwrap_or_else(|| die("REVEX_BASE_URL is not set (or `base_url` in ~/.revex)")),
        key: std::env::var("REVEX_API_KEY").unwrap_or_default(),
        model: get("model", "REVEX_MODEL"),
        yes: false,
    };
    if cfg.model.is_none() {
        cfg.model = default_model(&cfg.base, &cfg.key);
    }
    let mut task = vec![];
    for a in std::env::args().skip(1) {
        match a.as_str() {
            "--yes" | "-y" => cfg.yes = true,
            "--local" => cfg.route = Route::Local,
            "--remote" => cfg.route = Route::Remote,
            "--mcp" => {
                cfg.yes = true;
                mcp(&cfg);
                return;
            }
            _ => task.push(a),
        }
    }
    let mut msgs = vec![json!({"role": "system", "content": SYSTEM})];
    if !task.is_empty() {
        match agent(&cfg, &mut msgs, &task.join(" ")) {
            Ok(r) => println!("{r}"),
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
        return;
    }
    let mut piped = String::new();
    if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        std::io::stdin().read_to_string(&mut piped).ok();
        match agent(&cfg, &mut msgs, piped.trim()) {
            Ok(r) => println!("{r}"),
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
        return;
    }
    loop {
        eprint!("> ");
        let mut s = String::new();
        if std::io::stdin().read_line(&mut s).unwrap_or(0) == 0 {
            break;
        }
        let line = s.trim();
        if line.is_empty() {
            continue;
        }
        match command(&mut cfg, &mut msgs, line) {
            Cmd::Exit => break,
            Cmd::Done => continue,
            Cmd::No => {}
        }
        if let Err(e) = agent(&cfg, &mut msgs, line) {
            eprintln!("error: {e}");
        }
    }
}
