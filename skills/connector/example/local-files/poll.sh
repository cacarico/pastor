#!/bin/sh
# Worked example poll connector (skills/connector/SKILL.md): reads the
# handshake, lists config.dir, and emits one item per file in it. Relies on
# pastor's own item-key dedup instead of tracking a cursor, since re-reading
# a directory listing each run is cheap. Needs jq.
set -eu

input=$(cat)
dir=$(printf '%s' "$input" | jq -r '.config.dir // empty')

log() {
	jq -nc --arg level "$1" --arg message "$2" '{type:"log", level:$level, message:$message}'
}

if [ -z "$dir" ]; then
	log error "config.dir is required"
	exit 1
fi
if [ ! -d "$dir" ]; then
	log error "$dir does not exist"
	exit 1
fi

count=0
for f in "$dir"/*; do
	[ -f "$f" ] || continue
	count=$((count + 1))
	jq -nc --arg key "$f" --arg title "$(basename "$f")" '{type:"item", key:$key, title:$title}'
done

log info "$count file(s) in $dir"
