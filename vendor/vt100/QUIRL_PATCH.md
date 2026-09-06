# Quirl patch of vt100 0.15.2

The upstream MIT license is retained in `LICENSE`. `UPSTREAM.json` records the
registry archive checksum and hashes of the retained original source files.
The workspace selects this source using `[patch.crates-io]`.

## Failure model and invariants

An embedded child controls terminal bytes and can race resize with editing or
saved-cursor restoration. It can also scroll longer than the visible viewport.
Three upstream grid invariants failed on these ordinary transitions:

- Shrinking through a wide glyph left its leading half at the last column;
  subsequent erase accessed a missing paired cell. Row resize now clears that
  incomplete glyph while preserving every complete cell.
- A saved cursor could lie outside the resized viewport. Restore now clamps it
  to the active viewport and origin/scroll region before any subsequent edit.
- Viewing scrollback deeper than the viewport subtracted a larger offset from
  the row count. Visible rows now chain history and live rows, then take exactly
  the viewport height, preserving both ordering and bounded iteration.

A saturating primary-history eviction counter and read-only screen accessor make
scrollback truncation observable to the owner, including while the alternate
screen is active. No dependencies, features or unsafe code are added. Quirl's owning
`child_terminal` adapter independently limits viewport dimensions, retained
history, admitted byte turns, control lengths and edit counts; it discards
OSC/DCS strings before they reach VTE. This patch does not make the upstream
parser safe for arbitrary unbounded use by itself.

Canonical consumer regressions prove shrinking through a wide cell then erasing,
restoring a saved cursor after shrink, deep scrollback ordering and bounded
seeded terminal/resize transitions:

```console
cargo test -p quirl-ui child_terminal --lib
```

Remove this patch when an upstream version provides equivalent invariants, after
rerunning these regressions and real embedded-PTY application checks.
