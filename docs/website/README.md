# website

The pastor website: a homepage and short docs pages, drawn like a small TUI
(panes, tabs, a status bar). It is a Hugo site with no theme. `static/keys.js`
adds keys: 1-4 for tabs, and Vimium's j/k, h/l, d/u, gg/G to scroll. Every
tab is also a plain link.

- `layouts/home.html` is the homepage. Its install command mirrors the
  README's Install; keep them in step when it changes.
- `data/demo.toml` is the homepage's live terminal: two acts of commands and
  their output, copied from pastor's own tables with placeholder names.
  `static/term.js` types it out; without scripts, or with reduced motion, it
  reads as a plain transcript. The unit test `website_demo_commands_are_real`
  fails when one of its commands or flags does not exist.
- `content/docs/` holds five sections, each a folder with an `_index.md`:
  `start`, `concepts` (one page per part of pastor, explaining it),
  `deploy` (the deployment modes: solo, flock, remote, as a service),
  `examples` (real workflows to copy) and `reference` (tables to look up).
  A page's front matter is `title`, `summary` (shown in the indexes),
  `weight` (its order in the section) and, for a page that moved,
  `aliases` with its old URL. The nav, the pager and the status bar all
  read the order from `layouts/_partials/docpages.html`.
- The site is for people learning pastor. `docs/manual.md` stays in the
  repo for agents and is not mounted: what a reader needs goes in a page.
- `content/docs/reference/cli.md` is generated from the clap definitions
  by `make cli-reference`: the prose above its "Generated" marker is kept,
  everything below is rewritten. The test `website_cli_reference_is_current`
  fails `make check` when the page is stale.
- `layouts/home.llms.txt` renders `llms.txt` at the site's root, an output
  format of the homepage set in `hugo.toml`. It lists the docs pages in the
  nav's order, so a new page shows up there by itself.
- The unit test that checks commands in the README and the manual reads
  every page under `content/docs/` too, so a page that names a command or
  flag that does not exist fails `make check`.
- `sh` and `bash` code blocks go through `layouts/_partials/cmd.html`, which
  adds a `$ ` prompt (not copied with the text) and dims comments. Other
  blocks are plain.
- `static/style.css` holds the slate palette. Every text colour passes WCAG
  AA on the background and on code blocks; check a new one before adding it.
- The version in the header comes from `Cargo.toml`.

`make site` builds into `docs/website/public/`; `make site-serve` serves it
with live reload. Both need `hugo` (from mise).

`.github/workflows/website.yml` builds it on pushes to main and on pull
requests that touch the docs, but publishes it to GitHub Pages only from
`cacarico/pastor`; elsewhere Pages is off and only the build runs. To see
the site before then, download the pull request's `website` artifact from
its checks, unzip it and serve the folder (`python3 -m http.server` in it),
or run `make site-serve`. Opening `index.html` from disk does not work: the
pretty URLs (`docs/install/`) show a folder listing there.

The design rounds that led here live in the private `cacarico-layouts` repo.
