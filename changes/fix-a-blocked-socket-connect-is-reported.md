### Fixed

- A connect to `pastor.sock` denied for permissions (EPERM or EACCES, most
  often a sandbox around the command) fails with `socket_denied` and says
  the command may be sandboxed. It was `head_unresponsive`, which reads as
  a sick head and sent agents into retries.
