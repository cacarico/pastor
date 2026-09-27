#!/bin/sh
# Fixture finish command: append the task-end object from stdin to the job's
# scratch dir.
cat >> "$PASTOR_CONNECTOR_STATE_DIR/finish-records.jsonl"
