use serde_json::{Value, json};
use std::{
    fs::{File, OpenOptions},
    io::{self, BufRead, BufReader, IsTerminal, Read, Write},
    os::unix::{
        fs::{FileTypeExt, MetadataExt},
        net::UnixStream,
    },
    path::Path,
    time::Duration,
};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

/// The exact command sudo is about to run, read from our parent process.
/// pam_exec forks us from sudo, so the parent's argv is e.g. "sudo apt install foo".
/// sysinfo reads /proc on Linux and sysctl(KERN_PROCARGS2) on macOS.
fn parent_cmdline() -> String {
    let ppid = Pid::from_u32(std::os::unix::process::parent_id());
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[ppid]),
        true,
        ProcessRefreshKind::nothing().with_cmd(UpdateKind::Always),
    );
    sys.process(ppid)
        .map(|p| {
            p.cmd()
                .iter()
                .map(|s| quote(&s.to_string_lossy()))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown command".into())
}

/// One argument as you'd type it in a shell, so `sudo sh -c 'a; b'` isn't
/// shown as `sudo sh -c a; b`.
fn quote(arg: &str) -> String {
    let plain = |c: char| c.is_ascii_alphanumeric() || "_-./=:,+@%".contains(c);
    if !arg.is_empty() && arg.chars().all(plain) {
        arg.to_string()
    } else {
        format!("'{}'", arg.replace('\'', "'\\''"))
    }
}

#[derive(PartialEq)]
enum Answer {
    Approved,
    Denied,
    /// Nobody answered in time.
    Unanswered,
}

/// ayed's socket for approvals; src/bin/ayed.rs has the same path. It is
/// fixed, with no setting to change it, because sudo lets you through on what
/// comes back from it: /run/aye is made by systemd for the daemon and is gone
/// while it's stopped, and /var/db/aye-ask belongs to the daemon's user, so
/// nothing else on the machine can put a socket there.
const ASK_SOCKET: &str = if cfg!(target_os = "macos") {
    "/var/db/aye-ask/ask.sock"
} else {
    "/run/aye/ask.sock"
};

fn ask_socket() -> String {
    // A debug build can be pointed at a test copy of ayed.
    #[cfg(debug_assertions)]
    if let Ok(path) = std::env::var("AYE_ASK_SOCKET") {
        return path;
    }
    ASK_SOCKET.into()
}

/// A socket, owned by root or by whoever owns its directory, in a directory
/// nobody else can write to. Catches a directory set up wrongly, where
/// another user could have swapped the socket for their own.
fn trusted(path: &Path) -> io::Result<bool> {
    let sock = std::fs::metadata(path)?;
    let dir = std::fs::metadata(path.parent().unwrap_or(Path::new("/")))?;
    Ok(sock.file_type().is_socket()
        && dir.mode() & 0o022 == 0
        && (sock.uid() == 0 || sock.uid() == dir.uid()))
}

/// Ask ayed and wait for a signed answer from the phone: one line of JSON
/// each way. The connection stays open while waiting, and closing it (when
/// this process exits) withdraws the request.
fn ask(request: &Value) -> io::Result<Answer> {
    let path = ask_socket();
    if !trusted(Path::new(&path))? {
        return Err(io::Error::other(format!("{path} isn't ayed's socket")));
    }
    let mut stream = UnixStream::connect(&path)?;
    stream.set_read_timeout(Some(Duration::from_secs(70)))?;
    stream.write_all(format!("{request}\n").as_bytes())?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    let v: Value = serde_json::from_str(&line).unwrap_or_default();
    match v["answer"].as_str() {
        Some("approved") => Ok(Answer::Approved),
        Some("denied") => Ok(Answer::Denied),
        Some("unanswered") => Ok(Answer::Unanswered),
        // Too long a command, or too many requests waiting already.
        _ => Err(io::Error::other("ayed refused the request")),
    }
}

/// PAM mode, run by `auth sufficient pam_exec.so quiet /usr/local/bin/aye`.
/// Exit 0 lets sudo through; anything else falls on to the next PAM line
/// (the password), so a down daemon or a missed push only costs the wait.
fn pam(user: String) -> ! {
    if std::env::var("PAM_TYPE").is_ok_and(|t| t != "auth") {
        std::process::exit(1);
    }
    let request = json!({ "user": user, "kind": "sudo", "command": parent_cmdline() });

    // pam_exec gives us /dev/null for stdin, so talk to the terminal directly.
    // No terminal (a GUI askpass, say) just means waiting without the hint.
    let mut tty: Option<File> = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .ok();
    if let Some(t) = tty.as_mut() {
        let _ = writeln!(
            t,
            "Approve on your phone, or press Enter to type your password."
        );
        if let Ok(r) = t.try_clone() {
            std::thread::spawn(move || {
                let _ = BufReader::new(r).read_line(&mut String::new());
                std::process::exit(1);
            });
        }
    }

    let answer = ask(&request);
    if let Some(t) = tty.as_mut() {
        let _ = match &answer {
            Ok(Answer::Approved) => writeln!(t, "Approved on your phone."),
            Ok(Answer::Denied) => writeln!(t, "Denied on your phone."),
            Ok(Answer::Unanswered) => writeln!(t, "No answer from your phone."),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                ) =>
            {
                writeln!(t, "Phone approval isn't running here.")
            }
            Err(_) => writeln!(t, "Phone approval failed."),
        };
    }
    std::process::exit(if matches!(answer, Ok(Answer::Approved)) {
        0
    } else {
        1
    });
}

/// What the phone shows for a Claude Code tool call: the command itself for
/// Bash, the file or URL for the common tools, the raw input otherwise. None
/// if it is too long to show whole: an approval covers the whole call, so a
/// call that would be cut goes to Claude's own prompt instead.
fn describe(v: &Value) -> Option<String> {
    let tool = v["tool_name"].as_str().unwrap_or("?");
    let input = &v["tool_input"];
    let field = |k: &str| input[k].as_str().map(str::to_string);
    let what = match tool {
        "Bash" => field("command"),
        "Edit" | "MultiEdit" | "Write" | "Read" => {
            field("file_path").map(|f| format!("{tool} {f}"))
        }
        "NotebookEdit" => field("notebook_path").map(|f| format!("{tool} {f}")),
        "WebFetch" => field("url").map(|u| format!("{tool} {u}")),
        "WebSearch" => field("query").map(|q| format!("{tool} {q}")),
        _ => None,
    }
    .unwrap_or_else(|| format!("{tool} {input}"));
    let text = match v["cwd"].as_str() {
        Some(cwd) => {
            let home = std::env::var("HOME").unwrap_or_default();
            let cwd = match cwd.strip_prefix(&home) {
                Some(rest) if !home.is_empty() => format!("~{rest}"),
                _ => cwd.to_string(),
            };
            format!("# in {cwd}\n{what}")
        }
        None => what,
    };
    (text.chars().count() <= 4000).then_some(text)
}

/// Claude Code hook mode. Meant for PermissionRequest, which only fires when
/// Claude would ask you anyway: a signed approval allows the call, a signed
/// deny refuses it, and anything else (no answer, daemon down, screen not
/// locked with --only-if) prints nothing, so Claude shows its usual prompt.
/// PreToolUse works too, with the same fallbacks.
fn hook(only_if: Option<String>) {
    if let Some(cmd) = only_if {
        let ok = std::process::Command::new("sh")
            .args(["-c", &cmd])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !ok {
            return;
        }
    }
    let mut s = String::new();
    std::io::stdin().read_to_string(&mut s).ok();
    let v: Value = serde_json::from_str(&s).unwrap_or_default();
    let event = v["hook_event_name"]
        .as_str()
        .unwrap_or("PermissionRequest")
        .to_string();

    let Some(command) = describe(&v) else { return };
    let answer = ask(&json!({
        "user": std::env::var("USER").unwrap_or_default(),
        "kind": "claude",
        "command": command,
    }));
    let approved = match answer {
        Ok(Answer::Approved) => true,
        Ok(Answer::Denied) => false,
        _ => return,
    };
    let out = if event == "PreToolUse" {
        json!({ "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": if approved { "allow" } else { "deny" },
            "permissionDecisionReason": if approved { "Approved on the phone (passkey-signed)" } else { "Denied on the phone (passkey-signed)" },
        }})
    } else if approved {
        json!({ "hookSpecificOutput": { "hookEventName": "PermissionRequest", "decision": { "behavior": "allow" } } })
    } else {
        json!({ "hookSpecificOutput": { "hookEventName": "PermissionRequest", "decision": {
            "behavior": "deny",
            "message": "The user denied this on their phone.",
        } } })
    };
    println!("{out}");
}

const USAGE: &str = "\
usage:
  aye [--only-if <shell command>]   Claude Code PermissionRequest hook (JSON on stdin)
  aye                               from pam_exec (or pam_aye on a Mac), for sudo
  sudo aye status|register|passkeys|push|log ...
                                    run ayed's admin commands (see: aye help)";

/// Commands `aye` hands to ayed, so there's one name to type.
const ADMIN: &[&str] = &[
    "status", "register", "passkeys", "push", "log", "help", "-h", "--help",
];

/// Runs ayed with the same arguments: the ayed next to this program, else
/// /usr/local/bin/ayed. Only returns if it can't be started.
fn admin(args: &[String]) -> ! {
    use std::os::unix::process::CommandExt;
    let beside = std::env::current_exe()
        .ok()
        .and_then(|p| Some(p.parent()?.join("ayed")))
        .filter(|p| p.is_file());
    let ayed = beside.unwrap_or_else(|| "/usr/local/bin/ayed".into());
    let err = std::process::Command::new(&ayed).args(args).exec();
    eprintln!("aye: can't run {}: {err}", ayed.display());
    std::process::exit(1);
}

fn main() {
    if let Ok(user) = std::env::var("PAM_USER") {
        pam(user);
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        [] if !std::io::stdin().is_terminal() => hook(None),
        ["--only-if", cmd] => hook(Some(cmd.to_string())),
        [first, ..] if ADMIN.contains(first) => admin(&args),
        _ => {
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_like_a_shell() {
        assert_eq!(quote("apt"), "apt");
        assert_eq!(quote("--config=/etc/x.conf"), "--config=/etc/x.conf");
        assert_eq!(quote("a; b"), "'a; b'");
        assert_eq!(quote(""), "''");
        assert_eq!(quote("it's"), r"'it'\''s'");
    }

    #[test]
    fn a_long_call_is_not_cut() {
        let call = |cmd: &str| json!({ "tool_name": "Bash", "tool_input": { "command": cmd } });
        assert_eq!(describe(&call("ls")).as_deref(), Some("ls"));
        assert_eq!(describe(&call(&"x".repeat(4001))), None);
    }
}
