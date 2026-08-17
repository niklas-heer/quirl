#!/usr/bin/env python3
"""Focused tests for the deterministic PTY and VT screen harness."""

from __future__ import annotations

import os
from pathlib import Path
import tempfile
import unittest

from pty_harness import PtySession, VirtualScreen


class VirtualScreenTests(unittest.TestCase):
    def test_cursor_query_reports_modeled_bottom_row(self) -> None:
        screen = VirtualScreen(6, 20, initial_cursor_row=5)

        replies = screen.feed(b"\x1b[6n")

        self.assertEqual(replies, [b"\x1b[6;1R"])

    def test_chunked_utf8_and_cursor_sequences_update_visible_cells(self) -> None:
        screen = VirtualScreen(4, 12)

        screen.feed(b"old\x1b[2")
        screen.feed(b"J\x1b[2;3Hdata \xe2")
        screen.feed(b"\x97\x86")

        self.assertEqual(screen.lines()[0], "")
        self.assertEqual(screen.lines()[1], "  data ◆")

    def test_clear_to_end_removes_expanded_picker_rows(self) -> None:
        screen = VirtualScreen(6, 20)
        screen.feed(b"\x1b[1;1Hpicker\x1b[6;1Hdata | results")

        screen.feed(b"\x1b[1;1H\x1b[Jcommand\x1b[6;1Hcommand | ready")

        self.assertNotIn("picker", screen.text())
        self.assertEqual(screen.bottom_line(), "command | ready")


class PtySessionTests(unittest.TestCase):
    def test_close_kills_and_reaps_the_session_leader(self) -> None:
        with tempfile.TemporaryDirectory(prefix="quirl-harness-test-") as directory:
            session = PtySession(
                ["/bin/sh", "-c", "printf READY; sleep 30"],
                cwd=Path(directory),
                environment={"PATH": "/usr/bin:/bin", "TERM": "xterm-256color"},
                rows=8,
                columns=40,
                timeout_seconds=2.0,
                output_bytes_max=4096,
            )
            child_pid = session.pid
            try:
                session.wait_for_output(b"READY")
            finally:
                session.close()

        self.assertEqual(session.pid, -1)
        with self.assertRaises(ChildProcessError):
            os.waitpid(child_pid, os.WNOHANG)

    def test_output_limit_fails_before_unbounded_retention(self) -> None:
        with tempfile.TemporaryDirectory(prefix="quirl-harness-test-") as directory:
            session = PtySession(
                ["/bin/sh", "-c", "while :; do printf 1234567890; done"],
                cwd=Path(directory),
                environment={"PATH": "/usr/bin:/bin", "TERM": "xterm-256color"},
                rows=8,
                columns=40,
                timeout_seconds=2.0,
                output_bytes_max=1024,
            )
            try:
                with self.assertRaisesRegex(AssertionError, "output exceeded its byte limit"):
                    session.read(1.0)
                self.assertLessEqual(len(session.output), 1024)
            finally:
                session.close()


if __name__ == "__main__":
    unittest.main()
