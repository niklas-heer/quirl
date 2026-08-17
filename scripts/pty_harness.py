#!/usr/bin/env python3
"""Bounded Unix PTY driver and small deterministic VT screen model.

The harness deliberately uses only the Python standard library. It models the
terminal operations emitted by Crossterm/Ratatui, answers cursor-position
queries from its current screen state, and retains a bounded raw transcript.
"""

from __future__ import annotations

import codecs
import errno
import fcntl
import os
from pathlib import Path
import pty
import select
import signal
import struct
import termios
import time
import unicodedata
from collections.abc import Callable, Sequence


DEFAULT_TIMEOUT_SECONDS = 5.0
DEFAULT_OUTPUT_BYTES_MAX = 16 * 1024 * 1024
READ_BYTES_MAX = 64 * 1024
SCREEN_CELLS_MAX = 512 * 512
CONTROL_SEQUENCE_CHARS_MAX = 128
CLEANUP_TIMEOUT_SECONDS = 2.0


class Key:
    """Exact legacy-terminal byte encodings used by interactive checks."""

    ALT_M = b"\x1bm"
    CTRL_C = b"\x03"
    CTRL_D = b"\x04"
    CTRL_K = b"\x0b"
    CTRL_U = b"\x15"
    ESCAPE = b"\x1b"
    ENTER = b"\r"


class VirtualScreen:
    """Bounded VT screen model for observable end-to-end assertions.

    It implements the cursor movement, erasure, scrolling, UTF-8 width, and
    device-status operations used by Quirl's Crossterm backend. Unsupported
    styling and private-mode sequences are consumed without changing cells.
    """

    def __init__(
        self,
        rows: int,
        columns: int,
        *,
        initial_cursor_row: int | None = None,
    ) -> None:
        _validate_screen_size(rows, columns)
        self.rows = rows
        self.columns = columns
        self.cells = [[" "] * columns for _ in range(rows)]
        self.cursor_row = min(rows - 1, initial_cursor_row or 0)
        self.cursor_column = 0
        self.saved_cursor = (self.cursor_row, self.cursor_column)
        self.scroll_top = 0
        self.scroll_bottom = rows - 1
        self.wrap_pending = False
        self._state = "ground"
        self._control = ""
        self._decoder = codecs.getincrementaldecoder("utf-8")("replace")

    def resize(self, rows: int, columns: int) -> None:
        """Resize while preserving the bounded top-left screen contents."""
        _validate_screen_size(rows, columns)
        resized = [[" "] * columns for _ in range(rows)]
        for row in range(min(rows, self.rows)):
            for column in range(min(columns, self.columns)):
                resized[row][column] = self.cells[row][column]
        self.rows = rows
        self.columns = columns
        self.cells = resized
        self.cursor_row = min(self.cursor_row, rows - 1)
        self.cursor_column = min(self.cursor_column, columns - 1)
        self.scroll_top = 0
        self.scroll_bottom = rows - 1
        self.wrap_pending = False

    def feed(self, data: bytes) -> list[bytes]:
        """Apply output bytes and return terminal replies in emission order."""
        replies: list[bytes] = []
        for character in self._decoder.decode(data, final=False):
            reply = self._feed_character(character)
            if reply is not None:
                replies.append(reply)
        return replies

    def lines(self) -> list[str]:
        """Return every physical row with trailing blank cells removed."""
        return ["".join(cell for cell in row if cell).rstrip() for row in self.cells]

    def text(self) -> str:
        """Return the complete physical screen, retaining blank row positions."""
        return "\n".join(self.lines())

    def bottom_line(self) -> str:
        """Return the final physical terminal row without trailing blanks."""
        return self.lines()[-1]

    def _feed_character(self, character: str) -> bytes | None:
        if self._state == "ground":
            return self._ground(character)
        if self._state == "escape":
            return self._escape(character)
        if self._state == "csi":
            return self._csi(character)
        if self._state == "osc":
            if character == "\x07":
                self._state = "ground"
            elif character == "\x1b":
                self._state = "osc_escape"
            return None
        if self._state == "osc_escape":
            self._state = "ground" if character == "\\" else "osc"
            return None
        self._state = "ground"
        return None

    def _ground(self, character: str) -> bytes | None:
        if character == "\x1b":
            self._state = "escape"
        elif character == "\r":
            self.cursor_column = 0
            self.wrap_pending = False
        elif character in ("\n", "\x0b", "\x0c"):
            self._line_feed()
        elif character == "\b":
            self.cursor_column = max(0, self.cursor_column - 1)
            self.wrap_pending = False
        elif character == "\t":
            self.cursor_column = min(self.columns - 1, ((self.cursor_column // 8) + 1) * 8)
            self.wrap_pending = False
        elif character >= " " and character != "\x7f":
            self._put(character)
        return None

    def _escape(self, character: str) -> bytes | None:
        self._state = "ground"
        if character == "[":
            self._state = "csi"
            self._control = ""
        elif character == "]":
            self._state = "osc"
        elif character == "7":
            self.saved_cursor = (self.cursor_row, self.cursor_column)
        elif character == "8":
            self.cursor_row, self.cursor_column = self.saved_cursor
            self._clamp_cursor()
        elif character in ("D", "E"):
            if character == "E":
                self.cursor_column = 0
            self._line_feed()
        elif character == "M":
            self._reverse_index()
        elif character == "c":
            self._reset()
        return None

    def _csi(self, character: str) -> bytes | None:
        if "@" <= character <= "~":
            control = self._control
            self._control = ""
            self._state = "ground"
            return self._apply_csi(control, character)
        if len(self._control) >= CONTROL_SEQUENCE_CHARS_MAX:
            self._control = ""
            self._state = "ground"
            return None
        self._control += character
        return None

    def _apply_csi(self, control: str, final: str) -> bytes | None:
        private = control.startswith(("?", ">", "!"))
        body = control[1:] if private else control
        parameters = _csi_parameters(body)
        first = parameters[0] if parameters else 0
        amount = max(1, first)
        if final in ("H", "f"):
            row = max(1, parameters[0] if parameters else 1)
            column = max(1, parameters[1] if len(parameters) > 1 else 1)
            self.cursor_row = min(self.rows - 1, row - 1)
            self.cursor_column = min(self.columns - 1, column - 1)
            self.wrap_pending = False
        elif final == "A":
            self.cursor_row = max(self.scroll_top, self.cursor_row - amount)
            self.wrap_pending = False
        elif final == "B":
            self.cursor_row = min(self.scroll_bottom, self.cursor_row + amount)
            self.wrap_pending = False
        elif final == "C":
            self.cursor_column = min(self.columns - 1, self.cursor_column + amount)
            self.wrap_pending = False
        elif final == "D":
            self.cursor_column = max(0, self.cursor_column - amount)
            self.wrap_pending = False
        elif final == "E":
            self.cursor_row = min(self.scroll_bottom, self.cursor_row + amount)
            self.cursor_column = 0
            self.wrap_pending = False
        elif final == "F":
            self.cursor_row = max(self.scroll_top, self.cursor_row - amount)
            self.cursor_column = 0
            self.wrap_pending = False
        elif final in ("G", "`"):
            self.cursor_column = min(self.columns - 1, amount - 1)
            self.wrap_pending = False
        elif final == "d":
            self.cursor_row = min(self.rows - 1, amount - 1)
            self.wrap_pending = False
        elif final == "J":
            self._erase_display(first)
        elif final == "K":
            self._erase_line(first)
        elif final == "S":
            for _ in range(amount):
                self._scroll_up()
        elif final == "T":
            for _ in range(amount):
                self._scroll_down()
        elif final == "r" and not private:
            top = max(1, parameters[0] if parameters else 1)
            bottom = max(1, parameters[1] if len(parameters) > 1 else self.rows)
            if top < bottom <= self.rows:
                self.scroll_top = top - 1
                self.scroll_bottom = bottom - 1
                self.cursor_row = 0
                self.cursor_column = 0
                self.wrap_pending = False
        elif final == "s":
            self.saved_cursor = (self.cursor_row, self.cursor_column)
        elif final == "u":
            self.cursor_row, self.cursor_column = self.saved_cursor
            self._clamp_cursor()
        elif final == "n" and not private and first == 6:
            return f"\x1b[{self.cursor_row + 1};{self.cursor_column + 1}R".encode()
        return None

    def _put(self, character: str) -> None:
        width = _cell_width(character)
        if width == 0:
            column = max(0, self.cursor_column - 1)
            self.cells[self.cursor_row][column] += character
            return
        if self.wrap_pending:
            self.cursor_column = 0
            self._line_feed()
            self.wrap_pending = False
        if width == 2 and self.cursor_column == self.columns - 1:
            self.cursor_column = 0
            self._line_feed()
        self.cells[self.cursor_row][self.cursor_column] = character
        if width == 2 and self.cursor_column + 1 < self.columns:
            self.cells[self.cursor_row][self.cursor_column + 1] = ""
        final_column = self.cursor_column + width
        if final_column >= self.columns:
            self.cursor_column = self.columns - 1
            self.wrap_pending = True
        else:
            self.cursor_column = final_column

    def _line_feed(self) -> None:
        self.wrap_pending = False
        if self.cursor_row == self.scroll_bottom:
            self._scroll_up()
        else:
            self.cursor_row = min(self.rows - 1, self.cursor_row + 1)

    def _reverse_index(self) -> None:
        self.wrap_pending = False
        if self.cursor_row == self.scroll_top:
            self._scroll_down()
        else:
            self.cursor_row = max(0, self.cursor_row - 1)

    def _scroll_up(self) -> None:
        del self.cells[self.scroll_top]
        self.cells.insert(self.scroll_bottom, [" "] * self.columns)

    def _scroll_down(self) -> None:
        del self.cells[self.scroll_bottom]
        self.cells.insert(self.scroll_top, [" "] * self.columns)

    def _erase_display(self, mode: int) -> None:
        if mode in (2, 3):
            self.cells = [[" "] * self.columns for _ in range(self.rows)]
        elif mode == 1:
            for row in range(self.cursor_row):
                self.cells[row] = [" "] * self.columns
            for column in range(self.cursor_column + 1):
                self.cells[self.cursor_row][column] = " "
        else:
            for column in range(self.cursor_column, self.columns):
                self.cells[self.cursor_row][column] = " "
            for row in range(self.cursor_row + 1, self.rows):
                self.cells[row] = [" "] * self.columns
        self.wrap_pending = False

    def _erase_line(self, mode: int) -> None:
        if mode == 1:
            start, end = 0, self.cursor_column + 1
        elif mode == 2:
            start, end = 0, self.columns
        else:
            start, end = self.cursor_column, self.columns
        for column in range(start, end):
            self.cells[self.cursor_row][column] = " "
        self.wrap_pending = False

    def _reset(self) -> None:
        self.cells = [[" "] * self.columns for _ in range(self.rows)]
        self.cursor_row = 0
        self.cursor_column = 0
        self.saved_cursor = (0, 0)
        self.scroll_top = 0
        self.scroll_bottom = self.rows - 1
        self.wrap_pending = False

    def _clamp_cursor(self) -> None:
        self.cursor_row = min(self.rows - 1, max(0, self.cursor_row))
        self.cursor_column = min(self.columns - 1, max(0, self.cursor_column))
        self.wrap_pending = False


class PtySession:
    """Own a PTY child with bounded I/O, waits, screen state, and cleanup."""

    def __init__(
        self,
        argv: Sequence[str],
        *,
        cwd: Path,
        environment: dict[str, str],
        rows: int = 30,
        columns: int = 120,
        timeout_seconds: float = DEFAULT_TIMEOUT_SECONDS,
        output_bytes_max: int = DEFAULT_OUTPUT_BYTES_MAX,
        stderr_path: Path | None = None,
    ) -> None:
        _validate_timeout(timeout_seconds)
        _validate_screen_size(rows, columns)
        if not argv:
            raise ValueError("PTY argv must not be empty")
        if output_bytes_max <= 0:
            raise ValueError("PTY output byte limit must be positive")
        self.timeout_seconds = timeout_seconds
        self.output_bytes_max = output_bytes_max
        self.output = bytearray()
        self.screen = VirtualScreen(rows, columns, initial_cursor_row=rows - 1)
        self.pid = -1
        self.master = -1

        pid, master = pty.fork()
        if pid == 0:
            try:
                os.chdir(cwd)
                os.environ.clear()
                os.environ.update(environment)
                if stderr_path is not None:
                    error_fd = os.open(
                        stderr_path,
                        os.O_WRONLY | os.O_CREAT | os.O_TRUNC,
                        0o600,
                    )
                    os.dup2(error_fd, 2)
                    os.close(error_fd)
                os.execv(argv[0], list(argv))
            except BaseException as error:
                message = f"PTY exec failed: {error}\n".encode("utf-8", "replace")
                os.write(2, message[:4096])
                os._exit(127)

        self.pid = pid
        self.master = master
        fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack("HHHH", rows, columns, 0, 0))
        flags = fcntl.fcntl(master, fcntl.F_GETFL)
        fcntl.fcntl(master, fcntl.F_SETFL, flags | os.O_NONBLOCK)

    def close(self) -> None:
        """Kill the foreground/session process groups, close, and reap once."""
        if self.master >= 0:
            try:
                foreground_group = os.tcgetpgrp(self.master)
            except OSError:
                foreground_group = -1
            if foreground_group > 0 and foreground_group != os.getpgrp():
                _kill_process_group(foreground_group)
        if self.pid > 0:
            _kill_process_group(self.pid)
            try:
                os.kill(self.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        if self.master >= 0:
            try:
                os.close(self.master)
            except OSError:
                pass
            self.master = -1
        if self.pid > 0:
            self._reap(CLEANUP_TIMEOUT_SECONDS)

    def send(self, data: bytes) -> None:
        """Write an exact byte sequence before the configured deadline."""
        if self.master < 0:
            raise AssertionError("PTY input is closed")
        offset = 0
        deadline = time.monotonic() + self.timeout_seconds
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
        """Encode and send UTF-8 text exactly once."""
        self.send(text.encode("utf-8"))

    def resize(self, rows: int, columns: int) -> None:
        """Resize both the kernel PTY and modeled terminal screen."""
        _validate_screen_size(rows, columns)
        if self.master < 0:
            raise AssertionError("cannot resize a closed PTY")
        self.screen.resize(rows, columns)
        fcntl.ioctl(
            self.master,
            termios.TIOCSWINSZ,
            struct.pack("HHHH", rows, columns, 0, 0),
        )

    def read(self, duration: float = 0.15) -> bytes:
        """Drain bounded output for at most ``duration`` seconds."""
        _validate_timeout(duration)
        deadline = time.monotonic() + duration
        chunk = bytearray()
        while self.master >= 0 and time.monotonic() < deadline:
            remaining = max(0.0, deadline - time.monotonic())
            ready, _, _ = select.select([self.master], [], [], min(0.05, remaining))
            if not ready:
                continue
            try:
                data = os.read(self.master, READ_BYTES_MAX)
            except OSError as error:
                if error.errno in (errno.EAGAIN, errno.EIO):
                    continue
                raise
            if not data:
                break
            observed = len(self.output) + len(data)
            if observed > self.output_bytes_max:
                raise AssertionError(
                    "PTY output exceeded its byte limit; "
                    f"observed={observed} limit={self.output_bytes_max}"
                )
            chunk.extend(data)
            self.output.extend(data)
            for response in self.screen.feed(data):
                self.send(response)
        return bytes(chunk)

    def wait_for_output(
        self,
        marker: bytes,
        timeout: float | None = None,
        *,
        since: int | None = None,
    ) -> bytes:
        """Wait for a raw marker emitted after ``since`` (now by default)."""
        timeout = self.timeout_seconds if timeout is None else timeout
        _validate_timeout(timeout)
        start = len(self.output) if since is None else since
        deadline = time.monotonic() + timeout
        while marker not in self.output[start:] and time.monotonic() < deadline:
            self.read(min(0.1, max(0.001, deadline - time.monotonic())))
        observed = bytes(self.output[start:])
        if marker not in observed:
            raise AssertionError(self._timeout_message(f"output marker {marker!r}", observed))
        return observed

    def wait_for_screen(
        self,
        predicate: Callable[[VirtualScreen], bool],
        description: str,
        timeout: float | None = None,
    ) -> str:
        """Wait until a predicate accepts the current modeled screen."""
        timeout = self.timeout_seconds if timeout is None else timeout
        _validate_timeout(timeout)
        deadline = time.monotonic() + timeout
        while not predicate(self.screen) and time.monotonic() < deadline:
            self.read(min(0.1, max(0.001, deadline - time.monotonic())))
        snapshot = self.screen.text()
        if not predicate(self.screen):
            raise AssertionError(self._timeout_message(f"screen state {description}", b""))
        return snapshot

    def wait_for_screen_text(self, marker: str, timeout: float | None = None) -> str:
        """Wait until visible screen cells contain ``marker``."""
        return self.wait_for_screen(
            lambda screen: marker in screen.text(),
            repr(marker),
            timeout,
        )

    def wait_exit(self, timeout: float | None = None) -> int:
        """Wait for and reap the session leader within the configured bound."""
        timeout = self.timeout_seconds if timeout is None else timeout
        _validate_timeout(timeout)
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            self.read(min(0.05, max(0.001, deadline - time.monotonic())))
            pid, status = os.waitpid(self.pid, os.WNOHANG)
            if pid == self.pid:
                self.pid = -1
                return os.waitstatus_to_exitcode(status)
        raise AssertionError(self._timeout_message("child exit", b""))

    # Compatibility name used by the established rich-PTY checks.
    wait_for = wait_for_output

    def _reap(self, timeout: float) -> None:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            try:
                pid, _ = os.waitpid(self.pid, os.WNOHANG)
            except ChildProcessError:
                self.pid = -1
                return
            if pid == self.pid:
                self.pid = -1
                return
            time.sleep(0.01)
        raise AssertionError(f"PTY child {self.pid} could not be reaped after SIGKILL")

    def _timeout_message(self, expected: str, observed: bytes) -> str:
        tail = observed[-1000:] if observed else bytes(self.output[-1000:])
        return (
            f"timed out waiting for {expected}; raw_tail={tail!r}; "
            f"screen=\n{self.screen.text()}"
        )


def _validate_timeout(timeout_seconds: float) -> None:
    if timeout_seconds <= 0 or timeout_seconds > 60:
        raise ValueError("PTY timeout must be greater than zero and at most 60 seconds")


def _validate_screen_size(rows: int, columns: int) -> None:
    if rows <= 0 or columns <= 0 or rows * columns > SCREEN_CELLS_MAX:
        raise ValueError(
            "PTY screen dimensions must be positive and retain at most "
            f"{SCREEN_CELLS_MAX} cells"
        )


def _kill_process_group(process_group: int) -> None:
    try:
        os.killpg(process_group, signal.SIGKILL)
    except ProcessLookupError:
        pass


def _csi_parameters(control: str) -> list[int]:
    parameter_text = control.split(" ", 1)[0]
    if not parameter_text:
        return []
    result = []
    for value in parameter_text.split(";"):
        try:
            result.append(int(value) if value else 0)
        except ValueError:
            result.append(0)
    return result


def _cell_width(character: str) -> int:
    if unicodedata.combining(character):
        return 0
    return 2 if unicodedata.east_asian_width(character) in ("W", "F") else 1
