#!/bin/sh
# Fixture poll connector. Reads the handshake, then answers in JSON lines.
read -r input
echo "{\"type\":\"log\",\"level\":\"info\",\"message\":\"handshake $(echo "$input" | sed 's/"/\\"/g')\"}"
echo "{\"type\":\"log\",\"level\":\"debug\",\"message\":\"env $PASTOR_PLUGIN_ID ${PASTOR_JOB:-none} $(pwd)\"}"
echo "token is $FIXTURE_TOKEN" >&2
echo "{\"type\":\"log\",\"level\":\"info\",\"message\":\"logging token $FIXTURE_TOKEN\"}"
case "$FIXTURE_MODE" in
  fail)
    echo '{"type":"item","key":"never-used"}'
    echo '{"type":"cursor","value":"never-used"}'
    echo "failing on purpose" >&2
    exit 4
    ;;
  hang)
    sleep 30
    ;;
esac
echo '{"type":"item","key":"k1","title":"first","body":"b1","url":"https://example.com/1","author":"ana"}'
echo 'this is not json'
echo '{"type":"item","title":"no key"}'
echo '{"type":"item","key":"k2","title":"second","extra":{"n":2}}'
echo '{"type":"item","key":"k1","title":"duplicate"}'
echo '{"type":"cursor","value":"c-1"}'
echo '{"type":"what"}'
echo '{"type":"cursor","value":"c-2"}'
