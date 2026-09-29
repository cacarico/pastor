#!/usr/bin/env bash
# Changelog entries as files. A pull request adds its entry as
# changes/<branch>.md (slashes in the branch name become dashes) instead of
# a line under Unreleased in CHANGELOG.md: every PR editing the same lines
# made each merge conflict with the next PR, and separate files never do.
#
#   scripts/changelog.sh check                   CHANGELOG.md has no Unreleased
#                                                section and every change file
#                                                is well formed (make check)
#   scripts/changelog.sh notes                   print the pending entries as
#                                                one section body
#   scripts/changelog.sh gather VERSION [DATE]   write them into CHANGELOG.md as
#                                                "## VERSION - DATE" and remove
#                                                the files (the release PR)
#
# A change file holds Keep a Changelog subsections ("### Added", "### Fixed",
# ...) with their entries. gather merges them, one subsection per heading in
# the usual order, entries in the order their files reached main (the first
# parent history of HEAD, merge commits included); files git does not know
# yet come last, by name. Run it from the repository root.
set -euo pipefail

headings="Added Changed Deprecated Removed Fixed Security"

die() {
	echo "changelog: $*" >&2
	exit 1
}

change_files() {
	local f
	for f in changes/*.md; do
		[ -e "$f" ] || continue
		[ "${f##*/}" = README.md ] && continue
		printf '%s\n' "$f"
	done
}

# The change files in merge order: by the position of the latest commit on
# HEAD's first-parent line that added each one.
ordered_files() {
	{
		if git rev-parse --git-dir >/dev/null 2>&1; then
			git log --reverse --first-parent --diff-merges=first-parent \
				--diff-filter=A --name-only --format= -- changes/ 2>/dev/null || true
		fi
		echo --
		change_files
	} | awk '
		$0 == "--" { files = 1; next }
		!files { if ($0 != "") pos[$0] = ++n; next }
		{ printf "%012d\t%s\n", ($0 in pos) ? pos[$0] : 999999999999, $0 }
	' | sort | cut -f2-
}

check() {
	local bad=0 f
	if grep -qi '^## *unreleased' CHANGELOG.md; then
		echo "changelog: CHANGELOG.md has an Unreleased section; add the entry as changes/<branch>.md instead (see changes/README.md)" >&2
		bad=1
	fi
	if [ -d changes ]; then
		for f in changes/*; do
			[ -e "$f" ] || continue
			case "$f" in
			*.md) [ -d "$f" ] || continue ;;
			esac
			echo "changelog: $f: only changes/*.md files belong in changes/" >&2
			bad=1
		done
	fi
	while IFS= read -r f; do
		awk -v f="$f" -v allowed=" $headings " '
			function fail(msg, line) { print "changelog: " f ":" line ": " msg > "/dev/stderr"; bad = 1 }
			function close_heading() {
				if (h != "" && !body) fail("### " h " has no entries", hline)
			}
			/^### / {
				close_heading()
				h = substr($0, 5); sub(/[ \t]+$/, "", h); hline = FNR; body = 0
				if (index(allowed, " " h " ") == 0) fail("unknown heading \"### " h "\"; use one of" allowed, FNR)
				next
			}
			/^#/ { fail("only ### subsection headings belong in a change file", FNR); next }
			/[^ \t]/ {
				if (h == "") fail("entries must sit under a ### heading", FNR)
				body = 1
			}
			END {
				close_heading()
				if (hline == 0 && !bad) fail("no ### heading", 1)
				exit bad
			}
		' "$f" || bad=1
	done < <(change_files)
	released_entries || bad=1
	return "$bad"
}

# Fails when a change file holds an entry that a released section of
# CHANGELOG.md already has, word for word: a branch cut before a release can
# bring back entries that release gathered, and the next gather would publish
# them again. An entry is a "- " line and its indented continuation lines.
released_entries() {
	local files=() f
	while IFS= read -r f; do
		files+=("$f")
	done < <(change_files)
	[ "${#files[@]}" -gt 0 ] || return 0
	awk '
		function end_entry() {
			if (entry == "") return
			if (src == "CHANGELOG.md") released[entry] = 1
			else if (entry in released) {
				print "changelog: " src ":" eline ": entry already released in CHANGELOG.md; delete it" > "/dev/stderr"
				bad = 1
			}
			entry = ""
		}
		FNR == 1 { end_entry() }
		{ sub(/[ \t]+$/, "") }
		/^- / { end_entry(); entry = $0; src = FILENAME; eline = FNR; next }
		entry != "" && /^[ \t]+[^ \t]/ { entry = entry "\n" $0; next }
		{ end_entry() }
		END { end_entry(); exit bad }
	' CHANGELOG.md "${files[@]}"
}

# The pending entries as one section body, without the "## " line.
render() {
	local files=() f
	while IFS= read -r f; do
		files+=("$f")
	done < <(ordered_files)
	[ "${#files[@]}" -gt 0 ] || return 0
	awk -v order="$headings" '
		function flush(   s) {
			s = chunk
			sub(/^\n+/, "", s); sub(/\n+$/, "", s)
			if (h != "" && s != "") body[h] = body[h] (body[h] == "" ? "" : "\n") s
			chunk = ""
		}
		FNR == 1 { flush(); h = "" }
		/^### / { flush(); h = substr($0, 5); sub(/[ \t]+$/, "", h); next }
		{ chunk = chunk $0 "\n" }
		END {
			flush()
			n = split(order, o, " ")
			for (i = 1; i <= n; i++) {
				if (body[o[i]] == "") continue
				if (out) print ""
				print "### " o[i]; print ""; print body[o[i]]
				out = 1
			}
		}
	' "${files[@]}"
}

gather() {
	local version=$1 date=${2:-$(date +%F)} notes
	check || die "fix the errors above first"
	grep -q "^## $version " CHANGELOG.md && die "CHANGELOG.md already has a $version section"
	notes=$(render)
	[ -n "$notes" ] || die "no change files to gather in changes/"
	{
		printf '## %s - %s\n\n%s\n\n' "$version" "$date" "$notes"
	} >changes/.section.tmp
	awk -v sect=changes/.section.tmp '
		!done && /^## / { while ((getline l < sect) > 0) print l; done = 1 }
		{ print }
		END { if (!done) { print ""; while ((getline l < sect) > 0) print l } }
	' CHANGELOG.md >CHANGELOG.md.tmp
	mv CHANGELOG.md.tmp CHANGELOG.md
	rm -f changes/.section.tmp
	change_files | while IFS= read -r f; do rm -- "$f"; done
	echo "changelog: wrote ## $version - $date and removed the change files"
}

case "${1:-}" in
check) check ;;
notes) render ;;
gather)
	[ $# -ge 2 ] || die "usage: scripts/changelog.sh gather VERSION [DATE]"
	gather "$2" "${3:-}"
	;;
*) die "usage: scripts/changelog.sh check | notes | gather VERSION [DATE]" ;;
esac
