#!/bin/sh
# Fixture hook: append the event record from stdin to the job's scratch dir.
cat >> "$PASTOR_PLUGIN_STATE_DIR/notify.jsonl"
