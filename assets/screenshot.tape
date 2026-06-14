# babysit screenshot demo — render with:  vhs assets/screenshot.tape
# Produces assets/screenshot.gif
#
# Shows: launch a full-screen TUI (bottom) in a 120x80 PTY session, then read
# the current screen from another terminal with `babysit screenshot`.

Output assets/screenshot.gif

Set Shell "bash"
Set FontSize 16
Set Width 900
Set Height 760
Set Padding 20
Set TypingSpeed 45ms
Set Theme "Dracula"

# --- setup (hidden) ---
Hide
Type "export PATH=$PWD/target/release:$HOME/.local/state/nixy/env/bin:$PATH" Enter
Type "export BABYSIT_DIR=$(mktemp -d)/babysit" Enter
Type "clear" Enter
Show

# --- demo ---
Type "# Start a full-screen TUI in a fixed 80x40 PTY — runs in the background"
Sleep 700ms
Enter
Type "babysit run -d --id top --size 80x40 -- btm"
Sleep 300ms
Enter
Sleep 2500ms

Type "# Read the current screen at any time, from anywhere"
Sleep 600ms
Enter
Type "babysit screenshot -s top --trim"
Sleep 300ms
Enter
Sleep 3000ms

Type "# Take over interactively, then detach with Ctrl-\ Ctrl-\ — it keeps running"
Sleep 600ms
Enter
Type "babysit attach -s top"
Sleep 300ms
Enter
Sleep 2500ms
Ctrl+\
Ctrl+\
Sleep 2000ms

Type "babysit kill -s top"
Sleep 300ms
Enter
Sleep 1200ms
