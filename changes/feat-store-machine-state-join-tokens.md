### Added

- The store, at schema 19, keeps each machine's bootstrap state
  (`machine_state`) and one-use join tokens (`join_tokens`), which a token
  redeems at most once and only before it expires. An older store gains
  both tables when it is first opened. Nothing reads or writes them yet.
