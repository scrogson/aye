#!/usr/bin/env bash
# Adds phone approval to sudo on this Mac, the macOS version of deploy/pam.sh.
# macOS has no pam_exec, so pam/pam_approver.c (built here) runs approve in
# its place. The line goes in /etc/pam.d/sudo_local, the file macOS keeps for
# local additions across updates, after Touch ID if that's on:
#
#   auth  sufficient  pam_tid.so                           (unchanged, if there)
#   auth  sufficient  /usr/local/lib/pam/pam_approver.so   (added)
#
# then /etc/pam.d/sudo carries on to your password. At the Mac you use Touch
# ID as before; where Touch ID can't work (over SSH, lid closed) a push goes
# to the phone. Enter, a deny, a timeout or a stopped approverd all fall
# through to the password. /etc/pam.d/sudo itself isn't touched.
#
#   deploy/macos/pam.sh test      try it on a throwaway PAM service first
#   deploy/macos/pam.sh install   add the line to sudo_local (backed up first)
#   deploy/macos/pam.sh remove    take it out again
#
# Run as yourself from the repo root, after `cargo build --release` and
# deploy/macos/setup.sh. Needs the Xcode command line tools (cc).
set -euo pipefail
cd "$(dirname "$0")/../.."

MODULE=/usr/local/lib/pam/pam_approver.so
LINE="auth       sufficient     $MODULE"
LOCAL=/etc/pam.d/sudo_local
BACKUP=/etc/pam.d/sudo_local.approver-backup

[[ $(uname) == Darwin ]] || { echo "this is the macOS version; on Linux use deploy/pam.sh"; exit 1; }
[[ $EUID -ne 0 ]] || { echo "run as your user, not root"; exit 1; }

# sudo_local as it is now (or the template macOS ships, if there's none yet),
# with our line after the last pam_tid/pam_reattach line, else at the end.
with_line() {
  local src=$LOCAL
  [[ -f $src ]] || src=/etc/pam.d/sudo_local.template
  [[ -f $src ]] || src=/dev/null
  awk -v line="$LINE" '
    { lines[NR] = $0 } /^auth.*(pam_tid|pam_reattach)/ { after = NR }
    END {
      if (!after) after = NR
      for (i = 1; i <= NR; i++) { print lines[i]; if (i == after) print line }
      if (NR == 0) print line
    }' "$src"
}

install_parts() {
  [[ -x target/release/approve ]] || { echo "build first: cargo build --release"; exit 1; }
  sudo approverd status >/dev/null 2>&1 || { echo "approverd isn't running; set it up first (deploy/macos/setup.sh)"; exit 1; }
  grep -q '^auth.*include.*sudo_local' /etc/pam.d/sudo || { echo "/etc/pam.d/sudo doesn't include sudo_local (macOS 14 or later needed)"; exit 1; }
  cc -bundle -Wall -Wextra -Werror -o target/release/pam_approver.so pam/pam_approver.c -lpam
  sudo install -d -m755 -o root -g wheel /usr/local/bin /usr/local/lib/pam
  sudo install -m755 -o root -g wheel target/release/approve /usr/local/bin/approve
  sudo install -m444 -o root -g wheel target/release/pam_approver.so "$MODULE"
}

case "${1:-}" in
  test)
    install_parts
    cc -Wall -o target/release/pamtest pam/pamtest.c -lpam
    tmp=$(mktemp); trap 'rm -f "$tmp"' EXIT
    {
      echo "# Throwaway copy of the planned sudo stack for deploy/macos/pam.sh test. Nothing uses it."
      with_line | grep '^auth'
      grep '^auth' /etc/pam.d/sudo | grep -v sudo_local
    } > "$tmp"
    sudo install -m644 -o root -g wheel "$tmp" /etc/pam.d/approver-test
    echo "Trying /etc/pam.d/approver-test (your real sudo is untouched):"
    sed 's/^/    /' /etc/pam.d/approver-test
    echo
    if target/release/pamtest approver-test; then echo "=> would have let sudo through"; else echo "=> would have refused"; fi
    echo "Run it again to try the other ways in: Touch ID, approve, deny, Enter, wrong password."
    echo "Clean up when done: sudo rm /etc/pam.d/approver-test"
    ;;
  install)
    if [[ -f $LOCAL ]] && grep -qF "$MODULE" "$LOCAL"; then echo "already installed"; exit 0; fi
    install_parts
    new=$(mktemp); trap 'rm -f "$new"' EXIT
    with_line > "$new"
    if [[ -f $LOCAL ]]; then
      # Exactly one line added, nothing else changed.
      if [[ $(diff "$LOCAL" "$new" | grep -c '^>') != 1 || $(diff "$LOCAL" "$new" | grep -c '^<') != 0 ]]; then
        echo "unexpected result, not installing:"; diff "$LOCAL" "$new" || true; exit 1
      fi
      [[ -e $BACKUP ]] || sudo cp -p "$LOCAL" "$BACKUP"
    fi
    sudo install -m444 -o root -g wheel "$new" "$LOCAL"
    echo "Installed. $LOCAL is now:"; sed 's/^/    /' "$LOCAL"
    echo
    echo "Keep this terminal open. In a NEW terminal, try:  sudo -k; sudo true"
    echo "If sudo is broken there, this takes the line out again without sudo (it asks for"
    echo "your password in a macOS dialog, which doesn't go through sudo's PAM stack):"
    echo "  osascript -e 'do shell script \"/usr/bin/sed -i .bak /pam_approver/d $LOCAL\" with administrator privileges'"
    ;;
  remove)
    if ! [[ -f $LOCAL ]] || ! grep -qF "$MODULE" "$LOCAL"; then echo "not installed"; exit 0; fi
    new=$(mktemp); trap 'rm -f "$new"' EXIT
    grep -vF "$MODULE" "$LOCAL" > "$new" || true
    sudo install -m444 -o root -g wheel "$new" "$LOCAL"
    echo "Removed. $LOCAL is now:"; sed 's/^/    /' "$LOCAL"
    ;;
  *)
    sed -n '2,/^set -euo/p' "$0" | sed '$d; s/^# \{0,1\}//'; exit 2 ;;
esac
