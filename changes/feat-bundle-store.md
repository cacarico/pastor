### Added

- Bundles: a directory with a `pastor-bundle.toml` of skills, files, an
  instruction block, settings fragments per harness, MCP servers, Claude
  plugins and requirements, for the agents' homes on a flock's machines.
  `pastor bundle add <dir>` or `add --git URL --ref REF [--path SUB]`
  validates one and keeps it on the head, with the last 5 versions by
  content hash; `list`, `show` and `remove` (refused while flock.toml names
  it) go with it, all with `--json`. A bundle that looks like it holds a
  secret is refused, naming the file and line. Placing bundles on machines
  comes later.
- flock.toml: `bundles`, `bundle_vars` and `bundle_apply` (`auto` or
  `manual`, the default) on a `[[flock]]`, and `bundles` and `bundle_apply`
  on a `[[machine]]`. A head does not load a flock.toml that names a bundle
  it lacks or leaves one of its vars unset.
