#!/bin/sh
set -eu

if [ "$#" -ne 2 ]; then
  echo "usage: scripts/record-demo.sh <release-binary> <expected-sha256>" >&2
  echo "Build and measure the exact candidate before recording it." >&2
  exit 2
fi

demo_bin_input=$1
expected_sha256=$(printf '%s' "$2" | tr 'A-F' 'a-f')
case $expected_sha256 in
  *[!0-9a-f]* | '')
    echo "expected SHA-256 must be exactly 64 hexadecimal characters" >&2
    exit 2
    ;;
esac
if [ "${#expected_sha256}" -ne 64 ]; then
  echo "expected SHA-256 must be exactly 64 hexadecimal characters" >&2
  exit 2
fi

script_dir=$(CDPATH='' cd "$(dirname "$0")" && pwd -P)
repo_dir=$(CDPATH='' cd "$script_dir/.." && pwd -P)
demo_bin_dir=$(dirname "$demo_bin_input")
demo_bin_name=$(basename "$demo_bin_input")
demo_bin_dir=$(CDPATH='' cd "$demo_bin_dir" 2>/dev/null && pwd -P) || {
  echo "Release binary directory not found: $demo_bin_dir" >&2
  exit 1
}
demo_bin=$demo_bin_dir/$demo_bin_name
if [ ! -x "$demo_bin" ]; then
  echo "Release binary is not executable: $demo_bin" >&2
  exit 1
fi
demo_build_info=$("$demo_bin" --build-info) || {
  echo "Could not inspect the Quirl release build identity." >&2
  exit 1
}
case $demo_build_info in
  *'"official_release": true'*) ;;
  *)
    echo "Recording refused a development build: $demo_bin" >&2
    echo "Install the official release with 'brew install niklas-heer/tap/quirl'." >&2
    exit 1
    ;;
esac

for demo_program in vhs ttyd ffmpeg; do
  if ! command -v "$demo_program" >/dev/null 2>&1; then
    echo "Missing demo prerequisite: $demo_program" >&2
    echo "VHS requires both ttyd and ffmpeg; see https://github.com/charmbracelet/vhs" >&2
    exit 1
  fi
done

demo_font='JetBrainsMono Nerd Font'
if command -v fc-match >/dev/null 2>&1; then
  matched_families=$(fc-match -f '%{family}\n' "$demo_font")
  case $matched_families in
    *JetBrainsMono*Nerd*Font*) ;;
    *)
      echo "Missing demo font: $demo_font" >&2
      echo "Install the JetBrainsMono Nerd Font before recording scripts/demo.tape." >&2
      exit 1
      ;;
  esac
elif command -v system_profiler >/dev/null 2>&1; then
  if ! system_profiler SPFontsDataType | grep -F "$demo_font" >/dev/null 2>&1; then
    echo "Missing demo font: $demo_font" >&2
    echo "Install the JetBrainsMono Nerd Font before recording scripts/demo.tape." >&2
    exit 1
  fi
else
  echo "Cannot verify the required font: install fontconfig's fc-match." >&2
  exit 1
fi

if command -v sha256sum >/dev/null 2>&1; then
  actual_sha256=$(sha256sum "$demo_bin" | awk '{print $1}')
elif command -v shasum >/dev/null 2>&1; then
  actual_sha256=$(shasum -a 256 "$demo_bin" | awk '{print $1}')
else
  echo "A SHA-256 tool is required (sha256sum or shasum)." >&2
  exit 1
fi
if [ "$actual_sha256" != "$expected_sha256" ]; then
  echo "Release binary digest does not match the measured candidate." >&2
  echo "expected: $expected_sha256" >&2
  echo "actual:   $actual_sha256" >&2
  exit 1
fi

QUIRL_DEMO_BIN=$demo_bin
export QUIRL_DEMO_BIN
cd "$repo_dir"
vhs scripts/demo.tape
