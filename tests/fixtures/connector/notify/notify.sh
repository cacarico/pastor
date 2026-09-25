#!/bin/sh
# Fixture hook: append the event record from stdin to the job's scratch dir.
cat >> "$PASTOR_CONNECTOR_STATE_DIR/notify.jsonl"
