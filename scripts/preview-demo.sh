#!/bin/sh
# Render the demo and screenshot tapes from any Quirl binary, including a
# development build, into target/demo-preview. Published assets are written
# only by scripts/record-demo.sh from an official release.
set -eu

if [ "$#" -ne 1 ]; then
  echo "usage: scripts/preview-demo.sh <quirl-binary>" >&2
  exit 2
fi
for demo_program in vhs ttyd ffmpeg; do
  if ! command -v "$demo_program" >/dev/null 2>&1; then
    echo "Missing demo prerequisite: $demo_program" >&2
    exit 1
  fi
done

script_dir=$(CDPATH='' cd "$(dirname "$0")" && pwd -P)
repo_dir=$(CDPATH='' cd "$script_dir/.." && pwd -P)
preview_bin_dir=$(CDPATH='' cd "$(dirname "$1")" && pwd -P)
QUIRL_DEMO_BIN=$preview_bin_dir/$(basename "$1")
export QUIRL_DEMO_BIN
preview_dir=$repo_dir/target/demo-preview
mkdir -p "$preview_dir/assets/screenshots" "$preview_dir/website/public" "$preview_dir/target/screenshots"

unset NO_COLOR
cd "$repo_dir"
for tape in demo screenshots; do
  # Redirect every output path into the preview directory.
  sed -e "s#^Output #Output target/demo-preview/#" \
      -e "s#^Screenshot #Screenshot target/demo-preview/#" \
      "scripts/$tape.tape" >"$preview_dir/$tape.tape"
  vhs "$preview_dir/$tape.tape"
done
echo "Preview written to $preview_dir"
