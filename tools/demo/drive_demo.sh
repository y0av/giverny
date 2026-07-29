#!/bin/sh
EXE=/home/yoz/Dev/claude_test/giverny/target/release/giverny
CD=/tmp/giverny-demo/.claude; LAB=/tmp/giverny-demo/envs/lab/claude
relay() { printf '%s' "$2" | env XDG_RUNTIME_DIR=/tmp/giverny-demo-run GIVERNY_TAB_ID="giverny-$1" CLAUDE_CONFIG_DIR="$3" "$EXE" relay; }
sleep 3
relay 10 '{"hook_event_name":"SessionStart","session_id":"aaaa1111-2222-3333-4444-555555555555"}' "$CD"
relay 12 '{"hook_event_name":"SessionStart","session_id":"dddd1111-2222-3333-4444-555555555555"}' "$CD"
relay 11 '{"hook_event_name":"SessionStart","session_id":"bbbb1111-2222-3333-4444-555555555555"}' "$LAB"
relay 11 '{"hook_event_name":"Stop","session_id":"bbbb1111-2222-3333-4444-555555555555"}' "$LAB"
relay 20 '{"hook_event_name":"SessionStart","session_id":"cccc1111-2222-3333-4444-555555555555"}' "$LAB"
printf '{"session_id":"a","model":{"display_name":"Fable"},"rate_limits":{"five_hour":{"used_percentage":34},"seven_day":{"used_percentage":41}}}' | env XDG_RUNTIME_DIR=/tmp/giverny-demo-run GIVERNY_TAB_ID=giverny-10 CLAUDE_CONFIG_DIR="$CD" "$EXE" statusline >/dev/null
# Heartbeat: hook-driven states lapse without a live session, so keep both
# working tabs asserted; the permission prompt lands partway through.
i=0
while [ $i -lt 14 ]; do
  relay 10 '{"hook_event_name":"UserPromptSubmit","session_id":"aaaa1111-2222-3333-4444-555555555555"}' "$CD"
  if [ $i -lt 2 ]; then
    relay 12 '{"hook_event_name":"UserPromptSubmit","session_id":"dddd1111-2222-3333-4444-555555555555"}' "$CD"
  else
    relay 12 '{"hook_event_name":"Notification","notification_type":"permission_prompt","message":"needs permission","session_id":"dddd1111-2222-3333-4444-555555555555"}' "$CD"
  fi
  i=$((i+1)); sleep 2
done
