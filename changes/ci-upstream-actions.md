### Changed

- cacarico/pastor is the source of truth; CI runs there on GitHub Actions
  (`.github/workflows/ci.yml`), free on a public repo. The homelab
  Woodpecker runner (`.woodpecker/`) is dropped: it ran out of memory on
  clippy. A fork syncs from upstream when it wants, and a change meant for
  upstream goes on its own branch, as one pull request on cacarico/pastor.
- Dependabot's pull requests merge on cacarico/pastor itself now; the
  workflow that closed them with a note (`dependabot-close.yml`) is gone,
  since that was only needed while a fork carried the merges.
