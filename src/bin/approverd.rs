use axum::{
    extract::{Path, State},
    http::{header, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use ct_codecs::{Base64UrlSafeNoPadding, Encoder};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::HashMap, os::unix::fs::{MetadataExt, PermissionsExt}, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::{oneshot, Mutex},
    time::Instant,
};
use web_push::{ContentEncoding, SubscriptionInfo, Urgency, VapidSignatureBuilder, WebPushMessageBuilder};
use webauthn_rs::prelude::*;

/// How long a request waits for an answer before it counts as a deny.
const TIMEOUT: Duration = Duration::from_secs(60);
/// How long `approverd register open` keeps registration open by default.
const REGISTER_FOR: Duration = Duration::from_secs(10 * 60);

const USAGE: &str = "\
usage:
  approverd                          run the daemon
  approverd status                   show address, passkeys and registration
  approverd register open [--for 10m]
                                     let devices add a passkey (closes itself)
  approverd register close
  approverd passkeys                 list registered passkeys
  approverd passkeys remove <id>     remove one (an id prefix is enough)
  approverd push                     list devices that get notifications
  approverd push test                send a test notification to each
  approverd push remove <n>          stop notifying one (number from the list)

The commands talk to the running daemon over its control socket, which only
root can open: run them with sudo.";

struct Pending {
    user: String,
    kind: String,
    command: String,
    deadline: Instant,
    auth: Option<PasskeyAuthentication>,
    done: Option<oneshot::Sender<bool>>,
}

/// A device that turned on notifications from the home page.
#[derive(Clone, Serialize, Deserialize)]
struct PushSub {
    endpoint: String,
    p256dh: String,
    auth: String,
    /// "iPhone", "Mac", ... from its user agent, for `approverd push`.
    device: String,
    /// Unix seconds.
    added: u64,
}

struct App {
    webauthn: Webauthn,
    origin: String,
    rp_id: String,
    host: String,
    keys_file: String,
    /// VAPID private key (raw P-256, base64url) and its public half.
    vapid_key: String,
    vapid_public: String,
    subs_file: String,
    subs: Mutex<Vec<PushSub>>,
    /// Passkey challenges for turning on notifications, by id.
    push_logins: Mutex<HashMap<Uuid, (PasskeyAuthentication, Instant)>>,
    passkeys: Mutex<Vec<Passkey>>,
    reg: Mutex<Option<PasskeyRegistration>>,
    pending: Mutex<HashMap<Uuid, Pending>>,
    /// Registration is open until this moment; None means closed.
    register_until: Mutex<Option<Instant>>,
}

impl App {
    async fn registration_open(&self) -> bool {
        self.register_until.lock().await.is_some_and(|t| t > Instant::now())
    }
}

type S = State<Arc<App>>;

fn env(key: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| panic!("{key} must be set"))
}

/// The daemon's control socket, in the runtime directory systemd makes for it.
fn socket_path() -> String {
    std::env::var("APPROVER_SOCKET").unwrap_or_else(|_| {
        let dir = std::env::var("RUNTIME_DIRECTORY").unwrap_or_else(|_| "/run/approver".into());
        format!("{dir}/control.sock")
    })
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let cmd = match args.as_slice() {
        [] | ["serve"] => return serve().await,
        ["status"] => json!({ "cmd": "status" }),
        ["register", "open"] => json!({ "cmd": "register_open", "seconds": REGISTER_FOR.as_secs() }),
        ["register", "open", "--for", d] => match parse_duration(d) {
            Some(d) => json!({ "cmd": "register_open", "seconds": d.as_secs() }),
            None => exit_usage(&format!("can't read {d:?} as a duration; try 10m, 90s or 1h")),
        },
        ["register", "close"] => json!({ "cmd": "register_close" }),
        ["passkeys"] | ["passkeys", "list"] => json!({ "cmd": "passkeys" }),
        ["passkeys", "remove", id] => json!({ "cmd": "passkeys_remove", "id": id }),
        ["push"] | ["push", "list"] => json!({ "cmd": "push" }),
        ["push", "test"] => json!({ "cmd": "push_test" }),
        ["push", "remove", n] => match n.parse::<u64>() {
            Ok(n) => json!({ "cmd": "push_remove", "n": n }),
            Err(_) => exit_usage("give the device's number from: approverd push"),
        },
        ["-h" | "--help" | "help"] => {
            println!("{USAGE}");
            return;
        }
        _ => exit_usage("unknown command"),
    };
    std::process::exit(control_client(cmd).await);
}

fn exit_usage(why: &str) -> ! {
    eprintln!("approverd: {why}\n\n{USAGE}");
    std::process::exit(2);
}

fn parse_duration(s: &str) -> Option<Duration> {
    let (n, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let n: u64 = n.parse().ok()?;
    let secs = match unit {
        "s" => n,
        "" | "m" => n * 60,
        "h" => n * 3600,
        _ => return None,
    };
    (secs > 0 && secs <= 24 * 3600).then(|| Duration::from_secs(secs))
}

/// Send one command to the running daemon and print its answer.
async fn control_client(cmd: Value) -> i32 {
    let path = socket_path();
    // Right after `systemctl restart` the daemon may not have made its socket
    // yet, so wait a moment for it before giving up.
    let mut tries = 0;
    let connected = loop {
        match UnixStream::connect(&path).await {
            Err(e) if tries < 30 && matches!(e.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused) => {
                tries += 1;
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            r => break r,
        }
    };
    let mut stream = match connected {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            eprintln!("approverd: permission denied on {path}; run this with sudo");
            return 1;
        }
        Err(e) => {
            eprintln!("approverd: can't reach the daemon at {path}: {e}\nIs it running? systemctl status approverd");
            return 1;
        }
    };
    if stream.write_all(format!("{cmd}\n").as_bytes()).await.is_err() {
        eprintln!("approverd: the daemon closed the connection");
        return 1;
    }
    let mut line = String::new();
    let _ = BufReader::new(stream).read_line(&mut line).await;
    let reply: Value = serde_json::from_str(&line).unwrap_or_else(|_| json!({ "ok": false, "text": "no answer from the daemon" }));
    let text = reply["text"].as_str().unwrap_or_default();
    if reply["ok"].as_bool() == Some(true) {
        println!("{text}");
        0
    } else {
        eprintln!("approverd: {text}");
        1
    }
}

async fn serve() {
    // This machine's Tailscale name, e.g. desk.tailnet-name.ts.net. It is both the
    // WebAuthn relying party id and the origin the approval pages are served on.
    let rp_id = env("APPROVER_HOST");
    let origin = format!("https://{rp_id}");
    let webauthn = WebauthnBuilder::new(&rp_id, &Url::parse(&origin).unwrap())
        .unwrap()
        .rp_name("Approver")
        .build()
        .unwrap();
    let state_dir = std::env::var("STATE_DIRECTORY").unwrap_or_else(|_| "/var/lib/approver".into());
    let keys_file = format!("{state_dir}/passkeys.json");
    let passkeys: Vec<Passkey> = std::fs::read_to_string(&keys_file)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let (vapid_key, vapid_public) = vapid_keys(&format!("{state_dir}/vapid.key"));
    let subs_file = format!("{state_dir}/push.json");
    let subs: Vec<PushSub> = std::fs::read_to_string(&subs_file)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    eprintln!("approverd: {origin}, {} passkey(s), {} device(s) for notifications", passkeys.len(), subs.len());
    let app = Arc::new(App {
        webauthn,
        host: rp_id.split('.').next().unwrap_or(&rp_id).to_string(),
        rp_id,
        origin,
        keys_file,
        vapid_key,
        vapid_public,
        subs_file,
        subs: Mutex::new(subs),
        push_logins: Mutex::new(HashMap::new()),
        passkeys: Mutex::new(passkeys),
        reg: Mutex::new(None),
        pending: Mutex::new(HashMap::new()),
        register_until: Mutex::new(None),
    });
    let control = tokio::spawn(control_server(app.clone(), socket_path()));

    let local = Router::new()
        .route("/local/request", post(request))
        .route("/local/ask", post(ask))
        .with_state(app.clone());
    let public = Router::new()
        .route("/", get(|| async { page("text/html; charset=utf-8", include_str!("home.html")) }))
        .route("/sw.js", get(|| async { page("text/javascript; charset=utf-8", include_str!("sw.js")) }))
        .route("/manifest.webmanifest", get(manifest))
        .route("/icon-180.png", get(|| async { page("image/png", &include_bytes!("icon-180.png")[..]) }))
        .route("/icon-192.png", get(|| async { page("image/png", &include_bytes!("icon-192.png")[..]) }))
        .route("/icon-512.png", get(|| async { page("image/png", &include_bytes!("icon-512.png")[..]) }))
        .route("/push/key", get(push_key))
        .route("/push/check", post(push_check))
        .route("/push/challenge", post(push_challenge))
        .route("/push/subscribe", post(push_subscribe))
        .route("/push/test", post(push_test_one))
        .route("/ui.css", get(|| async { page("text/css; charset=utf-8", include_str!("ui.css")) }))
        .route("/approve/{id}", get(|| async { page("text/html; charset=utf-8", include_str!("approve.html")) }))
        .route("/approve/{id}/challenge", post(challenge))
        .route("/approve/{id}/verify", post(verify))
        .route("/register", get(|| async { page("text/html; charset=utf-8", include_str!("register.html")) }))
        .route("/register/status", get(reg_status))
        .route("/register/start", post(reg_start))
        .route("/register/finish", post(reg_finish))
        // A real page, not an empty body a browser might keep showing.
        .fallback(|| async { (StatusCode::NOT_FOUND, page("text/html; charset=utf-8", NOT_FOUND)) })
        .with_state(app);

    // Overridable only so a test copy can run beside the installed one.
    let local_addr = std::env::var("LOCAL_ADDR").unwrap_or_else(|_| "127.0.0.1:7070".into());
    let pages_addr = std::env::var("PAGES_ADDR").unwrap_or_else(|_| "127.0.0.1:7071".into());
    let l = tokio::net::TcpListener::bind(&local_addr).await.unwrap();
    let p = tokio::net::TcpListener::bind(&pages_addr).await.unwrap();
    tokio::select! {
        r = axum::serve(l, local) => r.unwrap(),
        r = axum::serve(p, public) => r.unwrap(),
        r = control => r.unwrap(),
    }
}

// ---------- control socket: one JSON line in, one JSON line out ----------

async fn control_server(s: Arc<App>, path: String) {
    let _ = std::fs::remove_file(&path); // left over from a crash
    let listener = UnixListener::bind(&path).unwrap_or_else(|e| panic!("bind {path}: {e}"));
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let owner = std::fs::metadata(&path).unwrap().uid();
    loop {
        let Ok((stream, _)) = listener.accept().await else { continue };
        // The file mode already keeps others out; this also refuses anyone
        // who got hold of the socket some other way.
        let uid = stream.peer_cred().map(|c| c.uid()).unwrap_or(u32::MAX);
        let s = s.clone();
        tokio::spawn(async move {
            let (r, mut w) = stream.into_split();
            let reply = if uid != 0 && uid != owner {
                json!({ "ok": false, "text": "only root can control approverd; run this with sudo" })
            } else {
                let mut line = String::new();
                let _ = BufReader::new(r).read_line(&mut line).await;
                match serde_json::from_str::<Value>(&line) {
                    Ok(cmd) => control_command(&s, &cmd).await,
                    Err(_) => json!({ "ok": false, "text": "unreadable command" }),
                }
            };
            let _ = w.write_all(format!("{reply}\n").as_bytes()).await;
        });
    }
}

fn cred_id(pk: &Passkey) -> String {
    serde_json::to_value(pk.cred_id()).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default()
}

fn mins_secs(d: Duration) -> String {
    format!("{}:{:02}", d.as_secs() / 60, d.as_secs() % 60)
}

async fn control_command(s: &App, cmd: &Value) -> Value {
    let ok = |text: String| json!({ "ok": true, "text": text });
    let fail = |text: String| json!({ "ok": false, "text": text });
    match cmd["cmd"].as_str().unwrap_or_default() {
        "status" => {
            let keys = s.passkeys.lock().await.len();
            let reg = match *s.register_until.lock().await {
                Some(t) if t > Instant::now() => format!("open, closes in {}", mins_secs(t - Instant::now())),
                _ => "closed".into(),
            };
            let waiting = s.pending.lock().await.len();
            let phones = s.subs.lock().await.len();
            ok(format!(
                "address       {}\npasskeys      {keys}\nregistration  {reg}\ndevices       {phones} get notifications\nwaiting       {waiting} request(s)",
                s.origin
            ))
        }
        "push" => {
            let subs = s.subs.lock().await;
            if subs.is_empty() {
                return ok(format!("No devices get notifications. Open {}/ on the phone to turn them on.", s.origin));
            }
            let lines: Vec<String> = subs
                .iter()
                .enumerate()
                .map(|(i, p)| format!("{:>2}  {:<8} added {}  via {}", i + 1, p.device, ago(p.added), endpoint_host(&p.endpoint)))
                .collect();
            ok(lines.join("\n"))
        }
        "push_test" => {
            let subs = s.subs.lock().await.clone();
            if subs.is_empty() {
                return fail("no devices to notify".into());
            }
            let payload = json!({ "title": "Test from approverd", "body": format!("Notifications from {} work.", s.host), "url": "/" });
            let mut lines = Vec::new();
            for (i, sub) in subs.iter().enumerate() {
                let r = send_push(s, sub, &payload).await;
                if let Err((true, _)) = &r {
                    drop_sub(s, &sub.endpoint).await;
                }
                lines.push(match r {
                    Ok(()) => format!("{:>2}  {:<8} sent", i + 1, sub.device),
                    Err((true, e)) => format!("{:>2}  {:<8} gone, removed ({e})", i + 1, sub.device),
                    Err((false, e)) => format!("{:>2}  {:<8} failed: {e}", i + 1, sub.device),
                });
            }
            ok(lines.join("\n"))
        }
        "push_remove" => {
            let n = cmd["n"].as_u64().unwrap_or(0) as usize;
            let mut subs = s.subs.lock().await;
            if n == 0 || n > subs.len() {
                return fail(format!("no device {n}; see: approverd push"));
            }
            let gone = subs.remove(n - 1);
            save_subs(s, &subs);
            ok(format!("Stopped notifying {} ({}). {} device(s) left.", gone.device, ago(gone.added), subs.len()))
        }
        "register_open" => {
            let secs = cmd["seconds"].as_u64().unwrap_or(REGISTER_FOR.as_secs()).clamp(1, 24 * 3600);
            *s.register_until.lock().await = Some(Instant::now() + Duration::from_secs(secs));
            eprintln!("approverd: registration open for {secs}s");
            ok(format!(
                "Registration is open for {}. On the device, open {}/register\nIt closes by itself; to close it now: sudo approverd register close",
                mins_secs(Duration::from_secs(secs)),
                s.origin
            ))
        }
        "register_close" => {
            *s.register_until.lock().await = None;
            *s.reg.lock().await = None;
            eprintln!("approverd: registration closed");
            ok("Registration is closed.".into())
        }
        "passkeys" => {
            let keys = s.passkeys.lock().await;
            if keys.is_empty() {
                return ok("No passkeys registered. Add one with: sudo approverd register open".into());
            }
            let lines: Vec<String> = keys.iter().enumerate().map(|(i, k)| format!("{:>2}  {}", i + 1, cred_id(k))).collect();
            ok(lines.join("\n"))
        }
        "passkeys_remove" => {
            let want = cmd["id"].as_str().unwrap_or_default();
            if want.len() < 4 {
                return fail("give at least 4 characters of the passkey id (see: approverd passkeys)".into());
            }
            let mut keys = s.passkeys.lock().await;
            let hits: Vec<usize> = keys.iter().enumerate().filter(|(_, k)| cred_id(k).starts_with(want)).map(|(i, _)| i).collect();
            match hits.as_slice() {
                [i] => {
                    let gone = cred_id(&keys.remove(*i));
                    save_passkeys(s, &keys).await;
                    eprintln!("approverd: removed passkey {gone}");
                    ok(format!("Removed {gone}. {} passkey(s) left.", keys.len()))
                }
                [] => fail(format!("no passkey id starts with {want:?}")),
                _ => fail(format!("{} passkeys start with {want:?}; give more of the id", hits.len())),
            }
        }
        other => fail(format!("unknown command {other:?}")),
    }
}

/// A page or stylesheet: never cached, never framed (so the Approve button
/// can't be overlaid by another site), and only able to talk to this origin.
const NOT_FOUND: &str = r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><meta name="color-scheme" content="light dark"><title>Not found</title><link rel="stylesheet" href="/ui.css"></head><body><main class="screen"><div class="body"><div class="result"><h1>Nothing here</h1><p>Approval requests open from their notification. <a href="/">Go to Approver</a>.</p></div></div></main></body></html>"#;

fn page<B: IntoResponse>(content_type: &'static str, body: B) -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "no-store"),
            (
                header::CONTENT_SECURITY_POLICY,
                "default-src 'self'; script-src 'self' 'unsafe-inline'; style-src 'self'; \
                 img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'",
            ),
            (header::REFERRER_POLICY, "no-referrer"),
        ],
        body,
    )
}

async fn save_passkeys(s: &App, keys: &[Passkey]) {
    let tmp = format!("{}.tmp", s.keys_file);
    if std::fs::write(&tmp, serde_json::to_string(keys).unwrap()).is_ok() {
        let _ = std::fs::rename(&tmp, &s.keys_file);
    }
}

// ---------- local API: blocks until approved, denied, or timed out ----------

/// What a caller asks to approve. Newer callers send `user`, `kind` and
/// `command`; older ones (pam_approver) send one `summary` like
/// "alice@laptop: sudo brew upgrade", which is split as a fallback.
#[derive(Deserialize)]
struct Req {
    user: Option<String>,
    kind: Option<String>,
    command: Option<String>,
    summary: Option<String>,
}

/// The original endpoint: true only for a signed approval.
async fn request(State(s): S, Json(r): Json<Req>) -> Json<bool> {
    Json(decide(s, r).await == Some(true))
}

/// Tells "denied" apart from "nobody answered", for callers that fall back
/// to something else (the Claude Code hook shows its own dialog then).
async fn ask(State(s): S, Json(r): Json<Req>) -> Json<Value> {
    let answer = match decide(s, r).await {
        Some(true) => "approved",
        Some(false) => "denied",
        None => "unanswered",
    };
    Json(json!({ "answer": answer }))
}

/// Push to the phone and wait for a signed answer; None if none came in time.
async fn decide(s: Arc<App>, r: Req) -> Option<bool> {
    let (user, command) = match (r.user, r.command, r.summary) {
        (user, Some(command), _) => (user.unwrap_or_default(), command),
        (_, None, Some(summary)) => match summary.split_once(": ") {
            Some((who, cmd)) => (who.split('@').next().unwrap_or(who).to_string(), cmd.to_string()),
            None => (String::new(), summary),
        },
        _ => return None,
    };
    let kind = r.kind.unwrap_or_else(|| "sudo".into());
    let what = kind_label(&kind);

    let id = Uuid::new_v4();
    let (tx, rx) = oneshot::channel();
    let deadline = Instant::now() + TIMEOUT;
    s.pending.lock().await.insert(id, Pending { user, kind, command, deadline, auth: None, done: Some(tx) });
    // If the asker gives up (Enter at the sudo prompt, Ctrl-C), axum drops
    // this future; the guard then withdraws the request so the phone page
    // shows it as expired instead of offering to approve nothing.
    let mut guard = Withdraw { s: s.clone(), id, live: true };

    tokio::spawn(notify(s.clone(), id, what));

    let answer = tokio::time::timeout_at(deadline, rx).await.ok().and_then(Result::ok);
    guard.live = false;
    s.pending.lock().await.remove(&id);
    let said = match answer { Some(true) => "approved", Some(false) => "denied", None => "timed out" };
    eprintln!("approverd: {id} {said}");
    answer
}

struct Withdraw {
    s: Arc<App>,
    id: Uuid,
    live: bool,
}

impl Drop for Withdraw {
    fn drop(&mut self) {
        if !self.live {
            return;
        }
        let (s, id) = (self.s.clone(), self.id);
        tokio::spawn(async move {
            s.pending.lock().await.remove(&id);
            eprintln!("approverd: {id} withdrawn by the asker");
        });
    }
}

// ---------- approval flow ----------

async fn challenge(State(s): S, Path(id): Path<Uuid>) -> Result<Json<Value>, StatusCode> {
    let keys = s.passkeys.lock().await.clone();
    let (rcr, auth) = s.webauthn.start_passkey_authentication(&keys)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let mut p = s.pending.lock().await;
    let entry = p.get_mut(&id).ok_or(StatusCode::NOT_FOUND)?;
    entry.auth = Some(auth); // a new challenge replaces any old one
    Ok(Json(json!({
        "host": s.host,
        "user": entry.user,
        "kind": entry.kind,
        "command": entry.command,
        // Seconds left rather than a clock time, so the phone's clock doesn't matter.
        "expires_in": entry.deadline.saturating_duration_since(Instant::now()).as_secs_f64(),
        "options": rcr,
    })))
}

#[derive(Deserialize)]
struct Verify { approve: bool, credential: PublicKeyCredential }

async fn verify(State(s): S, Path(id): Path<Uuid>, Json(v): Json<Verify>) -> StatusCode {
    let auth = {
        let mut p = s.pending.lock().await;
        match p.get_mut(&id).and_then(|e| e.auth.take()) {
            Some(a) => a, // take() makes the challenge single-use
            None => return StatusCode::NOT_FOUND,
        }
    };
    let Ok(result) = s.webauthn.finish_passkey_authentication(&v.credential, &auth) else {
        return StatusCode::UNAUTHORIZED;
    };
    {
        let mut keys = s.passkeys.lock().await;
        let mut changed = false;
        for pk in keys.iter_mut() {
            // bumps the signature counter
            changed |= pk.update_credential(&result).unwrap_or(false);
        }
        if changed {
            save_passkeys(&s, &keys).await;
        }
    }
    if let Some(tx) = s.pending.lock().await.get_mut(&id).and_then(|e| e.done.take()) {
        let _ = tx.send(v.approve);
    }
    StatusCode::OK
}

// ---------- registration (only while `approverd register open` is in effect) ----------

async fn reg_status(State(s): S) -> Json<Value> {
    Json(json!({
        "open": s.registration_open().await,
        "passkeys": s.passkeys.lock().await.len(),
        "host": s.host,
        "rp_id": s.rp_id,
    }))
}

async fn reg_start(State(s): S) -> Result<Json<CreationChallengeResponse>, StatusCode> {
    if !s.registration_open().await { return Err(StatusCode::FORBIDDEN); }
    let exclude: Vec<_> = s.passkeys.lock().await.iter().map(|k| k.cred_id().clone()).collect();
    let (ccr, reg) = s.webauthn
        .start_passkey_registration(Uuid::new_v4(), "approver", "Approver", Some(exclude))
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    *s.reg.lock().await = Some(reg);
    Ok(Json(ccr))
}

async fn reg_finish(State(s): S, Json(c): Json<RegisterPublicKeyCredential>) -> StatusCode {
    if !s.registration_open().await { return StatusCode::FORBIDDEN; }
    let Some(reg) = s.reg.lock().await.take() else { return StatusCode::BAD_REQUEST };
    let Ok(pk) = s.webauthn.finish_passkey_registration(&c, &reg) else { return StatusCode::UNAUTHORIZED };
    let mut keys = s.passkeys.lock().await;
    keys.push(pk);
    save_passkeys(&s, &keys).await;
    eprintln!("approverd: registered passkey #{}", keys.len());
    StatusCode::OK
}

// ---------- notifications: Web Push to the home-screen app ----------

fn kind_label(kind: &str) -> &'static str {
    if kind == "claude" { "Claude Code" } else { "sudo" }
}

/// Every push carries only a link and what kind of request it is, never the
/// command or the decision: the page loads those over the tailnet.
async fn notify(s: Arc<App>, id: Uuid, what: &'static str) {
    let payload = json!({
        "title": "Approval needed",
        "body": format!("{what} on {}", s.host),
        "url": format!("/approve/{id}"),
        "tag": id.to_string(),
    });
    let subs = s.subs.lock().await.clone();
    if subs.is_empty() {
        eprintln!("approverd: no device gets notifications; open {}/ on the phone to turn them on", s.origin);
    }
    for sub in &subs {
        match send_push(&s, sub, &payload).await {
            Ok(()) => {}
            Err((true, e)) => {
                eprintln!("approverd: {} stopped accepting pushes ({e}); removed", sub.device);
                drop_sub(&s, &sub.endpoint).await;
            }
            Err((false, e)) => eprintln!("approverd: push to {} failed: {e}", sub.device),
        }
    }
}

/// Encrypt (RFC 8291) and sign (VAPID, RFC 8292) one push and hand it to the
/// device's push service. Err((true, _)) means the subscription is gone.
async fn send_push(s: &App, sub: &PushSub, payload: &Value) -> Result<(), (bool, String)> {
    let request = build_push(&s.vapid_key, &s.origin, sub, payload).map_err(|e| (false, e))?;
    let (parts, body) = request;
    let mut req = reqwest::Client::new().post(&sub.endpoint).body(body);
    for (k, v) in parts {
        req = req.header(k, v);
    }
    let r = req.timeout(Duration::from_secs(15)).send().await.map_err(|e| (false, e.to_string()))?;
    match r.status().as_u16() {
        200..=299 => Ok(()),
        404 | 410 => Err((true, format!("HTTP {}", r.status()))),
        code => {
            let text = r.text().await.unwrap_or_default();
            Err((false, format!("HTTP {code} {}", text.chars().take(200).collect::<String>())))
        }
    }
}

/// Headers and the encrypted body, ready to POST to the push service.
type PushRequest = (Vec<(String, String)>, Vec<u8>);

fn build_push(vapid_key: &str, origin: &str, sub: &PushSub, payload: &Value) -> Result<PushRequest, String> {
    let info = SubscriptionInfo::new(sub.endpoint.as_str(), sub.p256dh.as_str(), sub.auth.as_str());
    let mut sig = VapidSignatureBuilder::from_base64(vapid_key, &info).map_err(|e| e.to_string())?;
    // Apple wants a contact; this machine's address is enough and isn't personal.
    sig.add_claim("sub", origin);
    let sig = sig.build().map_err(|e| e.to_string())?;
    let body = payload.to_string();
    let mut b = WebPushMessageBuilder::new(&info);
    b.set_payload(ContentEncoding::Aes128Gcm, body.as_bytes());
    b.set_ttl(TIMEOUT.as_secs() as u32); // no use arriving after the request expired
    b.set_urgency(Urgency::High);
    b.set_vapid_signature(sig);
    let message = b.build().map_err(|e| e.to_string())?;
    let request = web_push::request_builder::build_request::<Vec<u8>>(message);
    let headers = request
        .headers()
        .iter()
        .filter(|(k, _)| k.as_str() != "content-length")
        .filter_map(|(k, v)| Some((k.as_str().to_string(), v.to_str().ok()?.to_string())))
        .collect();
    Ok((headers, request.into_body()))
}

/// Load the VAPID key, or make one on first start.
fn vapid_keys(path: &str) -> (String, String) {
    let key = match std::fs::read_to_string(path) {
        Ok(k) => k.trim().to_string(),
        Err(_) => {
            let raw = jwt_simple::prelude::ES256KeyPair::generate().to_bytes();
            let k = Base64UrlSafeNoPadding::encode_to_string(raw).unwrap();
            std::fs::write(path, &k).unwrap_or_else(|e| panic!("write {path}: {e}"));
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
            k
        }
    };
    let public = VapidSignatureBuilder::from_base64_no_sub(&key)
        .unwrap_or_else(|_| panic!("{path} isn't a VAPID key; delete it to make a new one (devices then turn notifications on again)"))
        .get_public_key();
    (key, Base64UrlSafeNoPadding::encode_to_string(public).unwrap())
}

fn save_subs(s: &App, subs: &[PushSub]) {
    let tmp = format!("{}.tmp", s.subs_file);
    if std::fs::write(&tmp, serde_json::to_string(subs).unwrap()).is_ok() {
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
        let _ = std::fs::rename(&tmp, &s.subs_file);
    }
}

async fn drop_sub(s: &App, endpoint: &str) {
    let mut subs = s.subs.lock().await;
    subs.retain(|p| p.endpoint != endpoint);
    save_subs(s, &subs);
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn ago(t: u64) -> String {
    match now_secs().saturating_sub(t) {
        s if s < 3600 => "just now".into(),
        s if s < 2 * 86400 => format!("{}h ago", s / 3600),
        s => format!("{}d ago", s / 86400),
    }
}

fn endpoint_host(endpoint: &str) -> String {
    Url::parse(endpoint).ok().and_then(|u| u.host_str().map(str::to_string)).unwrap_or_else(|| "?".into())
}

fn device_name(ua: &str) -> String {
    let name = ["iPhone", "iPad", "Android", "Macintosh", "Windows", "Linux"]
        .into_iter()
        .find(|d| ua.contains(d))
        .unwrap_or("browser");
    if name == "Macintosh" { "Mac".into() } else { name.into() }
}

async fn manifest(State(s): S) -> impl IntoResponse {
    let body = json!({
        "name": format!("Approver ({})", s.host),
        "short_name": "Approver",
        "description": format!("Approve sudo and Claude Code on {} with your passkey", s.host),
        "start_url": "/",
        "scope": "/",
        "display": "standalone",
        "background_color": "#122033",
        "theme_color": "#122033",
        "icons": [
            { "src": "/icon-192.png", "sizes": "192x192", "type": "image/png" },
            { "src": "/icon-512.png", "sizes": "512x512", "type": "image/png" }
        ]
    });
    page("application/manifest+json", body.to_string())
}

async fn push_key(State(s): S) -> Json<Value> {
    Json(json!({ "key": s.vapid_public, "host": s.host, "passkeys": s.passkeys.lock().await.len() }))
}

#[derive(Deserialize)]
struct Endpoint {
    endpoint: String,
}

async fn push_check(State(s): S, Json(e): Json<Endpoint>) -> Json<Value> {
    let known = s.subs.lock().await.iter().any(|p| p.endpoint == e.endpoint);
    Json(json!({ "known": known }))
}

/// A passkey challenge for turning notifications on, so only someone with a
/// registered passkey can have approval links sent to their device.
async fn push_challenge(State(s): S) -> Result<Json<Value>, StatusCode> {
    let keys = s.passkeys.lock().await.clone();
    if keys.is_empty() {
        return Err(StatusCode::CONFLICT);
    }
    let (rcr, auth) = s.webauthn.start_passkey_authentication(&keys).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let id = Uuid::new_v4();
    let mut logins = s.push_logins.lock().await;
    logins.retain(|_, (_, t)| t.elapsed() < Duration::from_secs(300));
    logins.insert(id, (auth, Instant::now()));
    Ok(Json(json!({ "id": id, "options": rcr })))
}

#[derive(Deserialize)]
struct SubKeys {
    p256dh: String,
    auth: String,
}

#[derive(Deserialize)]
struct Subscription {
    endpoint: String,
    keys: SubKeys,
}

#[derive(Deserialize)]
struct Subscribe {
    id: Uuid,
    credential: PublicKeyCredential,
    subscription: Subscription,
}

async fn push_subscribe(State(s): S, headers: axum::http::HeaderMap, Json(r): Json<Subscribe>) -> StatusCode {
    let Some((auth, t)) = s.push_logins.lock().await.remove(&r.id) else {
        return StatusCode::NOT_FOUND;
    };
    if t.elapsed() > Duration::from_secs(300) {
        return StatusCode::NOT_FOUND;
    }
    let Ok(result) = s.webauthn.finish_passkey_authentication(&r.credential, &auth) else {
        return StatusCode::UNAUTHORIZED;
    };
    {
        let mut keys = s.passkeys.lock().await;
        let mut changed = false;
        for pk in keys.iter_mut() {
            changed |= pk.update_credential(&result).unwrap_or(false);
        }
        if changed {
            save_passkeys(&s, &keys).await;
        }
    }
    let sub = r.subscription;
    if !sub.endpoint.starts_with("https://") {
        return StatusCode::BAD_REQUEST;
    }
    let ua = headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok()).unwrap_or_default();
    let new = PushSub { endpoint: sub.endpoint, p256dh: sub.keys.p256dh, auth: sub.keys.auth, device: device_name(ua), added: now_secs() };
    eprintln!("approverd: notifications on for {} ({})", new.device, endpoint_host(&new.endpoint));
    let mut subs = s.subs.lock().await;
    subs.retain(|p| p.endpoint != new.endpoint);
    subs.push(new);
    save_subs(&s, &subs);
    StatusCode::OK
}

/// "Send a test" on the home page: only to a device that is already on the list.
async fn push_test_one(State(s): S, Json(e): Json<Endpoint>) -> StatusCode {
    let Some(sub) = s.subs.lock().await.iter().find(|p| p.endpoint == e.endpoint).cloned() else {
        return StatusCode::NOT_FOUND;
    };
    let payload = json!({ "title": "Test from approverd", "body": format!("Notifications from {} work.", s.host), "url": "/" });
    match send_push(&s, &sub, &payload).await {
        Ok(()) => StatusCode::OK,
        Err((true, _)) => {
            drop_sub(&s, &sub.endpoint).await;
            StatusCode::GONE
        }
        Err((false, e)) => {
            eprintln!("approverd: test push to {} failed: {e}", sub.device);
            StatusCode::BAD_GATEWAY
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What we send must decrypt with the subscriber's keys, as a phone would.
    #[test]
    fn push_round_trip() {
        let (keypair, auth) = ece::generate_keypair_and_auth_secret().unwrap();
        let components = keypair.raw_components().unwrap();
        let sub = PushSub {
            endpoint: "https://web.push.apple.com/QGuQyavXutnMH9lOCbG2xZ".into(),
            p256dh: Base64UrlSafeNoPadding::encode_to_string(components.public_key()).unwrap(),
            auth: Base64UrlSafeNoPadding::encode_to_string(auth).unwrap(),
            device: "iPhone".into(),
            added: 0,
        };
        let (vapid, public) = {
            let dir = std::env::temp_dir().join(format!("approverd-test-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let k = vapid_keys(dir.join("vapid.key").to_str().unwrap());
            std::fs::remove_dir_all(&dir).unwrap();
            k
        };
        let payload = json!({ "title": "Approval needed", "body": "sudo on desk", "url": "/approve/x" });
        let (headers, body) = build_push(&vapid, "https://desk.example.ts.net", &sub, &payload).unwrap();

        let plain = ece::decrypt(&components, &auth, &body).unwrap();
        assert_eq!(serde_json::from_slice::<Value>(&plain).unwrap(), payload);

        let h: HashMap<_, _> = headers.into_iter().collect();
        assert_eq!(h["content-encoding"], "aes128gcm");
        assert_eq!(h["ttl"], "60");
        assert_eq!(h["urgency"], "high");
        // "vapid t=<jwt>, k=<public key>": the key is the one browsers subscribe
        // with, and the token is signed by it for this push service and origin.
        let (t, k) = h["authorization"].strip_prefix("vapid t=").unwrap().split_once(", k=").unwrap();
        assert_eq!(k, public);
        use jwt_simple::prelude::*;
        let pk = ES256PublicKey::from_bytes(&ct_codecs::Base64UrlSafeNoPadding::decode_to_vec(k, None).unwrap()).unwrap();
        let claims = pk.verify_token::<Value>(t, None).unwrap();
        assert_eq!(claims.audiences.unwrap().into_string().unwrap(), "https://web.push.apple.com");
        assert_eq!(claims.subject.as_deref(), Some("https://desk.example.ts.net"));
    }

    #[test]
    fn device_names() {
        assert_eq!(device_name("Mozilla/5.0 (iPhone; CPU iPhone OS 18_1 like Mac OS X)"), "iPhone");
        assert_eq!(device_name("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7)"), "Mac");
        assert_eq!(device_name(""), "browser");
    }
}
