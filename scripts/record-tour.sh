#!/bin/sh
set -eu

script_dir=$(CDPATH='' cd "$(dirname "$0")" && pwd -P)
repo_dir=$(CDPATH='' cd "$script_dir/.." && pwd -P)
quirl_tour_input=${1:-$repo_dir/target/release/quirl}
quirl_tour_dir=$(dirname "$quirl_tour_input")
quirl_tour_name=$(basename "$quirl_tour_input")
quirl_tour_dir=$(CDPATH='' cd "$quirl_tour_dir" 2>/dev/null && pwd -P) || {
  echo "Quirl binary directory not found: $quirl_tour_dir" >&2
  exit 1
}
quirl_tour_bin=$quirl_tour_dir/$quirl_tour_name
if [ ! -x "$quirl_tour_bin" ]; then
  echo "Quirl binary not found at $quirl_tour_bin" >&2
  echo "Run 'cargo build --release -p quirl-cli' or pass an executable path." >&2
  exit 1
fi

for tour_program in asciinema asg; do
  if ! command -v "$tour_program" >/dev/null 2>&1; then
    echo "Missing tour prerequisite: $tour_program" >&2
    echo "Install asciinema (https://asciinema.org) and asg (cargo install asg)." >&2
    exit 1
  fi
done

cast_path=$repo_dir/assets/quirl-tour.cast
svg_path=$repo_dir/assets/quirl-tour.svg

QUIRL_DEMO_PACE_SECONDS=2 asciinema rec \
  --overwrite \
  --headless \
  --window-size 100x28 \
  --command "$script_dir/demo.sh $quirl_tour_bin" \
  --title "Quirl tour" \
  "$cast_path"

asg --theme github-dark --idle-time-limit 1.5 "$cast_path" "$svg_path"
