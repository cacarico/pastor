# website

The pastor website: a homepage and short docs pages, drawn like a small TUI
(panes, tabs, a status bar). It is a Hugo site with no theme. The only
script, `static/keys.js`, adds keys: 1-4 for tabs, and Vimium's j/k, h/l,
d/u, gg/G to scroll. Every tab is also a plain link.

- `layouts/home.html` is the homepage. Its commands mirror the README's
  Install and Quick start; keep them in step when those change.
- `content/docs/*.md` are the docs pages, one topic each. Front matter:
  `title`, `summary` (shown in the index), `group` (start, use, run,
  reference), `weight` (order: 1x start, 2x use, 3x run, 4x reference) and
  `manual` (an anchor in the manual for the "More in the manual" link).
- The last page is `docs/manual.md` itself, mounted by `hugo.toml`: the full
  reference stays one file. Edit the manual there, not a copy.
- The unit test that checks commands in the README and the manual reads
  `content/docs/` too, so a page that names a command or flag that does not
  exist fails `make check`.
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
