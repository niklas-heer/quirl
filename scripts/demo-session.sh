#!/bin/sh
set -eu

usage() {
  echo "usage: scripts/demo-session.sh [--nerd-font] <quirl-binary> [arguments...]" >&2
}

demo_symbols=auto
if [ "${1:-}" = --nerd-font ]; then
  demo_symbols=nerd_font
  shift
fi
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

umask 077
demo_temp_base=${TMPDIR:-/tmp}
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
mkdir -p \
  "$demo_home" \
  "$demo_config" \
  "$demo_plugins" \
  "$demo_cache" \
  "$demo_state" \
  "$demo_data" \
  "$demo_recovery" \
  "$demo_tmp" \
  "$demo_workspace"

printf 'demo notes\n' >"$demo_workspace/notes.txt"
printf 'service,status\napi,up\nworker,degraded\n' >"$demo_workspace/services.csv"
printf '%s\n' \
  'return {' \
  '  answer = 6 * 7,' \
  '  runtime = _VERSION,' \
  '}' >"$demo_workspace/hello.lua"
if [ "$demo_symbols" = nerd_font ]; then
  printf '%s\n' \
    'return quirl.config {' \
    '  prompt = { symbols = "nerd_font" },' \
    '}' >"$demo_config/config.lua"
fi

demo_path=/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin:/opt/homebrew/bin
demo_term=${TERM:-xterm-256color}
cd "$demo_workspace"

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
    QUIRL_INDEX_PATH="$demo_cache/catalog.json" \
    QUIRL_HISTORY="$demo_state/history" \
    QUIRL_RECOVERY_DIR="$demo_recovery" \
    QUIRL_SESSION_ID=release-demo \
    NO_COLOR=1 \
    "$demo_bin" "$@"
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
    QUIRL_INDEX_PATH="$demo_cache/catalog.json" \
    QUIRL_HISTORY="$demo_state/history" \
    QUIRL_RECOVERY_DIR="$demo_recovery" \
    QUIRL_SESSION_ID=release-demo \
    "$demo_bin" "$@"
fi
