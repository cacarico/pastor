# A scheduled job: written, listed, fired, and the task it made.
# Recorded by `make demo` against a demo head (see flock.example.toml).

Output docs/demo/jobs.gif

Set Shell fish
Set FontSize 15
Set Width 1000
Set Height 520
Set Padding 16
Set Theme "Catppuccin Mocha"
Set TypingSpeed 35ms

Env PASTOR_CONFIG_DIR docs/demo/local
Env PASTOR_STATE_DIR docs/demo/local/state

Type "cat docs/demo/local/jobs/hourly.toml"
Enter
Sleep 3s

Type "pastor job list"
Enter
Sleep 2.5s

Type "pastor job run hourly"
Enter
Sleep 3s

Type "pastor task list --job hourly"
Enter
Sleep 4s
