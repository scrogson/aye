# approver

Approve `sudo` (and Claude Code's permission prompts) from your phone, with a
passkey and Face ID, over your Tailscale network.

When something on the machine needs approval, `approverd` sends a Web Push
notification to the Approver web app on your phone's Home Screen. The
notification says only what kind of request it is. Tapping it opens a page
that shows the exact command, and Approve or Deny is signed with a passkey that
only works for this machine. No answer in 60 seconds counts as no answer: sudo
falls back to your password, and Claude Code shows its own prompt.

```
sudo / Claude Code ──► approve ──► approverd (127.0.0.1:7070) ──► Web Push ──► phone
                                        ▲                                       │
                                        └────── signed answer (Face ID) ◄───────┘
                                   pages on https://<machine>.<tailnet>.ts.net
```

Nothing to install on the phone: the app is the pages approverd serves, added
to the Home Screen.

## What's in here

| Path | What it is |
| --- | --- |
| `src/bin/approverd.rs` | The daemon: local API, Web Push, the pages, control socket, and the `approverd` CLI |
| `src/bin/*.html`, `sw.js` | The Home Screen app: notifications setup, approval and passkey pages, service worker |
| `src/bin/approve.rs` | The client: a PAM helper for sudo and a Claude Code hook |
| `deploy/setup.sh` | Installs approverd and `tailscale serve` on this machine |
| `deploy/pam.sh` | Adds phone approval to sudo (`test`, `install`, `remove`) |
| `deploy/claude-hook.sh` | Adds the Claude Code hook to `~/.claude/settings.json` (`install`, `remove`) |

## Setup

Needs Tailscale with HTTPS enabled for the tailnet, `jq`, a Rust toolchain,
systemd, and an iPhone on iOS 16.4 or later (or any current browser). Run
everything below from a normal terminal: `sudo` doesn't work inside the Claude
app's terminal.

1. Build:

   ```bash
   cargo build --release
   ```

2. Install approverd and the Tailscale HTTPS endpoint. The script finds this
   machine's Tailscale name by itself:

   ```bash
   deploy/setup.sh
   ```

3. Add the phone's passkey. Registration closes by itself after 10 minutes:

   ```bash
   sudo approverd register open
   ```

   Then open `https://<machine>.<tailnet>.ts.net/register` on the phone and
   tap **Add passkey**. Add a second device (or a YubiKey) as a backup.

4. Turn on notifications. Open `https://<machine>.<tailnet>.ts.net/` in
   Safari, tap Share › **Add to Home Screen**, then open **Approver** from the
   Home Screen and tap **Turn on notifications**. It asks for Face ID, so only
   someone with a registered passkey can have requests sent to their device.
   **Send a test** checks the whole path.

On an iPhone, turn off iCloud Private Relay if Safari says the connection
isn't private: it breaks tailnet names on port 443.

## Managing it

All commands talk to the running daemon over a root-only socket, so they need
`sudo`:

```bash
sudo approverd status                    # address, passkeys, registration, devices, waiting requests
sudo approverd register open --for 5m    # 90s, 10m and 1h work too
sudo approverd register close
sudo approverd passkeys                  # list passkey ids
sudo approverd passkeys remove <id>      # 4 or more characters of the id
sudo approverd push                      # devices that get notifications
sudo approverd push test                 # send each one a test
sudo approverd push remove <n>           # stop notifying one (number from the list)
```

A device that deletes the app or turns notifications off is dropped by itself
the next time a push to it bounces.

## sudo

`deploy/pam.sh` adds one line to `/etc/pam.d/sudo`, after any YubiKey
(`pam_u2f`) line and before the password:

```
auth  sufficient  pam_u2f.so cue                           ← unchanged
auth  sufficient  pam_exec.so quiet /usr/local/bin/approve ← added
auth  include     system-auth                              ← unchanged
```

With the YubiKey plugged in you touch it as before. Without it, sudo says
"Approve on your phone, or press Enter to type your password." A deny, a
timeout, Enter, or a stopped approverd all lead to the password prompt.

1. Try it on a throwaway PAM service first. Your real sudo isn't touched:

   ```bash
   deploy/pam.sh test
   ```

2. Install it. The script backs up `/etc/pam.d/sudo` and refuses unless
   exactly one line changes. Keep that terminal open and try
   `sudo -k; sudo true` in a new one:

   ```bash
   deploy/pam.sh install
   ```

To take it out: `deploy/pam.sh remove`. If sudo itself ever breaks, polkit is
untouched, so this restores the old file:

```bash
pkexec cp /etc/pam.d/sudo.approver-backup /etc/pam.d/sudo
```

## Claude Code

`approve` is also a Claude Code
[`PermissionRequest` hook](https://code.claude.com/docs/en/hooks). It fires
only when Claude would ask you for permission anyway: commands and edits your
settings already allow never reach the phone. The page shows the command (or
the file, URL or tool input) and the folder Claude is working in.

| On the phone | In Claude Code |
| --- | --- |
| Approve | The tool call runs |
| Deny | It's refused, and Claude is told you denied it on your phone |
| No answer in 60s, approverd down, or the screen isn't locked | Claude's usual permission prompt, as if there were no hook |

`deploy/pam.sh` installs `approve` to `/usr/local/bin`. Then add the hook:

```bash
deploy/claude-hook.sh install
```

By default it pushes only while the screen is locked, so at the desk you get
Claude's prompt straight away. On Omarchy the script finds the lock check by
itself; elsewhere give it a command that exits 0 when the screen is locked, or
push for every prompt:

```bash
ONLY_IF='my-lock-check' deploy/claude-hook.sh install
ALWAYS=1 deploy/claude-hook.sh install
```

It adds this to `~/.claude/settings.json` (backed up to
`settings.json.approver-backup` the first time):

```json
{
  "hooks": {
    "PermissionRequest": [
      {
        "matcher": "*",
        "hooks": [
          {
            "type": "command",
            "command": "/usr/local/bin/approve --only-if /usr/share/omarchy/bin/omarchy-hyprland-session-locked",
            "timeout": 90
          }
        ]
      }
    ]
  }
}
```

New Claude Code sessions pick it up. To take it out:
`deploy/claude-hook.sh remove`.

Use `"matcher": "Bash"` instead of `"*"` to send only shell commands to the
phone. In `claude -p` and other sessions that can't show a prompt, an
unanswered request is denied.

## Security notes

- The pages are reachable only on the tailnet, through `tailscale serve`.
  Don't turn on Funnel for them.
- Every answer, Approve or Deny, needs a passkey signature from a registered
  device. Each challenge works once and only until the request expires.
- A notification says only "sudo on <machine>" or "Claude Code on
  <machine>", and a link back to this machine. The command is shown only on
  the page, after it loads over the tailnet.
- Web Push goes through Apple's push service. The payload is encrypted for
  the device (RFC 8291) and signed with this machine's VAPID key (RFC 8292),
  so Apple can't read it and nobody else can push to the app. The key is
  made on first start in `/var/lib/private/approver/vapid.key`; deleting it
  means every device turns notifications on again.
- Turning on notifications needs a passkey signature, so a visitor on the
  tailnet can't add their own device.
- The local API on 127.0.0.1:7070 accepts requests from any local user, so
  any of them can send you a push. Approving still needs your passkey.
