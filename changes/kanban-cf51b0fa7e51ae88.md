### Added

- pastor reads opencode's usage limits as it reads Claude's: an opencode
  task whose provider ran out (OpenAI's quota or rate limit, Anthropic's 429
  or 529, a ChatGPT sign-in's usage limit) goes `waiting` or moves to its
  next model instead of `done`. Out of quota, it waits
  `retry_after_no_credit`; at its time it starts again from its prompt with
  the handover, since opencode keeps no session.
