# Run through scripts/record-demo.sh. It verifies the prebuilt candidate digest,
# VHS/ttyd/ffmpeg, and the JetBrainsMono Nerd Font before opening this tape.
Output assets/quirl-demo.gif

Require bash

Set Shell "bash"
Set FontFamily "JetBrainsMono Nerd Font"
Set FontSize 18
Set Width 1200
Set Height 720
Set Padding 24
Set TypingSpeed 42ms
Set Theme { "name": "Quirl", "black": "#17202a", "red": "#ff6b6b", "green": "#7ee787", "yellow": "#f2cc60", "blue": "#79c0ff", "magenta": "#d2a8ff", "cyan": "#56d4dd", "white": "#f0f6fc", "brightBlack": "#6e7681", "brightRed": "#ff7b72", "brightGreen": "#aff5b4", "brightYellow": "#e3b341", "brightBlue": "#a5d6ff", "brightMagenta": "#e2c5ff", "brightCyan": "#a2f2f2", "brightWhite": "#ffffff", "background": "#0d1117", "foreground": "#f0f6fc", "selection": "#264f78", "cursor": "#56d4dd" }

Hide
Type `scripts/demo-session.sh --nerd-font --ai-ready "$QUIRL_DEMO_BIN"`
Enter
Wait+Screen /v[0-9]+\.[0-9]+\.[0-9]+/
Wait+Screen /Alt-Q Quirl/
Show
Sleep 3s

# Start with the mental model. Ctrl-Space is reliable in recorded and remote PTYs.
Type "# THREE MODES — Ctrl-Space changes grammar"
Sleep 4s
Ctrl+U

# Normal mode preserves shell muscle memory and owns process lifecycle.
Type "# NORMAL — familiar commands, pipes, and status"
Sleep 4s
Ctrl+U
Type "git commit --"
Tab
Sleep 4s
Tab
Sleep 2s
Escape
Ctrl+U

# One native process graph owns the byte pipe and boolean continuation.
Type `printf 'alpha\nbeta\n' | wc -l && echo pipeline-ok`
Enter
Sleep 4s

# Replay the native pipeline from durable history using the real picker.
Type "# HISTORY — inspect before choosing what runs"
Sleep 4s
Ctrl+U
Ctrl+R
Sleep 4s
Tab
Sleep 2s
Enter
Sleep 2s
Enter
Sleep 4s

# The same prompt switches explicitly into typed data mode.
Ctrl+Space
Wait+Screen /DATA/
Sleep 2s
Type "# DATA — filter typed fields before anything becomes display text"
Sleep 4s
Ctrl+U
Type `[{"service":"api","region":"eu","latency_ms":18},{"service":"worker","region":"us","latency_ms":91},{"service":"jobs","region":"eu","latency_ms":54}] | where latency_ms > 50 | sort latency_ms desc | select service region latency_ms`
Enter
Sleep 5s

# AI mode searches local command knowledge and returns to Normal for review.
Ctrl+Space
Wait+Screen /AI/
Sleep 2s
Type "show git branch status"
Sleep 6s
Enter
Wait+Screen /NORMAL/
Wait+Screen /git status/
Sleep 5s
Ctrl+U

# Compatibility stays explicit without forcing Quirl to imitate every shell.
Type "# BASH — explicit island, full compatibility"
Sleep 4s
Ctrl+U
Type `bash { cat <(printf hi) }`
Enter
Sleep 4s

# Lua is the bounded extension surface, not another invisible prompt grammar.
Type "# LUA SDK — sandboxed, bounded extension logic"
Sleep 4s
Ctrl+U
Type `lua return { answer = 6 * 7, safe = true }`
Enter
Sleep 5s

Hide
Type "exit"
Enter
