# revex

A minimal coding and reverse-engineering agent harness for local LLMs, written in Rust.

Most agent harnesses spend thousands of tokens on a system prompt and tool JSON
schemas before the first user message. On a local model that cost is paid in prefill
time and context window. `revex` keeps the whole prompt to roughly 200 tokens and
skips tool schemas entirely: the model calls tools by writing one-line commands.

- Single binary of about 1.3 MB, two dependencies (`ureq`, `serde_json`)
- Talks to any OpenAI-compatible endpoint with streaming, through either Chat Completions or the Responses API
- One-shot, interactive and MCP server modes
- Tool output is truncated to protect the context window
- Refusals from the provider are detected and reported to the caller

## What kind of harness is this?

`revex` is a **sidecar worker**, not a general-purpose coding assistant. It is the
smallest loop that lets a local model act on your machine: read files, run commands,
edit code, and report back. It is meant to sit next to a hosted agent such as Claude
Code or Codex and take over the part of the work that suits a local model.

Typical jobs to hand over:

- Working out how a feature is implemented in a binary or application you have no
  source for, using `objdump`, `otool`, `radare2` or `lldb`
- Documenting a file format or protocol, and reimplementing it for interoperability
- Comparing two builds of the same program to find what changed
- Long, repetitive analysis that you do not want to pay for per token or send off
  your machine

`revex` is for understanding software, not for scanning it for vulnerabilities or
exploiting it. Use it only on software you own or are authorized to analyze.

Running every step on a local model is slow, so the intended split is: the hosted
agent plans and reviews, and `revex` does the targeted work through the `delegate`
MCP tool. It also works on its own from a terminal.

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
| Four one-line commands instead of JSON tool schemas | The schemas cost nothing, and small models write a plain line more reliably than a nested JSON call.                                              |
| System prompt of about 600 characters               | Keeps prefill short and leaves the context window for the actual work.                                                                            |
| Everything else goes through `sh`                   | No dedicated wrappers to maintain; any installed tool is available.                                                                               |
| Tool output cut at 2000 bytes                       | Disassembly and logs are the fastest way to fill a small context. The model is told how many bytes were omitted and can ask for a narrower range. |
| Unknown verbs run as shell commands                 | Small models often write `>> cat file`. Treating it as `sh` is more useful than failing.                                                          |
| Synchronous HTTP, no async runtime                  | The loop is strictly sequential. This keeps the binary small and the build fast.                                                                  |
| One source file                                     | The whole harness can be read in a sitting.                                                                                                       |

### What it deliberately does not do

- **No sandbox.** Commands run with your user's permissions.
- **No context management.** The history grows until the model's window is full;
  there is no summarization or compaction.
- **No planning, sub-agents or memory.** It is one model in one loop.
- **No native tool calling or vision.** Only text over `/chat/completions`.
- **No retries or backoff** beyond ignoring a stray `>>`-only reply.

If you need those, use a full harness. `revex` trades them for a prompt you can
read in one screen.

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

All configuration is through environment variables.

| Variable         | Required | Description                                                                                                            |
| ---------------- | -------- | ---------------------------------------------------------------------------------------------------------------------- |
| `REVEX_BASE_URL` | yes      | Base URL of the OpenAI-compatible API, including `/v1`                                                                 |
| `REVEX_API`      | no       | `chat` (default, `/chat/completions`) or `responses` (`/responses`)                                                    |
| `REVEX_MODEL`    | no       | Model ID. If unset, the first model from `/models` is used; if that fails, the field is omitted and the server decides |
| `REVEX_API_KEY`  | no       | Sent as `Authorization: Bearer <key>`                                                                                  |
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

## Usage

```sh
# One-shot: the final answer goes to stdout, progress goes to stderr
revex --yes "Read Cargo.toml and list the dependencies"

# Task from stdin
echo "Summarize src/main.rs" | revex --yes

# Interactive
revex

# MCP server over stdio
revex --mcp
```

Without `--yes`, every shell command and file write asks for confirmation first.
`--mcp` implies `--yes`, because an MCP call has no terminal to ask on.

## How the model calls tools

The system prompt teaches four commands. The model emits them at the start of a
line, and `revex` runs them and replies with the result in `<out>` tags:

```
>> read PATH [A-B]      read file lines, with line numbers
>> sh COMMAND           run a shell command
>> write PATH <<EOF     create a file; the body follows and ends with an EOF line
>> replace PATH <<EOF   old text, a line containing ===, new text, then EOF
```

An unknown verb is treated as a shell command, which absorbs the most common
mistake of small models. A reply with no `>>` line ends the loop, and that reply is
the answer. The loop stops after 30 steps.

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

`delegate` takes `task` (required) and `cwd`. It returns the final answer. If the
provider refuses, the result has `isError: true`, so the caller can decide what to
do next.

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

## Development

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features --workspace
```

Commits follow [Conventional Commits](https://www.conventionalcommits.org/) with a
header of at most 128 characters. CI runs commitlint, rustfmt, Clippy and the tests
on every pull request. Pushing a `v*` tag builds the binaries and creates a release.
