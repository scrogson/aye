# approver

Approve `sudo` (and Claude Code's permission prompts) from your phone, with a
passkey and Face ID, over your Tailscale network.

When something on the machine needs approval, `approverd` sends a push through
a self-hosted ntfy. The push carries only a link. The page it opens shows the
exact command, and Approve or Deny is signed with a passkey that only works for
this machine. No answer in 60 seconds counts as no answer: sudo falls back to
your password, and Claude Code shows its own prompt.

```
sudo / Claude Code ──► approve ──► approverd (127.0.0.1:7070) ──► ntfy ──► phone
                                        ▲                                    │
                                        └──── signed answer (Face ID) ◄──────┘
                                   pages on https://<machine>.<tailnet>.ts.net
```

## What's in here

| Path | What it is |
| --- | --- |
| `src/bin/approverd.rs` | The daemon: local API, approval and registration pages, control socket, and the `approverd` CLI |
| `src/bin/approve.rs` | The client: a PAM helper for sudo and a Claude Code hook |
| `deploy/setup.sh` | Installs ntfy, approverd and `tailscale serve` on this machine |
| `deploy/pam.sh` | Adds phone approval to sudo (`test`, `install`, `remove`) |
| `deploy/claude-hook.sh` | Adds the Claude Code hook to `~/.claude/settings.json` (`install`, `remove`) |

## Setup

Needs Arch (ntfy comes from the AUR via `yay`), Tailscale with HTTPS enabled
for the tailnet, `jq`, and a Rust toolchain. Run everything below from a normal
terminal: `sudo` doesn't work inside the Claude app's terminal.

1. Build:

   ```bash
   cargo build --release
   ```

2. Install ntfy, approverd and the Tailscale HTTPS endpoints. The script finds
   this machine's Tailscale name by itself. Set `PHONE_USER` to the ntfy account
   your phone's ntfy app signs in with (it defaults to your login name, and asks
   for a password the first time):

   ```bash
   PHONE_USER=alice deploy/setup.sh
   ```

3. On the phone, in the ntfy app, add the server
   `https://<machine>.<tailnet>.ts.net:8443`, sign in as `PHONE_USER` and
   subscribe to `approvals`.

4. Add the phone's passkey. Registration closes by itself after 10 minutes:

   ```bash
   sudo approverd register open
   ```

   Then open `https://<machine>.<tailnet>.ts.net/register` on the phone and
   tap **Add passkey**. Add a second device (or a YubiKey) as a backup.

On an iPhone, turn off iCloud Private Relay if Safari says the connection
isn't private: it breaks tailnet names on port 443.

## Managing it

All commands talk to the running daemon over a root-only socket, so they need
`sudo`:

```bash
sudo approverd status                    # address, passkeys, registration, waiting requests
sudo approverd register open --for 5m    # 90s, 10m and 1h work too
sudo approverd register close
sudo approverd passkeys                  # list passkey ids
sudo approverd passkeys remove <id>      # 4 or more characters of the id
```

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

- The pages and ntfy are reachable only on the tailnet, through
  `tailscale serve`. Don't turn on Funnel for them.
- Every answer, Approve or Deny, needs a passkey signature from a registered
  device. Each challenge works once and only until the request expires.
- The push says only "Approval waiting on <machine>". The command is shown
  only on the page, after it loads from the machine over the tailnet.
- iOS needs ntfy.sh to wake the app (`upstream-base-url`). ntfy.sh sees a
  hash of the topic URL, never the message. Someone who guesses the topic
  could send you a fake push, but it can't approve anything.
- The local API on 127.0.0.1:7070 accepts requests from any local user, so
  any of them can send you a push. Approving still needs your passkey.
