### Fixed

- The `authorized_keys` editor refuses a link that takes the file's place while
  an edit is made, not only one that was there at the start, and says where a
  relative link points. An edit that finds the file changing under it eight
  times in a row stops with `authorized_keys_busy` instead of a plain error.
