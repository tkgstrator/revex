"""End-to-end terminal smoke checks. Run after cargo build: python3 tests/tui_pty.py.

Uses an isolated HOME, a PTY, and a local fake OpenAI-compatible SSE server.
No installed model or user settings are touched.
"""

import fcntl
import json
import os
import pathlib
import pty
import re
import select
import socket
import struct
import subprocess
import tempfile
import termios
import threading
import time
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]
BIN = pathlib.Path(os.environ.get("REVEX_BIN", ROOT / "target/debug/revex"))
if not BIN.is_absolute():
    BIN = ROOT / BIN


def settings_file(home):
    return pathlib.Path(home, ".revex", "settings.json")


def auth_file(home):
    return pathlib.Path(home, ".revex", "auth.json")


def seed_settings(home, data):
    path = settings_file(home)
    path.parent.mkdir(parents=True, exist_ok=True)
    settings = {}
    auth = {}
    for key, value in data.items():
        if key == "base_url":
            auth["baseUrl"] = value
        elif key == "api_key":
            auth["apiKey"] = value
        elif key == "helper_providers":
            settings["helperProviders"] = value.split(",")
        elif key == "reasoning_effort":
            settings["reasoningEffort"] = value
        elif key == "decision_model":
            settings["decisionModel"] = value
        elif key == "response_language":
            settings["language"] = {"en": "English", "ja": "Japanese"}[value]
        else:
            settings[key] = value
    path.write_text(json.dumps(settings))
    path.chmod(0o600)
    if auth:
        auth_path = auth_file(home)
        auth_path.write_text(json.dumps(auth))
        auth_path.chmod(0o600)
    return path


class Terminal:
    def __init__(self, cwd, home, extra=None, columns=100, args=()):
        self.columns = columns
        self.master, self.slave = pty.openpty()
        fcntl.ioctl(self.slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, columns, 0, 0))
        self.before = termios.tcgetattr(self.slave)
        env = dict(
            os.environ,
            HOME=str(home),
            XDG_STATE_HOME=str(pathlib.Path(home, ".local/state")),
            TERM="xterm-256color",
        )
        for key in (
            "REVEX_BASE_URL", "REVEX_MODEL", "REVEX_API_KEY", "REVEX_API",
            "REVEX_ROUTE", "REVEX_FRONTIER", "REVEX_EXTRA",
            "REVEX_RESPONSE_LANGUAGE",
            "REVEX_APPROVAL_MODE",
        ):
            env.pop(key, None)
        env.update(extra or {})
        self.child = subprocess.Popen(
            [str(BIN), *args], cwd=cwd, stdin=self.slave, stdout=self.slave,
            stderr=self.slave, env=env,
        )
        self.output = b""

    def send(self, keys):
        os.write(self.master, keys)

    def read_for(self, seconds):
        new = b""
        until = time.monotonic() + seconds
        while time.monotonic() < until:
            if select.select([self.master], [], [], 0.02)[0]:
                try:
                    new += os.read(self.master, 65536)
                except OSError:
                    break
        self.output += new
        return new

    def wait_for(self, marker, seconds=4):
        until = time.monotonic() + seconds
        while marker not in self.output and time.monotonic() < until:
            self.read_for(0.05)
        if marker not in self.output:
            raise AssertionError(f"missing {marker!r} in PTY output tail {self.output[-1200:]!r}")

    def screen_text(self):
        """Reconstruct visible cells; ratatui may rewrite only parts of a word."""
        width, height = self.columns, 24
        cells = [[" "] * width for _ in range(height)]
        text = self.output.decode("utf-8", errors="ignore")
        x = y = i = 0
        while i < len(text):
            ch = text[i]
            if ch == "\x1b":
                if i + 1 < len(text) and text[i + 1] == "[":
                    end = i + 2
                    while end < len(text) and not ("@" <= text[end] <= "~"):
                        end += 1
                    if end == len(text):
                        break
                    params, operation = text[i + 2:end], text[end]
                    parts = params.lstrip("?").split(";")
                    number = lambda index, default=1: int(parts[index]) if len(parts) > index and parts[index].isdigit() else default
                    if operation in "Hf":
                        y = min(height - 1, max(0, number(0) - 1))
                        x = min(width - 1, max(0, number(1) - 1))
                    elif operation == "G":
                        x = min(width - 1, max(0, number(0) - 1))
                    elif operation == "d":
                        y = min(height - 1, max(0, number(0) - 1))
                    elif operation == "C":
                        x = min(width - 1, x + number(0))
                    elif operation == "D":
                        x = max(0, x - number(0))
                    elif operation == "K":
                        mode = number(0, 0)
                        start, stop = (0, width) if mode == 2 else ((0, x + 1) if mode == 1 else (x, width))
                        cells[y][start:stop] = [" "] * (stop - start)
                    elif operation == "J" and number(0, 0) == 2:
                        cells = [[" "] * width for _ in range(height)]
                    elif params == "?1049" and operation == "h":
                        cells = [[" "] * width for _ in range(height)]
                    i = end + 1
                    continue
                if i + 1 < len(text) and text[i + 1] in "()":
                    i += 3
                else:
                    i += 2
                continue
            if ch == "\r":
                x = 0
            elif ch == "\n":
                y = min(height - 1, y + 1)
                x = 0
            elif ch >= " ":
                if x >= width:
                    x, y = 0, min(height - 1, y + 1)
                cells[y][x] = ch
                x += 1
            i += 1
        return "\n".join("".join(row) for row in cells)

    def wait_for_screen(self, marker, seconds=4):
        until = time.monotonic() + seconds
        while marker not in self.screen_text() and time.monotonic() < until:
            self.read_for(0.05)
        if marker not in self.screen_text():
            raise AssertionError(f"missing visible {marker!r}: {self.screen_text()[-500:]!r}")

    @staticmethod
    def last_cursor(frame):
        matches = re.findall(rb"\x1b\[(\d+);(\d+)H", frame)
        if not matches:
            raise AssertionError(f"no hardware cursor position in frame {frame[-600:]!r}")
        return tuple(map(int, matches[-1]))

    def cursor_visible(self):
        changes = re.findall(rb"\x1b\[\?25([hl])", self.output)
        return bool(changes) and changes[-1] == b"h"

    def wait_for_cursor(self, expected, seconds=2):
        until = time.monotonic() + seconds
        while time.monotonic() < until:
            self.read_for(0.05)
            if self.cursor_visible() and Terminal.last_cursor(self.output) == expected:
                return
        raise AssertionError(
            f"cursor did not reach {expected}: latest output {self.output[-600:]!r}"
        )

    def exit(self):
        deadline = time.monotonic() + 5
        while self.child.poll() is None and time.monotonic() < deadline:
            self.read_for(0.05)
        if self.child.poll() is None:
            raise subprocess.TimeoutExpired(self.child.args, 5)
        self.read_for(0.1)
        assert self.child.returncode == 0, self.output[-1200:]
        assert b"\x1b[?1049l" in self.output, "alternate screen not restored"
        assert termios.tcgetattr(self.slave) == self.before, "terminal mode not restored"

    def close(self):
        if self.child.poll() is None:
            self.child.terminate()
            try:
                self.child.wait(timeout=3)
            except subprocess.TimeoutExpired:
                self.child.kill()
                self.child.wait(timeout=3)
        os.close(self.master)
        os.close(self.slave)


def sse(conn, text):
    delta = json.dumps({"choices": [{"delta": {"content": text}}]})
    usage = json.dumps({"choices": [], "usage": {"prompt_tokens": 5, "completion_tokens": 2}})
    conn.sendall((
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n"
        f"data: {delta}\n\ndata: {usage}\n\ndata: [DONE]\n\n"
    ).encode())


class FakeServer:
    def __init__(self, first_reply=None, later_replies=("done",)):
        self.listener = socket.socket()
        self.listener.bind(("127.0.0.1", 0))
        self.listener.listen(8)
        self.listener.settimeout(6)
        self.base = f"http://127.0.0.1:{self.listener.getsockname()[1]}"
        self.blocked = threading.Event()
        self.release = threading.Event()
        self.finished = threading.Event()
        self.error = None
        self.requests = []
        self.first_reply = first_reply or ">> write edited.txt <<EOF\nchanged\nEOF"
        self.later_replies = later_replies
        self.thread = threading.Thread(target=self.serve, daemon=True)
        self.thread.start()

    def serve(self):
        try:
            posts = 0
            while posts < 1 + len(self.later_replies):
                conn, _ = self.listener.accept()
                conn.settimeout(4)
                with conn:
                    head = b""
                    while b"\r\n\r\n" not in head:
                        head += conn.recv(4096)
                    headers, body_read = head.split(b"\r\n\r\n", 1)
                    length = 0
                    for line in headers.split(b"\r\n"):
                        if line.lower().startswith(b"content-length:"):
                            length = int(line.split(b":", 1)[1].strip())
                    while len(body_read) < length:
                        body_read += conn.recv(4096)
                    if b"GET /models" in headers:
                        body = b'{"data":[{"id":"fake-model"}]}'
                        conn.sendall(
                            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n"
                            + f"Content-Length: {len(body)}\r\nConnection: close\r\n\r\n".encode()
                            + body
                        )
                        continue
                    assert b"POST /chat/completions" in headers
                    while len(body_read) < length:
                        body_read += conn.recv(4096)
                    self.requests.append(json.loads(body_read[:length]))
                    posts += 1
                    if posts == 1:
                        self.blocked.set()
                        assert self.release.wait(5), "first model reply was never released"
                        try:
                            sse(conn, self.first_reply)
                        except BrokenPipeError:
                            pass  # steering may abort the old model response
                    else:
                        sse(conn, self.later_replies[posts - 2])
            self.finished.set()
        except Exception as exc:
            self.error = exc

    def close(self):
        self.release.set()
        self.thread.join(timeout=7)
        self.listener.close()
        if self.error:
            raise self.error
        assert self.finished.is_set(), "fake server did not finish"


class FakeHeldResponseSteeringServer:
    """Hold the first response until the test releases it, then serve the next request."""

    def __init__(self):
        self.listener = socket.socket()
        self.listener.bind(("127.0.0.1", 0))
        self.listener.listen(8)
        self.listener.settimeout(0.05)
        self.base = f"http://127.0.0.1:{self.listener.getsockname()[1]}"
        self.first_started = threading.Event()
        self.release_first = threading.Event()
        self.first_finished = threading.Event()
        self.second_finished = threading.Event()
        self.stopping = threading.Event()
        self.requests = []
        self.first_conn = None
        self.error = None
        self.thread = threading.Thread(target=self.serve, daemon=True)
        self.thread.start()

    def serve(self):
        try:
            while len(self.requests) < 2 and not self.stopping.is_set():
                if self.first_conn is not None and self.release_first.is_set() and not self.first_finished.is_set():
                    sse(self.first_conn, "completed first response")
                    self.first_conn.close()
                    self.first_conn = None
                    self.first_finished.set()
                try:
                    conn, _ = self.listener.accept()
                except socket.timeout:
                    continue
                conn.settimeout(4)
                data = b""
                while b"\r\n\r\n" not in data:
                    data += conn.recv(4096)
                headers, body = data.split(b"\r\n\r\n", 1)
                length = 0
                for line in headers.split(b"\r\n"):
                    if line.lower().startswith(b"content-length:"):
                        length = int(line.split(b":", 1)[1].strip())
                while len(body) < length:
                    body += conn.recv(4096)
                if b"GET /models" in headers:
                    payload = b'{"data":[{"id":"fake-model"}]}'
                    conn.sendall(
                        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n"
                        + f"Content-Length: {len(payload)}\r\nConnection: close\r\n\r\n".encode()
                        + payload
                    )
                    conn.close()
                    continue
                assert b"POST /chat/completions" in headers
                self.requests.append(json.loads(body[:length]))
                if len(self.requests) == 1:
                    self.first_conn = conn
                    self.first_started.set()
                    continue  # Deliberately delay response headers until released.
                sse(conn, "steered final answer")
                conn.close()
                self.second_finished.set()
        except Exception as exc:
            self.error = exc

    def close(self):
        self.stopping.set()
        self.release_first.set()
        if self.first_conn is not None:
            self.first_conn.close()
        self.thread.join(timeout=7)
        self.listener.close()
        if self.error:
            raise self.error


class FakeHelperDispatchServer:
    """Parent SSE sequence that asks for two helpers, waits, then finishes."""

    def __init__(self, interrupt_first=False):
        self.listener = socket.socket()
        self.listener.bind(("127.0.0.1", 0))
        self.listener.listen(8)
        self.listener.settimeout(7)
        self.base = f"http://127.0.0.1:{self.listener.getsockname()[1]}"
        self.interrupt_first = interrupt_first
        self.blocked = threading.Event()
        self.release = threading.Event()
        self.finished = threading.Event()
        self.error = None
        self.thread = threading.Thread(target=self.serve, daemon=True)
        self.thread.start()

    def serve(self):
        try:
            posts = 0
            while posts < 4:
                conn, _ = self.listener.accept()
                conn.settimeout(5)
                with conn:
                    head = b""
                    while b"\r\n\r\n" not in head:
                        head += conn.recv(4096)
                    headers, body_read = head.split(b"\r\n\r\n", 1)
                    length = 0
                    for line in headers.split(b"\r\n"):
                        if line.lower().startswith(b"content-length:"):
                            length = int(line.split(b":", 1)[1].strip())
                    while len(body_read) < length:
                        body_read += conn.recv(4096)
                    if b"GET /models" in headers:
                        body = b'{"data":[{"id":"fake-parent"}]}'
                        conn.sendall(
                            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n"
                            + f"Content-Length: {len(body)}\r\nConnection: close\r\n\r\n".encode()
                            + body
                        )
                        continue
                    assert b"POST /chat/completions" in headers
                    posts += 1
                    body = json.loads(body_read[:length])
                    if self.interrupt_first:
                        if posts == 1:
                            self.blocked.set()
                            assert self.release.wait(5), "interrupted request was never released"
                            reply = "late response"
                        elif posts == 2:
                            reply = ">> spawn @codex inspect after interrupt"
                        elif posts == 3:
                            reply = ">> wait a1"
                        else:
                            last = body["messages"][-1]["content"]
                            assert "codex-helper-ok" in last, last
                            reply = "after-interrupt done"
                    elif posts == 1:
                        reply = ">> spawn inspect one"
                    elif posts == 2:
                        reply = ">> spawn inspect two"
                    elif posts == 3:
                        reply = ">> wait a1 a2"
                    else:
                        last = body["messages"][-1]["content"]
                        assert "codex-helper-ok" in last, last
                        assert "claude-helper-ok" in last, last
                        reply = "both helpers finished"
                    sse(conn, reply)
            self.finished.set()
        except Exception as exc:
            self.error = exc

    def close(self):
        self.release.set()
        self.thread.join(timeout=8)
        self.listener.close()
        if self.error:
            raise self.error
        assert self.finished.is_set(), "helper dispatch server did not finish"


class FakeSingleReplyServer:
    def __init__(self, reply_count=1, reasoning=None):
        self.listener = socket.socket()
        self.listener.bind(("127.0.0.1", 0))
        self.listener.listen(4)
        self.listener.settimeout(6)
        self.base = f"http://127.0.0.1:{self.listener.getsockname()[1]}"
        self.user_input = None
        self.reply_count = reply_count
        self.reasoning = reasoning
        self.reasoning_sent = threading.Event()
        self.release_final = threading.Event()
        self.requests = []
        self.finished = threading.Event()
        self.error = None
        self.thread = threading.Thread(target=self.serve, daemon=True)
        self.thread.start()

    def serve(self):
        try:
            while len(self.requests) < self.reply_count:
                conn, _ = self.listener.accept()
                conn.settimeout(4)
                with conn:
                    data = b""
                    while b"\r\n\r\n" not in data:
                        data += conn.recv(4096)
                    headers, body = data.split(b"\r\n\r\n", 1)
                    length = 0
                    for line in headers.split(b"\r\n"):
                        if line.lower().startswith(b"content-length:"):
                            length = int(line.split(b":", 1)[1].strip())
                    while len(body) < length:
                        body += conn.recv(4096)
                    if b"GET /models" in headers:
                        payload = b'{"data":[{"id":"fake-model"}]}'
                        conn.sendall(
                            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n"
                            + f"Content-Length: {len(payload)}\r\nConnection: close\r\n\r\n".encode()
                            + payload
                        )
                        continue
                    assert b"POST /chat/completions" in headers
                    request = json.loads(body[:length])
                    self.requests.append(request)
                    self.user_input = request["messages"][-1]["content"]
                    if self.reasoning is None:
                        sse(conn, "multiline done")
                    else:
                        delta = json.dumps({"choices": [{"delta": {"reasoning_content": self.reasoning}}]})
                        conn.sendall((
                            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n"
                            f"data: {delta}\n\n"
                        ).encode())
                        self.reasoning_sent.set()
                        assert self.release_final.wait(5), "final reply was never released"
                        final = json.dumps({"choices": [{"delta": {"content": "final-only reply"}}]})
                        conn.sendall((f"data: {final}\n\ndata: [DONE]\n\n").encode())
            self.finished.set()
        except Exception as exc:
            self.error = exc

    def close(self):
        self.release_final.set()
        self.thread.join(timeout=7)
        self.listener.close()
        if self.error:
            raise self.error
        assert self.finished.is_set(), "single reply server did not finish"


class CountingModelServer:
    """Allow startup model discovery while counting local generation requests."""

    def __init__(self):
        self.listener = socket.socket()
        self.listener.bind(("127.0.0.1", 0))
        self.listener.listen(4)
        self.listener.settimeout(0.1)
        self.base = f"http://127.0.0.1:{self.listener.getsockname()[1]}"
        self.model_lists = 0
        self.model_posts = 0
        self.error = None
        self.stopping = threading.Event()
        self.thread = threading.Thread(target=self.serve, daemon=True)
        self.thread.start()

    def serve(self):
        try:
            while not self.stopping.is_set():
                try:
                    conn, _ = self.listener.accept()
                except socket.timeout:
                    continue
                with conn:
                    conn.settimeout(2)
                    head = b""
                    while b"\r\n" not in head:
                        chunk = conn.recv(4096)
                        if not chunk:
                            break
                        head += chunk
                    request_line = head.split(b"\r\n", 1)[0]
                    if request_line.startswith(b"GET /models "):
                        self.model_lists += 1
                        payload = b'{"data":[{"id":"fake-local"}]}'
                        conn.sendall(
                            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n"
                            + f"Content-Length: {len(payload)}\r\nConnection: close\r\n\r\n".encode()
                            + payload
                        )
                    elif request_line.startswith(b"POST /chat/completions "):
                        self.model_posts += 1
                        conn.sendall(b"HTTP/1.1 500 Unexpected local model request\r\nContent-Length: 0\r\n\r\n")
                    else:
                        raise AssertionError(f"unexpected model server request: {request_line!r}")
        except Exception as exc:
            self.error = exc

    def close(self):
        self.stopping.set()
        self.thread.join(timeout=2)
        self.listener.close()
        if self.error:
            raise self.error


class TuiPtyTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        if os.name != "posix":
            raise unittest.SkipTest("PTY checks require a POSIX terminal")
        if not BIN.is_file():
            raise AssertionError(f"build Revex first: {BIN}")

    def test_screen_reconstructs_fragmented_ansi_redraw(self):
        terminal = object.__new__(Terminal)
        terminal.columns = 40
        terminal.output = (
            "\x1b[?1049h\x1b[24;1H| Thinking"
            "\x1b[24;3HTh\x1b[24;7Hking"
        ).encode()
        self.assertIn("| Thinking", terminal.screen_text().splitlines()[-1])

    def test_workspace_no_exits_without_opening_history(self):
        with tempfile.TemporaryDirectory() as home, tempfile.TemporaryDirectory() as cwd:
            terminal = Terminal(cwd, home)
            try:
                terminal.wait_for(b"workspace?")
                terminal.wait_for(str(pathlib.Path(cwd).resolve()).encode())
                terminal.send(b"n\r")
                terminal.exit()
                self.assertFalse(pathlib.Path(home, ".local/state/revex").exists())
            finally:
                terminal.close()

    def test_metadata_footer_at_eighty_columns(self):
        with tempfile.TemporaryDirectory() as home:
            terminal = Terminal(
                ROOT, home,
                {"REVEX_BASE_URL": "http://127.0.0.1:9/v1"},
                columns=80,
            )
            try:
                terminal.wait_for(b"workspace?")
                terminal.send(b"\r")
                terminal.wait_for_screen("> Ask Revex to do anything")
                footer = terminal.screen_text().splitlines()[-1]
                self.assertIn("server model", footer)
                self.assertIn("/revex", footer)
                self.assertNotIn("Revex v", footer)
                self.assertNotIn("0 agents", footer)
                self.assertNotIn(b"server default", terminal.output)
                self.assertNotIn(b"0 queued", terminal.output)
                terminal.send(b"\x04")
                terminal.exit()
            finally:
                terminal.close()

    def test_empty_chat_uses_composer_hint_without_stale_transcript_line(self):
        with tempfile.TemporaryDirectory() as home, tempfile.TemporaryDirectory() as cwd:
            terminal = Terminal(cwd, home, {"REVEX_BASE_URL": "http://127.0.0.1:9/v1"})
            try:
                terminal.wait_for(b"workspace?")
                terminal.send(b"\r")
                terminal.wait_for_screen("> Ask Revex to do anything")
                self.assertNotIn("Ready. Enter a task below.", terminal.screen_text())
                terminal.send(b"hello\r")
                terminal.wait_for_screen("> hello")
                self.assertNotIn("Ready. Enter a task below.", terminal.screen_text())
                terminal.send(b"\x04")
                terminal.exit()
            finally:
                terminal.close()

    def test_model_and_reasoning_save_to_existing_project_settings(self):
        with tempfile.TemporaryDirectory() as home, tempfile.TemporaryDirectory() as cwd:
            project = pathlib.Path(cwd, ".revex")
            project.write_text('{"model":"old-model"}')
            terminal = Terminal(cwd, home, {"REVEX_BASE_URL": "http://127.0.0.1:9/v1"})
            try:
                terminal.wait_for(b"workspace?")
                terminal.send(b"\r")
                terminal.wait_for_screen("> Ask Revex to do anything")
                terminal.send(b"/model new-model\r")
                terminal.wait_for(b"Reasoning")
                terminal.send(b"\r\x1b[B\x1b[B\x1b[B\r")  # Edit Reasoning, choose High.
                terminal.send(b"\x1b[B\r\x1b[B\r")  # Edit Decision model, choose clef-flash.
                terminal.send(b"\x1b[B\r\x1b[B\r\x13")  # Edit Language, choose Japanese, save.
                until = time.monotonic() + 3
                while time.monotonic() < until:
                    saved = json.loads(project.read_text())
                    if saved.get("model") == "new-model":
                        break
                    terminal.read_for(0.05)
                self.assertEqual(saved["model"], "new-model")
                self.assertEqual(saved["reasoning_effort"], "high")
                self.assertEqual(saved["decision_model"], "clef-flash")
                self.assertEqual(saved["response_language"], "ja")
                until = time.monotonic() + 4
                while time.monotonic() < until:
                    screen = terminal.screen_text()
                    if ("> Ask Revex to do anything" in screen
                            and "new-model" in screen.splitlines()[-1]
                            and "Reasoning:" not in screen):
                        break
                    terminal.read_for(0.05)
                else:
                    self.fail(f"settings did not return to chat: {terminal.screen_text()[-500:]!r}")
                terminal.send(b"exit\r")
                terminal.exit()
            finally:
                terminal.close()

    def test_config_model_tab_edits_one_item_and_saves_directly(self):
        with tempfile.TemporaryDirectory() as home, tempfile.TemporaryDirectory() as cwd:
            seed_settings(home, {
                "base_url": "http://127.0.0.1:9/v1",
                "model": "saved-model",
                "reasoning_effort": "low",
                "decision_model": "clef",
                "response_language": "en",
                "helper_providers": "local",
            })
            terminal = Terminal(cwd, home)
            try:
                terminal.wait_for(b"workspace?")
                terminal.send(b"\r")
                terminal.wait_for_screen("> Ask Revex to do anything")
                terminal.send(b"/config\r")
                terminal.wait_for_screen("Provider:")
                terminal.send(b"\t\r")  # Model tab, browse Model item.
                terminal.wait_for_screen("Model: saved-model")
                terminal.send(b"\x1b[B\r\x1b[B\r")  # Reasoning item, edit, choose medium, done.
                terminal.wait_for_screen("Reasoning: medium")
                terminal.send(b"\x13")  # Ctrl+S saves from Model tab.
                terminal.wait_for_screen("> Ask Revex to do anything")
                saved = json.loads(settings_file(home).read_text())
                self.assertEqual(saved["model"], "saved-model")
                self.assertEqual(saved["reasoningEffort"], "medium")
                self.assertEqual(saved["decisionModel"], "clef")
                self.assertEqual(saved["language"], "English")
                terminal.send(b"exit\r")
                terminal.exit()
            finally:
                terminal.close()

    def test_config_approval_mode_and_runtime_command_save_to_home(self):
        with tempfile.TemporaryDirectory() as home, tempfile.TemporaryDirectory() as cwd:
            seed_settings(home, {
                "base_url": "http://127.0.0.1:9/v1",
                "model": "fake-model",
                "helper_providers": "local",
            })
            terminal = Terminal(cwd, home)
            try:
                terminal.wait_for(b"workspace?")
                terminal.send(b"\r")
                terminal.wait_for_screen("> Ask Revex to do anything")
                terminal.send(b"/config\r")
                terminal.wait_for_screen("Provider:")
                terminal.send(b"\t\t\r\x1b[B\r\x1b[B\r\x13")
                terminal.wait_for_screen("> Ask Revex to do anything")
                self.assertEqual(json.loads(settings_file(home).read_text())["approvalMode"], "auto")
                project = pathlib.Path(cwd, ".revex")
                project.write_text('{"model":"project-only"}')
                terminal.send(b"/approval bypass\r")
                terminal.wait_for_screen("Approval mode: bypass")
                self.assertEqual(json.loads(settings_file(home).read_text())["approvalMode"], "bypass")
                self.assertEqual(json.loads(project.read_text()), {"model": "project-only"})
                terminal.send(b"/approval manual\r")
                terminal.wait_for_screen("Approval mode: manual")
                self.assertEqual(json.loads(settings_file(home).read_text())["approvalMode"], "manual")
                for mode in ("auto", "bypass", "plan", "manual"):
                    terminal.send(b"\x1b[Z")  # Shift+Tab cycles chat approval modes.
                    terminal.wait_for_screen(f"Mode: {mode}")
                    self.assertEqual(json.loads(settings_file(home).read_text())["approvalMode"], mode)
                terminal.send(b"\x04")
                terminal.exit()
            finally:
                terminal.close()

    def test_config_unrelated_save_does_not_persist_yes_override(self):
        with tempfile.TemporaryDirectory() as home, tempfile.TemporaryDirectory() as cwd:
            seed_settings(home, {
                "base_url": "http://127.0.0.1:9/v1",
                "model": "fake-model",
                "helper_providers": "local",
                "approvalMode": "manual",
            })
            terminal = Terminal(cwd, home, args=("--yes",))
            try:
                terminal.wait_for(b"workspace?")
                terminal.send(b"\r")
                terminal.wait_for_screen("Mode: bypass")
                terminal.send(b"/config\r")
                terminal.wait_for_screen("Provider:")
                terminal.send(b"\x13")
                terminal.wait_for_screen("Mode: bypass")
                self.assertEqual(json.loads(settings_file(home).read_text())["approvalMode"], "manual")
                terminal.send(b"/config\r")
                terminal.wait_for_screen("Provider:")
                terminal.send(b"\t\t\r\x1b[B\r\x1b[B\r\x13")
                terminal.wait_for_screen("Mode: plan")
                self.assertEqual(json.loads(settings_file(home).read_text())["approvalMode"], "plan")
                terminal.send(b"\x04")
                terminal.exit()
            finally:
                terminal.close()

    def test_slash_suggestions_complete_without_submitting(self):
        with tempfile.TemporaryDirectory() as home, tempfile.TemporaryDirectory() as cwd:
            seed_settings(home, {
                "base_url": "http://127.0.0.1:9/v1",
                "model": "fake-model",
                "helper_providers": "local",
            })
            terminal = Terminal(cwd, home, columns=52)
            try:
                terminal.wait_for(b"workspace?")
                terminal.send(b"\r")
                terminal.wait_for_screen("> Ask Revex to do anything")
                terminal.send(b"/")
                terminal.wait_for_screen("/agents")
                self.assertIn("/help", terminal.screen_text())
                self.assertNotIn("/quit", terminal.screen_text())
                terminal.send(b"\x1b[B\r")  # Down, then Enter completes /agents.
                terminal.wait_for_screen("> /agents")
                self.assertNotIn(" Agents ", terminal.screen_text())
                terminal.send(b"\r")  # A second Enter runs it.
                terminal.wait_for_screen(" Agents ")
                terminal.send(b"\x1b")  # Close overlay before typing a new command.
                terminal.wait_for_screen("> Ask Revex to do anything")
                terminal.send(b"/")
                terminal.wait_for_screen("/agents")
                terminal.send(b"\x1b[Z")  # Chat Shift+Tab still cycles mode.
                terminal.wait_for_screen("Mode: auto")
                self.assertIn("/agents", terminal.screen_text())
                terminal.send(b"mo")
                terminal.wait_for_screen("> /mo")
                self.assertIn("/model", terminal.screen_text())
                self.assertNotIn("/agents", terminal.screen_text())
                terminal.send(b"\t")
                terminal.wait_for_screen("> /model")
                terminal.send(b"x")
                terminal.wait_for_screen("> /model x")
                terminal.send(b"\x04")
                terminal.exit()
            finally:
                terminal.close()

    def test_saved_japanese_reaches_next_request_after_english_history(self):
        with tempfile.TemporaryDirectory() as home, tempfile.TemporaryDirectory() as cwd:
            server = FakeSingleReplyServer(reply_count=2)
            seed_settings(home, {
                "base_url": server.base,
                "model": "fake-model",
                "response_language": "en",
                "helper_providers": "local",
            })
            terminal = Terminal(cwd, home)
            try:
                terminal.wait_for(b"workspace?")
                terminal.send(b"\r")
                terminal.wait_for_screen("> Ask Revex to do anything")
                terminal.send(b"hello\r")
                terminal.wait_for_screen("multiline done")
                terminal.send(b"/config\r")
                terminal.wait_for_screen("Provider:")
                terminal.send(b"\t\r\x1b[B\x1b[B\x1b[B\r\x1b[B\r\x13")
                terminal.wait_for_screen("> Ask Revex to do anything")
                self.assertEqual(json.loads(settings_file(home).read_text())["language"], "Japanese")
                terminal.send("続きを\r".encode())
                self.assertTrue(server.finished.wait(5), "second model request was not sent")
                first, second = server.requests
                self.assertEqual(first["messages"][-1]["content"].splitlines()[0], "hello")
                self.assertEqual(first["messages"][-1]["role"], "user")
                self.assertEqual(second["messages"][2]["content"], "multiline done")
                self.assertTrue(second["messages"][0]["content"].startswith(
                    "Response language for this request: Japanese."))
                self.assertIn("[Revex response language: Japanese]",
                              second["messages"][-1]["content"])
                self.assertTrue(second["messages"][-1]["content"].startswith("続きを"))
                until = time.monotonic() + 4
                while time.monotonic() < until:
                    terminal.read_for(0.05)
                    screen = terminal.screen_text()
                    if screen.count("multiline done") >= 2 and "> Ask Revex to do anything" in screen:
                        break
                else:
                    self.fail("second reply did not return to chat")
                terminal.send(b"\x04")
                terminal.exit()
            finally:
                terminal.close()
                server.close()

    def test_provider_reasoning_is_transient_and_separate_from_answer(self):
        with tempfile.TemporaryDirectory() as home, tempfile.TemporaryDirectory() as cwd:
            server = FakeSingleReplyServer(reasoning="Inspecting the request.\nChecking details.")
            seed_settings(home, {
                "base_url": server.base,
                "model": "fake-model",
                "helper_providers": "local",
            })
            terminal = Terminal(cwd, home)
            try:
                terminal.wait_for(b"workspace?")
                terminal.send(b"\r")
                terminal.wait_for_screen("> Ask Revex to do anything")
                terminal.send(b"show reasoning\r")
                self.assertTrue(server.reasoning_sent.wait(4), "reasoning SSE was not emitted")
                terminal.wait_for_screen("Thinking")
                terminal.wait_for_screen("Inspecting the request")
                self.assertNotIn("final-only reply", terminal.screen_text())
                server.release_final.set()
                terminal.wait_for_screen("final-only reply")
                self.assertNotIn("Inspecting the request", terminal.screen_text())
                state = pathlib.Path(home, ".local/state/revex")
                for path in state.rglob("*.json"):
                    self.assertNotIn("Inspecting the request", path.read_text())
                terminal.send(b"\x04")
                terminal.exit()
            finally:
                terminal.close()
                server.close()

    def test_setup_tabs_keep_provider_uncommitted_until_enter(self):
        with tempfile.TemporaryDirectory() as home, tempfile.TemporaryDirectory() as cwd:
            terminal = Terminal(cwd, home)
            settings = settings_file(home)
            try:
                terminal.wait_for(b"workspace?")
                terminal.send(b"\r")
                terminal.wait_for_screen("Provider: Ollama (suggested)")
                self.assertNotIn("[Ollama]", terminal.screen_text())
                self.assertFalse(settings.exists())
                terminal.send(b"\x1b[C")  # Tab bar: Server -> Model.
                terminal.wait_for_screen("Decision model: clef")
                terminal.send(b"\t")  # Model -> Helpers.
                terminal.wait_for_screen("HELPER PROVIDERS")
                terminal.send(b"\x1b[Z")  # Shift+Tab -> Model.
                terminal.wait_for_screen("Decision model: clef")
                terminal.send(b"\x1b[Z")  # Model -> Server.
                terminal.wait_for_screen("Provider: Ollama (suggested)")
                terminal.send(b"\r\r\x1b[B")  # Browse, then edit Provider; candidate LM Studio.
                terminal.wait_for_screen("Provider: LM Studio (suggested)")
                self.assertIn("URL: http://127.0.0.1:11434/v1", terminal.screen_text())
                self.assertFalse(settings.exists())
                terminal.send(b"\r")  # Commit LM Studio to draft only.
                terminal.wait_for_screen("URL: http://127.0.0.1:1234/v1")
                self.assertFalse(settings.exists())
                terminal.send(b"\x1b[B\rx\t")  # Edit URL, then leave Server.
                terminal.wait_for_screen("Decision model: clef")
                terminal.send(b"\x1b[Z\r")  # Return to Server URL item.
                terminal.wait_for_screen("URL: http://127.0.0.1:1234/v1x")
                terminal.send(b"\x1b")
                terminal.read_for(0.15)
                terminal.send(b"\x1b")
                terminal.exit()
                self.assertFalse(settings.exists())
            finally:
                terminal.close()

    def test_setup_and_chat_hardware_cursor_follows_editing(self):
        with tempfile.TemporaryDirectory() as home:
            terminal = Terminal(ROOT, home)
            try:
                terminal.wait_for(b"workspace?")
                terminal.send(b"\r")
                terminal.wait_for(b"Provider:")
                terminal.send(b"\r\r\r\x1b[B\r")  # Browse Provider, commit it, edit URL.
                url_frame = terminal.read_for(0.15)
                self.assertTrue(terminal.cursor_visible())
                url_row, url_x = Terminal.last_cursor(url_frame)
                terminal.send("界".encode())
                unicode_frame = terminal.read_for(0.15)
                self.assertEqual(Terminal.last_cursor(unicode_frame), (url_row, url_x + 2))
                terminal.send(b"\x1b[D")
                left_frame = terminal.read_for(0.15)
                self.assertEqual(Terminal.last_cursor(left_frame), (url_row, url_x))
                terminal.send(b"\r\x1b[B\r")  # Finish URL, browse API key, edit it.
                key_frame = terminal.read_for(0.15)
                self.assertTrue(terminal.cursor_visible())
                key_row, key_x = Terminal.last_cursor(key_frame)
                self.assertNotEqual(key_row, url_row)
                terminal.send(b"secret")
                secret_frame = terminal.read_for(0.15)
                self.assertEqual(Terminal.last_cursor(secret_frame), (key_row, key_x + 6))
                self.assertNotIn(b"secret", secret_frame)
                terminal.send(b"\x1b")
                terminal.read_for(0.15)
                terminal.send(b"\x1b")
                terminal.read_for(0.15)
                terminal.send(b"\x1b")
                terminal.exit()
            finally:
                terminal.close()

            configured = Terminal(ROOT, home, {"REVEX_BASE_URL": "http://127.0.0.1:9/v1", "REVEX_MODEL": "fake"})
            try:
                configured.wait_for_screen("> Ask Revex to do anything")
                self.assertNotIn(b"workspace?", configured.output)
                configured.read_for(0.15)
                self.assertTrue(configured.cursor_visible())
                row, x = Terminal.last_cursor(configured.output)
                configured.send(b"x")
                configured.wait_for_cursor((row, x + 1))
                configured.send(b"\x04")  # Ctrl+D exits even with editor text.
                configured.exit()
            finally:
                configured.close()

    def test_first_run_then_returning_chat_and_terminal_restore(self):
        with tempfile.TemporaryDirectory() as home:
            first = Terminal(ROOT, home)
            try:
                first.wait_for(b"workspace?")
                first.wait_for(str(ROOT).encode())
                first.send(b"\r")
                first.wait_for(b"Provider:")
                first.send(b"\r\r\r\t\r\r")  # Browse and commit Provider, open Model ID editor.
                first.read_for(0.1)
                first.send(b"fake-model\r\x13")  # Finish Model ID and save from Model tab.
                first.wait_for_screen("> Ask Revex to do anything")
                self.assertTrue(settings_file(home).is_file())
                self.assertEqual(json.loads(settings_file(home).read_text())["decisionModel"], "clef")
                self.assertEqual(json.loads(settings_file(home).read_text())["language"], "English")
                first.send(b"/exit\r")
                first.exit()
            finally:
                first.close()

            returning = Terminal(ROOT, home)
            try:
                returning.wait_for_screen("> Ask Revex to do anything")
                self.assertNotIn(b"workspace?", returning.output)
                self.assertNotIn(b"Provider:", returning.output)
                returning.send(b"/status\r")
                returning.wait_for(b"Session:")
                returning.send(b"\x1b")
                returning.read_for(0.1)
                returning.send(b"exit\r")
                returning.exit()
            finally:
                returning.close()

            with tempfile.TemporaryDirectory() as another_cwd:
                fresh_workspace = Terminal(another_cwd, home)
                try:
                    fresh_workspace.wait_for(b"workspace?")
                    fresh_workspace.send(b"n\r")
                    fresh_workspace.exit()
                finally:
                    fresh_workspace.close()

    def test_prompt_history_recalls_draft_across_restart_and_clear(self):
        with tempfile.TemporaryDirectory() as home, tempfile.TemporaryDirectory() as cwd:
            options = {"REVEX_BASE_URL": "http://127.0.0.1:9/v1"}
            first = Terminal(cwd, home, options)
            try:
                first.wait_for(b"workspace?")
                first.send(b"\r")
                first.wait_for_screen("> Ask Revex to do anything")
                first.send(b"/help\r")
                first.wait_for(b"/model")
                first.send(b"/route\r")
                first.wait_for(b"Route:")
                first.send(b"draft")
                first.read_for(0.1)
                first.send(b"\x1b[A")
                self.assertEqual(Terminal.last_cursor(first.read_for(0.15))[1], 9)
                first.send(b"\x1b[A")
                self.assertEqual(Terminal.last_cursor(first.read_for(0.15))[1], 8)
                first.send(b"\x1b[B")
                self.assertEqual(Terminal.last_cursor(first.read_for(0.15))[1], 9)
                first.send(b"\x1b[B")
                self.assertEqual(Terminal.last_cursor(first.read_for(0.15))[1], 8)
                first.send(b"\x04")
                first.exit()
            finally:
                first.close()

            returning = Terminal(cwd, home, options)
            try:
                returning.wait_for_screen("> Ask Revex to do anything")
                self.assertNotIn(b"workspace?", returning.output)
                returning.send(b"\x1b[A")
                self.assertEqual(Terminal.last_cursor(returning.read_for(0.15))[1], 9)
                returning.send(b"\x1b")  # Clear the editor, then clear the session.
                returning.read_for(0.15)
                returning.send(b"/clear\r")
                returning.wait_for_screen("Session history and agent records cleared.")
                saved = list(pathlib.Path(home, ".local/state/revex").glob("input-*.json"))
                self.assertEqual(len(saved), 1)
                self.assertEqual(json.loads(saved[0].read_text())["entries"], ["/help", "/route", "/clear"])
                returning.send(b"\x04")
                returning.exit()
            finally:
                returning.close()

    def test_env_key_refuses_endpoint_change_before_restart(self):
        with tempfile.TemporaryDirectory() as home, tempfile.TemporaryDirectory() as cwd:
            settings = settings_file(home)
            original = {"model": "fake"}
            seed_settings(home, original)
            auth_file(home).write_text(json.dumps({"baseUrl": "http://127.0.0.1:9/v1"}))
            terminal = Terminal(cwd, home, {"REVEX_API_KEY": "test-secret"})
            try:
                terminal.wait_for(b"workspace?")
                terminal.send(b"\r")
                terminal.wait_for_screen("> Ask Revex to do anything")
                terminal.send(b"/config\r")
                terminal.wait_for(b"Provider:")
                terminal.send(b"\r\x1b[B\r")  # Browse URL and edit it.
                terminal.send(b"x\r\x13")  # Change URL, then try direct save.
                terminal.wait_for(b"REVEX_API_KEY")
                self.assertEqual(json.loads(settings.read_text()), original)
                self.assertNotIn(b"test-secret", terminal.output)
                terminal.send(b"\x1b")
                terminal.read_for(0.15)
                terminal.send(b"\x1b")
                terminal.read_for(0.15)
                terminal.send(b"exit\r")
                terminal.exit()
            finally:
                terminal.close()

    def test_blank_key_keeps_saved_key_and_ctrl_k_explicitly_clears_it(self):
        with tempfile.TemporaryDirectory() as home, tempfile.TemporaryDirectory() as cwd:
            settings = seed_settings(home, {
                "base_url": "http://127.0.0.1:9/v1",
                "model": "fake",
                "api_key": "saved-test-secret",
            })
            terminal = Terminal(cwd, home)
            try:
                terminal.wait_for(b"workspace?")
                terminal.send(b"\r")
                terminal.wait_for_screen("> Ask Revex to do anything")
                terminal.send(b"/config\r")
                terminal.wait_for_screen("Provider: Custom API")
                terminal.send(b"\x13")  # Save unchanged settings with blank key.
                terminal.wait_for_screen("> Ask Revex to do anything")
                self.assertEqual(json.loads(auth_file(home).read_text())["apiKey"], "saved-test-secret")
                terminal.send(b"/config\r")
                terminal.wait_for_screen("Provider: Custom API")
                terminal.send(b"\r\x1b[B\x1b[B\r")  # Browse API key and edit it.
                terminal.wait_for_screen("Blank keeps saved key; Ctrl+K clears it")
                terminal.send(b"\x0b")  # Ctrl+K explicitly clears saved API key.
                terminal.wait_for_screen("API key: (clear on Save)")
                terminal.send(b"\x13")
                terminal.wait_for_screen("> Ask Revex to do anything")
                self.assertNotIn("apiKey", json.loads(auth_file(home).read_text()))
                backups = list(settings.parent.glob("auth.json.bak-*"))
                self.assertGreaterEqual(len(backups), 1)
                self.assertTrue(any(json.loads(path.read_text()).get("apiKey") == "saved-test-secret" for path in backups))
                self.assertEqual(auth_file(home).stat().st_mode & 0o777, 0o600)
                self.assertEqual(settings.parent.stat().st_mode & 0o777, 0o700)
                self.assertNotIn(b"saved-test-secret", terminal.output)
                terminal.send(b"exit\r")
                terminal.exit()
            finally:
                terminal.close()

    def test_helper_multiselect_saves_home_only_and_survives_config(self):
        with tempfile.TemporaryDirectory() as home, tempfile.TemporaryDirectory() as cwd:
            first = Terminal(cwd, home)
            try:
                first.wait_for(b"workspace?")
                first.send(b"\r")
                first.wait_for(b"Provider:")
                first.send(b"\r\r\r\t\t\r\r")  # Commit Provider, switch to Helpers, edit choices.
                first.wait_for_screen("Space toggle")
                first.wait_for(b"Codex")
                first.wait_for(b"Claude")
                first.send(b" \x1b[B \x1b[B \r\x13")  # Local off, Codex + Claude on, save.
                first.wait_for_screen("> Ask Revex to do anything")
                settings = json.loads(settings_file(home).read_text())
                self.assertEqual(settings["helperProviders"], ["codex", "claude"])
                self.assertFalse(pathlib.Path(cwd, ".revex").exists())
                first.send(b"exit\r")
                first.exit()
            finally:
                first.close()

            project_settings = pathlib.Path(cwd, ".revex")
            project_settings.write_text('{"helper_providers":"local"}')
            returning = Terminal(cwd, home)
            try:
                returning.wait_for_screen("> Ask Revex to do anything")
                returning.send(b"/config\r")
                returning.wait_for(b"Provider:")
                returning.output = b""
                returning.send(b"\t\t\x13")  # Helpers tab, then save unchanged selection.
                returning.wait_for_screen("> Ask Revex to do anything")
                settings = json.loads(settings_file(home).read_text())
                self.assertEqual(settings["helperProviders"], ["codex", "claude"])
                self.assertNotIn("helper_providers", json.loads(project_settings.read_text()))
                returning.send(b"exit\r")
                returning.exit()
            finally:
                returning.close()

    def test_simple_model_identity_uses_model_reply_without_canned_text(self):
        with tempfile.TemporaryDirectory() as home, tempfile.TemporaryDirectory() as cwd:
            server = FakeSingleReplyServer()
            terminal = Terminal(cwd, home, {
                "REVEX_BASE_URL": server.base,
                "REVEX_MODEL": "selected-alias",
                "REVEX_REASONING_EFFORT": "high",
                "REVEX_API_KEY": "test-secret",
            })
            try:
                terminal.wait_for(b"workspace?")
                terminal.send(b"\r")
                terminal.wait_for_screen("> Ask Revex to do anything")
                terminal.send("あなたのモデルは？\r".encode())
                self.assertTrue(server.finished.wait(4), "identity question did not reach the model")
                self.assertEqual(server.user_input.split("\n\n[Revex response language:")[0], "あなたのモデルは？")
                self.assertIn("[Revex response language: English]", server.user_input)
                terminal.wait_for_screen("multiline done")
                messages = json.loads(next(pathlib.Path(home, ".local/state/revex").glob("history-*.json")).read_text())["messages"]
                self.assertEqual(messages[-1]["content"], "multiline done")
                terminal.send(b"\x04")
                terminal.exit()
            finally:
                terminal.close()
                server.close()

    def test_explicit_codex_invocation_calls_cli_once_without_local_model_request(self):
        with tempfile.TemporaryDirectory() as home, tempfile.TemporaryDirectory() as cwd:
            server = CountingModelServer()
            scripts = pathlib.Path(home, "bin")
            scripts.mkdir()
            codex = scripts / "codex"
            codex.write_text(
                "#!/bin/sh\n"
                'printf "call\\n" >> "$HOME/codex-calls"\n'
                'cat > "$HOME/codex-task"\n'
                "printf '%s\\n' "
                "'{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"codex-direct-ok\"}}' "
                "'{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":7,\"output_tokens\":3}}'\n"
            )
            codex.chmod(0o755)
            seed_settings(home, {
                "base_url": server.base,
                "model": "fake-local",
                "helper_providers": "codex",
                "route": "auto",
                "frontier": "missing-frontier-command",
            })
            terminal = Terminal(cwd, home, {"PATH": str(scripts) + os.pathsep + os.environ.get("PATH", "")})
            try:
                terminal.wait_for(b"workspace?")
                terminal.send(b"\r")
                terminal.wait_for_screen("> Ask Revex to do anything")
                prompt = "Codexを呼び出して、モデル名をきいてみて"
                terminal.send((prompt + "\r").encode())
                terminal.wait_for_screen("codex-direct-ok", seconds=8)
                self.assertEqual(pathlib.Path(home, "codex-calls").read_text().splitlines(), ["call"])
                self.assertEqual(pathlib.Path(home, "codex-task").read_text(), "モデル名をきいてみて")
                self.assertEqual(server.model_posts, 0)
                state = pathlib.Path(home, ".local/state/revex")
                messages = json.loads(next(state.glob("history-*.json")).read_text())["messages"]
                self.assertEqual(messages[-2]["content"], prompt)
                self.assertEqual(messages[-1]["content"], "codex-direct-ok")
                record_paths = list(state.glob("agents-history-*/a*.json"))
                records = [json.loads(path.read_text()) for path in record_paths]
                self.assertEqual(len(records), 1, [str(path.relative_to(state)) for path in record_paths])
                self.assertEqual(records[0]["provider"], "codex")
                self.assertEqual(records[0]["status"], "done")
                terminal.send(b"exit\r")
                terminal.exit()
            finally:
                terminal.close()
                server.close()

    def test_alt_enter_keeps_multiline_prompt_in_one_model_turn(self):
        with tempfile.TemporaryDirectory() as home, tempfile.TemporaryDirectory() as cwd:
            server = FakeSingleReplyServer()
            terminal = Terminal(cwd, home, {
                "REVEX_BASE_URL": server.base,
                "REVEX_MODEL": "fake-model",
            })
            try:
                terminal.wait_for(b"workspace?")
                terminal.send(b"\r")
                terminal.wait_for_screen("> Ask Revex to do anything")
                terminal.send(b"first\x1b\rsecond")  # Alt+Enter inserts one newline.
                terminal.read_for(0.2)
                terminal.send(b"\r")  # Plain Enter submits.
                self.assertTrue(server.finished.wait(4), "multiline request was not sent")
                self.assertEqual(server.user_input.split("\n\n[Revex response language:")[0], "first\nsecond")
                self.assertIn("[Revex response language: English]", server.user_input)
                terminal.wait_for(b"multiline")
                terminal.wait_for_screen("tok/s")
                footer = terminal.screen_text().splitlines()[-1].strip()
                self.assertRegex(footer, r"[0-9]+\.[0-9] tok/s \| [0-9]+\.[0-9]s \| -")
                terminal.send(b"exit\r")
                terminal.exit()
            finally:
                terminal.close()
                server.close()

    def test_shift_enter_csi_u_keeps_multiline_prompt_and_restores_mode(self):
        with tempfile.TemporaryDirectory() as home, tempfile.TemporaryDirectory() as cwd:
            server = FakeSingleReplyServer()
            terminal = Terminal(cwd, home, {
                "REVEX_BASE_URL": server.base,
                "REVEX_MODEL": "fake-model",
            })
            try:
                terminal.wait_for(b"workspace?")
                terminal.wait_for(b"\x1b[>9u")  # Request CSI-u for all keys, including Enter.
                terminal.send(b"\r")
                terminal.wait_for_screen("> Ask Revex to do anything")
                self.assertIn("-- INPUT", terminal.screen_text())
                self.assertNotIn("Ctrl+J newline", terminal.screen_text())
                terminal.send(b"first\x1b[13;2usecond")  # CSI-u Shift+Enter.
                terminal.read_for(0.2)
                self.assertFalse(server.finished.is_set(), "Shift+Enter submitted early")
                terminal.send(b"\x1b[13u")  # Unmodified Enter still submits in enhanced mode.
                self.assertTrue(server.finished.wait(4), "multiline request was not sent")
                self.assertEqual(server.user_input.split("\n\n[Revex response language:")[0], "first\nsecond")
                self.assertIn("[Revex response language: English]", server.user_input)
                terminal.wait_for(b"multiline")
                terminal.send(b"exit\r")
                terminal.exit()
                self.assertIn(b"\x1b[<1u", terminal.output)
            finally:
                terminal.close()
                server.close()

    def test_ctrl_j_inserts_newline_before_plain_enter_submits(self):
        with tempfile.TemporaryDirectory() as home, tempfile.TemporaryDirectory() as cwd:
            server = FakeSingleReplyServer()
            terminal = Terminal(cwd, home, {
                "REVEX_BASE_URL": server.base,
                "REVEX_MODEL": "fake-model",
            })
            try:
                terminal.wait_for(b"workspace?")
                terminal.send(b"\r")
                terminal.wait_for_screen("> Ask Revex to do anything")
                terminal.send(b"first\x0asecond")  # Ctrl+J emits LF in raw mode.
                terminal.read_for(0.2)
                self.assertFalse(server.finished.is_set(), "Ctrl+J submitted early")
                terminal.send(b"\r")
                self.assertTrue(server.finished.wait(4), "multiline request was not sent")
                self.assertEqual(server.user_input.split("\n\n[Revex response language:")[0], "first\nsecond")
                self.assertIn("[Revex response language: English]", server.user_input)
                terminal.wait_for(b"multiline")
                terminal.send(b"exit\r")
                terminal.exit()
            finally:
                terminal.close()
                server.close()

    def test_selected_codex_and_claude_helpers_both_dispatch_via_fake_clis(self):
        with tempfile.TemporaryDirectory() as home, tempfile.TemporaryDirectory() as cwd:
            server = FakeHelperDispatchServer()
            scripts = pathlib.Path(home, "bin")
            scripts.mkdir()
            for name in ("codex", "claude"):
                script = scripts / name
                if name == "codex":
                    response = (
                        "printf '%s\\n' "
                        "'{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"codex-helper-ok\"}}' "
                        "'{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":7,\"output_tokens\":3}}'\n"
                    )
                else:
                    response = (
                        "printf '%s\\n' "
                        "'{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"claude-helper-ok\","
                        "\"usage\":{\"input_tokens\":5,\"output_tokens\":2},"
                        "\"total_cost_usd\":0.125}'\n"
                    )
                script.write_text(
                    "#!/bin/sh\n"
                    f'printf "%s\\n" "$*" > "$HOME/{name}-args"\n'
                    + response
                )
                script.chmod(0o755)
            seed_settings(home, {
                "base_url": server.base,
                "model": "fake-parent",
                "helper_providers": "codex,claude",
            })
            terminal = Terminal(cwd, home, {"PATH": str(scripts) + os.pathsep + os.environ.get("PATH", "")})
            try:
                terminal.wait_for(b"workspace?")
                terminal.send(b"\r")
                terminal.wait_for_screen("> Ask Revex to do anything")
                terminal.send(b"use both helpers\r")
                terminal.wait_for(b"finished", seconds=12)
                codex_args = pathlib.Path(home, "codex-args").read_text()
                claude_args = pathlib.Path(home, "claude-args").read_text()
                self.assertIn("--dangerously-bypass-approvals-and-sandbox", codex_args)
                self.assertIn("--skip-git-repo-check", codex_args)
                self.assertIn("--dangerously-skip-permissions", claude_args)
                records = [json.loads(path.read_text()) for path in
                           pathlib.Path(home, ".local/state/revex").rglob("a*.json")]
                self.assertEqual(
                    next(record["cost_usd"] for record in records if record.get("provider") == "claude"),
                    0.125,
                )
                self.assertIsNone(
                    next(record["cost_usd"] for record in records if record.get("provider") == "codex")
                )
                terminal.send(b"/agents\r")
                terminal.wait_for(b"codex")
                terminal.wait_for(b"claude")
                terminal.send(b"\x1b")
                terminal.read_for(0.15)
                terminal.send(b"exit\r")
                terminal.exit()
            finally:
                terminal.close()
                server.close()

    def test_interrupt_keeps_selected_codex_helper_enabled(self):
        with tempfile.TemporaryDirectory() as home, tempfile.TemporaryDirectory() as cwd:
            server = FakeHelperDispatchServer(interrupt_first=True)
            scripts = pathlib.Path(home, "bin")
            scripts.mkdir()
            codex = scripts / "codex"
            codex.write_text(
                "#!/bin/sh\n"
                "printf '%s\\n' '{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"codex-helper-ok\"}}' "
                "'{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":7,\"output_tokens\":3}}'\n"
            )
            codex.chmod(0o755)
            seed_settings(home, {
                "base_url": server.base,
                "model": "fake-parent",
                "helper_providers": "codex",
            })
            terminal = Terminal(cwd, home, {"PATH": str(scripts) + os.pathsep + os.environ.get("PATH", "")})
            try:
                terminal.wait_for(b"workspace?")
                terminal.send(b"\r")
                terminal.wait_for_screen("> Ask Revex to do anything")
                terminal.send(b"first task\r")
                self.assertTrue(server.blocked.wait(4), "first model request did not block")
                terminal.send(b"\x03")  # Ctrl+C interrupts the turn.
                server.release.set()
                terminal.wait_for(b"another", seconds=5)
                terminal.send(b"second task\r")
                terminal.wait_for(b"after-interrupt", seconds=10)
                self.assertTrue(server.finished.is_set())
                terminal.send(b"/agents\r")
                terminal.wait_for(b"codex")
                terminal.send(b"\x1b")
                terminal.read_for(0.15)
                terminal.send(b"exit\r")
                terminal.exit()
            finally:
                terminal.close()
                server.close()

    def test_blocked_sse_agents_and_fresh_approval(self):
        with tempfile.TemporaryDirectory() as home, tempfile.TemporaryDirectory() as cwd:
            server = FakeServer()
            terminal = Terminal(cwd, home, {"REVEX_BASE_URL": server.base, "REVEX_MODEL": "fake-model"})
            try:
                terminal.wait_for(b"workspace?")
                terminal.wait_for(str(pathlib.Path(cwd).resolve()).encode())
                terminal.send(b"\r")
                terminal.wait_for_screen("> Ask Revex to do anything")
                terminal.send(b"edit file\r")
                self.assertTrue(server.blocked.wait(4), "model request did not start")
                terminal.wait_for_screen("Thinking")
                self.assertNotIn(b"[main] started", terminal.output)
                self.assertNotIn(b"[main] requesting model", terminal.output)
                terminal.send(b"/agents\r")
                terminal.wait_for(b"Agents")
                terminal.wait_for(b"running")
                self.assertFalse(server.release.is_set())
                terminal.send(b"\x1b")
                terminal.read_for(0.1)
                server.release.set()
                terminal.wait_for(b"Approval")
                self.assertFalse(pathlib.Path(cwd, "edited.txt").exists())
                terminal.send(b"\x1b")
                terminal.wait_for(b"Denied")
                self.assertFalse(pathlib.Path(cwd, "edited.txt").exists())
                terminal.wait_for(b"done")
                terminal.send(b"/exit\r")
                terminal.exit()
            finally:
                terminal.close()
                server.close()

    def test_new_chat_steers_blocked_model_without_running_stale_tool(self):
        with tempfile.TemporaryDirectory() as home, tempfile.TemporaryDirectory() as cwd:
            server = FakeServer()
            terminal = Terminal(cwd, home, {"REVEX_BASE_URL": server.base, "REVEX_MODEL": "fake-model"})
            try:
                terminal.wait_for(b"workspace?")
                terminal.send(b"\r")
                terminal.wait_for_screen("> Ask Revex to do anything")
                terminal.send(b"edit file\r")
                self.assertTrue(server.blocked.wait(4), "first model request did not start")
                terminal.wait_for_screen("Thinking")
                terminal.send(b"instead summarize\r")
                terminal.wait_for_screen("Instruction added; applying at next step")
                server.release.set()
                self.assertTrue(server.finished.wait(5), "steered model request did not complete")
                terminal.wait_for_screen("done")
                self.assertFalse(pathlib.Path(cwd, "edited.txt").exists())
                second = server.requests[1]["messages"]
                contents = [m["content"] for m in second if m["role"] == "user"]
                self.assertIn("edit file", contents[0])
                self.assertIn("instead summarize", contents[-1])
                self.assertTrue(any("write edited.txt" in m["content"] for m in second))
                self.assertTrue(any("skipped after user steering" in m["content"] for m in second))
                terminal.send(b"\x04")
                terminal.exit()
            finally:
                terminal.close()
                server.close()

    def test_new_chat_waits_for_current_response_before_next_request(self):
        with tempfile.TemporaryDirectory() as home, tempfile.TemporaryDirectory() as cwd:
            server = FakeHeldResponseSteeringServer()
            terminal = Terminal(cwd, home, {"REVEX_BASE_URL": server.base, "REVEX_MODEL": "fake-model"})
            try:
                terminal.wait_for(b"workspace?")
                terminal.send(b"\r")
                terminal.wait_for_screen("> Ask Revex to do anything")
                terminal.send(b"original task\r")
                self.assertTrue(server.first_started.wait(4), "first HTTP request did not start")
                terminal.send(b"new direction\r")
                terminal.wait_for_screen("Instruction added; applying at next step")
                self.assertFalse(server.second_finished.wait(0.25))
                server.release_first.set()
                self.assertTrue(server.first_finished.wait(3), "first response did not complete")
                self.assertTrue(server.second_finished.wait(3), "next request did not complete")
                terminal.wait_for_screen("steered final answer")
                second = server.requests[1]["messages"]
                users = [m["content"] for m in second if m["role"] == "user"]
                self.assertIn("original task", users[0])
                self.assertIn("new direction", users[-1])
                self.assertTrue(any(m["content"] == "completed first response" for m in second))
                terminal.send(b"\x04")
                terminal.exit()
            finally:
                terminal.close()
                server.close()

    def test_steering_waits_for_active_tool_and_skips_later_calls(self):
        with tempfile.TemporaryDirectory() as home, tempfile.TemporaryDirectory() as cwd:
            script = pathlib.Path(cwd, "hold-tool.sh")
            script.write_text(
                "#!/bin/sh\n"
                "printf 'first\\n' >> trace.txt\n"
                "while [ ! -f release-tool ]; do sleep 0.02; done\n"
                "printf 'completed\\n' >> trace.txt\n"
            )
            script.chmod(0o755)
            server = FakeServer(
                first_reply=">> sh ./hold-tool.sh\n>> sh printf 'second\\n' >> trace.txt"
            )
            seed_settings(home, {"approvalMode": "bypass"})
            terminal = Terminal(cwd, home, {"REVEX_BASE_URL": server.base, "REVEX_MODEL": "fake-model"})
            try:
                terminal.wait_for(b"workspace?")
                terminal.send(b"\r")
                terminal.wait_for_screen("> Ask Revex to do anything")
                terminal.send(b"do work\r")
                self.assertTrue(server.blocked.wait(4))
                server.release.set()
                trace = pathlib.Path(cwd, "trace.txt")
                deadline = time.monotonic() + 4
                while not trace.exists() and time.monotonic() < deadline:
                    terminal.read_for(0.05)
                self.assertTrue(trace.exists(), "first tool never started")
                terminal.send(b"change direction\r")
                terminal.wait_for_screen("Instruction added; applying at next step")
                pathlib.Path(cwd, "release-tool").touch()
                self.assertTrue(server.finished.wait(5), "follow-up model request did not complete")
                terminal.wait_for_screen("done")
                self.assertEqual(trace.read_text().splitlines(), ["first", "completed"])
                second = server.requests[1]["messages"]
                self.assertIn("change direction", second[-1]["content"])
                self.assertTrue(any("<out>" in m["content"] for m in second))
                terminal.send(b"\x04")
                terminal.exit()
            finally:
                pathlib.Path(cwd, "release-tool").touch()
                terminal.close()
                server.close()

    def test_tab_queues_next_task_without_steering_current_tool_loop(self):
        with tempfile.TemporaryDirectory() as home, tempfile.TemporaryDirectory() as cwd:
            server = FakeServer(
                first_reply=">> sh printf old > old.txt",
                later_replies=("old task finished", "queued task finished"),
            )
            seed_settings(home, {"approvalMode": "bypass"})
            terminal = Terminal(cwd, home, {"REVEX_BASE_URL": server.base, "REVEX_MODEL": "fake-model"})
            try:
                terminal.wait_for(b"workspace?")
                terminal.send(b"\r")
                terminal.wait_for_screen("> Ask Revex to do anything")
                terminal.send(b"do old task\r")
                self.assertTrue(server.blocked.wait(4))
                terminal.send(b"do new task\t")
                terminal.wait_for_screen("Task queued for after this task")
                server.release.set()
                self.assertTrue(server.finished.wait(5), "queued follow-up never reached model")
                terminal.wait_for_screen("queued task finished")
                self.assertEqual(pathlib.Path(cwd, "old.txt").read_text(), "old")
                self.assertEqual(len(server.requests), 3)
                second_users = [m["content"] for m in server.requests[1]["messages"] if m["role"] == "user"]
                self.assertFalse(any("do new task" in content for content in second_users))
                third_users = [m["content"] for m in server.requests[2]["messages"] if m["role"] == "user"]
                self.assertTrue(any("do new task" in content for content in third_users))
                terminal.send(b"\x04")
                terminal.exit()
            finally:
                terminal.close()
                server.close()

    def test_approval_requires_explicit_selection_and_applies_edit(self):
        with tempfile.TemporaryDirectory() as home, tempfile.TemporaryDirectory() as cwd:
            server = FakeServer()
            terminal = Terminal(cwd, home, {"REVEX_BASE_URL": server.base, "REVEX_MODEL": "fake-model"})
            try:
                terminal.wait_for(b"workspace?")
                terminal.send(b"\r")
                terminal.wait_for_screen("> Ask Revex to do anything")
                terminal.send(b"edit file\r")
                self.assertTrue(server.blocked.wait(4))
                server.release.set()
                terminal.wait_for(b"Approval")
                self.assertFalse(pathlib.Path(cwd, "edited.txt").exists())
                terminal.send(b"\x1b[C\r")  # Right selects Approve, Enter confirms.
                terminal.wait_for(b"Approved")
                terminal.wait_for(b"done")
                self.assertEqual(pathlib.Path(cwd, "edited.txt").read_text(), "changed\n")
                terminal.send(b"/exit\r")
                terminal.exit()
            finally:
                terminal.close()
                server.close()


if __name__ == "__main__":
    unittest.main()
