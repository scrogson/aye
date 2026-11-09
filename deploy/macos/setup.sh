#!/usr/bin/env bash
# Sets up ayed on this Mac behind `tailscale serve`, the macOS version
# of deploy/setup.sh. Phones get pushes through Web Push once the Home Screen
# app is added. Does not touch PAM.
#
#   - a hidden _aye user for the daemon (made the first time)
#   - /usr/local/bin/ayed and a launchd daemon, local.ayed; /usr/local/bin/aye
#   - state in /var/db/aye, log in /var/log/ayed.log
#   - https://<this Mac's Tailscale name>/ served from 127.0.0.1:7071
#
# Run as yourself (not root) from the repo root, after `cargo build --release`.
# Safe to re-run. Settings, all optional:
#   HOST=name.tailnet.ts.net   default: this Mac's Tailscale name
set -euo pipefail
cd "$(dirname "$0")/../.."

[[ $(uname) == Darwin ]] || { echo "this is the macOS setup; on Linux use deploy/setup.sh"; exit 1; }
[[ $EUID -ne 0 ]] || { echo "run as your user, not root"; exit 1; }
[[ -x target/release/ayed && -x target/release/aye ]] || { echo "build first: cargo build --release"; exit 1; }
command -v jq >/dev/null || { echo "needs jq (built in from macOS 15, or: brew install jq)"; exit 1; }

TS=$(command -v tailscale || echo /Applications/Tailscale.app/Contents/MacOS/Tailscale)
[[ -x $TS ]] || { echo "can't find the tailscale command; install Tailscale (or put its CLI on PATH)"; exit 1; }
HOST="${HOST:-$("$TS" status --json 2>/dev/null | jq -r '.Self.DNSName // empty' | sed 's/\.$//')}"
[[ -n $HOST ]] || { echo "can't find this Mac's Tailscale name; is Tailscale connected? (or set HOST=...)"; exit 1; }
echo "Setting up for $HOST"
sudo -v

echo "==> _aye user"
if ! dscl . -read /Users/_aye >/dev/null 2>&1; then
  # The first id from 300 up that no user or group has.
  used=$( { dscl . -list /Users UniqueID; dscl . -list /Groups PrimaryGroupID; } | awk '{print $2}' | sort -n -u)
  id=300; while grep -qx "$id" <<<"$used"; do id=$((id + 1)); done
  sudo dscl . -create /Groups/_aye
  sudo dscl . -create /Groups/_aye PrimaryGroupID "$id"
  sudo dscl . -create /Groups/_aye RealName "Aye daemon"
  sudo dscl . -create /Users/_aye
  sudo dscl . -create /Users/_aye UniqueID "$id"
  sudo dscl . -create /Users/_aye PrimaryGroupID "$id"
  sudo dscl . -create /Users/_aye RealName "Aye daemon"
  sudo dscl . -create /Users/_aye UserShell /usr/bin/false
  sudo dscl . -create /Users/_aye NFSHomeDirectory /var/empty
  sudo dscl . -create /Users/_aye Password '*'
  sudo dscl . -create /Users/_aye IsHidden 1
  echo "made _aye ($id)"
else
  echo "already there"
fi

echo "==> ayed"
PLIST=/Library/LaunchDaemons/local.ayed.plist
sudo install -d -m700 -o _aye -g _aye /var/db/aye
sudo touch /var/log/ayed.log
sudo chown _aye:_aye /var/log/ayed.log
sudo chmod 600 /var/log/ayed.log
sudo launchctl bootout system "$PLIST" 2>/dev/null || true
sudo install -d -m755 -o root -g wheel /usr/local/bin # not there by default on Apple Silicon
sudo install -m755 -o root -g wheel target/release/ayed /usr/local/bin/ayed
sudo install -m755 -o root -g wheel target/release/aye /usr/local/bin/aye
tmp=$(mktemp); trap 'rm -f "$tmp"' EXIT
sed "s|@HOST@|$HOST|" deploy/macos/local.ayed.plist > "$tmp"
sudo install -m644 -o root -g wheel "$tmp" "$PLIST"
plutil -lint "$PLIST" >/dev/null
sudo launchctl bootstrap system "$PLIST"
for _ in $(seq 20); do sudo ayed status >/dev/null 2>&1 && break; sleep 0.5; done
sudo ayed status || { echo "ayed didn't start; see: sudo tail /var/log/ayed.log"; exit 1; }

echo "==> tailscale serve"
echo "(if this prints a login.tailscale.com link, open it to enable Serve/HTTPS for the tailnet; it continues once enabled)"
"$TS" serve --bg --https=443 http://127.0.0.1:7071
"$TS" serve status

echo
echo "Done. On the phone, in this order:"
echo "  1. sudo aye register open, then open https://$HOST/register and add a passkey"
echo "  2. open https://$HOST/ in Safari, Share > Add to Home Screen"
echo "  3. open the new Aye from the Home Screen and turn on notifications"
echo "Then deploy/macos/pam.sh for sudo, and deploy/claude-hook.sh for Claude Code."
