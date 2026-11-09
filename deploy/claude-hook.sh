#!/usr/bin/env bash
# Sends Claude Code's permission prompts to the phone, as a PermissionRequest
# hook in ~/.claude/settings.json. It only fires when Claude would ask you
# anyway. Approve allows the call, Deny refuses it, and no answer leaves
# Claude's usual prompt in place.
#
#   deploy/claude-hook.sh install   add the hook (replaces an earlier one)
#   deploy/claude-hook.sh remove    take it out
#
# By default the hook only pushes while the screen is locked, so at the desk
# you just get Claude's prompt. That needs a command that exits 0 when the
# screen is locked; Omarchy's is found by itself. Settings, both optional:
#   ONLY_IF='<command>'   your own lock check (or any condition)
#   ALWAYS=1              push for every prompt, locked or not
set -euo pipefail

SETTINGS=~/.claude/settings.json
APPROVE=/usr/local/bin/approve
command -v jq >/dev/null || { echo "needs jq"; exit 1; }
[[ -f $SETTINGS ]] || echo '{}' > "$SETTINGS"

# Every hook group that runs approve, removed (empty lists dropped too).
WITHOUT='
  if .hooks.PermissionRequest then
    .hooks.PermissionRequest |= map(select(any(.hooks[]?; (.command // "") | startswith($approve)) | not))
    | if .hooks.PermissionRequest == [] then del(.hooks.PermissionRequest) else . end
    | if .hooks == {} then del(.hooks) else . end
  else . end'

write() {
  local new; new=$(mktemp)
  jq --arg approve "$APPROVE" "$@" "$SETTINGS" > "$new"
  [[ -e $SETTINGS.approver-backup ]] || cp -p "$SETTINGS" "$SETTINGS.approver-backup"
  cat "$new" > "$SETTINGS"; rm -f "$new"
}

case "${1:-}" in
  install)
    [[ -x $APPROVE ]] || { echo "install approve first (deploy/pam.sh test or install puts it in $APPROVE)"; exit 1; }
    cmd=$APPROVE
    if [[ ${ALWAYS:-} == 1 ]]; then
      echo "Pushing every permission prompt to the phone."
    else
      only_if=${ONLY_IF:-$(command -v omarchy-hyprland-session-locked || true)}
      [[ -n $only_if ]] || { echo "no lock check found: set ONLY_IF='<command that exits 0 when locked>' or ALWAYS=1"; exit 1; }
      cmd="$APPROVE --only-if $(printf '%q' "$only_if")"
      echo "Pushing permission prompts to the phone while this is true: $only_if"
    fi
    write --arg cmd "$cmd" "$WITHOUT"' | .hooks.PermissionRequest += [{
      matcher: "*",
      hooks: [{ type: "command", command: $cmd, timeout: 90 }]
    }]'
    echo "Added to $SETTINGS:"; jq '.hooks.PermissionRequest' "$SETTINGS"
    echo "New Claude Code sessions pick it up."
    ;;
  remove)
    write "$WITHOUT"
    echo "Removed from $SETTINGS."
    ;;
  *)
    sed -n '2,/^set -euo/p' "$0" | sed '$d; s/^# \{0,1\}//'; exit 2 ;;
esac
