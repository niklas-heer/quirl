# Run through scripts/record-demo.sh. It verifies the prebuilt candidate digest,
# VHS/ttyd/ffmpeg, and the JetBrainsMono Nerd Font before opening this tape.
Output assets/quirl-demo.gif
Output website/public/quirl-demo.mp4
Output website/public/quirl-demo.webm

Require bash

Set Shell "bash"
Set FontFamily "JetBrainsMono Nerd Font"
Set FontSize 18
Set Width 1200
Set Height 720
Set Padding 28
Set Margin 16
Set MarginFill "#070a10"
Set BorderRadius 12
Set WindowBar ColorfulRight
Set WindowBarSize 34
Set LineHeight 1.1
Set TypingSpeed 28ms
Set Theme { "name": "Quirl", "black": "#17202a", "red": "#ff6b6b", "green": "#7ee787", "yellow": "#f2cc60", "blue": "#79c0ff", "magenta": "#d2a8ff", "cyan": "#56d4dd", "white": "#f0f6fc", "brightBlack": "#6e7681", "brightRed": "#ff7b72", "brightGreen": "#aff5b4", "brightYellow": "#e3b341", "brightBlue": "#a5d6ff", "brightMagenta": "#e2c5ff", "brightCyan": "#a2f2f2", "brightWhite": "#ffffff", "background": "#0d1117", "foreground": "#f0f6fc", "selection": "#264f78", "cursor": "#56d4dd" }

Hide
Type `scripts/demo-session.sh --nerd-font --ai-ready "$QUIRL_DEMO_BIN"`
Enter
Wait+Screen /v[0-9]+\.[0-9]+\.[0-9]+/
Wait+Screen /Alt-Q Quirl/

# Open on a brand card instead of an empty prompt.
Type "./tour intro"
Enter
Wait+Screen /BASH MUSCLE MEMORY/
Show
Sleep 4s

# Normal mode starts with the shell grammar people already know.
Hide
Ctrl+L
Show
Type "# familiar commands · native pipes · real process status"
Sleep 2s
Ctrl+U
Type `printf 'api\nworker\njobs\n' | wc -l && echo 'pipeline: healthy'`
Enter
Sleep 3s
Type "git commit --am"
Tab
Sleep 3s
Escape
Ctrl+U

# Give typed data its own hero beat and name every verb before it moves.
Hide
Type "./tour data"
Enter
Wait+Screen /DATA THAT STAYS DATA/
Show
Sleep 4s
Hide
Ctrl+L
Ctrl+Space
Wait+Screen /DATA/
Show

# First show JSON as JSON so syntax color and typed literals are unmistakable.
Type@20ms `[{"service":"search","latency":91},{"service":"billing","latency":54}]`
Sleep 1500ms
Enter
Sleep 3s

# Then reveal the file adapter as a typed source, before transforming it.
Hide
Ctrl+L
Show
Type "open services.json"
Sleep 1s
Enter
Sleep 3s
Hide
Ctrl+L
Show
Type `open services.json | where status == "degraded"`
Sleep 1s
Enter
Sleep 3s
Hide
Ctrl+L
Show
Type@22ms "open services.json | where latency > 50 | sort latency desc | select service region latency"
Sleep 1500ms
Enter
Sleep 4s

# AI mode searches local knowledge, then inserts a suggestion for review.
Hide
Ctrl+L
Ctrl+Space
Wait+Screen /AI/
Show
Type "copy a directory while preserving permissions"
Sleep 6s
Enter
Wait+Screen /NORMAL/
Sleep 3s
Ctrl+U

# Full Bash compatibility stays explicit and visible.
Hide
Ctrl+L
Show
Type "# full Bash when you need it · explicit when you cross the boundary"
Sleep 2s
Ctrl+U
Type `bash { cat <(printf 'Bash, on purpose.\n') }`
Enter
Sleep 3s

# Lua is the small, resource-bounded extension surface.
Type `lua return { answer = 6 * 7, sandboxed = true }`
Enter
Sleep 3s

# Finish on proof, not promises.
Hide
Ctrl+L
Type "./tour proof"
Enter
Wait+Screen /RELEASE PROOF/
Show
Sleep 6s

Hide
Type "exit"
Enter
