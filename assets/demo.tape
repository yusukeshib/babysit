# babysit demo tape — render with:  vhs assets/demo.tape
# Produces assets/demo.gif
#
# Shows: run several sessions in different states, list them with `babysit ls`,
# read a session's logs, screenshot a running TUI, then attach / detach.

Output assets/demo.gif

Set Shell "bash"
Set FontSize 16
Set Width 900
Set Height 820
Set Padding 20
Set TypingSpeed 45ms
Set Theme "Dracula"

# --- setup (hidden) ---
Hide
Type "export PATH=$PWD/target/release:$HOME/.local/state/nixy/env/bin:$PATH" Enter
Type "export BABYSIT_DIR=$(mktemp -d)/babysit" Enter
Type "clear" Enter
Show

# --- start a few sessions in different states ---
Type "# Start a long-running TUI in a fixed 80x40 PTY"
Sleep 600ms
Enter
Type "babysit run -d --id top --size 80x40 -- btm"
Sleep 300ms
Enter
Sleep 1200ms

Type "# A build that succeeds, and a test run that fails — both supervised"
Sleep 600ms
Enter
Type "babysit run -d --id build -- bash -c 'echo compiling...; sleep 0.6; echo build ok'"
Sleep 300ms
Enter
Type "babysit run -d --id test  -- bash -c 'echo running tests...; sleep 0.6; echo FAILED; exit 1'"
Sleep 300ms
Enter
Sleep 2000ms

Type "# List every session and its state (running / exited code)"
Sleep 600ms
Enter
Type "babysit ls"
Sleep 300ms
Enter
Sleep 3000ms

Type "# Read a finished session's output"
Sleep 600ms
Enter
Type "babysit log -s build"
Sleep 300ms
Enter
Sleep 2500ms

# --- screenshot the live TUI ---
Type "# Read the current screen of the running TUI, from anywhere"
Sleep 600ms
Enter
Type "babysit screenshot -s top --trim"
Sleep 300ms
Enter
Sleep 3000ms

# --- attach / detach ---
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

# --- clean up ---
Type "babysit kill -s top"
Sleep 300ms
Enter
Sleep 1500ms
