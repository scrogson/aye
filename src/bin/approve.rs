use serde_json::{json, Value};
use std::io::Read;
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

fn main() {
    let pam = std::env::var("PAM_USER").ok();
    // The daemon adds the machine name itself, so the page can't be told a different host.
    let request = match &pam {
        Some(user) => json!({ "user": user, "kind": "sudo", "command": parent_cmdline() }),
        None => {
            let mut s = String::new();
            std::io::stdin().read_to_string(&mut s).ok();
            let v: Value = serde_json::from_str(&s).unwrap_or_default();
            json!({
                "user": std::env::var("USER").unwrap_or_default(),
                "kind": "claude",
                "command": v["tool_input"]["command"].as_str().unwrap_or("?"),
            })
        }
    };

    let approved = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(70))
        .build()
        .and_then(|c| c.post("http://127.0.0.1:7070/local/request")
            .json(&request)
            .send())
        .and_then(|r| r.json::<bool>())
        .unwrap_or(false);

    if pam.is_some() {
        std::process::exit(if approved { 0 } else { 1 });
    }
    println!("{}", json!({ "hookSpecificOutput": {
        "hookEventName": "PreToolUse",
        "permissionDecision": if approved { "allow" } else { "deny" },
        "permissionDecisionReason": "Passkey-signed remote approval"
    }}));
}
