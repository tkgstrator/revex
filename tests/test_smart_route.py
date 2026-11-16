"""Smart-route integration checks with a fake Decisions server and Codex CLI.

Run after building Revex: REVEX_BIN=target/debug/revex python3 tests/test_smart_route.py
"""

import http.server
import json
import os
import pathlib
import subprocess
import tempfile
import threading
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]
BIN = pathlib.Path(os.environ.get("REVEX_BIN", ROOT / "target/debug/revex"))
if not BIN.is_absolute():
    BIN = ROOT / BIN


class FakeModelServer:
    def __init__(self, fit=0.9732, refusal=0.0244, decision_status=200):
        self.fit = fit
        self.refusal = refusal
        self.decision_status = decision_status
        self.requests = []
        outer = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_POST(self):
                body = self.rfile.read(int(self.headers["Content-Length"]))
                outer.requests.append((self.path, json.loads(body)))
                if self.path == "/v1/decisions":
                    response = json.dumps({
                        "model": outer.requests[-1][1]["model"],
                        "answers": [
                            {"type": "predicate", "name": "codex_fit", "probability": outer.fit},
                            {"type": "predicate", "name": "refusal_risk", "probability": outer.refusal},
                        ],
                        "usage": {"input_tokens": 9, "output_tokens": 0},
                    }).encode()
                    self.send_response(outer.decision_status)
                    self.send_header("Content-Type", "application/json")
                elif self.path == "/v1/chat/completions":
                    response = (
                        'data: {"choices":[{"delta":{"content":"local-answer"}}]}\n\n'
                        'data: [DONE]\n\n'
                    ).encode()
                    self.send_response(200)
                    self.send_header("Content-Type", "text/event-stream")
                else:
                    raise AssertionError(f"unexpected endpoint {self.path}")
                self.send_header("Content-Length", str(len(response)))
                self.end_headers()
                self.wfile.write(response)

            def log_message(self, _format, *_args):
                pass

        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.base = f"http://127.0.0.1:{self.server.server_port}/v1"

    def close(self):
        self.server.shutdown()
        self.thread.join(timeout=2)
        self.server.server_close()


class SmartRouteTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        if not BIN.is_file():
            raise AssertionError(f"build Revex first: {BIN}")

    def run_case(self, prompt, *, fit=0.9732, refusal=0.0244, decision_status=200,
                 cli_success=True, decision_model="clef", helper_providers="codex"):
        server = FakeModelServer(fit, refusal, decision_status)
        try:
            with tempfile.TemporaryDirectory() as home, tempfile.TemporaryDirectory() as cwd:
                scripts = pathlib.Path(home, "bin")
                scripts.mkdir()
                codex = scripts / "codex"
                if cli_success:
                    result = (
                        "printf '%s\\n' "
                        "'{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"codex-own-answer\"}}' "
                        "'{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":7,\"output_tokens\":3}}'\n"
                    )
                else:
                    result = "printf '%s\\n' 'simulated Codex failure' >&2\nexit 7\n"
                codex.write_text(
                    "#!/bin/sh\n"
                    'printf "call\\n" >> "$HOME/codex-calls"\n'
                    'cat > "$HOME/codex-task"\n'
                    + result
                )
                codex.chmod(0o755)
                pathlib.Path(home, ".revex").write_text(json.dumps({
                    "base_url": server.base,
                    "model": "deepseek-v4.1-flash-uncensored",
                    "decision_model": decision_model,
                    "route": "smart",
                    "helper_providers": helper_providers,
                }))
                env = {key: value for key, value in os.environ.items()
                       if not key.startswith("REVEX_")}
                env.update({
                    "HOME": home,
                    "XDG_STATE_HOME": str(pathlib.Path(home, ".local/state")),
                    "PATH": os.pathsep.join((str(scripts), "/usr/bin", "/bin")),
                })
                process = subprocess.run(
                    [str(BIN), prompt], cwd=cwd, env=env, text=True,
                    capture_output=True, timeout=20,
                )
                calls = pathlib.Path(home, "codex-calls")
                task = pathlib.Path(home, "codex-task")
                return process, list(server.requests), (
                    calls.read_text().splitlines() if calls.exists() else []
                ), (task.read_text() if task.exists() else None)
        finally:
            server.close()

    def test_high_fit_uses_codex_without_local_generation(self):
        process, requests, calls, task = self.run_case("fix this code")
        self.assertEqual(process.returncode, 0, process.stderr)
        self.assertEqual(process.stdout.strip(), "codex-own-answer")
        self.assertEqual([path for path, _ in requests], ["/v1/decisions"])
        self.assertEqual(requests[0][1]["model"], "clef")
        self.assertEqual(requests[0][1]["input"], "fix this code")
        self.assertEqual(calls, ["call"])
        self.assertEqual(task, "fix this code")

    def test_low_fit_and_high_refusal_stay_local(self):
        for fit, refusal in [(0.009, 0.007), (0.413, 0.956)]:
            with self.subTest(fit=fit, refusal=refusal):
                process, requests, calls, _ = self.run_case(
                    "explain this", fit=fit, refusal=refusal,
                )
                self.assertEqual(process.returncode, 0, process.stderr)
                self.assertEqual(process.stdout.strip(), "local-answer")
                self.assertEqual([path for path, _ in requests],
                                 ["/v1/decisions", "/v1/chat/completions"])
                self.assertEqual(calls, [])

    def test_decision_failure_stays_local_but_started_codex_failure_does_not_retry(self):
        unavailable, requests, calls, _ = self.run_case(
            "fix this code", decision_status=503,
        )
        self.assertEqual(unavailable.returncode, 0, unavailable.stderr)
        self.assertEqual(unavailable.stdout.strip(), "local-answer")
        self.assertEqual([path for path, _ in requests],
                         ["/v1/decisions", "/v1/chat/completions"])
        self.assertEqual(calls, [])

        failed, requests, calls, _ = self.run_case("fix this code", cli_success=False)
        self.assertNotEqual(failed.returncode, 0)
        self.assertEqual([path for path, _ in requests], ["/v1/decisions"])
        self.assertEqual(calls, ["call"])

    def test_explicit_codex_bypasses_decision_and_self_uses_chat_model(self):
        direct, requests, calls, task = self.run_case(
            "Codexを呼び出して、モデル名をきいてみて",
        )
        self.assertEqual(direct.returncode, 0, direct.stderr)
        self.assertEqual(requests, [])
        self.assertEqual(calls, ["call"])
        self.assertEqual(task, "モデル名をきいてみて")

        selected, requests, calls, _ = self.run_case(
            "fix this code", decision_model="self",
        )
        self.assertEqual(selected.returncode, 0, selected.stderr)
        self.assertEqual(requests[0][1]["model"], "deepseek-v4.1-flash-uncensored")
        self.assertEqual(calls, ["call"])

    def test_flash_model_and_disabled_codex_fallback(self):
        flash, requests, calls, _ = self.run_case(
            "fix this code", decision_model="clef-flash",
        )
        self.assertEqual(flash.returncode, 0, flash.stderr)
        self.assertEqual(requests[0][1]["model"], "clef-flash")
        self.assertEqual(calls, ["call"])

        local, requests, calls, _ = self.run_case(
            "fix this code", helper_providers="local",
        )
        self.assertEqual(local.returncode, 0, local.stderr)
        self.assertEqual(local.stdout.strip(), "local-answer")
        self.assertEqual([path for path, _ in requests], ["/v1/chat/completions"])
        self.assertEqual(calls, [])

    def test_invalid_decision_model_is_rejected_before_requests(self):
        result, requests, calls, _ = self.run_case(
            "fix this code", decision_model="unknown",
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("decision_model must be self, clef or clef-flash", result.stderr)
        self.assertEqual(requests, [])
        self.assertEqual(calls, [])


if __name__ == "__main__":
    unittest.main()
