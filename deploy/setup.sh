#!/usr/bin/env bash
# Sets up approverd behind `tailscale serve` on whichever machine you run it
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
[[ -x target/release/approverd ]] || { echo "build first: cargo build --release"; exit 1; }
command -v jq >/dev/null || { echo "needs jq"; exit 1; }

HOST="${HOST:-$(tailscale status --json 2>/dev/null | jq -r '.Self.DNSName // empty' | sed 's/\.$//')}"
[[ -n $HOST ]] || { echo "can't find this machine's Tailscale name; is tailscale up? (or set HOST=...)"; exit 1; }
echo "Setting up for $HOST"
sudo -v

echo "==> approverd"
sudo install -m755 target/release/approverd /usr/local/bin/approverd
sudo install -d -m700 /etc/approver
echo "APPROVER_HOST=$HOST" | sudo install -m600 /dev/stdin /etc/approver/approverd.env
sudo install -m644 deploy/approverd.service /etc/systemd/system/approverd.service
sudo systemctl daemon-reload
sudo systemctl enable approverd
sudo systemctl restart approverd
for _ in $(seq 20); do sudo approverd status >/dev/null 2>&1 && break; sleep 0.5; done
sudo approverd status

echo "==> tailscale serve"
echo "(if this prints a login.tailscale.com link, open it to enable Serve/HTTPS for the tailnet; it continues once enabled)"
sudo tailscale serve --bg --https=443 http://127.0.0.1:7071
tailscale serve status

echo
echo "Done. On the phone, in this order:"
echo "  1. sudo approverd register open, then open https://$HOST/register and add a passkey"
echo "  2. open https://$HOST/ in Safari, Share > Add to Home Screen"
echo "  3. open Approver from the Home Screen and turn on notifications"
