# Still images for the README and website, rendered from the same isolated
# demo session as scripts/demo.tape. Run through scripts/record-demo.sh (or
# scripts/preview-demo.sh for a development build).
Output target/screenshots/session.gif

Require bash

Set Shell "bash"
Set WaitTimeout 120s
Set FontFamily "JetBrainsMono Nerd Font"
Set FontSize 18
Set Width 1200
Set Height 600
Set Padding 28
Set Margin 16
Set MarginFill "#070a10"
Set BorderRadius 12
Set WindowBar ColorfulRight
Set WindowBarSize 34
Set LineHeight 1.1
Set TypingSpeed 10ms
Set CursorBlink false
Set Theme { "name": "Quirl", "black": "#17202a", "red": "#ff6b6b", "green": "#7ee787", "yellow": "#f2cc60", "blue": "#79c0ff", "magenta": "#d2a8ff", "cyan": "#56d4dd", "white": "#f0f6fc", "brightBlack": "#6e7681", "brightRed": "#ff7b72", "brightGreen": "#aff5b4", "brightYellow": "#e3b341", "brightBlue": "#a5d6ff", "brightMagenta": "#e2c5ff", "brightCyan": "#a2f2f2", "brightWhite": "#ffffff", "background": "#0d1117", "foreground": "#f0f6fc", "selection": "#264f78", "cursor": "#56d4dd" }

Hide
Type `scripts/demo-session.sh --nerd-font "$QUIRL_DEMO_BIN"`
Enter
Wait+Screen /Alt-Q Quirl/
Set WaitTimeout 40s

# Completion: Git branches from the project, with documentation alongside.
Ctrl+L
Type "git checkout fe"
Tab
Sleep 2s
Show
Sleep 300ms
Screenshot assets/screenshots/completion.png
Hide
Escape
Ctrl+U

# Pasted Bash: setup code and a loop, each keeping the session's variables.
Ctrl+L
Type `source .env && echo "deploying to $DEPLOY_REGION via $API_URL"`
Enter
Sleep 1s
Type `for svc in api worker search; do echo "✓ $svc deployed to $DEPLOY_REGION"; done`
Enter
Sleep 1500ms
Show
Sleep 300ms
Screenshot assets/screenshots/posix-habits.png
Hide

# Typed data: a colored table straight from a file.
Ctrl+L
Ctrl+Space
Wait+Screen /DATA/
Type "open services.json"
Enter
Sleep 1500ms
Show
Sleep 300ms
Screenshot assets/screenshots/data-table.png
Hide

# A readable pipeline, grouping, and exact math.
Ctrl+L
Type "open services.json | where latency > 50 | sort-by latency -r | select service region latency"
Enter
Sleep 1500ms
Type "open services.json | group-by status"
Enter
Sleep 1200ms
Type "open services.json | get latency | math avg"
Enter
Sleep 1500ms
Show
Sleep 300ms
Screenshot assets/screenshots/data-pipeline.png
Hide

# History search: Ctrl-R ranks this directory's commands first.
Ctrl+L
Type "mode normal"
Enter
Wait+Screen /NORMAL/
Ctrl+L
Ctrl+R
Sleep 1500ms
Show
Sleep 300ms
Screenshot assets/screenshots/history.png
Hide
Escape
Sleep 500ms

Type "exit"
Enter
