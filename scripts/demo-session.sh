#!/bin/sh
set -eu

usage() {
  echo "usage: scripts/demo-session.sh [--nerd-font] [--ai-ready] <quirl-binary> [arguments...]" >&2
}

# A refresh can invalidate embeddings after initial preparation. The recording
# therefore verifies again in the live session; a missing asset or lexical
# fallback is a failed take, never evidence of semantic retrieval.
demo_asset_manifest=${QUIRL_DEMO_ASSET_MANIFEST:-}
demo_timeout=
demo_symbols=auto
demo_ai_ready=false
while [ "$#" -gt 0 ]; do
  case $1 in
    --nerd-font)
      demo_symbols=nerd_font
      shift
      ;;
    --ai-ready)
      demo_ai_ready=true
      shift
      ;;
    --)
      shift
      break
      ;;
    *)
      break
      ;;
  esac
done
if [ "$#" -lt 1 ]; then
  usage
  exit 2
fi

demo_bin_input=$1
shift
demo_bin_dir=$(dirname "$demo_bin_input")
demo_bin_name=$(basename "$demo_bin_input")
demo_bin_dir=$(CDPATH='' cd "$demo_bin_dir" 2>/dev/null && pwd -P) || {
  echo "Quirl binary directory not found: $demo_bin_dir" >&2
  exit 1
}
demo_bin=$demo_bin_dir/$demo_bin_name
if [ ! -x "$demo_bin" ]; then
  echo "Quirl binary is not executable: $demo_bin" >&2
  exit 1
fi
demo_script_dir=$(CDPATH='' cd "$(dirname "$0")" && pwd -P)
demo_model_input=$demo_script_dir/../models/quirl-command-v3-int8/quirl-command-v3-9bc5efbd14096b54
demo_model=$(CDPATH='' cd "$demo_model_input" 2>/dev/null && pwd -P) || {
  echo "Quirl demo model directory not found: $demo_model_input" >&2
  exit 1
}
if [ ! -f "$demo_model/quirl-model.json" ]; then
  echo "Quirl demo model manifest not found: $demo_model/quirl-model.json" >&2
  exit 1
fi

if [ "$demo_ai_ready" = true ]; then
  if command -v timeout >/dev/null 2>&1; then
    demo_timeout=$(command -v timeout)
  elif command -v gtimeout >/dev/null 2>&1; then
    demo_timeout=$(command -v gtimeout)
  else
    echo "AI demo preparation requires GNU timeout (coreutils; gtimeout on macOS)." >&2
    exit 1
  fi
  if ! "$demo_timeout" --version 2>/dev/null | grep -F 'GNU coreutils' >/dev/null; then
    echo "AI demo preparation requires GNU timeout with --kill-after support." >&2
    exit 1
  fi
  if [ -n "$demo_asset_manifest" ]; then
    demo_manifest_dir=$(CDPATH='' cd "$(dirname "$demo_asset_manifest")" && pwd -P)
    demo_asset_manifest=$demo_manifest_dir/$(basename "$demo_asset_manifest")
    if [ ! -f "$demo_asset_manifest" ] || [ ! -r "$demo_asset_manifest" ]; then
      echo "Demo asset manifest must be a readable local file: $demo_asset_manifest" >&2
      exit 1
    fi
  fi
fi

umask 077
demo_temp_base_input=${TMPDIR:-/tmp}
demo_temp_base=$(CDPATH='' cd "$demo_temp_base_input" 2>/dev/null && pwd -P) || {
  echo "Demo temporary directory not found: $demo_temp_base_input" >&2
  exit 1
}
demo_root=$(mktemp -d "$demo_temp_base/quirl-demo.XXXXXX") || {
  echo "Could not create an isolated Quirl demo directory" >&2
  exit 1
}
cleanup() {
  if [ -n "${demo_root:-}" ] && [ -d "$demo_root" ]; then
    rm -rf "$demo_root"
  fi
}
trap cleanup EXIT HUP INT TERM

demo_home=$demo_root/home
demo_config=$demo_root/config
demo_plugins=$demo_root/plugins
demo_cache=$demo_root/cache
demo_state=$demo_root/state
demo_data=$demo_root/data
demo_recovery=$demo_root/recovery
demo_tmp=$demo_root/tmp
demo_workspace=$demo_root/workspace
demo_empty=$demo_root/empty
demo_tools=$demo_root/bin
mkdir -p \
  "$demo_home" \
  "$demo_config" \
  "$demo_plugins" \
  "$demo_cache" \
  "$demo_state" \
  "$demo_data" \
  "$demo_recovery" \
  "$demo_tmp" \
  "$demo_workspace" \
  "$demo_empty" \
  "$demo_tools"

# A private executable provides observable evidence that real PATH discovery
# published; it contributes no fabricated description or search result.
printf '#!/bin/sh\nexit 0\n' >"$demo_tools/demo-discovery-ready"
chmod 700 "$demo_tools/demo-discovery-ready"

printf 'demo notes\n' >"$demo_workspace/notes.txt"
printf 'service,status\napi,up\nworker,degraded\n' >"$demo_workspace/services.csv"
printf '%s\n' \
  '[' \
  '  {"service":"checkout","region":"fra","latency":18,"status":"healthy"},' \
  '  {"service":"search","region":"iad","latency":91,"status":"degraded"},' \
  '  {"service":"billing","region":"fra","latency":54,"status":"healthy"},' \
  '  {"service":"identity","region":"sin","latency":73,"status":"degraded"}' \
  ']' >"$demo_workspace/services.json"
printf '%s\n' \
  'return {' \
  '  answer = 6 * 7,' \
  '  runtime = _VERSION,' \
  '}' >"$demo_workspace/hello.lua"
cp "$demo_script_dir/demo-card.sh" "$demo_workspace/tour"
chmod 700 "$demo_workspace/tour"
cat >"$demo_workspace/prepare-search" <<'PREPARE_SEARCH'
#!/bin/sh
set -eu
# One 30-second owner covers discovery, index, status and retrieval. These four
# JSON reports use the CLI's bounded metadata and three-result output contracts.
# Do not impose RLIMIT_FSIZE: indexing must write its full bounded SQLite store.
if [ "${1:-}" != --bounded ]; then
  exec "$QUIRL_DEMO_TIMEOUT" --kill-after=2s 30s "$0" --bounded
fi
fail() {
  echo "Demo search is not ready: let catalog discovery settle, then retry ./prepare-search." >&2
  echo "Inspect .demo-discovery.json and .demo-search-{index,status,result}.json in this private fixture." >&2
  exit 1
}
# Initial native facts alone can be hybrid until the first discovery publication
# replaces them. Wait for our private PATH entry before rebuilding that image.
# Both the attempt count and the enclosing 30-second deadline bound this wait.
discovery_attempt=0
until "$QUIRL_DEMO_BINARY" index explain demo-discovery-ready --format json >.demo-discovery.json 2>&1; do
  discovery_attempt=$((discovery_attempt + 1))
  [ "$discovery_attempt" -lt 100 ] || fail
  sleep 0.1
done
grep -Eq '"value":[[:space:]]*"external:demo-discovery-ready"' .demo-discovery.json || fail
grep -Eq '"source":[[:space:]]*"external"' .demo-discovery.json || fail
"$QUIRL_DEMO_BINARY" ai index --format json >.demo-search-index.json || fail
"$QUIRL_DEMO_BINARY" ai status --format json >.demo-search-status.json || fail
grep -Eq '"semantic_ready":[[:space:]]*true' .demo-search-status.json || fail
"$QUIRL_DEMO_BINARY" ai search 'copy a directory while preserving permissions' --limit 3 --format json >.demo-search-result.json || fail
grep -Eq '"mode":[[:space:]]*"hybrid"' .demo-search-result.json || fail
if grep -Eq '"mode":[[:space:]]*"lexical"' .demo-search-result.json; then
  fail
fi
printf 'DEMO_HYBRID_READY\n'
PREPARE_SEARCH
chmod 700 "$demo_workspace/prepare-search"
if [ "$demo_symbols" = nerd_font ]; then
  printf '%s\n' \
    'return quirl.config {' \
    '  prompt = { symbols = "nerd_font" },' \
    '  completion = { auto = false, min_chars = 1 },' \
    '}' >"$demo_config/config.lua"
fi

# Nested `quirl` commands in the recording must use the same candidate binary.
demo_path=$demo_bin_dir:$demo_tools:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin:/opt/homebrew/bin
demo_term=${TERM:-xterm-256color}
cd "$demo_workspace"

run_in_demo_environment() {
  if [ -n "${NO_COLOR+x}" ]; then
    env -i \
      HOME="$demo_home" \
      USER=demo \
      LOGNAME=demo \
      SHELL=/bin/sh \
      PATH="$demo_path" \
      TERM="$demo_term" \
      LANG=C.UTF-8 \
      TMPDIR="$demo_tmp" \
      XDG_CONFIG_HOME="$demo_root/xdg-config" \
      XDG_STATE_HOME="$demo_state" \
      XDG_CACHE_HOME="$demo_cache" \
      XDG_DATA_HOME="$demo_data" \
      QUIRL_CONFIG_DIR="$demo_config" \
      QUIRL_PLUGIN_HOME="$demo_plugins" \
      QUIRL_INDEX_PATH="$demo_cache/catalog.sqlite3" \
      QUIRL_MODEL_PATH="$demo_model" \
      QUIRL_HISTORY="$demo_state/history" \
      QUIRL_RECOVERY_DIR="$demo_recovery" \
      QUIRL_SESSION_ID=release-demo \
      QUIRL_DEMO_BINARY="$demo_bin" \
      QUIRL_DEMO_TIMEOUT="$demo_timeout" \
      QUIRL_FISH_PATH="$demo_empty" \
      QUIRL_BASH_PATH="$demo_empty" \
      QUIRL_ZSH_PATH="$demo_empty" \
      QUIRL_HELP_PATH="$demo_empty" \
      QUIRL_MAN_PATH="$demo_empty" \
      NO_COLOR=1 \
      "$@"
  else
    env -i \
      HOME="$demo_home" \
      USER=demo \
      LOGNAME=demo \
      SHELL=/bin/sh \
      PATH="$demo_path" \
      TERM="$demo_term" \
      LANG=C.UTF-8 \
      TMPDIR="$demo_tmp" \
      XDG_CONFIG_HOME="$demo_root/xdg-config" \
      XDG_STATE_HOME="$demo_state" \
      XDG_CACHE_HOME="$demo_cache" \
      XDG_DATA_HOME="$demo_data" \
      QUIRL_CONFIG_DIR="$demo_config" \
      QUIRL_PLUGIN_HOME="$demo_plugins" \
      QUIRL_INDEX_PATH="$demo_cache/catalog.sqlite3" \
      QUIRL_MODEL_PATH="$demo_model" \
      QUIRL_HISTORY="$demo_state/history" \
      QUIRL_RECOVERY_DIR="$demo_recovery" \
      QUIRL_SESSION_ID=release-demo \
      QUIRL_DEMO_BINARY="$demo_bin" \
      QUIRL_DEMO_TIMEOUT="$demo_timeout" \
      QUIRL_FISH_PATH="$demo_empty" \
      QUIRL_BASH_PATH="$demo_empty" \
      QUIRL_ZSH_PATH="$demo_empty" \
      QUIRL_HELP_PATH="$demo_empty" \
      QUIRL_MAN_PATH="$demo_empty" \
      "$@"
  fi
}

if [ "$demo_ai_ready" = true ]; then
  # Native command facts are a runtime asset. Use the normal verified updater;
  # a local manifest keeps unpublished-candidate and offline captures possible.
  # Each of these three preparation phases has a 120-second wall bound, plus
  # two seconds for termination. No personal cache is read or modified.
  if [ -n "$demo_asset_manifest" ]; then
    run_in_demo_environment "$demo_timeout" --kill-after=2s 120s \
      "$demo_bin" assets update --manifest "$demo_asset_manifest" --format json >/dev/null
  else
    run_in_demo_environment "$demo_timeout" --kill-after=2s 120s \
      "$demo_bin" assets update --format json >/dev/null
  fi
  run_in_demo_environment "$demo_timeout" --kill-after=2s 120s \
    "$demo_bin" index build --output "$demo_cache/catalog.sqlite3" --format json >/dev/null
  run_in_demo_environment "$demo_timeout" --kill-after=2s 120s \
    "$demo_bin" ai index --format json >/dev/null
fi

run_in_demo_environment "$demo_bin" "$@"
