# revex

A minimal coding and reverse-engineering agent harness for local LLMs, written in Rust.

Most agent harnesses spend thousands of tokens on a system prompt and tool JSON
schemas before the first user message. On a local model that cost is paid in prefill
time and context window. `revex` keeps the prompt short and
skips tool schemas entirely: the model calls tools by writing one-line commands.

- Small single binary with three direct dependencies (`ureq`, `serde_json`, `libc`)
- Talks to any OpenAI-compatible endpoint with streaming, through either Chat Completions or the Responses API
- One-shot, interactive and MCP server modes
- Persistent interactive history, read-only helper agents and live `/agents` status with provider-reported token usage
- Tool output is truncated to protect the context window
- Refusals from the provider are detected and reported to the caller

## What kind of harness is this?

`revex` is a small coding assistant for a local model. In a terminal, you can give it
a task, approve commands and edits, and inspect its helper agents while it works.
It can also serve as a sidecar worker for Claude Code or Codex through MCP.

Typical jobs to hand over:

- Working out how a feature is implemented in a binary or application you have no
  source for, using `objdump`, `otool`, `radare2` or `lldb`
- Documenting a file format or protocol, and reimplementing it for interoperability
- Comparing two builds of the same program to find what changed
- Long, repetitive analysis that you do not want to pay for per token or send off
  your machine

`revex` is for understanding software, not for scanning it for vulnerabilities or
exploiting it. Use it only on software you own or are authorized to analyze.

Local models vary in speed and tool-use ability. The `delegate` MCP tool lets a hosted
agent assign a targeted task to Revex when that is useful.

### The loop

```
   task ──► model ──► reply
              ▲          │
              │          ▼
         <out> result   lines starting with ">> " ?
              ▲          │ yes: run each one, truncate its output
              └──────────┘
                         │ no: the reply is the final answer
```

1. Your task is sent to the model together with a short fixed system prompt.
2. The model replies. Any line starting with `>> ` is a tool call.
3. `revex` runs every call in order, truncates each output, and sends the results
   back inside `<out>` tags.
4. This repeats until a reply contains no `>> ` line. That reply is the answer.
   A step limit of 30 stops runaway loops.

The conversation is append-only and the system prompt never changes, so a server
that caches the prompt prefix can reuse it between steps.

### Design choices

| Choice                                              | Why                                                                                                                                               |
| --------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------- |
| Six one-line commands instead of JSON tool schemas  | No schema tokens are sent, and small models write a plain line more reliably than a nested JSON call.                                              |
| Short fixed system prompt                            | Keeps prefill short and leaves the context window for the actual work.                                                                            |
| Parent commands can use `sh`                         | No dedicated wrappers to maintain; any installed tool is available to the parent. Helpers are restricted to read and search.                       |
| Tool output cut at 2000 bytes                       | Disassembly and logs are the fastest way to fill a small context. The model is told how many bytes were omitted and can ask for a narrower range. |
| Unknown verbs run as shell commands                 | Small models often write `>> cat file`. Treating it as `sh` is more useful than failing.                                                          |
| Synchronous HTTP, no async runtime                  | Each loop is strictly sequential; helper agents run on plain threads. This keeps the binary small and the build fast.                              |
| Three source files                                  | The loop, helper agents and terminal coordinator live in separate modules.                                                                        |

### What it deliberately does not do

- **No sandbox.** Commands run with your user's permissions.
- **No context management.** The saved history grows
  until the model's window is full; there is no summarization or compaction.
- **No built-in plan mode.** One model runs the loop; it may start read-only
  [helper agents](#helper-agents), but only it edits files. Saved conversation
  history is available in interactive and MCP sessions.
- **No native tool calling or vision.** Only text over Chat Completions or the Responses API.
- **No general retries or backoff.** A server that rejects streamed usage gets one
  compatibility retry without that option.

Revex keeps these limits to remain a compact local-model client.

## Install

```sh
cargo install --git https://github.com/tkgstrator/revex
```

Or build from source:

```sh
cargo build --release
```

Prebuilt binaries for macOS (arm64) and Linux (x86_64, arm64) are attached to each
[release](https://github.com/tkgstrator/revex/releases).

## Configuration

On the first interactive launch, Revex presents a setup screen for the base URL of a
running OpenAI-compatible model API, an optional API key and an optional model ID.
It saves these in `~/.revex` with mode `0600` on Unix. A key is entered without
terminal echo. The setup does not start a model server. One-shot, piped and MCP
modes require prior configuration and do not prompt. Environment variables can
override saved settings.

On later interactive launches, the welcome screen shows the project directory,
saved history, API reachability and selected model. At `start >`, enter a task,
press Enter for the regular prompt, or enter `s` or `/config` to edit the saved
connection settings before starting.

| Variable         | Required | Description                                                                                                            |
| ---------------- | -------- | ---------------------------------------------------------------------------------------------------------------------- |
| `REVEX_BASE_URL` | yes      | Base URL of the OpenAI-compatible API, including `/v1`. Interactive setup can save it as `base_url` in `~/.revex` |
| `REVEX_API`      | no       | `chat` (default, `/chat/completions`) or `responses` (`/responses`)                                                    |
| `REVEX_MODEL`    | no       | Model ID. If unset, the first model from `/models` is used; if that fails, the field is omitted and the server decides |
| `REVEX_API_KEY`  | no       | Sent as `Authorization: Bearer <key>`; setup can save it in private `~/.revex` |
| `REVEX_FRONTIER` | no | Command that handles a task instead of the local model, for example `claude -p`. The task is appended as the last argument |
| `REVEX_ROUTE` | no | Set to `auto` to let the local model decide per task whether to forward it. Off by default |
| `REVEX_EXTRA`    | no       | JSON object merged into every request body                                                                             |
| `REVEX_DEBUG`    | no       | Set to print raw SSE lines to stderr                                                                                   |

The Responses API has no `stop` parameter, so with `REVEX_API=responses` the reply is cut
at `<out>` on the client side. It is stateless (`store: false`): the full history is sent
every step.

```sh
export REVEX_BASE_URL=http://localhost:8000/v1
export REVEX_MODEL=your-model-id
export REVEX_API_KEY=your-key
```

### Settings files

Settings can also live in a JSON file. `~/.revex` and the `.revex` in the current
directory are both read and merged key by key, and the project file wins.
Environment variables beat both.

```json
{ "model": "your-model-id", "route": "auto", "api": "chat" }
```

| Key | Allowed in `./.revex` | Environment variable |
| --- | --- | --- |
| `model`, `route`, `api` | yes | `REVEX_MODEL`, `REVEX_ROUTE`, `REVEX_API` |
| `frontier`, `base_url`, `api_key` | no, `~/.revex` only | `REVEX_FRONTIER`, `REVEX_BASE_URL`, `REVEX_API_KEY` |

`frontier` runs a command, and `base_url` receives your API key. A cloned repository
cannot set these or `api_key`; project-file values are ignored with a warning. The
optional key entered in first-run setup is stored in the private user settings file.

## Usage

```sh
# One-shot: the final answer goes to stdout, progress goes to stderr
revex --yes "Read Cargo.toml and list the dependencies"

# Task from stdin
echo "Summarize src/main.rs" | revex --yes

# Interactive: run from the project directory you want Revex to work on
revex

# MCP server over stdio
revex --mcp
```

In the interactive mode these slash commands are available:

| Command | Effect |
| --- | --- |
| `/help` | List the commands |
| `/agents` | Show the parent and helpers, their state, tasks, model and reported token usage |
| `/clear` | Clear parent and helper history, state and token totals when no helper is active |
| `/model [ID]` | Show the current model and the server's list, or switch |
| `/route [off\|auto\|local\|remote]` | Show or switch the forwarding mode |
| `/status` | Show the active API, model, route, parent state, queued tasks and directory |
| `/exit` | Quit |

`/model` and `/route` save the choice: to `./.revex` if it exists, otherwise to
`~/.revex`. When writing a project file, keys that a project file may not set are
dropped. If the matching environment variable is set, it still wins on the next start.

The interactive input stays available while a model request or helper runs.
`/agents` and `/status` work during a task; another natural-language task is queued.
The parent may start up to two read-only helpers concurrently with `>> spawn` and
collect their answers with `>> wait`. Token counts come from the model provider;
missing counts are shown as unavailable or partial rather than estimated.
If a helper is still running, `/clear` reports that the session was not cleared;
wait for the helper to finish, then run `/clear` again.

#### Saved history

The interactive mode (no arguments, terminal on stdin) keeps its conversation across
restarts, one history per canonical working directory. It is stored as versioned JSON in
`$XDG_STATE_HOME/revex/`, or `~/.local/state/revex/` if that is unset. (`~/.revex` is the
settings file and is not used.) The file is rewritten atomically (temp file and rename)
after every message, including partial progress when a request fails. A save failure
stops the current request and exits interactive mode. On start the
messages are reloaded, tool outputs included, and the stored system prompt is replaced by
the current one. `/clear` rewrites the saved file as empty and removes the parent
and helper records.

The directory is created `0700` and the file `0600` on Unix. The history holds your
prompts and everything tools printed, so it may contain secrets from files or commands.
The API key saved by first-run setup is kept in private user settings, outside this
history. A tool output that shows it could still end up in the history.
Forwarded exchanges (the task and the remote answer) are saved too, and later local
turns see them. Forwarding still passes only the current task to the remote command.
If the history directory cannot be determined (no absolute
`XDG_STATE_HOME` or `HOME`), interactive mode exits with an error rather than running
without persistence. Two interactive sessions in the same directory are not coordinated:
the last one to write wins. If the file is corrupt, has an unknown version or belongs to another directory, revex
exits with an error and leaves it untouched; fix or delete it to continue. If `/clear`
cannot save, it keeps the current in-memory conversation. One-shot,
piped runs do not read or write this history. `--mcp` keeps separate sessions; see
[Use from Claude Code or Codex](#use-from-claude-code-or-codex).

Helper agents started with `>> spawn` (see [Helper agents](#helper-agents)) are saved
beside the history as `agents-history-<hash>/a<N>.json` (`agents-session-<id>/` for MCP
sessions), with the same permissions: task, cwd, model, status, result or error,
reported token usage and the helper's own messages. A helper that was queued or running
when revex stopped is marked `interrupted` on the next start and is never rerun. A corrupt
record is reported and left in place. One-shot and piped runs keep helpers in memory only.
The parent state and reported token usage are saved beside these records in `main.json`.

Without `--yes`, every shell command and file write asks for confirmation first.
The interactive prompt shows a bounded preview before an edit, including existing
content when a file will be overwritten. Tool activity and shell exit status are shown
as the task runs. If the target changes while approval is pending, Revex refuses the
edit. `/exit` and Ctrl+C stop an active interactive shell process group before exiting.
`--mcp` implies `--yes`, because an MCP call has no terminal to ask on.

## How the model calls tools

The parent system prompt teaches four file and shell commands, plus `spawn` and
`wait` (below). The model emits them at the start of a line, and `revex` runs them
and replies with the result in `<out>` tags:

```
>> read PATH [A-B]      read file lines, with line numbers
>> sh COMMAND           run a shell command
>> write PATH <<EOF     create a file; the body follows and ends with an EOF line
>> replace PATH <<EOF   old text, a line containing ===, new text, then EOF
```

An unknown verb is treated as a shell command, which absorbs the most common
mistake of small models. A reply with no `>>` line ends the loop, and that reply is
the answer. The loop stops after 30 steps.

### Helper agents

The main agent can also delegate independent read-only questions:

```
>> spawn TASK           start a helper; replies at once with its ID, e.g. `a1 started`
>> wait ID [ID ...]     block until those helpers finish; each result is returned once
```

A helper has its own conversation and a different system prompt, and works in the same
directory as the main agent. It can only use `read` and
`>> search PATTERN [PATH ...]`, which runs `rg` directly (no shell), with the pattern after
`-e`, the paths after `--` and `--no-config`. Everything else, including `sh`, `write`,
`replace`, `spawn`, `wait` and unknown verbs, is refused in code, not only by the prompt.
Helpers cannot ask for confirmation, so none is needed. At most two run at once; up to
eight more queue and start when a slot frees, and a further `spawn` is refused with an
error. A repeated `wait` reports that the result was already returned instead of
inserting it again, and a failed helper returns its error. Each helper's token usage is
recorded separately from the main agent's, counting only what the provider reported.
Helpers still unfinished when a run or session ends are stopped and marked `interrupted`.

Because `sh` is the escape hatch, reverse-engineering tools need no dedicated
wrappers. `objdump`, `otool`, `nm`, `strings`, `radare2` and `lldb` work as they are.

## Forwarding to a hosted agent

`revex` can also act as a thin gateway. When `REVEX_FRONTIER` is set, a task can be
handed to another agent CLI and its answer returned unchanged. This is optional:
without it, `revex` never starts another program, and Claude Code, Codex or
OpenCode do not need to be installed.

```sh
export REVEX_FRONTIER='claude -p --output-format json'
revex --remote "Write unit tests for src/parse.ts"   # always forward
revex --local  "Disassemble main in ./a.out"         # never forward
```

| Mode | How | Behavior |
| --- | --- | --- |
| Off | default | Everything runs on the local model |
| `--remote` | flag | Forward the task to `REVEX_FRONTIER` |
| `--local` | flag | Run locally even if `REVEX_ROUTE=auto` |
| Auto | `REVEX_ROUTE=auto` | The local model answers `F` (forward) or `L` (local) at temperature 0 |

If the command cannot start, exits with an error, returns nothing, or reports a
refusal, `revex` says so on stderr and handles the task locally. The task is never
reworded or retried against the forwarded agent.

The command receives the task as its last argument and must print the answer to
stdout. Examples:

| Agent | Command | Status |
| --- | --- | --- |
| Claude Code | `claude -p --output-format json` | Tried: the answer is read from `result`, and `stop_reason: "refusal"` is detected |
| Codex | `codex exec --skip-git-repo-check` | Tried: the answer is printed on stdout. Refusals are detected only by a non-zero exit |
| OpenCode | `opencode run` | Not tried |

Notes:

- Forwarded tasks are sent to that agent's provider and subject to its policies.
- Each forwarded call has the cost of a full agent run. A one-line question to
  `claude -p` used about 15k input tokens in a test.
- Automatic routing was checked on six clear-cut tasks only. A borderline task can
  go either way, so use `--remote` and `--local` when it matters.

## Use from Claude Code or Codex

`revex --mcp` exposes a single tool, `delegate`, so a hosted agent can hand work to a
local model. This is useful for long-running tasks, or for work you would rather
keep on your own machine.

```sh
claude mcp add revex \
  -e REVEX_BASE_URL=http://localhost:8000/v1 \
  -e REVEX_MODEL=your-model-id \
  -e REVEX_API_KEY=your-key \
  -- revex --mcp

codex mcp add revex \
  --env REVEX_BASE_URL=http://localhost:8000/v1 \
  --env REVEX_MODEL=your-model-id \
  --env REVEX_API_KEY=your-key \
  -- revex --mcp
```

`delegate` takes `task` (required), `cwd` and `session_id`. It returns the final answer
as text. If the provider refuses, the result has `isError: true`, so the caller can
decide what to do next.

`cwd` defaults to the directory the server started in, not the previous call's. A `cwd`
that cannot be entered is an error; the task is not run elsewhere. Relative `cwd` paths
are resolved from the server's startup directory.

#### Continuing a session

Every call is a session, saved after each message in the same private state directory as
the interactive history, as `session-<id>.json`. Without `session_id`, a new session with
a fresh ID is created, so such calls stay independent. The ID is returned in the result's
`structuredContent`:

```json
{"content": [{"type": "text", "text": "..."}], "isError": false,
 "structuredContent": {"session_id": "0123456789abcdef0123456789abcdef"}}
```

Pass it as `session_id` in a later call to continue with the earlier messages, tool
outputs included. Repeat `cwd` on every call if the session used a non-default directory.
The ID must be 1-64 characters of `A-Z a-z 0-9 - _`. An unknown ID is
an error, never a new session, and so is a session started in a different (canonical)
`cwd` or whose file is corrupt. If saving fails, the call returns `isError: true` with
the failure and no `session_id`, since persistence is not guaranteed. A task that fails
for another reason still returns its ID if the session was saved. Concurrent calls on
one session are not coordinated. Revex never deletes session files, and they may hold
secrets from tool output.

### Provider-specific request fields

`REVEX_EXTRA` is merged into the request body as-is. Use it for fields that only your
provider understands, for example a program or policy selector:

```sh
export REVEX_EXTRA='{"some_provider_field": "value"}'
```

`revex` detects `finish_reason: "content_filter"` and a streamed `refusal` field.
Provider-specific refusal formats, such as a different `stop_reason`, are not handled.

## Safety

`revex` runs shell commands and writes files with your user's permissions.

- In interactive mode, confirm each action.
- In `--yes` and `--mcp` modes there is no confirmation. Run it in a directory and
  on a machine where that is acceptable.
- Paths are not sandboxed. Do not point it at credentials.
- Helper agents cannot write or run commands, but they read without confirmation,
  in every mode, anything your user can read, and `search` needs `rg` on `PATH`.

## Development

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features --workspace
```

Commits follow [Conventional Commits](https://www.conventionalcommits.org/) with a
header of at most 128 characters. CI runs commitlint, rustfmt, Clippy and the tests
on every pull request. Pushing a `v*` tag builds the binaries and creates a release.
