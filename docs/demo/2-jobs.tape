# A scheduled job: written, listed, fired, and the task it made.
# Recorded by `make demo`, which starts the demo head and sets PASTOR_* for
# this shell (see flock.example.toml).

Output docs/demo/jobs.gif

Set Shell fish
Set FontSize 15
Set Width 1000
Set Height 520
Set Padding 16
Set Theme "Catppuccin Mocha"
Set TypingSpeed 35ms

Type "cat docs/demo/local/jobs/morning.toml"
Enter
Sleep 3s

Type "pastor job list"
Enter
Sleep 2.5s

Type "pastor job run morning"
Enter
Sleep 8s

Type "pastor task list --job morning"
Enter
Sleep 4s
