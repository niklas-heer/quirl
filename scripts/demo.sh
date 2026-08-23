#!/bin/sh
set -eu

script_dir=$(CDPATH='' cd "$(dirname "$0")" && pwd -P)
demo_session=$script_dir/demo-session.sh
if [ "$#" -gt 1 ]; then
  echo "usage: scripts/demo.sh [release-binary]" >&2
  exit 2
fi
if [ "$#" -eq 1 ]; then
  quirl_demo_input=$1
  quirl_demo_display=${QUIRL_DEMO_DISPLAY:-$1}
elif [ -n "${QUIRL_DEMO_BIN:-}" ]; then
  quirl_demo_input=$QUIRL_DEMO_BIN
  quirl_demo_display=${QUIRL_DEMO_DISPLAY:-quirl}
else
  if ! command -v brew >/dev/null 2>&1; then
    echo "Homebrew is required for the release demo." >&2
    echo "Install Homebrew, then run 'brew install niklas-heer/tap/quirl'." >&2
    exit 1
  fi
  quirl_demo_prefix=$(brew --prefix quirl 2>/dev/null) || {
    echo "The Homebrew Quirl release is not installed." >&2
    echo "Run 'brew install niklas-heer/tap/quirl' and retry." >&2
    exit 1
  }
  quirl_demo_input=$quirl_demo_prefix/bin/quirl
  quirl_demo_display=quirl
fi
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
quirl_build_info=$("$quirl_demo_bin" --build-info) || {
  echo "Could not inspect the Quirl release build identity." >&2
  exit 1
}
case $quirl_build_info in
  *'"official_release": true'*) ;;
  *)
    echo "Demo refused a development build: $quirl_demo_bin" >&2
    echo "Install the official release with 'brew install niklas-heer/tap/quirl'." >&2
    exit 1
    ;;
esac

demo_pace_seconds=${QUIRL_DEMO_PACE_SECONDS:-0}

run_demo() {
  printf '\n%s%s %s%s\n' "$demo_cyan" "$demo_prompt" "$*" "$demo_reset"
  "$demo_session" "$quirl_demo_bin" "$@"
  if [ "$demo_pace_seconds" != 0 ]; then
    sleep "$demo_pace_seconds"
  fi
}

run_ai_demo() {
  printf '\n%s%s %s%s\n' "$demo_cyan" "$demo_prompt" "$*" "$demo_reset"
  "$demo_session" --ai-ready "$quirl_demo_bin" "$@"
  if [ "$demo_pace_seconds" != 0 ]; then
    sleep "$demo_pace_seconds"
  fi
}

explain() {
  printf '\n%s%s%s\n' "$demo_magenta" "$*" "$demo_reset"
  if [ "$demo_pace_seconds" != 0 ]; then
    sleep "$demo_pace_seconds"
  fi
}

printf '%sQuirl %s a well-stirred shell%s\n' \
  "$demo_magenta" "$demo_dash" "$demo_reset"
printf 'The three prompt modes make bytes, typed values, and intent visibly different.\n'

explain 'NORMAL MODE — run familiar programs; Quirl owns pipes and process status.'
run_demo exec "printf 'api ready\\nworker degraded\\n' | grep degraded && printf 'pipeline status: healthy\\n'"

explain 'COMPLETION — learn flags from the same catalog that powers help and docs.'
run_demo complete 'git commit --am'

explain 'DATA MODE — filter and sort real typed fields before rendering a table.'
run_demo data '[{"service":"api","region":"eu","latency_ms":18},{"service":"worker","region":"us","latency_ms":91},{"service":"jobs","region":"eu","latency_ms":54}] | where latency_ms > 50 | sort latency_ms desc | select service region latency_ms'

explain 'AI MODE — describe the goal; local results are suggested, never auto-executed.'
run_ai_demo ai search 'show git branch status' --limit 3

explain 'LUA SDK — bounded extension logic for scripts and config, not a hidden mode.'
run_demo eval 'return { answer = 6 * 7, runtime = "sandboxed Lua 5.4" }'
run_demo check hello.lua

explain 'ONE CATALOG — the interactive UI, CLI, docs, and agents share one contract.'
run_demo describe 'quirl complete'

printf '\n%s%s Tour complete. Run %s for the full interactive shell.%s\n' \
  "$demo_green" "$demo_success" "$quirl_demo_display" "$demo_reset"
