# babysit demo tape — render with:  vhs assets/demo.tape
# Produces assets/demo.gif

Output assets/demo.gif

Set Shell "bash"
Set FontSize 18
Set Width 1100
Set Height 660
Set Padding 20
Set TypingSpeed 45ms
Set Theme "Dracula"

# --- setup (hidden) ---
# Use an isolated state dir so the demo never touches your real ~/.babysit.
Hide
Type "export PATH=$PWD/target/release:$PATH" Enter
Type "export BABYSIT_DIR=$(mktemp -d)/babysit" Enter
Type "clear" Enter
Show

# --- demo ---
Type "# Wrap an interactive program — it runs in the background under babysit"
Sleep 700ms
Enter
Type "babysit run -d --id demo -- bash"
Sleep 300ms
Enter
Sleep 1500ms

Type "# Drive it from outside: send input..."
Sleep 500ms
Enter
Type "babysit send -s demo 'uname -sr'"
Sleep 300ms
Enter
Sleep 900ms

Type "# ...and block until the expected output appears (race-free)"
Sleep 500ms
Enter
Type "babysit expect -s demo 'Darwin'"
Sleep 300ms
Enter
Sleep 1500ms

Type "# Read the current screen at any time, from anywhere"
Sleep 500ms
Enter
Type "babysit screenshot -s demo --trim"
Sleep 300ms
Enter
Sleep 2000ms

Type "babysit ls"
Sleep 300ms
Enter
Sleep 1800ms

Type "babysit kill -s demo"
Sleep 300ms
Enter
Sleep 1800ms
