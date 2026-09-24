#!/bin/sh
# Fixture stream connector.
read -r input
f="$PASTOR_PLUGIN_STATE_DIR/starts"
n=$(cat "$f" 2>/dev/null || echo 0)
n=$((n + 1))
echo "$n" > "$f"
echo "{\"type\":\"log\",\"level\":\"info\",\"message\":\"start $n handshake $(echo "$input" | sed 's/"/\\"/g')\"}"
echo "{\"type\":\"log\",\"level\":\"info\",\"message\":\"logging token $FIXTURE_TOKEN\"}"
echo "{\"type\":\"item\",\"key\":\"start-$n\"}"
echo "{\"type\":\"cursor\",\"value\":\"cur-$n\"}"
if [ "$FIXTURE_MODE" = crash ]; then
  echo "crashing on purpose" >&2
  exit 1
fi
while :; do sleep 1; done
