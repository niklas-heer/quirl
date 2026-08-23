#!/bin/sh
set -eu

script_dir=$(CDPATH='' cd "$(dirname "$0")" && pwd -P)
repo_dir=$(CDPATH='' cd "$script_dir/.." && pwd -P)
if [ "$#" -gt 1 ]; then
  echo "usage: scripts/record-tour.sh [release-binary]" >&2
  exit 2
fi
if [ "$#" -eq 1 ]; then
  quirl_tour_input=$1
else
  if ! command -v brew >/dev/null 2>&1; then
    echo "Homebrew is required for the release tour." >&2
    echo "Install Homebrew, then run 'brew install niklas-heer/tap/quirl'." >&2
    exit 1
  fi
  quirl_tour_prefix=$(brew --prefix quirl 2>/dev/null) || {
    echo "The Homebrew Quirl release is not installed." >&2
    echo "Run 'brew install niklas-heer/tap/quirl' and retry." >&2
    exit 1
  }
  quirl_tour_input=$quirl_tour_prefix/bin/quirl
fi
quirl_tour_dir=$(dirname "$quirl_tour_input")
quirl_tour_name=$(basename "$quirl_tour_input")
quirl_tour_dir=$(CDPATH='' cd "$quirl_tour_dir" 2>/dev/null && pwd -P) || {
  echo "Quirl binary directory not found: $quirl_tour_dir" >&2
  exit 1
}
quirl_tour_bin=$quirl_tour_dir/$quirl_tour_name
if [ ! -x "$quirl_tour_bin" ]; then
  echo "Quirl binary not found at $quirl_tour_bin" >&2
  echo "Install it with 'brew install niklas-heer/tap/quirl' or pass an executable path." >&2
  exit 1
fi
quirl_build_info=$("$quirl_tour_bin" --build-info) || {
  echo "Could not inspect the Quirl release build identity." >&2
  exit 1
}
case $quirl_build_info in
  *'"official_release": true'*) ;;
  *)
    echo "Tour refused a development build: $quirl_tour_bin" >&2
    echo "Install the official release with 'brew install niklas-heer/tap/quirl'." >&2
    exit 1
    ;;
esac

for tour_program in asciinema asg; do
  if ! command -v "$tour_program" >/dev/null 2>&1; then
    echo "Missing tour prerequisite: $tour_program" >&2
    echo "Install asciinema (https://asciinema.org) and asg (cargo install asg)." >&2
    exit 1
  fi
done

cast_path=$repo_dir/assets/quirl-tour.cast
svg_path=$repo_dir/assets/quirl-tour.svg

cd "$repo_dir"
QUIRL_DEMO_BIN="$quirl_tour_bin" \
  QUIRL_DEMO_DISPLAY=quirl \
  QUIRL_DEMO_PACE_SECONDS=4 \
  asciinema rec \
  --overwrite \
  --headless \
  --window-size 100x28 \
  --command "scripts/demo.sh" \
  --title "Quirl tour" \
  "$cast_path"

asg --theme github-dark --idle-time-limit 4 "$cast_path" "$svg_path"
