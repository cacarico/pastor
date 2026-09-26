# Connectors: install one from GitHub, see it in full, and a job that uses it.
# Recorded by `make demo`, which starts the demo head and sets PASTOR_* for
# this shell (see flock.example.toml).

Output docs/demo/connectors.gif

Set Shell fish
Set FontSize 15
Set Width 1000
Set Height 620
Set Padding 16
Set Theme "Catppuccin Mocha"
Set TypingSpeed 35ms

Type "pastor connector install cacarico/pastor-connectors/github-issues --yes"
Enter
Sleep 6s

Type "clear"
Enter
Type "pastor connector describe "
Tab
Sleep 1s
Enter
Sleep 5s

Type "clear"
Enter
Type "cat docs/demo/local/jobs/issues.toml"
Enter
Sleep 4s

Type "pastor job list"
Enter
Sleep 4s
