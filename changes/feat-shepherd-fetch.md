### Added

- A pull machine's shepherd can fetch an artifact from the head's cache over
  its locked bridge: the request is a line on stdin (`artifact_locate`), never
  an argument, and the reply is one line with the file's SHA-256 and size,
  then its bytes. Only what the cache already holds is sent, named by plain
  path parts, and a file outside the cache is never read. An agent's bridge
  refuses it.
