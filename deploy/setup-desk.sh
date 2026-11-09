#!/usr/bin/env bash
# Steps 1 and 3 of the plan on desk: approverd behind `tailscale serve`, and a
# Tailscale-only ntfy. Does not touch PAM or sudoers.
#
# Run as yourself (not root) from a regular terminal outside the Claude app,
# from the repo root, after `cargo build --release`. Safe to re-run.
set -euo pipefail

HOST=desk.tailnet-name.ts.net
TOPIC=approvals
PHONE_USER=alice
cd "$(dirname "$0")/.."

[[ $EUID -ne 0 ]] || { echo "run as your user, not root (yay refuses root)"; exit 1; }
[[ -x target/release/approverd ]] || { echo "build first: cargo build --release"; exit 1; }
sudo -v

echo "==> ntfy package"
yay -S --needed --noconfirm ntfysh-bin

echo "==> ntfy config"
sudo install -m644 deploy/ntfy/server.yml /etc/ntfy/server.yml
sudo install -Dm644 deploy/ntfy/ntfy.service.d/approver.conf /etc/systemd/system/ntfy.service.d/approver.conf
sudo systemctl daemon-reload
sudo systemctl enable ntfy
sudo systemctl restart ntfy
for _ in $(seq 20); do curl -fsS http://127.0.0.1:2586/v1/health >/dev/null 2>&1 && break; sleep 0.5; done
curl -fsS http://127.0.0.1:2586/v1/health; echo

ntfy_as() { sudo -u ntfy ntfy "$@"; }

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

echo "==> approverd"
sudo install -m755 target/release/approverd /usr/local/bin/approverd
sudo install -d -m700 /etc/approver
if ! sudo grep -q '^NTFY_TOKEN=tk_' /etc/approver/approverd.env 2>/dev/null; then
  token=$(ntfy_as token add --label approverd approver | grep -o 'tk_[A-Za-z0-9]*')
  sudo sh -c "umask 077; printf '%s\n' \
    'APPROVER_HOST=$HOST' \
    'NTFY_URL=http://127.0.0.1:2586/$TOPIC' \
    'NTFY_TOKEN=$token' > /etc/approver/approverd.env"
  unset token
fi
sudo install -m644 deploy/approverd.service /etc/systemd/system/approverd.service
sudo systemctl daemon-reload
sudo systemctl enable approverd
sudo systemctl restart approverd
sleep 1
systemctl --no-pager --lines=3 status approverd | tail -4

echo "==> tailscale serve"
echo "(if this prints a login.tailscale.com link, open it to enable Serve/HTTPS for the tailnet; it continues once enabled)"
sudo tailscale serve --bg --https=443 http://127.0.0.1:7071
sudo tailscale serve --bg --https=8443 http://127.0.0.1:2586
tailscale serve status

echo
echo "Done. Approval pages: https://$HOST/   ntfy: https://$HOST:8443/$TOPIC"
echo "Next: phone on Tailscale, then deploy/register.sh open (step 2)."
