### Fixed

- A resumed task whose start stops at a startup dialog gets its resume
  prompt once the dialog clears (or the limit resume, handover or model
  switch prompt its start meant), instead of its own prompt, which its
  session had already run. A prompt sent again because the agent did not
  take it is that one too.
