### Changed

- In the public repository, a workflow closes the pull requests Dependabot
  opens, with a comment that the update arrives with the sync from the
  private copy, where it is merged. It runs on `pull_request_target` with a
  token that can only write pull requests, and never checks their code out.
