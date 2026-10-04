# Run through scripts/record-demo.sh. It verifies the prebuilt release digest,
# VHS/ttyd/ffmpeg, and the JetBrainsMono Nerd Font before opening this tape.
# scripts/preview-demo.sh renders a development build to target/demo-preview.
Output assets/quirl-demo.gif
Output website/public/quirl-demo.mp4
Output website/public/quirl-demo.webm

Require bash

Set Shell "bash"
Set WaitTimeout 120s
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
Set CursorBlink false
Set Theme { "name": "Quirl", "black": "#17202a", "red": "#ff6b6b", "green": "#7ee787", "yellow": "#f2cc60", "blue": "#79c0ff", "magenta": "#d2a8ff", "cyan": "#56d4dd", "white": "#f0f6fc", "brightBlack": "#6e7681", "brightRed": "#ff7b72", "brightGreen": "#aff5b4", "brightYellow": "#e3b341", "brightBlue": "#a5d6ff", "brightMagenta": "#e2c5ff", "brightCyan": "#a2f2f2", "brightWhite": "#ffffff", "background": "#0d1117", "foreground": "#f0f6fc", "selection": "#264f78", "cursor": "#56d4dd" }

Hide
Type `scripts/demo-session.sh --nerd-font "$QUIRL_DEMO_BIN"`
Enter
Wait+Screen /Alt-Q Quirl/
Set WaitTimeout 40s

# Open on a brand card instead of an empty prompt.
Type "./tour intro"
Enter
Wait+Screen /YOUR SHELL HABITS/
Show
Sleep 4s

# 1 · Familiar commands, and completion that knows the project.
Hide
Ctrl+L
Show
Type "# familiar commands · completion that knows your project"
Sleep 2s
Ctrl+U
Type `printf 'api\nworker\nsearch\n' | wc -l && echo 'pipeline: healthy'`
Enter
Sleep 2500ms
Type "git checkout fe"
Sleep 600ms
Tab
Sleep 3500ms
Escape
Ctrl+U

# 2 · Pasted Bash keeps working, including setup code that changes the session.
Hide
Ctrl+L
Type "./tour habits"
Enter
Wait+Screen /PASTE IT/
Show
Sleep 4s
Hide
Ctrl+L
Show
Type `source .env && echo "deploying to $DEPLOY_REGION via $API_URL"`
Sleep 800ms
Enter
Sleep 2500ms
# Present the loop as a paste: it arrives whole, then runs.
Hide
Type@2ms `for svc in api worker search; do echo "✓ $svc deployed to $DEPLOY_REGION"; done`
Show
Sleep 1200ms
Enter
Sleep 3s
Type `eval "$(ssh-agent -s)" && echo "agent $SSH_AGENT_PID is ready"`
Sleep 800ms
Enter
Sleep 3s
Hide
Type "kill $SSH_AGENT_PID"
Enter
Sleep 500ms

# 3 · Typed data, named once, then readable verbs.
Ctrl+L
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
Type "open services.json"
Sleep 800ms
Enter
Sleep 3s
Hide
Ctrl+L
Show
Type@22ms "open services.json | where latency > 50 | sort-by latency -r | select service region latency"
Sleep 1200ms
Enter
Sleep 3500ms
Type "open services.json | group-by status"
Sleep 800ms
Enter
Sleep 3s
Type "open services.json | get latency | math avg"
Sleep 800ms
Enter
Sleep 3s

# Lua is the small, resource-bounded extension surface.
Hide
Ctrl+L
Type "mode normal"
Enter
Wait+Screen /NORMAL/
Show
Type `lua return { answer = 6 * 7, runtime = _VERSION, sandboxed = true }`
Sleep 800ms
Enter
Sleep 3s

# End on the product boundaries and the install line.
Hide
Ctrl+L
Type "./tour proof"
Enter
Wait+Screen /EXPLICIT BOUNDARIES/
Show
Sleep 6s

Hide
Type "exit"
Enter
