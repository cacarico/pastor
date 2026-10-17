### Changed

- CI runs on GitHub Actions (`.github/workflows/ci.yml`). The Woodpecker
  pipelines (`.woodpecker/`) are removed.
- Dependabot's pull requests merge like any other; the workflow that closed
  them with a note (`dependabot-close.yml`) is gone.
