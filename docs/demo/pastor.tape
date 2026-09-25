# The README gif: a task goes out to a machine, gets watched and read.
# Recorded by `make demo` against a demo head (see flock.example.toml).

Output docs/demo/pastor.gif

Set Shell fish
Set FontSize 15
Set Width 1000
Set Height 520
Set Padding 16
Set Theme "Catppuccin Mocha"
Set TypingSpeed 35ms

Env PASTOR_CONFIG_DIR docs/demo/local
Env PASTOR_STATE_DIR docs/demo/local/state

Type "pastor machine list"
Enter
Sleep 2.5s

Type 'pastor task run "Run the test suite and fix what broke" --repo ~/work/api'
Enter
Sleep 3s

Type "pastor task list"
Enter
Sleep 2.5s

Type "pastor task read t-1"
Enter
Sleep 5s
