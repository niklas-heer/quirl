#!/bin/sh
set -eu

card=${1:-}
if [ "$#" -ne 1 ]; then
  echo "usage: demo-card.sh <intro|data|proof>" >&2
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
    printf '    %sBASH MUSCLE MEMORY%s   %s→%s   %sTYPED DATA%s   %s→%s   %sLOCAL INTENT%s\n\n' \
      "$magenta" "$reset" "$muted" "$reset" "$cyan" "$reset" \
      "$muted" "$reset" "$green" "$reset"
    printf '    %sOne fast Rust binary. Three explicit modes. No mystery grammar.%s\n\n' \
      "$dim" "$reset"
    rule
    ;;
  data)
    clear_card
    printf '\n\n'
    rule
    printf '\n'
    printf '    %s%sDATA THAT STAYS DATA%s\n\n' "$bold" "$cyan" "$reset"
    printf '    %sopen%s      JSON → typed records\n' "$yellow" "$reset"
    printf '    %swhere%s     compare numbers as numbers\n' "$magenta" "$reset"
    printf '    %ssort%s      order by a typed field\n' "$green" "$reset"
    printf '    %sselect%s    keep only what matters\n\n' "$cyan" "$reset"
    printf '    %sReadable verbs. Typed all the way through.%s\n\n' "$dim" "$reset"
    rule
    ;;
  proof)
    clear_card
    printf '\n'
    rule
    printf '\n'
    printf '    %s%sRELEASE PROOF%s   %smeasured on the 0.1 release artifact%s\n\n' \
      "$bold" "$green" "$reset" "$dim" "$reset"
    printf '    %s4.46 MiB%s release artifact     %s19.2 ms%s first-prompt P95\n' \
      "$magenta" "$reset" "$cyan" "$reset"
    printf '    %s1,100+%s Rust tests             %s11%s real-PTY journeys\n' \
      "$yellow" "$reset" "$green" "$reset"
    printf '    %sLua 5.4%s, resource bounded     %sAI suggests%s — you decide\n\n' \
      "$cyan" "$reset" "$magenta" "$reset"
    printf '    %sFamiliar where it should be. Explicit where it matters.%s\n\n' \
      "$white" "$reset"
    rule
    ;;
  *)
    echo "unknown demo card: $card" >&2
    echo "usage: demo-card.sh <intro|data|proof>" >&2
    exit 2
    ;;
esac
