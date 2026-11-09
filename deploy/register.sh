#!/usr/bin/env bash
# Open or close passkey registration on approverd.
#   deploy/register.sh open    # then visit https://<host>/register on each device
#   deploy/register.sh close
# The open state lives under /run, so a reboot closes it too.
set -euo pipefail
dropin=/run/systemd/system/approverd.service.d/register.conf
case "${1:-}" in
  open)
    sudo install -d /run/systemd/system/approverd.service.d
    printf '[Service]\nEnvironment=ALLOW_REGISTER=1\n' | sudo tee "$dropin" >/dev/null ;;
  close)
    sudo rm -f "$dropin" ;;
  *) echo "usage: $0 open|close"; exit 1 ;;
esac
sudo systemctl daemon-reload
sudo systemctl restart approverd
sleep 1
journalctl -u approverd -n 1 --no-pager -o cat
