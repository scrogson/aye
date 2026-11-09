#!/usr/bin/env bash
# Adds phone approval to sudo, between the YubiKey and the password:
#
#   auth  sufficient  pam_u2f.so cue                           (unchanged)
#   auth  sufficient  pam_exec.so quiet /usr/local/bin/approve (added)
#   auth  include     system-auth                              (unchanged)
#
# With the key plugged in you touch it as before. Without it, a push goes to
# the phone; Enter, a deny, a timeout or a stopped approverd all fall through
# to the password prompt. Only /etc/pam.d/sudo changes: polkit (and so
# pkexec, the way back if sudo ever breaks) is left alone.
#
#   deploy/pam.sh test      try it on a throwaway PAM service with pamtester
#   deploy/pam.sh install   add the line to /etc/pam.d/sudo (backed up first)
#   deploy/pam.sh remove    take the line out again
#
# Run as yourself from a regular terminal outside the Claude app, from the
# repo root, after `cargo build --release`.
set -euo pipefail
cd "$(dirname "$0")/.."

LINE='auth       sufficient   pam_exec.so quiet /usr/local/bin/approve'
MARK='pam_exec.so quiet /usr/local/bin/approve'
BACKUP=/etc/pam.d/sudo.approver-backup

[[ $EUID -ne 0 ]] || { echo "run as your user, not root"; exit 1; }

# The current sudo stack with our line before the first auth line that isn't
# pam_u2f, i.e. after the YubiKey and before the password.
with_line() {
  awk -v line="$LINE" '!done && /^auth/ && !/pam_u2f\.so/ { print line; done = 1 } { print }' /etc/pam.d/sudo
}

install_client() {
  [[ -x target/release/approve ]] || { echo "build first: cargo build --release"; exit 1; }
  systemctl is-active --quiet approverd || { echo "approverd isn't running; set it up first (deploy/setup.sh)"; exit 1; }
  sudo install -m755 -o root -g root target/release/approve /usr/local/bin/approve
}

case "${1:-}" in
  test)
    install_client
    with_line | sed '1a # Throwaway copy of the planned sudo stack for deploy/pam.sh test. Nothing uses it.' \
      | sudo install -m644 /dev/stdin /etc/pam.d/approver-test
    echo "Trying /etc/pam.d/approver-test (your real sudo is untouched):"
    sed 's/^/    /' /etc/pam.d/approver-test
    echo
    if pamtester approver-test "$USER" authenticate; then echo "=> would have let sudo through"; else echo "=> would have refused"; fi
    echo "Run it again to try the other ways in: key plugged/unplugged, approve, deny, Enter, wrong password."
    echo "Clean up when done: sudo rm /etc/pam.d/approver-test"
    ;;
  install)
    if grep -qF "$MARK" /etc/pam.d/sudo; then echo "already installed"; exit 0; fi
    install_client
    new=$(mktemp); trap 'rm -f "$new"' EXIT
    with_line > "$new"
    # Exactly one line added, nothing else changed.
    if [[ $(diff /etc/pam.d/sudo "$new" | grep -c '^>') != 1 || $(diff /etc/pam.d/sudo "$new" | grep -c '^<') != 0 ]]; then
      echo "unexpected result, not installing:"; diff /etc/pam.d/sudo "$new" || true; exit 1
    fi
    [[ -e $BACKUP ]] || sudo cp -p /etc/pam.d/sudo "$BACKUP"
    sudo install -m644 "$new" /etc/pam.d/sudo
    echo "Installed. /etc/pam.d/sudo is now:"; sed 's/^/    /' /etc/pam.d/sudo
    echo
    echo "Keep this terminal open. In a NEW terminal, try:  sudo -k; sudo true"
    echo "If sudo is broken there, undo with:  pkexec cp $BACKUP /etc/pam.d/sudo"
    ;;
  remove)
    if ! grep -qF "$MARK" /etc/pam.d/sudo; then echo "not installed"; exit 0; fi
    grep -vF "$MARK" /etc/pam.d/sudo | sudo install -m644 /dev/stdin /etc/pam.d/sudo
    echo "Removed. /etc/pam.d/sudo is now:"; sed 's/^/    /' /etc/pam.d/sudo
    ;;
  *)
    sed -n '2,/^set -euo/p' "$0" | sed '$d; s/^# \{0,1\}//'; exit 2 ;;
esac
