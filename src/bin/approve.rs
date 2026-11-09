use serde_json::{json, Value};
use std::{
    fs::{File, OpenOptions},
    io::{BufRead, BufReader, Read, Write},
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
        .map(|p| p.cmd().iter().map(|s| s.to_string_lossy()).collect::<Vec<_>>().join(" "))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown command".into())
}

/// Ask approverd and wait for the phone. Any failure is a no.
fn ask(request: &Value) -> Result<bool, reqwest::Error> {
    reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(70))
        .build()?
        .post("http://127.0.0.1:7070/local/request")
        .json(request)
        .send()?
        .error_for_status()?
        .json::<bool>()
}

/// PAM mode, run by `auth sufficient pam_exec.so quiet /usr/local/bin/approve`.
/// Exit 0 lets sudo through; anything else falls on to the next PAM line
/// (the password), so a down daemon or a missed push only costs the wait.
fn pam(user: String) -> ! {
    if std::env::var("PAM_TYPE").is_ok_and(|t| t != "auth") {
        std::process::exit(1);
    }
    let request = json!({ "user": user, "kind": "sudo", "command": parent_cmdline() });

    // pam_exec gives us /dev/null for stdin, so talk to the terminal directly.
    // No terminal (a GUI askpass, say) just means waiting without the hint.
    let mut tty: Option<File> = OpenOptions::new().read(true).write(true).open("/dev/tty").ok();
    if let Some(t) = tty.as_mut() {
        let _ = writeln!(t, "Approve on your phone, or press Enter to type your password.");
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
            Ok(true) => writeln!(t, "Approved on your phone."),
            Ok(false) => writeln!(t, "Not approved on your phone."),
            Err(e) if e.is_connect() => writeln!(t, "Phone approval isn't running here."),
            Err(_) => writeln!(t, "Phone approval failed."),
        };
    }
    std::process::exit(if matches!(answer, Ok(true)) { 0 } else { 1 });
}

fn main() {
    if let Ok(user) = std::env::var("PAM_USER") {
        pam(user);
    }
    // Claude Code PreToolUse hook: the tool call arrives as JSON on stdin.
    let mut s = String::new();
    std::io::stdin().read_to_string(&mut s).ok();
    let v: Value = serde_json::from_str(&s).unwrap_or_default();
    let approved = ask(&json!({
        "user": std::env::var("USER").unwrap_or_default(),
        "kind": "claude",
        "command": v["tool_input"]["command"].as_str().unwrap_or("?"),
    }))
    .unwrap_or(false);
    println!("{}", json!({ "hookSpecificOutput": {
        "hookEventName": "PreToolUse",
        "permissionDecision": if approved { "allow" } else { "deny" },
        "permissionDecisionReason": "Passkey-signed remote approval"
    }}));
}
