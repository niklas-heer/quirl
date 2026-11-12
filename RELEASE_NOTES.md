# Quirl 0.7.0

### Changed

- `source`, `.`, and `eval` keep the variables their code sets without
  `export`, as Bash and Zsh do: a plain `source .env` now makes its variables
  available to later commands, directly and in loops, without passing them to
  child processes. Variables the code unsets are removed. Pasted lines that run
  in the `/bin/sh` island behave the same way. Values are never parsed: the
  shell prints each one itself, so no value can spoof another variable or
  inject code. Tracing, aliases, and redefined commands left by the code do
  not affect the import, and a dump that is incomplete or oversized imports
  nothing and prints a warning saying why
  ([#25](https://github.com/niklas-heer/quirl/issues/25)).

### Added

- `set -a` and `set +a`, and their long forms `set -o allexport` and
  `set +o allexport`, work natively, so `set -a; source .env; set +a` exports
  a dotenv file. While allexport is on, every assignment, including those of
  sourced code, is exported; `set +a` keeps variables it already exported.
  Other `set` options, and `set` inside a pipeline, explain what is supported
  instead of failing with `exec: set: not found`.
