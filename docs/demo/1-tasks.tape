# The README gif: a task goes out to a machine, gets watched and read.
# Recorded by `make demo`, which starts the demo head and sets PASTOR_* for
# this shell (see flock.example.toml).

Output docs/demo/tasks.gif

Set Shell fish
Set FontSize 15
Set Width 1000
Set Height 520
Set Padding 16
Set Theme "Catppuccin Mocha"
Set TypingSpeed 35ms

Type "pastor machine list"
Enter
Sleep 2.5s

Type `pastor task run "Explain what this repository does, in three lines" --repo '~/ghq/github.com/cacarico/pastor'`
Enter
Sleep 8s

Type "pastor task list"
Enter
Sleep 3s

Type "pastor task read t-1 | head -n 16"
Enter
Sleep 6s
