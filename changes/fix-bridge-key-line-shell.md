### Fixed

- `pastor machine authorized-key` refuses a machine name, a pastor path or a key
  that the shell sshd runs the forced command in would read as more than a
  word: a name must start with a letter or digit and hold only letters, digits,
  `.`, `_` and `-`; the path must be absolute with only those and `/`; the key
  must be a key kind, its base64 and an optional comment, with no options of its
  own. Before, `;`, `|`, `&`, `$(` and a backtick got through the name and the
  path, and options through the key.
