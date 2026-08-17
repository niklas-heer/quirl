#!/usr/bin/env python3
"""Exercise Quirl's interactive surfaces through a real Unix pseudo-terminal."""

from __future__ import annotations

import argparse
import errno
import fcntl
import os
from pathlib import Path
import pty
import select
import signal
import shutil
import struct
import sys
import tempfile
import termios
import time


STARTUP_MARKER = b"Tab complete"
TIMEOUT = 5.0


class Session:
    def __init__(
        self,
        binary: Path,
        root: Path,
        *,
        term: str = "xterm-256color",
        stderr_path: Path | None = None,
        shell: Path | None = None,
        symbols: str = "plain",
        semantic_hints: bool = True,
        no_color: bool = False,
        catalog_gate: bool = False,
        catalog_failure: bool = False,
    ) -> None:
        self.temp = tempfile.TemporaryDirectory(prefix="quirl-pty-")
        private = Path(self.temp.name)
        config_dir = private / "config"
        config_dir.mkdir()
        temporary_dir = private / "tmp"
        temporary_dir.mkdir()
        (config_dir / "config.lua").write_text(
            f"""---@type quirl.Config
return quirl.config {{
  schema_version = 3,
  editor = {{ keymap = "emacs", semantic_hints = {str(semantic_hints).lower()}, banner = "none" }},
  picker = {{ layout = "adaptive", preview = true }},
  prompt = {{
    symbols = "{symbols}",
    left = {{ "directory" }},
    right = {{ "duration", "status" }},
    transient = false,
  }},
  ui = {{ theme = "tokyo-night", surface = "rich", statusline = {{ hints = true }} }},
  completion = {{ auto = false, min_chars = 2 }},
}}
""",
            encoding="utf-8",
        )

        # The PTY child must not inherit developer credentials, plugins, history,
        # or prompt state. Keep the environment small and deterministic while
        # retaining PATH so reference shells can start ordinary commands.
        environment = {
            "HOME": str(private),
            "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
            "TERM": term,
            "LC_ALL": "en_US.UTF-8" if sys.platform == "darwin" else "C.UTF-8",
            "TMPDIR": str(temporary_dir),
            "QUIRL_CONFIG_DIR": str(config_dir),
            "QUIRL_HISTORY": str(private / "history"),
            "QUIRL_PLUGIN_HOME": str(private / "plugins"),
            "QUIRL_INDEX_DIR": str(private / "index"),
            "QUIRL_RECOVERY_DIR": str(private / "recovery"),
            "XDG_CACHE_HOME": str(private / "cache"),
            "XDG_CONFIG_HOME": str(private / "xdg-config"),
            "XDG_DATA_HOME": str(private / "data"),
            "XDG_STATE_HOME": str(private / "state"),
        }
        self.catalog_gate = private / "catalog-admission.gate"
        self.catalog_gate_reached = Path(f"{self.catalog_gate}.reached")
        if catalog_gate:
            environment["QUIRL_TEST_CATALOG_GATE"] = str(self.catalog_gate)
        if catalog_failure:
            environment["QUIRL_TEST_CATALOG_FAILURE"] = "1"
        if no_color:
            environment["NO_COLOR"] = "1"

        pid, master = pty.fork()
        if pid == 0:
            os.chdir(private)
            os.environ.clear()
            os.environ.update(environment)
            if stderr_path is not None:
                error_fd = os.open(stderr_path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
                os.dup2(error_fd, 2)
                os.close(error_fd)
            if shell is not None:
                arguments = (
                    [shell.name, "-f"]
                    if shell.name == "zsh"
                    else [shell.name, "--noprofile", "--norc", "-i"]
                )
                os.execv(str(shell), arguments)
            os.execv(str(binary), [str(binary)])
            raise AssertionError("exec returned")

        self.pid = pid
        self.master = master
        self.private = private
        self.binary = binary
        self.root = root
        self.output = bytearray()
        self.shell = shell is not None
        fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 120, 0, 0))
        flags = fcntl.fcntl(master, fcntl.F_GETFL)
        fcntl.fcntl(master, fcntl.F_SETFL, flags | os.O_NONBLOCK)

    def close(self) -> None:
        try:
            foreground_group = os.tcgetpgrp(self.master)
        except OSError:
            foreground_group = -1
        if foreground_group > 0 and foreground_group != os.getpgrp():
            try:
                os.killpg(foreground_group, signal.SIGKILL)
            except ProcessLookupError:
                pass
        if self.pid > 0:
            try:
                os.killpg(self.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        try:
            os.close(self.master)
        except OSError:
            pass
        if self.pid > 0:
            try:
                os.kill(self.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            try:
                os.waitpid(self.pid, 0)
            except ChildProcessError:
                pass
        self.temp.cleanup()

    def send(self, data: bytes) -> None:
        offset = 0
        deadline = time.monotonic() + TIMEOUT
        while offset < len(data):
            try:
                written = os.write(self.master, data[offset:])
            except OSError as error:
                if error.errno != errno.EAGAIN:
                    raise
                if time.monotonic() >= deadline:
                    raise AssertionError("timed out writing PTY input") from error
                select.select([], [self.master], [], 0.05)
                continue
            if written == 0:
                raise AssertionError("PTY input closed during write")
            offset += written

    def type(self, text: str) -> None:
        self.send(text.encode("utf-8"))

    def resize(self, rows: int, columns: int) -> None:
        fcntl.ioctl(
            self.master,
            termios.TIOCSWINSZ,
            struct.pack("HHHH", rows, columns, 0, 0),
        )

    def read(self, duration: float = 0.15) -> bytes:
        deadline = time.monotonic() + duration
        chunk = bytearray()
        while time.monotonic() < deadline:
            ready, _, _ = select.select([self.master], [], [], min(0.05, deadline - time.monotonic()))
            if not ready:
                continue
            try:
                data = os.read(self.master, 65536)
            except OSError as error:
                if error.errno in (errno.EAGAIN, errno.EIO):
                    continue
                raise
            if not data:
                break
            # Ratatui's inline viewport asks the terminal for its current cursor
            # position during initialization. A bare PTY has no emulator to
            # answer the CPR query, so provide the same response a terminal at
            # the first row and column would send.
            for _ in range(data.count(b"\x1b[6n")):
                os.write(self.master, b"\x1b[1;1R")
            chunk.extend(data)
            self.output.extend(data)
        return bytes(chunk)

    def wait_for(self, marker: bytes, timeout: float = TIMEOUT) -> bytes:
        start = len(self.output)
        deadline = time.monotonic() + timeout
        while marker not in self.output[start:] and time.monotonic() < deadline:
            self.read(0.1)
        observed = bytes(self.output[start:])
        if marker not in observed:
            raise AssertionError(
                f"timed out waiting for {marker!r}; tail={observed[-1000:]!r}"
            )
        return observed

    def wait_exit(self, timeout: float = TIMEOUT) -> int:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            self.read(0.05)
            pid, status = os.waitpid(self.pid, os.WNOHANG)
            if pid == self.pid:
                self.pid = -1
                return os.waitstatus_to_exitcode(status)
        raise AssertionError("Quirl did not exit")


def enter_and_wait(session: Session, command: str, marker: bytes) -> bytes:
    session.type(command)
    session.send(b"\r")
    return session.wait_for(marker)


def wait_for_prompt(session: Session) -> None:
    if not (
        os.tcgetpgrp(session.master) == session.pid
        and STARTUP_MARKER in session.output[-2000:]
    ):
        session.wait_for(STARTUP_MARKER)
    deadline = time.monotonic() + TIMEOUT
    while os.tcgetpgrp(session.master) != session.pid and time.monotonic() < deadline:
        time.sleep(0.01)
    if os.tcgetpgrp(session.master) != session.pid:
        raise AssertionError("Quirl did not recover terminal ownership at the prompt")


def check_rich_editing(binary: Path, root: Path) -> None:
    session = Session(binary, root)
    try:
        session.wait_for(STARTUP_MARKER)

        session.type("/usr/bin/printf BACKSPACE_BAD")
        session.send(b"\x7f\x7f\x7f")
        enter_and_wait(session, "OK", b"BACKSPACE_OK")

        session.type("/usr/bin/printf DELETE_XOK")
        session.send(b"\x1b[D\x1b[D\x1b[D\x1b[3~\r")
        session.wait_for(b"DELETE_OK")

        session.type("/usr/bin/printf UNICODE_e\u0301")
        session.send(b"\x7f")
        enter_and_wait(session, "OK", b"UNICODE_OK")

        session.type("/usr/bin/printf CTRLD_XOK")
        session.send(b"\x1b[D\x1b[D\x1b[D\x04\r")
        session.wait_for(b"CTRLD_OK")

        session.type("/usr/bin/printf SHOULD_NOT_RUN")
        session.send(b"\x03")
        session.wait_for(b"^C")
        enter_and_wait(session, "/usr/bin/printf AFTER_CTRLC", b"AFTER_CTRLC")

        # Alt-M is encoded as Escape followed by `m` in a legacy PTY. The rich
        # surface must repaint in place, preserve the active edit buffer, and
        # avoid committing one feedback line to scrollback per toggle.
        session.type("/usr/bin/printf MODE_BUFFER_OK")
        session.send(b"\x1bm")
        toggled_to_data = session.wait_for(b"data")
        if b"Alt-M mode" not in toggled_to_data or b"data" not in toggled_to_data:
            raise AssertionError("Alt-M did not repaint the rich data-mode status")
        if b"typed values and data pipelines" in toggled_to_data:
            raise AssertionError("Alt-M committed rich mode feedback to scrollback")
        session.send(b"\x1bm")
        toggled_to_command = session.wait_for(b"command")
        if b"Alt-M mode" not in toggled_to_command or b"command" not in toggled_to_command:
            raise AssertionError("Alt-M did not repaint the rich command-mode status")
        if b"processes and byte pipelines" in toggled_to_command:
            raise AssertionError("Alt-M committed rich mode feedback to scrollback")
        session.send(b"\r")
        session.wait_for(b"MODE_BUFFER_OK")

        session.type("/usr/bin/printf 'MULTI_ONE")
        session.send(b"\r")
        session.read(0.2)
        session.type("_TWO'")
        session.send(b"\r")
        session.wait_for(b"MULTI_ONE\r\n_TWO")

        enter_and_wait(
            session,
            "/bin/sh -c 'printf STDOUT_OK; printf STDERR_OK >&2'",
            b"STDERR_OK",
        )
        if b"STDOUT_OK" not in session.output:
            raise AssertionError("interactive command stdout was not handed back to the PTY")

        session.send(b"\x04")
        status = session.wait_exit()
        if status != 0:
            raise AssertionError(f"Ctrl-D exited with status {status}")
    finally:
        session.close()


def check_completion(binary: Path, root: Path) -> None:
    session = Session(binary, root)
    try:
        session.wait_for(STARTUP_MARKER)
        session.type("git st")
        session.send(b"\t")
        session.wait_for(b"git status [--short]")
        session.send(b"\x1b")
        session.read(0.2)
        session.send(b"\x03")
        session.wait_for(b"^C")

        session.type("git st")
        session.send(b"\t")
        session.wait_for(b"git status [--short]")
        session.send(b"\r")
        session.read(0.2)
        session.send(b"\r")
        session.wait_for(b"not a git repository")

        session.type("git")
        session.send(b"\x1b[Z")
        session.wait_for(b"picker")
        # Paste the query as one event so a diff-rendering terminal writes the
        # complete value, rather than one independently styled cell per key.
        session.send(b"\x1b[200~zzzz-no-match\x1b[201~")
        session.wait_for(b"zzzz-no-match")
        session.send(b"\x1b")
        session.read(0.1)
        session.send(b"\x03")
        session.wait_for(b"^C")
        session.send(b"\x04")
        session.wait_exit()
    finally:
        session.close()


def wait_for_file(session: Session, path: Path, timeout: float = TIMEOUT) -> None:
    deadline = time.monotonic() + timeout
    while not path.is_file() and time.monotonic() < deadline:
        session.read(0.02)
    if not path.is_file():
        raise AssertionError(f"timed out waiting for catalog gate marker {path}")


def check_deferred_catalog_admission(binary: Path, root: Path) -> None:
    session = Session(binary, root, catalog_gate=True)
    try:
        wait_for_file(session, session.catalog_gate_reached)
        session.read(0.1)
        if STARTUP_MARKER not in session.output:
            raise AssertionError("catalog loader ran before the first frame was flushed")
        gated_modes = termios.tcgetattr(session.master)
        if gated_modes[3] & (termios.ICANON | termios.ECHO):
            raise AssertionError("catalog gate did not run inside the owned raw terminal session")

        # Resize and input arrive while admission is blocked. Neither can be
        # consumed before publication; the PTY keeps both queued for the first
        # post-admission event turns.
        session.resize(4, 40)
        session.send(b"\x1b[200~/usr/bin/printf QUEUED_AFTER_ADMISSION\x1b[201~\r")
        session.read(0.15)
        if b"QUEUED_AFTER_ADMISSION" in session.output:
            raise AssertionError("terminal input was consumed before catalog publication")

        session.catalog_gate.write_text("release\n", encoding="utf-8")
        session.wait_for(b"QUEUED_AFTER_ADMISSION")
        session.resize(30, 120)
        wait_for_prompt(session)
        session.type("git st")
        session.send(b"\t")
        session.wait_for(b"git status [--short]")
        session.send(b"\x03")
        session.wait_for(b"^C")
        session.send(b"\x04")
        session.wait_exit()
    finally:
        session.close()


def check_catalog_failure_restores_terminal(binary: Path, root: Path) -> None:
    session = Session(binary, root, catalog_gate=True, catalog_failure=True)
    try:
        wait_for_file(session, session.catalog_gate_reached)
        session.read(0.1)
        if STARTUP_MARKER not in session.output:
            raise AssertionError("catalog failure was injected before the first frame")
        session.catalog_gate.write_text("fail\n", encoding="utf-8")
        status = session.wait_exit()
        if status == 0:
            raise AssertionError("injected catalog failure exited successfully")
        observed = bytes(session.output)
        if b"injected catalog admission failure" not in observed:
            raise AssertionError("catalog error was replaced during terminal cleanup")
        for marker, state in [
            (b"\x1b[?2004l", "bracketed paste"),
            (b"\x1b[?25h", "cursor visibility"),
            (b"\x1b[0 q", "cursor shape"),
            (b"\x1b[J", "inline viewport"),
        ]:
            if marker not in observed:
                raise AssertionError(f"catalog failure did not restore {state}")
        restored_modes = termios.tcgetattr(session.master)
        if not restored_modes[3] & termios.ICANON or not restored_modes[3] & termios.ECHO:
            raise AssertionError("catalog failure did not restore cooked terminal modes")
    finally:
        session.close()


def check_interactive_runtime(binary: Path, root: Path) -> None:
    session = Session(binary, root)
    try:
        session.wait_for(STARTUP_MARKER)

        session.resize(4, 40)
        session.read(0.2)
        # Paste is one editor event, so the diff renderer emits the complete
        # value in one write instead of styling one cell per keypress.
        session.send(b"\x1b[200~resize-safe\x1b[201~")
        session.wait_for(b"resize-safe")
        session.send(b"\x03")
        session.wait_for(b"^C")
        session.resize(30, 120)
        session.wait_for(STARTUP_MARKER)

        # Populate and select a real process-owned job snapshot. Acceptance
        # inserts an explicit command, so a changed/pruned job is revalidated by
        # quirl-process instead of retaining a process handle in the picker.
        enter_and_wait(session, "/bin/sleep 30 &", STARTUP_MARKER)
        session.send(b"\x07")  # Ctrl-G: jobs picker
        session.wait_for(b"fg job 1")
        session.send(b"\r")
        session.read(0.1)
        session.send(b"\x03")
        session.wait_for(b"^C")

        # A successful typed stream becomes a bounded cached data source without
        # rerunning the expression when Alt-D opens the data picker.
        session.send(b"\x1bm")
        session.wait_for(b"data")
        enter_and_wait(session, "[1,2]", STARTUP_MARKER)
        session.send(b"\x1bd")
        session.wait_for(b"cached typed result")
        session.send(b"\x1b")
        session.read(0.1)
        session.send(b"\x03")
        session.wait_for(b"^C")

        # Fill the PTY faster than the reader drains it. The first row must be
        # visible before the complete source is consumed, and SIGINT must stop
        # the bounded pull loop while preserving prompt terminal ownership.
        csv_path = session.private / "stream.csv"
        with csv_path.open("w", encoding="utf-8") as stream:
            stream.write("name\n")
            for index in range(100_000):
                stream.write(f"row-{index:05d}\n")
        session.type(f"open {csv_path}")
        session.send(b"\r")
        session.wait_for(b"row-00000")
        session.send(b"\x03")
        session.wait_for(b"cancelled")
        wait_for_prompt(session)

        session.send(b"\x1bm")
        session.wait_for(b"command")

        # A Lua process callback narrows the shared plan deadline to the VM's
        # security budget. Expiry must reap the child and restore the real PTY
        # before the next native command is admitted.
        session.type("lua return quirl.process.run('/bin/sleep 30')")
        session.send(b"\r")
        session.wait_for(b"exceeded its deadline")
        wait_for_prompt(session)
        enter_and_wait(
            session,
            "/usr/bin/printf AFTER_%s DATA_CANCEL_RESTORED",
            b"AFTER_DATA_CANCEL_RESTORED",
        )
        wait_for_prompt(session)
        session.send(b"\x04")
        session.wait_exit()
    finally:
        session.close()


def check_rich_review_regressions(binary: Path, root: Path) -> None:
    session = Session(binary, root)
    try:
        startup = session.wait_for(STARTUP_MARKER)
        if any(
            marker in startup
            for marker in [
                "❯".encode("utf-8"),
                "◆".encode("utf-8"),
                "·".encode("utf-8"),
            ]
        ):
            raise AssertionError("plain prompt.symbols emitted Unicode input chrome")

        session.send(b"\x1b[200~" + (b"x" * 5_000) + b"\x1b[201~")
        session.read(0.5)
        session.send(b"\t")
        completion_notice = session.read(1.0)
        if b"completion limited" not in completion_notice:
            raise AssertionError(
                f"oversized completion notice missing; tail={bytes(session.output[-1500:])!r}"
            )

        session.send(b"\x15")
        session.read(0.1)
        session.type("git st")
        session.send(b"\x1bOP")
        session.wait_for(b"documentation")
        session.send(b"\x1bOP")
        session.read(0.1)
        session.send(b"\r")
        session.type("atus")
        session.send(b"\r")
        session.wait_for(b"not a git repository")

        long_line = b"echo " + (b"x" * 180) + b"VIEWPORT-END"
        session.send(b"\x1b[200~" + long_line + b"\x1b[201~")
        session.wait_for(b"VIEWPORT-END")
        session.send(b"\x03")
        session.wait_for(b"^C")

        cleanup_start = len(session.output)
        session.send(b"\x04")
        session.wait_exit()
        if b"\x1b[?25h" not in bytes(session.output[cleanup_start:]):
            raise AssertionError("rich terminal cleanup did not explicitly show the cursor")
    finally:
        session.close()

    no_hints = Session(binary, root, semantic_hints=False)
    try:
        no_hints.wait_for(STARTUP_MARKER)
        no_hints.type("definitely-not-a-command")
        if b"unknown command" in no_hints.read(0.3):
            raise AssertionError("semantic_hints=false rendered a semantic diagnostic")
        no_hints.send(b"\x03")
        no_hints.wait_for(b"^C")
        no_hints.send(b"\x04")
        no_hints.wait_exit()
    finally:
        no_hints.close()


def check_suspend_resume(binary: Path, root: Path) -> None:
    shell_name = shutil.which("zsh") or shutil.which("bash")
    if shell_name is None:
        print("skip: check_suspend_resume (zsh/bash unavailable)")
        return
    session = Session(binary, root, shell=Path(shell_name))
    try:
        session.read(0.2)
        session.type(f"'{binary}'")
        session.send(b"\r")
        session.wait_for(STARTUP_MARKER)
        session.send(b"\x1a")
        session.wait_for(b"suspended")
        session.type("fg")
        session.send(b"\r")
        session.wait_for(STARTUP_MARKER)
        session.send(b"\x04")
        session.read(0.3)
        session.type("exit")
        session.send(b"\r")
        session.wait_exit()
    finally:
        session.close()


def check_native_job_control(binary: Path, root: Path) -> None:
    session = Session(binary, root)
    try:
        session.wait_for(STARTUP_MARKER)
        prompt_modes = termios.tcgetattr(session.master)
        if os.tcgetpgrp(session.master) != session.pid:
            raise AssertionError("Quirl did not own the terminal at the prompt")

        enter_and_wait(
            session,
            "/bin/sh -c 'test \"$(ps -o tpgid= -p $$)\" -eq $$ && printf TTY_%s OWNED'",
            b"TTY_OWNED",
        )
        wait_for_prompt(session)
        race = "; ".join(["/usr/bin/true | /bin/cat" for _ in range(8)])
        enter_and_wait(
            session,
            f"{race}; /usr/bin/printf LEADER_%s RACE_OK",
            b"LEADER_RACE_OK",
        )
        wait_for_prompt(session)

        pid_path = session.private / "construction.pid"
        gate_path = session.private / "construction.gate"
        os.mkfifo(gate_path, 0o600)
        session.type(
            f"/bin/sh -c 'printf %s $$ > {pid_path}; printf x > {gate_path}; sleep 30' | "
            f"/bin/cat < {gate_path} > /definitely/missing/quirl-construction-output"
        )
        session.send(b"\r")
        session.wait_for(b"cannot write redirected output")
        wait_for_prompt(session)
        observed_child = int(pid_path.read_text(encoding="utf-8").strip())
        try:
            os.kill(observed_child, 0)
        except ProcessLookupError:
            pass
        else:
            raise AssertionError(f"partial construction leaked child {observed_child}")
        enter_and_wait(
            session,
            "/usr/bin/printf AFTER_%s CONSTRUCTION_CLEANUP",
            b"AFTER_CONSTRUCTION_CLEANUP",
        )
        wait_for_prompt(session)

        session.type("/bin/sleep 30")
        session.send(b"\r")
        child_group = session.pid
        deadline = time.monotonic() + 2.0
        while child_group == session.pid and time.monotonic() < deadline:
            session.read(0.02)
            child_group = os.tcgetpgrp(session.master)
        if child_group <= 0 or child_group == session.pid:
            raise AssertionError(
                "foreground child did not receive the terminal; "
                f"tpgid={child_group} tail={bytes(session.output[-1200:])!r}"
            )
        session.send(b"\x1a")
        wait_for_prompt(session)
        if os.tcgetpgrp(session.master) != session.pid:
            raise AssertionError("Quirl did not recover the terminal after Ctrl-Z")
        enter_and_wait(session, "jobs", b"stopped")
        wait_for_prompt(session)
        session.type("bg %1")
        session.send(b"\r")
        wait_for_prompt(session)
        enter_and_wait(session, "jobs", b"running")
        wait_for_prompt(session)
        session.type("fg %1")
        session.send(b"\r")
        deadline = time.monotonic() + 2.0
        while os.tcgetpgrp(session.master) == session.pid and time.monotonic() < deadline:
            session.read(0.02)
        if os.tcgetpgrp(session.master) == session.pid:
            raise AssertionError(
                "fg did not return the terminal to the job; "
                f"tail={bytes(session.output[-1200:])!r}"
            )
        session.send(b"\x03")
        wait_for_prompt(session)
        enter_and_wait(
            session,
            "/usr/bin/printf AFTER_%s JOB_CTRLC",
            b"AFTER_JOB_CTRLC",
        )
        wait_for_prompt(session)

        session.type(
            "/bin/sh -c 'stty -echo; kill -STOP $$; "
            "stty -a | grep -q -- \"-echo\" && printf JOB_%s MODES_OK'"
        )
        session.send(b"\r")
        wait_for_prompt(session)
        if termios.tcgetattr(session.master) != prompt_modes:
            raise AssertionError("stopped child modes leaked into the Quirl prompt")
        session.type("fg %2")
        session.send(b"\r")
        session.wait_for(b"JOB_MODES_OK")
        wait_for_prompt(session)
        if termios.tcgetattr(session.master) != prompt_modes:
            raise AssertionError("Quirl did not restore termios after fg completion")

        session.send(b"\x04")
        session.wait_exit()
    finally:
        session.close()


def check_noninteractive_dialect_islands(binary: Path, root: Path) -> None:
    session = Session(binary, root)
    try:
        session.wait_for(STARTUP_MARKER)
        enter_and_wait(
            session,
            "bash { read value || printf ISLAND_%s STDIN_CLOSED; }",
            b"ISLAND_STDIN_CLOSED",
        )
        wait_for_prompt(session)
        session.type("bash { sleep 30; }")
        session.send(b"\r")
        session.read(0.2)
        session.send(b"\x1a")
        session.wait_for(b"cancelled")
        wait_for_prompt(session)
        enter_and_wait(
            session,
            "/usr/bin/printf AFTER_%s ISLAND_CTRLZ",
            b"AFTER_ISLAND_CTRLZ",
        )
        wait_for_prompt(session)
        session.send(b"\x04")
        session.wait_exit()
    finally:
        session.close()


def check_fallbacks(binary: Path, root: Path) -> None:
    # The rich-only injected failure must not run on the eager simple path.
    dumb = Session(binary, root, term="dumb", catalog_failure=True)
    try:
        dumb.read(0.5)
        if STARTUP_MARKER in dumb.output:
            raise AssertionError("TERM=dumb rendered the rich status line")
        enter_and_wait(dumb, "/usr/bin/printf DUMB_OK", b"DUMB_OK")
        dumb.send(b"\x04")
        dumb.wait_exit()
    finally:
        dumb.close()

    with tempfile.TemporaryDirectory(prefix="quirl-redirect-") as directory:
        stderr_path = Path(directory) / "stderr"
        redirected = Session(binary, root, stderr_path=stderr_path)
        try:
            redirected.read(0.5)
            if STARTUP_MARKER in redirected.output:
                raise AssertionError("redirected stderr rendered the rich status line")
            enter_and_wait(redirected, "/usr/bin/printf REDIRECT_OK", b"REDIRECT_OK")
            redirected.send(b"\x04")
            redirected.wait_exit()
        finally:
            redirected.close()


def check_no_color_preserves_semantic_hints(binary: Path, root: Path) -> None:
    session = Session(binary, root, no_color=True)
    try:
        session.wait_for(STARTUP_MARKER)
        session.type("quirl describe --unknown")
        session.wait_for(b"unknown flag")
        session.send(b"\x03")
        session.wait_for(b"^C")
        session.send(b"\x04")
        session.wait_exit()
    finally:
        session.close()


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("binary", nargs="?", default="target/debug/quirl")
    args = parser.parse_args()
    root = Path(__file__).resolve().parent.parent
    binary = (root / args.binary).resolve() if not Path(args.binary).is_absolute() else Path(args.binary)
    if not binary.is_file():
        raise SystemExit(f"missing Quirl binary: {binary}; run cargo build -p quirl-cli")

    checks = [
        check_rich_editing,
        check_deferred_catalog_admission,
        check_catalog_failure_restores_terminal,
        check_completion,
        check_interactive_runtime,
        check_rich_review_regressions,
        check_native_job_control,
        check_noninteractive_dialect_islands,
        check_suspend_resume,
        check_fallbacks,
        check_no_color_preserves_semantic_hints,
    ]
    for check in checks:
        check(binary, root)
        print(f"ok: {check.__name__}")


if __name__ == "__main__":
    main()
