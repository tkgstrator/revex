//! Client connections to user-configured MCP servers. Nothing in this module
//! reads the real home directory; callers supply the already-loaded maps.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::Future;
use std::process::Stdio;
use std::sync::Mutex;
use std::time::Duration;

use http::{HeaderName, HeaderValue};
use rmcp::model::{CallToolRequestParams, ContentBlock};
use rmcp::service::RunningService;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};
use rmcp::{RoleClient, ServiceExt};
use serde_json::{Map, Value};
use tokio::runtime::{Builder, Runtime};

const STARTUP_LIMIT: Duration = Duration::from_secs(12);
const LIST_LIMIT: Duration = Duration::from_secs(10);
const CALL_LIMIT: Duration = Duration::from_secs(60);
const CLOSE_LIMIT: Duration = Duration::from_secs(2);
const MAX_PAGES: usize = 32;
const MAX_TOOLS: usize = 128;
const MAX_SCHEMA_BYTES: usize = 8_192;
const MAX_OUTPUT_BYTES: usize = 8_192;

#[derive(Clone, Debug)]
enum ServerConfig {
    Stdio {
        command: String,
        args: Vec<String>,
        env: HashMap<String, String>,
    },
    Http {
        url: String,
        headers: HashMap<HeaderName, HeaderValue>,
    },
}

#[derive(Clone, Debug)]
pub(crate) struct ToolSpec {
    pub(crate) server: String,
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) input_schema: Value,
}

impl ToolSpec {
    pub(crate) fn qualified_name(&self) -> String {
        format!("mcp__{}__{}", self.server, self.name)
    }
}

#[derive(Default, Debug)]
pub(crate) struct Discovery {
    pub(crate) tools: Vec<ToolSpec>,
    pub(crate) errors: Vec<String>,
}

struct Session {
    service: RunningService<RoleClient, ()>,
    tools: HashMap<String, Value>,
}

/// Owns the MCP sessions for one Revex session. Each configured server fails
/// independently; its key and headers are never interpolated into diagnostics.
pub(crate) struct McpClient {
    runtime: Runtime,
    servers: BTreeMap<String, ServerConfig>,
    config_errors: Vec<String>,
    sessions: Mutex<HashMap<String, Session>>,
}

impl McpClient {
    pub(crate) fn from_maps(
        settings: &Map<String, Value>,
        auth: &Map<String, Value>,
    ) -> Result<Self, String> {
        let runtime = Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(|_| "could not start MCP client runtime".to_string())?;
        let configured = object_at(settings, "mcpServers")?;
        let secrets = object_at(auth, "mcpServers")?;
        let mut servers = BTreeMap::new();
        let mut config_errors = Vec::new();
        for (name, entry) in configured {
            if !valid_server_name(&name) {
                config_errors.push("MCP server name must use letters, digits, _ or -".into());
                continue;
            }
            let secret = secrets.get(&name);
            match parse_server(&entry, secret) {
                Ok(server) => {
                    servers.insert(name, server);
                }
                Err(reason) => config_errors.push(format!("MCP {name}: {reason}")),
            }
        }
        Ok(Self {
            runtime,
            servers,
            config_errors,
            sessions: Mutex::new(HashMap::new()),
        })
    }

    pub(crate) fn list_tools(&self, cancelled: &dyn Fn() -> bool) -> Discovery {
        let mut result = Discovery {
            errors: self.config_errors.clone(),
            ..Discovery::default()
        };
        for (name, config) in &self.servers {
            if cancelled() {
                result.errors.push("MCP discovery interrupted".into());
                break;
            }
            let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            let session = match self.ensure_session(&mut sessions, name, config, cancelled) {
                Ok(session) => session,
                Err(reason) => {
                    result.errors.push(format!("MCP {name}: {reason}"));
                    continue;
                }
            };
            match self.fetch_tools(name, session, cancelled) {
                Ok(tools) => result.tools.extend(tools),
                Err(reason) => {
                    result.errors.push(format!("MCP {name}: {reason}"));
                    self.close_session(&mut sessions, name);
                }
            }
        }
        result
    }

    pub(crate) fn call(
        &self,
        server: &str,
        tool: &str,
        arguments: &Value,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<String, String> {
        if cancelled() {
            return Err("MCP call interrupted".into());
        }
        let config = self
            .servers
            .get(server)
            .ok_or_else(|| "MCP server is not configured".to_string())?;
        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        let session = self
            .ensure_session(&mut sessions, server, config, cancelled)
            .map_err(str::to_string)?;
        if !session.tools.contains_key(tool) {
            self.fetch_tools(server, session, cancelled)
                .map_err(str::to_string)?;
        }
        let schema = session
            .tools
            .get(tool)
            .ok_or_else(|| "MCP tool is not available on this server".to_string())?;
        let object = arguments
            .as_object()
            .ok_or_else(|| "MCP tool arguments must be a JSON object".to_string())?;
        let validator = jsonschema::validator_for(schema)
            .map_err(|_| "MCP tool has an invalid input schema".to_string())?;
        if !validator.is_valid(arguments) {
            return Err("MCP tool arguments do not match its input schema".into());
        }
        let request = CallToolRequestParams::new(tool.to_owned()).with_arguments(object.clone());
        let response = self.runtime.block_on(wait_cancel(
            session.service.call_tool(request),
            cancelled,
            CALL_LIMIT,
        ));
        if cancelled() {
            self.close_session(&mut sessions, server);
            return Err("MCP call interrupted".into());
        }
        match response {
            Ok(Ok(value)) => {
                let output = result_text(&value);
                if value.is_error.unwrap_or(false) {
                    Err(if output.is_empty() {
                        "MCP tool reported an error".into()
                    } else {
                        output
                    })
                } else {
                    Ok(output)
                }
            }
            Ok(Err(_)) => {
                self.close_session(&mut sessions, server);
                Err("MCP tool call failed".into())
            }
            Err(reason) => {
                self.close_session(&mut sessions, server);
                Err(format!("MCP tool call {reason}"))
            }
        }
    }

    pub(crate) fn shutdown(&self) {
        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        for (_, mut session) in sessions.drain() {
            let _ = self
                .runtime
                .block_on(session.service.close_with_timeout(CLOSE_LIMIT));
        }
    }

    fn ensure_session<'a>(
        &self,
        sessions: &'a mut HashMap<String, Session>,
        name: &str,
        config: &ServerConfig,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<&'a mut Session, &'static str> {
        if !sessions.contains_key(name) {
            let connected =
                self.runtime
                    .block_on(wait_cancel(connect(config), cancelled, STARTUP_LIMIT));
            let service = match connected {
                Ok(Ok(service)) => service,
                Ok(Err(_)) => return Err("connection failed"),
                Err(reason) => return Err(reason),
            };
            sessions.insert(
                name.to_owned(),
                Session {
                    service,
                    tools: HashMap::new(),
                },
            );
        }
        Ok(sessions.get_mut(name).expect("inserted above"))
    }

    fn fetch_tools(
        &self,
        server: &str,
        session: &mut Session,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Vec<ToolSpec>, &'static str> {
        let mut cursor = None;
        let mut seen = HashSet::new();
        let mut seen_cursors = HashSet::new();
        let mut schemas = HashMap::new();
        let mut specs = Vec::new();
        let started = std::time::Instant::now();
        for _ in 0..MAX_PAGES {
            let remaining = LIST_LIMIT.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                return Err("tool discovery timed out");
            }
            let page = self.runtime.block_on(wait_cancel(
                session.service.list_tools(Some(
                    rmcp::model::PaginatedRequestParams::default().with_cursor(cursor.clone()),
                )),
                cancelled,
                remaining,
            ));
            let page = match page {
                Ok(Ok(page)) => page,
                Ok(Err(_)) => return Err("tool discovery failed"),
                Err(reason) => return Err(reason),
            };
            for tool in page.tools {
                if specs.len() >= MAX_TOOLS {
                    return Err("too many tools");
                }
                let name = tool.name.to_string();
                if name.is_empty() || name.len() > 120 || !seen.insert(name.clone()) {
                    return Err("invalid or duplicate tool name");
                }
                let schema = Value::Object((*tool.input_schema).clone());
                if schema.to_string().len() > MAX_SCHEMA_BYTES
                    || !local_schema_refs(&schema)
                    || jsonschema::validator_for(&schema).is_err()
                {
                    return Err("invalid or oversized input schema");
                }
                let description = single_line(tool.description.as_deref().unwrap_or(""), 512);
                schemas.insert(name.clone(), schema.clone());
                specs.push(ToolSpec {
                    server: server.to_owned(),
                    name,
                    description,
                    input_schema: schema,
                });
            }
            match page.next_cursor {
                None => {
                    session.tools = schemas;
                    return Ok(specs);
                }
                Some(next) if seen_cursors.insert(next.clone()) => cursor = Some(next),
                Some(_) => return Err("repeated discovery cursor"),
            }
        }
        Err("too many discovery pages")
    }

    fn close_session(&self, sessions: &mut HashMap<String, Session>, name: &str) {
        if let Some(mut session) = sessions.remove(name) {
            let _ = self
                .runtime
                .block_on(session.service.close_with_timeout(CLOSE_LIMIT));
        }
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        self.shutdown();
    }
}

async fn connect(config: &ServerConfig) -> Result<RunningService<RoleClient, ()>, ()> {
    match config {
        ServerConfig::Stdio { command, args, env } => {
            let mut process = tokio::process::Command::new(command);
            process.args(args).env_clear().stderr(Stdio::null());
            // A server receives only basic process context plus its own explicit
            // settings/auth entries, never Revex's model API key or debug flags.
            for name in [
                "PATH", "HOME", "LANG", "LC_ALL", "LC_CTYPE", "TMPDIR", "USER",
            ] {
                if let Some(value) = std::env::var_os(name) {
                    process.env(name, value);
                }
            }
            process.envs(env);
            let transport = TokioChildProcess::new(process).map_err(|_| ())?;
            ().serve(transport).await.map_err(|_| ())
        }
        ServerConfig::Http { url, headers } => {
            let mut options = StreamableHttpClientTransportConfig::with_uri(url.clone());
            options.custom_headers = headers.clone();
            options.allow_stateless = true;
            let transport = StreamableHttpClientTransport::from_config(options);
            ().serve(transport).await.map_err(|_| ())
        }
    }
}

async fn wait_cancel<T, E>(
    future: impl Future<Output = Result<T, E>>,
    cancelled: &dyn Fn() -> bool,
    limit: Duration,
) -> Result<Result<T, E>, &'static str> {
    let mut check = tokio::time::interval(Duration::from_millis(50));
    tokio::pin!(future);
    let sleep = tokio::time::sleep(limit);
    tokio::pin!(sleep);
    loop {
        tokio::select! {
            value = &mut future => return Ok(value),
            _ = &mut sleep => return Err("timed out"),
            _ = check.tick() => if cancelled() { return Err("interrupted"); },
        }
    }
}

fn object_at(map: &Map<String, Value>, key: &str) -> Result<Map<String, Value>, String> {
    match map.get(key) {
        None => Ok(Map::new()),
        Some(Value::Object(value)) => Ok(value.clone()),
        Some(_) => Err(format!("{key} must be a JSON object")),
    }
}

fn valid_server_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 48
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn string_map(value: Option<&Value>, field: &str) -> Result<HashMap<String, String>, &'static str> {
    let Some(value) = value else {
        return Ok(HashMap::new());
    };
    let Some(object) = value.as_object() else {
        return Err(if field == "env" {
            "env must be an object"
        } else {
            "headers must be an object"
        });
    };
    object
        .iter()
        .map(|(key, value)| value.as_str().map(|text| (key.clone(), text.to_owned())))
        .collect::<Option<HashMap<_, _>>>()
        .ok_or("env and header values must be strings")
}

fn parse_server(entry: &Value, auth: Option<&Value>) -> Result<ServerConfig, &'static str> {
    let object = entry.as_object().ok_or("entry must be an object")?;
    let secret = match auth {
        None => None,
        Some(value) => Some(value.as_object().ok_or("auth entry must be an object")?),
    };
    match object.get("type").and_then(Value::as_str) {
        Some("stdio") => {
            if object.contains_key("url") || object.contains_key("headers") {
                return Err("stdio entry cannot contain HTTP fields");
            }
            let command = object
                .get("command")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or("stdio entry needs command")?
                .to_owned();
            let args = match object.get("args") {
                None => Vec::new(),
                Some(value) => value
                    .as_array()
                    .ok_or("args must be an array")?
                    .iter()
                    .map(|value| value.as_str().map(str::to_owned))
                    .collect::<Option<Vec<_>>>()
                    .ok_or("args must contain strings")?,
            };
            let mut env = string_map(object.get("env"), "env")?;
            env.extend(string_map(
                secret.and_then(|value| value.get("env")),
                "env",
            )?);
            if secret.is_some_and(|value| value.contains_key("headers")) {
                return Err("stdio auth cannot contain headers");
            }
            Ok(ServerConfig::Stdio { command, args, env })
        }
        Some("http") | Some("streamable-http") => {
            if object.contains_key("command")
                || object.contains_key("args")
                || object.contains_key("env")
            {
                return Err("HTTP entry cannot contain stdio fields");
            }
            let url = object
                .get("url")
                .and_then(Value::as_str)
                .ok_or("HTTP entry needs url")?;
            let parsed: http::Uri = url.parse().map_err(|_| "invalid HTTP URL")?;
            if !matches!(parsed.scheme_str(), Some("http" | "https")) || parsed.host().is_none() {
                return Err("invalid HTTP URL");
            }
            let mut named = string_map(object.get("headers"), "headers")?;
            named.extend(string_map(
                secret.and_then(|value| value.get("headers")),
                "headers",
            )?);
            if secret.is_some_and(|value| value.contains_key("env")) {
                return Err("HTTP auth cannot contain env");
            }
            let mut headers = HashMap::new();
            for (name, value) in named {
                let name = HeaderName::from_bytes(name.as_bytes())
                    .map_err(|_| "invalid HTTP header name")?;
                let value =
                    HeaderValue::from_str(&value).map_err(|_| "invalid HTTP header value")?;
                headers.insert(name, value);
            }
            Ok(ServerConfig::Http {
                url: url.to_owned(),
                headers,
            })
        }
        _ => Err("type must be stdio or http"),
    }
}

fn single_line(value: &str, limit: usize) -> String {
    let mut result = String::new();
    for ch in value.chars() {
        let ch = if ch.is_control() { ' ' } else { ch };
        if result.len() + ch.len_utf8() > limit {
            break;
        }
        result.push(ch);
    }
    result
}

fn local_schema_refs(value: &Value) -> bool {
    match value {
        Value::Object(object) => object.iter().all(|(key, value)| {
            if matches!(key.as_str(), "$ref" | "$dynamicRef" | "$recursiveRef") {
                value
                    .as_str()
                    .is_some_and(|reference| reference.starts_with('#'))
            } else {
                local_schema_refs(value)
            }
        }),
        Value::Array(items) => items.iter().all(local_schema_refs),
        _ => true,
    }
}

fn push_bounded(output: &mut String, value: &str) -> bool {
    for ch in value.chars() {
        let ch = if ch == '\n' || ch == '\t' || !ch.is_control() {
            ch
        } else {
            ' '
        };
        if output.len() + ch.len_utf8() > MAX_OUTPUT_BYTES.saturating_sub(16) {
            output.push_str("\n[truncated]");
            return false;
        }
        output.push(ch);
    }
    true
}

#[cfg(test)]
fn bounded_text(value: &str) -> String {
    let mut result = String::new();
    push_bounded(&mut result, value);
    result
}

fn result_text(result: &rmcp::model::CallToolResult) -> String {
    let mut output = String::new();
    for content in &result.content {
        if !output.is_empty() && !push_bounded(&mut output, "\n") {
            return output;
        }
        let text = match content {
            ContentBlock::Text(text) => text.text.as_str(),
            ContentBlock::Resource(resource) => match &resource.resource {
                rmcp::model::ResourceContents::TextResourceContents { text, .. } => text,
                _ => "[binary resource omitted]",
            },
            ContentBlock::Image(_) => "[image omitted]",
            ContentBlock::Audio(_) => "[audio omitted]",
            ContentBlock::ResourceLink(link) => &link.uri,
            _ => "[unsupported content omitted]",
        };
        if !push_bounded(&mut output, text) {
            return output;
        }
    }
    if output.is_empty()
        && let Some(value) = &result.structured_content
    {
        let mut writer = LimitedWriter { bytes: Vec::new() };
        let truncated = serde_json::to_writer(&mut writer, value).is_err();
        output = String::from_utf8_lossy(&writer.bytes).into_owned();
        if truncated {
            output.push_str("\n[truncated]");
        }
    }
    output
}

struct LimitedWriter {
    bytes: Vec<u8>,
}

impl std::io::Write for LimitedWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let available = MAX_OUTPUT_BYTES.saturating_sub(16 + self.bytes.len());
        if bytes.len() > available {
            self.bytes.extend_from_slice(&bytes[..available]);
            return Err(std::io::Error::other("MCP result limit"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    static TEST_SEQ: AtomicU64 = AtomicU64::new(0);

    fn fake_stdio_script() -> (std::path::PathBuf, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "revex-mcp-test-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&dir).unwrap();
        let script = dir.join("server.py");
        std::fs::write(
            &script,
            r#"import json, os, sys, time
schema = {"type":"object","properties":{"text":{"type":"string"}},"required":["text"],"additionalProperties":False}
for line in sys.stdin:
    request = json.loads(line)
    if "id" not in request:
        continue
    method = request.get("method")
    params = request.get("params") or {}
    if method == "initialize":
        result = {"protocolVersion":params.get("protocolVersion", "2025-11-25"),"capabilities":{"tools":{"listChanged":True}},"serverInfo":{"name":"fake","version":"1"}}
    elif method == "tools/list":
        if params.get("cursor") is None:
            result = {"tools":[{"name":"echo","description":"Echo text","inputSchema":schema}],"nextCursor":"second"}
        else:
            result = {"tools":[{"name":"sleep","description":"Sleep then echo","inputSchema":schema}]}
    elif method == "tools/call":
        if params.get("name") == "sleep":
            time.sleep(1)
        result = {"content":[{"type":"text","text":os.getenv("REVEX_API_KEY", "absent") + ":" + os.getenv("TEST_TOKEN", "none") + ":" + params.get("arguments",{}).get("text","")}],"isError":False}
    else:
        result = {}
    print(json.dumps({"jsonrpc":"2.0","id":request["id"],"result":result}), flush=True)
"#,
        )
        .unwrap();
        (dir, script)
    }

    fn stdio_maps(script: &std::path::Path) -> (Map<String, Value>, Map<String, Value>) {
        let settings = json!({"mcpServers":{"fake":{"type":"stdio","command":"python3","args":[script],"env":{"TEST_TOKEN":"public"}}}});
        let auth = json!({"mcpServers":{"fake":{"env":{"TEST_TOKEN":"secret"}}}});
        (
            settings.as_object().unwrap().clone(),
            auth.as_object().unwrap().clone(),
        )
    }

    #[test]
    fn stdio_discovers_paginated_tools_validates_arguments_and_overlays_auth() {
        let (dir, script) = fake_stdio_script();
        let (settings, auth) = stdio_maps(&script);
        let client = McpClient::from_maps(&settings, &auth).unwrap();
        let found = client.list_tools(&|| false);
        assert!(found.errors.is_empty(), "{:?}", found.errors);
        assert_eq!(found.tools.len(), 2);
        assert_eq!(found.tools[0].qualified_name(), "mcp__fake__echo");
        assert_eq!(
            client.call("fake", "echo", &json!({"text":"hello"}), &|| false),
            Ok("absent:secret:hello".into())
        );
        assert!(
            client
                .call("fake", "echo", &json!({"text":7}), &|| false)
                .unwrap_err()
                .contains("input schema")
        );
        client.shutdown();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn stdio_call_can_be_interrupted_without_waiting_for_server() {
        let (dir, script) = fake_stdio_script();
        let (settings, auth) = stdio_maps(&script);
        let client = McpClient::from_maps(&settings, &auth).unwrap();
        assert_eq!(client.list_tools(&|| false).tools.len(), 2);
        let interrupted = std::sync::Arc::new(AtomicBool::new(false));
        let signal = interrupted.clone();
        let toggle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            signal.store(true, Ordering::Release);
        });
        let start = std::time::Instant::now();
        let error = client
            .call("fake", "sleep", &json!({"text":"hello"}), &|| {
                interrupted.load(Ordering::Acquire)
            })
            .unwrap_err();
        toggle.join().unwrap();
        assert!(error.contains("interrupted"));
        assert!(start.elapsed() < Duration::from_secs(4));
        client.shutdown();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn stdio_does_not_inherit_parent_api_key() {
        if std::env::var_os("REVEX_MCP_ENV_CHILD").is_some() {
            let (dir, script) = fake_stdio_script();
            let (settings, auth) = stdio_maps(&script);
            let client = McpClient::from_maps(&settings, &auth).unwrap();
            assert_eq!(
                client.call("fake", "echo", &json!({"text":"hello"}), &|| false),
                Ok("absent:secret:hello".into())
            );
            client.shutdown();
            std::fs::remove_dir_all(dir).unwrap();
            return;
        }
        let home = std::env::temp_dir().join(format!(
            "revex-mcp-home-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&home).unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "mcp_client::tests::stdio_does_not_inherit_parent_api_key",
            ])
            .env("HOME", &home)
            .env("REVEX_MCP_ENV_CHILD", "1")
            .env("REVEX_API_KEY", "parent-secret")
            .output()
            .unwrap();
        std::fs::remove_dir_all(home).unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
    }

    #[test]
    fn http_connects_and_uses_auth_headers_without_logging_values() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let worker = std::thread::spawn(move || {
            while !stopped.load(Ordering::Acquire) {
                let Ok((mut stream, _)) = listener.accept() else {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut bytes = Vec::new();
                let mut part = [0u8; 4096];
                while !bytes.windows(4).any(|chunk| chunk == b"\r\n\r\n") {
                    let count = stream.read(&mut part).unwrap();
                    if count == 0 {
                        break;
                    }
                    bytes.extend_from_slice(&part[..count]);
                }
                let Some(end) = bytes.windows(4).position(|chunk| chunk == b"\r\n\r\n") else {
                    continue;
                };
                let end = end + 4;
                let header = String::from_utf8_lossy(&bytes[..end]);
                let length = header
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .and_then(|value| value.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                let authorized = header
                    .lines()
                    .any(|line| line.eq_ignore_ascii_case("x-test: secret"));
                while bytes.len() < end + length {
                    let count = stream.read(&mut part).unwrap();
                    if count == 0 {
                        break;
                    }
                    bytes.extend_from_slice(&part[..count]);
                }
                let request: Value =
                    serde_json::from_slice(&bytes[end..end + length]).unwrap_or(Value::Null);
                let id = request.get("id").cloned();
                let method = request["method"].as_str().unwrap_or("");
                let result = match method {
                    "initialize" => {
                        json!({"protocolVersion":request["params"]["protocolVersion"],"capabilities":{"tools":{}},"serverInfo":{"name":"fake-http","version":"1"}})
                    }
                    "tools/list" => {
                        json!({"tools":[{"name":"ping","description":"Ping","inputSchema":{"type":"object","additionalProperties":false}}]})
                    }
                    "tools/call" => {
                        json!({"content":[{"type":"text","text":if authorized { "authorized" } else { "unauthorized" }}]})
                    }
                    _ => json!({}),
                };
                let body = id
                    .map(|id| json!({"jsonrpc":"2.0","id":id,"result":result}).to_string())
                    .unwrap_or_default();
                let status = if body.is_empty() {
                    "202 Accepted"
                } else {
                    "200 OK"
                };
                write!(stream,"HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
                stream.flush().unwrap();
            }
        });
        let settings = json!({"mcpServers":{"remote":{"type":"http","url":format!("http://{address}/mcp"),"headers":{"X-Test":"public"}}}});
        let auth = json!({"mcpServers":{"remote":{"headers":{"X-Test":"secret"}}}});
        let client =
            McpClient::from_maps(settings.as_object().unwrap(), auth.as_object().unwrap()).unwrap();
        let found = client.list_tools(&|| false);
        assert!(found.errors.is_empty(), "{:?}", found.errors);
        assert_eq!(found.tools[0].qualified_name(), "mcp__remote__ping");
        assert_eq!(
            client.call("remote", "ping", &json!({}), &|| false),
            Ok("authorized".into())
        );
        client.shutdown();
        stop.store(true, Ordering::Release);
        worker.join().unwrap();
    }

    #[test]
    fn malformed_server_is_isolated_and_diagnostics_do_not_include_secrets() {
        let settings = json!({"mcpServers":{"bad":{"type":"http","url":"not-a-url","headers":{"Authorization":"Bearer private"}},"good":{"type":"stdio","command":"nonexistent-mcp-command"}}});
        let auth = json!({"mcpServers":{"bad":{"headers":{"Authorization":"Bearer secret"}}}});
        let client =
            McpClient::from_maps(settings.as_object().unwrap(), auth.as_object().unwrap()).unwrap();
        let found = client.list_tools(&|| false);
        assert_eq!(found.errors.len(), 2);
        assert!(
            found
                .errors
                .iter()
                .all(|error| !error.contains("secret") && !error.contains("private"))
        );
    }

    #[test]
    fn output_is_bounded_and_control_characters_are_removed() {
        let input = format!("{}\u{1b}[31m", "x".repeat(MAX_OUTPUT_BYTES + 100));
        let result = bounded_text(&input);
        assert!(result.len() <= MAX_OUTPUT_BYTES);
        assert!(result.contains("[truncated]"));
        assert!(!bounded_text("bad\u{1b}value\r").contains('\u{1b}'));
    }

    #[test]
    fn schema_rejects_external_refs_without_resolving_them() {
        assert!(!local_schema_refs(
            &json!({"$ref":"https://example.invalid/schema"})
        ));
        assert!(!local_schema_refs(
            &json!({"items":{"$ref":"file:///etc/passwd"}})
        ));
        assert!(local_schema_refs(
            &json!({"$defs":{"x":{"type":"string"}},"$ref":"#/$defs/x"})
        ));
    }
}
