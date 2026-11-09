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
#   NTFY=1                     also push through a Tailscale-only ntfy (Arch,
#                              from the AUR); keeps existing ntfy users and token
#   TOPIC=approvals            ntfy topic the pushes go to
#   PHONE_USER=$USER           ntfy account the phone app signs in with
set -euo pipefail
cd "$(dirname "$0")/.."

[[ $EUID -ne 0 ]] || { echo "run as your user, not root"; exit 1; }
[[ -x target/release/approverd ]] || { echo "build first: cargo build --release"; exit 1; }
command -v jq >/dev/null || { echo "needs jq"; exit 1; }

HOST="${HOST:-$(tailscale status --json 2>/dev/null | jq -r '.Self.DNSName // empty' | sed 's/\.$//')}"
[[ -n $HOST ]] || { echo "can't find this machine's Tailscale name; is tailscale up? (or set HOST=...)"; exit 1; }
NTFY="${NTFY:-0}"
TOPIC="${TOPIC:-approvals}"
PHONE_USER="${PHONE_USER:-$USER}"
if [[ $NTFY == 1 ]]; then
  echo "Setting up for $HOST, with ntfy (topic $TOPIC, phone user $PHONE_USER)"
else
  echo "Setting up for $HOST"
fi
sudo -v

ntfy_as() { sudo -u ntfy ntfy "$@"; }
env_lines=("APPROVER_HOST=$HOST")

if [[ $NTFY == 1 ]]; then
echo "==> ntfy package"
yay -S --needed --noconfirm ntfysh-bin

echo "==> ntfy config"
sed "s|@HOST@|$HOST|g" deploy/ntfy/server.yml | sudo install -m644 /dev/stdin /etc/ntfy/server.yml
sudo install -Dm644 deploy/ntfy/ntfy.service.d/approver.conf /etc/systemd/system/ntfy.service.d/approver.conf
sudo systemctl daemon-reload
sudo systemctl enable ntfy
sudo systemctl restart ntfy
for _ in $(seq 20); do curl -fsS http://127.0.0.1:2586/v1/health >/dev/null 2>&1 && break; sleep 0.5; done
curl -fsS http://127.0.0.1:2586/v1/health; echo

echo "==> ntfy users"
if ! ntfy_as user list 2>&1 | grep -q '^user approver '; then
  # Publish-only account for approverd; its password is never used, only its token.
  sudo -u ntfy env NTFY_PASSWORD="$(head -c 32 /dev/urandom | base64)" ntfy user add approver
fi
ntfy_as access approver "$TOPIC" write-only
if ! ntfy_as user list 2>&1 | grep -q "^user $PHONE_USER "; then
  echo "Choose a password for the ntfy app on your phone (user '$PHONE_USER'):"
  ntfy_as user add "$PHONE_USER"
fi
ntfy_as access "$PHONE_USER" "$TOPIC" read-only

token=$(sudo sed -n 's/^NTFY_TOKEN=\(tk_[A-Za-z0-9]*\)$/\1/p' /etc/approver/approverd.env 2>/dev/null || true)
[[ -n $token ]] || token=$(ntfy_as token add --label approverd approver | grep -o 'tk_[A-Za-z0-9]*')
env_lines+=("NTFY_URL=http://127.0.0.1:2586/$TOPIC" "NTFY_TOKEN=$token")
unset token
fi

echo "==> approverd"
sudo install -m755 target/release/approverd /usr/local/bin/approverd
sudo install -d -m700 /etc/approver
printf '%s\n' "${env_lines[@]}" | sudo install -m600 /dev/stdin /etc/approver/approverd.env
sudo install -m644 deploy/approverd.service /etc/systemd/system/approverd.service
sudo systemctl daemon-reload
sudo systemctl enable approverd
sudo systemctl restart approverd
for _ in $(seq 20); do sudo approverd status >/dev/null 2>&1 && break; sleep 0.5; done
sudo approverd status

echo "==> tailscale serve"
echo "(if this prints a login.tailscale.com link, open it to enable Serve/HTTPS for the tailnet; it continues once enabled)"
sudo tailscale serve --bg --https=443 http://127.0.0.1:7071
[[ $NTFY == 1 ]] && sudo tailscale serve --bg --https=8443 http://127.0.0.1:2586
tailscale serve status

echo
echo "Done. On the phone, in this order:"
echo "  1. sudo approverd register open, then open https://$HOST/register and add a passkey"
echo "  2. open https://$HOST/ in Safari, Share > Add to Home Screen"
echo "  3. open Approver from the Home Screen and turn on notifications"
[[ $NTFY == 1 ]] && echo "ntfy: https://$HOST:8443/$TOPIC"
true
