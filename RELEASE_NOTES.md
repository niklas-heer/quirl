# Quirl 0.5.1

### Fixed

- `eval "$(ssh-agent -s)"` in the interactive shell could lose the agent it
  started, so `ssh-add` then reported "Connection refused". The terminal
  cleanup that ends a foreground command could kill the agent before it
  detached. `eval` and `source` now run outside that cleanup, so daemons
  they start keep running.
- Typing a pasted loop or `{ ...; }` group no longer shows an "unsupported
  dialect control form" error while it runs fine. The editor shows a quiet
  hint that the line runs through `/bin/sh`; function definitions keep an
  error that explains why they cannot persist.
- Tab no longer ranks a fuzzy catalog match above a candidate that extends
  what you typed from Zsh: `git log --one` offers `--oneline` alone.
