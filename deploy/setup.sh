#!/usr/bin/env bash
# Sets up ayed behind `tailscale serve` on whichever machine you run it
# on. Phones get pushes through Web Push once the home-screen app is added, so
# nothing else is needed. Does not touch PAM or sudoers.
#
# Run as yourself (not root) from a regular terminal outside the Claude app,
# from the repo root, after `cargo build --release`. Safe to re-run.
#
# Settings, all optional:
#   HOST=name.tailnet.ts.net   default: this machine's Tailscale name
set -euo pipefail
cd "$(dirname "$0")/.."

[[ $EUID -ne 0 ]] || { echo "run as your user, not root"; exit 1; }
[[ -x target/release/ayed && -x target/release/aye ]] || { echo "build first: cargo build --release"; exit 1; }
command -v jq >/dev/null || { echo "needs jq"; exit 1; }

HOST="${HOST:-$(tailscale status --json 2>/dev/null | jq -r '.Self.DNSName // empty' | sed 's/\.$//')}"
[[ -n $HOST ]] || { echo "can't find this machine's Tailscale name; is tailscale up? (or set HOST=...)"; exit 1; }
echo "Setting up for $HOST"
sudo -v

echo "==> ayed"
sudo install -m755 target/release/ayed /usr/local/bin/ayed
sudo install -m755 target/release/aye /usr/local/bin/aye
sudo install -d -m700 /etc/aye
echo "AYE_HOST=$HOST" | sudo install -m600 /dev/stdin /etc/aye/ayed.env
sudo install -m644 deploy/ayed.service /etc/systemd/system/ayed.service
sudo systemctl daemon-reload
sudo systemctl enable ayed
sudo systemctl restart ayed
for _ in $(seq 20); do sudo ayed status >/dev/null 2>&1 && break; sleep 0.5; done
sudo ayed status

echo "==> tailscale serve"
echo "(if this prints a login.tailscale.com link, open it to enable Serve/HTTPS for the tailnet; it continues once enabled)"
sudo tailscale serve --bg --https=443 http://127.0.0.1:7071
tailscale serve status

echo
echo "Done. On the phone, in this order:"
echo "  1. sudo aye register open, then scan the QR code it prints and add a passkey"
echo "  2. open https://$HOST/ in Safari, Share > Add to Home Screen"
echo "  3. open Aye from the Home Screen and turn on notifications"
