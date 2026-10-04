#!/bin/sh
set -eu

card=${1:-}
if [ "$#" -ne 1 ]; then
  echo "usage: demo-card.sh <intro|habits|data|proof>" >&2
  exit 2
fi

reset=$(printf '\033[0m')
bold=$(printf '\033[1m')
dim=$(printf '\033[2m')
magenta=$(printf '\033[38;2;210;168;255m')
cyan=$(printf '\033[38;2;86;212;221m')
green=$(printf '\033[38;2;126;231;135m')
yellow=$(printf '\033[38;2;242;204;96m')
white=$(printf '\033[38;2;240;246;252m')
muted=$(printf '\033[38;2;139;148;158m')

clear_card() {
  printf '\033[2J\033[H'
}

rule() {
  printf '    %s━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━%s\n' \
    "$muted" "$reset"
}

case $card in
  intro)
    clear_card
    printf '\n\n'
    rule
    printf '\n'
    printf '    %s%sQ U I R L%s\n' "$bold" "$magenta" "$reset"
    printf '    %sA WELL-STIRRED SHELL%s\n\n' "$white" "$reset"
    printf '    %sYOUR SHELL HABITS%s   %s→%s   %sTYPED DATA%s   %s→%s   %sBOUNDED LUA%s\n\n' \
      "$magenta" "$reset" "$muted" "$reset" "$cyan" "$reset" \
      "$muted" "$reset" "$green" "$reset"
    printf '    %sZsh-grade completion. Nushell-style tables. One fast Rust binary.%s\n\n' \
      "$dim" "$reset"
    rule
    ;;
  habits)
    clear_card
    printf '\n\n'
    rule
    printf '\n'
    printf '    %s%sPASTE IT. IT RUNS.%s\n\n' "$bold" "$magenta" "$reset"
    printf '    %sfor ... done%s           loops and groups straight from a tutorial\n' "$yellow" "$reset"
    printf '    %ssource .env%s            setup scripts keep their variables\n' "$cyan" "$reset"
    printf '    %seval "$(...)"%s          ssh-agent, brew shellenv, and friends\n' "$green" "$reset"
    printf '    %sssh host cmd%s           safe as your login shell\n\n' "$magenta" "$reset"
    printf '    %sPOSIX where tools expect it. Quirl where you want more.%s\n\n' "$dim" "$reset"
    rule
    ;;
  data)
    clear_card
    printf '\n\n'
    rule
    printf '\n'
    printf '    %s%sDATA THAT STAYS DATA%s\n\n' "$bold" "$cyan" "$reset"
    printf '    %sopen%s       JSON, YAML, TOML, CSV → typed records\n' "$yellow" "$reset"
    printf '    %swhere%s      numbers as numbers, sizes as sizes\n' "$magenta" "$reset"
    printf '    %ssort-by%s    order by a typed field\n' "$green" "$reset"
    printf '    %sgroup-by%s   bucket rows by a value\n' "$cyan" "$reset"
    printf '    %smath%s       exact sums and averages\n\n' "$yellow" "$reset"
    printf '    %sNushell verbs. Typed all the way through. Tables that fit.%s\n\n' "$dim" "$reset"
    rule
    ;;
  proof)
    clear_card
    printf '\n'
    rule
    printf '\n'
    printf '    %s%sEXPLICIT BOUNDARIES%s\n\n' "$bold" "$green" "$reset"
    printf '    %sNative commands%s and byte pipelines, POSIX for everything else\n' "$magenta" "$reset"
    printf '    %sTyped records%s in an explicit data mode\n' "$cyan" "$reset"
    printf '    %sBounded Lua%s for scripts and extensions\n' "$yellow" "$reset"
    printf '    %sCodex planning%s proposes commands; you review every one\n\n' \
      "$green" "$reset"
    printf '    %sbrew install niklas-heer/tap/quirl%s\n\n' "$bold" "$reset"
    printf '    %sFamiliar where it should be. Explicit where it matters.%s\n\n' \
      "$white" "$reset"
    rule
    ;;
  *)
    echo "unknown demo card: $card" >&2
    echo "usage: demo-card.sh <intro|habits|data|proof>" >&2
    exit 2
    ;;
esac
