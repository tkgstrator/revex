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


class Terminal:
    def __init__(self, cwd, home, extra=None, columns=100):
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
        ):
            env.pop(key, None)
        env.update(extra or {})
        self.child = subprocess.Popen(
            [str(BIN)], cwd=cwd, stdin=self.slave, stdout=self.slave,
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
        self.child.wait(timeout=5)
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
    def __init__(self):
        self.listener = socket.socket()
        self.listener.bind(("127.0.0.1", 0))
        self.listener.listen(8)
        self.listener.settimeout(6)
        self.base = f"http://127.0.0.1:{self.listener.getsockname()[1]}"
        self.blocked = threading.Event()
        self.release = threading.Event()
        self.finished = threading.Event()
        self.error = None
        self.thread = threading.Thread(target=self.serve, daemon=True)
        self.thread.start()

    def serve(self):
        try:
            posts = 0
            while posts < 2:
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
                    posts += 1
                    if posts == 1:
                        self.blocked.set()
                        assert self.release.wait(5), "first model reply was never released"
                        sse(conn, ">> write edited.txt <<EOF\nchanged\nEOF")
                    else:
                        sse(conn, "done")
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
    def __init__(self):
        self.listener = socket.socket()
        self.listener.bind(("127.0.0.1", 0))
        self.listener.listen(4)
        self.listener.settimeout(6)
        self.base = f"http://127.0.0.1:{self.listener.getsockname()[1]}"
        self.user_input = None
        self.finished = threading.Event()
        self.error = None
        self.thread = threading.Thread(target=self.serve, daemon=True)
        self.thread.start()

    def serve(self):
        try:
            while not self.finished.is_set():
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
                    self.user_input = request["messages"][-1]["content"]
                    sse(conn, "multiline done")
                    self.finished.set()
        except Exception as exc:
            self.error = exc

    def close(self):
        self.thread.join(timeout=7)
        self.listener.close()
        if self.error:
            raise self.error
        assert self.finished.is_set(), "single reply server did not finish"


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
                self.assertIn("Revex", footer)
                self.assertIn("server model", footer)
                self.assertIn("agents", footer)
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
                terminal.send(b"\x1b[B\x1b[B\x1b[B\r\r\r")  # Select high, helpers, confirm.
                until = time.monotonic() + 3
                while time.monotonic() < until:
                    saved = json.loads(project.read_text())
                    if saved.get("model") == "new-model":
                        break
                    terminal.read_for(0.05)
                self.assertEqual(saved["model"], "new-model")
                self.assertEqual(saved["reasoning_effort"], "high")
                terminal.send(b"exit\r")
                terminal.exit()
            finally:
                terminal.close()

    def test_setup_and_chat_hardware_cursor_follows_editing(self):
        with tempfile.TemporaryDirectory() as home:
            terminal = Terminal(ROOT, home)
            try:
                terminal.wait_for(b"workspace?")
                terminal.send(b"\r")
                terminal.wait_for(b"Provider:")
                terminal.send(b"\r")  # Provider -> editable URL.
                url_frame = terminal.read_for(0.15)
                self.assertTrue(terminal.cursor_visible())
                url_row, url_x = Terminal.last_cursor(url_frame)
                terminal.send("界".encode())
                unicode_frame = terminal.read_for(0.15)
                self.assertEqual(Terminal.last_cursor(unicode_frame), (url_row, url_x + 2))
                terminal.send(b"\x1b[D")
                left_frame = terminal.read_for(0.15)
                self.assertEqual(Terminal.last_cursor(left_frame), (url_row, url_x))
                terminal.send(b"\t")
                key_frame = terminal.read_for(0.15)
                self.assertTrue(terminal.cursor_visible())
                key_row, key_x = Terminal.last_cursor(key_frame)
                self.assertNotEqual(key_row, url_row)
                terminal.send(b"secret")
                secret_frame = terminal.read_for(0.15)
                self.assertEqual(Terminal.last_cursor(secret_frame), (key_row, key_x + 6))
                self.assertNotIn(b"secret", secret_frame)
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
                first.send(b"\r\t\t")
                first.read_for(0.1)
                first.send(b"fake-model\r\r\r\r")
                first.wait_for_screen("> Ask Revex to do anything")
                self.assertTrue(pathlib.Path(home, ".revex").is_file())
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
            settings = pathlib.Path(home, ".revex")
            original = {"base_url": "http://127.0.0.1:9/v1", "model": "fake"}
            settings.write_text(json.dumps(original))
            terminal = Terminal(cwd, home, {"REVEX_API_KEY": "test-secret"})
            try:
                terminal.wait_for(b"workspace?")
                terminal.send(b"\r")
                terminal.wait_for_screen("> Ask Revex to do anything")
                terminal.send(b"/config\r")
                terminal.wait_for(b"Provider:")
                terminal.send(b"\r")  # URL field.
                terminal.send(b"x\r\r\r\r\r\r")  # Change URL, skip key/model/effort/helpers, confirm.
                terminal.wait_for(b"REVEX_API_KEY")
                self.assertEqual(json.loads(settings.read_text()), original)
                self.assertNotIn(b"test-secret", terminal.output)
                terminal.send(b"\x1b")
                terminal.read_for(0.15)
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
                first.send(b"\r\r\r\r\r")  # Provider, URL, key, model, reasoning -> helpers.
                first.wait_for(b"choose helper")
                first.wait_for(b"Codex")
                first.wait_for(b"Claude")
                first.send(b" \x1b[B \x1b[B \r\r")  # Local off, Codex + Claude on, save.
                first.wait_for_screen("> Ask Revex to do anything")
                settings = json.loads(pathlib.Path(home, ".revex").read_text())
                self.assertEqual(settings["helper_providers"], "codex,claude")
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
                returning.send(b"\r\r\r\r\r\r\r")  # Confirm unchanged selection.
                returning.wait_for_screen("> Ask Revex to do anything")
                settings = json.loads(pathlib.Path(home, ".revex").read_text())
                self.assertEqual(settings["helper_providers"], "codex,claude")
                self.assertNotIn("helper_providers", json.loads(project_settings.read_text()))
                returning.send(b"exit\r")
                returning.exit()
            finally:
                returning.close()

    def test_simple_model_identity_is_local_and_uses_configured_id(self):
        with tempfile.TemporaryDirectory() as home, tempfile.TemporaryDirectory() as cwd:
            terminal = Terminal(cwd, home, {
                "REVEX_BASE_URL": "http://127.0.0.1:9/v1",
                "REVEX_MODEL": "selected-alias",
                "REVEX_REASONING_EFFORT": "high",
                "REVEX_API_KEY": "test-secret",
            })
            try:
                terminal.wait_for(b"workspace?")
                terminal.send(b"\r")
                terminal.wait_for_screen("> Ask Revex to do anything")
                terminal.send("あなたのモデルは？\r".encode())
                state = pathlib.Path(home, ".local/state/revex")
                until = time.monotonic() + 3
                answer = ""
                while time.monotonic() < until:
                    terminal.read_for(0.05)
                    histories = list(state.glob("history-*.json"))
                    if histories:
                        messages = json.loads(histories[0].read_text())["messages"]
                        if len(messages) >= 3:
                            answer = messages[-1]["content"]
                            break
                self.assertIn("selected-alias", answer)
                self.assertIn("high", answer)
                self.assertNotIn("test-secret", answer)
                self.assertNotIn("127.0.0.1", answer)
                self.assertNotIn(b"Thinking", terminal.output)
                self.assertNotIn(b"[main] started", terminal.output)
                until = time.monotonic() + 4
                while "selected-alias" not in terminal.screen_text().split("-- INPUT")[0] and time.monotonic() < until:
                    terminal.read_for(0.05)
                self.assertIn("selected-alias", terminal.screen_text().split("-- INPUT")[0])
                terminal.send(b"\x04")
                terminal.exit()
            finally:
                terminal.close()

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
                self.assertEqual(server.user_input, "first\nsecond")
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
                        "\"usage\":{\"input_tokens\":5,\"output_tokens\":2}}'\n"
                    )
                script.write_text(
                    "#!/bin/sh\n"
                    f'printf "%s\\n" "$*" > "$HOME/{name}-args"\n'
                    + response
                )
                script.chmod(0o755)
            pathlib.Path(home, ".revex").write_text(json.dumps({
                "base_url": server.base,
                "model": "fake-parent",
                "helper_providers": "codex,claude",
            }))
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
            pathlib.Path(home, ".revex").write_text(json.dumps({
                "base_url": server.base,
                "model": "fake-parent",
                "helper_providers": "codex",
            }))
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
