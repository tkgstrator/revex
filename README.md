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

| Variable | Required | Description |
| --- | --- | --- |
| `REVEX_BASE_URL` | yes | Base URL of the OpenAI-compatible API, including `/v1` |
| `REVEX_API` | no | `chat` (default, `/chat/completions`) or `responses` (`/responses`) |
| `REVEX_MODEL` | no | Model ID. If unset, the first model from `/models` is used; if that fails, the field is omitted and the server decides |
| `REVEX_API_KEY` | no | Sent as `Authorization: Bearer <key>` |
| `REVEX_EXTRA` | no | JSON object merged into every request body |
| `REVEX_DEBUG` | no | Set to print raw SSE lines to stderr |

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
