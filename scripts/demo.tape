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
Set TypingSpeed 35ms
Set Theme { "name": "Quirl", "black": "#17202a", "red": "#ff6b6b", "green": "#7ee787", "yellow": "#f2cc60", "blue": "#79c0ff", "magenta": "#d2a8ff", "cyan": "#56d4dd", "white": "#f0f6fc", "brightBlack": "#6e7681", "brightRed": "#ff7b72", "brightGreen": "#aff5b4", "brightYellow": "#e3b341", "brightBlue": "#a5d6ff", "brightMagenta": "#e2c5ff", "brightCyan": "#a2f2f2", "brightWhite": "#ffffff", "background": "#0d1117", "foreground": "#f0f6fc", "selection": "#264f78", "cursor": "#56d4dd" }

Hide
Type `scripts/demo-session.sh --nerd-font "$QUIRL_DEMO_BIN"`
Enter
Wait+Screen /(v[0-9]+\.[0-9]+\.[0-9]+|dev@)/
Wait+Screen /Alt-Q Quirl/
Show
Sleep 2s

# Semantic completion comes from the running candidate's command catalog.
Type "git commit --"
Tab
Sleep 2s
Tab
Sleep 1s
Escape
Ctrl+U

# One native process graph owns the byte pipe and boolean continuation.
Type `printf 'alpha\nbeta\n' | wc -l && echo pipeline-ok`
Enter
Sleep 3s

# Replay the native pipeline from durable history using the real picker.
Ctrl+R
Sleep 1s
Tab
Sleep 1s
Enter
Sleep 1s
Enter
Sleep 3s

# The same prompt switches explicitly into structured data mode.
Ctrl+Space
Sleep 1s
Type `[{"service":"api","status":"up"},{"service":"worker","status":"degraded"}] | where status == "degraded" | select service status`
Enter
Sleep 3s
Ctrl+Space
Sleep 1s
Ctrl+Space
Wait+Screen /NORMAL/
Sleep 1s

# Native mode is explicit about the reference-shell compatibility boundary.
Type "echo <(printf hi)"
Enter
Sleep 2s
Ctrl+U

# An explicit dialect island opts back in to full Bash semantics.
Type `bash { cat <(printf hi) }`
Enter
Sleep 3s

Hide
Type "exit"
Enter
