### Fixed

- `make completions` finds the built binary wherever `CARGO_TARGET_DIR` puts
  it, instead of emptying `contrib/completions/pastor.bash` when the binary
  is not under `target/`. A failed run, or a CLI that prints nothing, leaves
  the checked-in scripts as they were.
