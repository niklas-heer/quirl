#!/bin/sh
set -eu

script_dir=$(CDPATH='' cd "$(dirname "$0")" && pwd -P)
repo_dir=$(CDPATH='' cd "$script_dir/.." && pwd -P)
demo_session=$script_dir/demo-session.sh
quirl_demo_display=${1:-target/release/quirl}
quirl_demo_input=${1:-$repo_dir/target/release/quirl}
quirl_demo_dir=$(dirname "$quirl_demo_input")
quirl_demo_name=$(basename "$quirl_demo_input")
quirl_demo_dir=$(CDPATH='' cd "$quirl_demo_dir" 2>/dev/null && pwd -P) || {
  echo "Quirl binary directory not found: $quirl_demo_dir" >&2
  exit 1
}
quirl_demo_bin=$quirl_demo_dir/$quirl_demo_name

demo_magenta=
demo_cyan=
demo_green=
demo_reset=
demo_dash='—'
demo_prompt='›'
demo_success='✓'
if [ -t 1 ] && [ -z "${NO_COLOR+x}" ]; then
  demo_magenta=$(printf '\033[1;35m')
  demo_cyan=$(printf '\033[1;36m')
  demo_green=$(printf '\033[1;32m')
  demo_reset=$(printf '\033[0m')
fi
if [ "${TERM:-}" = dumb ]; then
  demo_dash=-
  demo_prompt='>'
  demo_success=OK
fi

if [ ! -x "$quirl_demo_bin" ]; then
  echo "Quirl binary not found at $quirl_demo_bin" >&2
  echo "Run 'cargo xtask demo' or pass an executable path." >&2
  exit 1
fi

demo_pace_seconds=${QUIRL_DEMO_PACE_SECONDS:-0}

run_demo() {
  printf '\n%s%s %s%s\n' "$demo_cyan" "$demo_prompt" "$*" "$demo_reset"
  "$demo_session" "$quirl_demo_bin" "$@"
  if [ "$demo_pace_seconds" != 0 ]; then
    sleep "$demo_pace_seconds"
  fi
}

printf '%sQuirl %s a well-stirred shell%s\n' \
  "$demo_magenta" "$demo_dash" "$demo_reset"
printf 'One binary for familiar commands, typed data, and a sandboxed Lua SDK.\n'

run_demo data '[{"service":"api","status":"up","latency_ms":18},{"service":"worker","status":"degraded","latency_ms":91}] | where status == "degraded" | select service status latency_ms'
run_demo complete 'git commit --am'
run_demo eval 'return { answer = 6 * 7, runtime = "sandboxed Lua 5.4" }'
run_demo check hello.lua
run_demo describe 'quirl run' --format markdown

printf '\n%s%s Tour complete. Run %s for the full interactive shell.%s\n' \
  "$demo_green" "$demo_success" "$quirl_demo_display" "$demo_reset"
