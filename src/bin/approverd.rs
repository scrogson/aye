use axum::{
    extract::{Path, State},
    http::{header, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::{sync::{oneshot, Mutex}, time::Instant};
use webauthn_rs::prelude::*;

/// How long a request waits for an answer before it counts as a deny.
const TIMEOUT: Duration = Duration::from_secs(60);

struct Pending {
    user: String,
    kind: String,
    command: String,
    deadline: Instant,
    auth: Option<PasskeyAuthentication>,
    done: Option<oneshot::Sender<bool>>,
}

struct App {
    webauthn: Webauthn,
    origin: String,
    rp_id: String,
    host: String,
    ntfy_url: String,
    ntfy_token: String,
    keys_file: String,
    passkeys: Mutex<Vec<Passkey>>,
    reg: Mutex<Option<PasskeyRegistration>>,
    pending: Mutex<HashMap<Uuid, Pending>>,
    allow_register: bool,
}

type S = State<Arc<App>>;

fn env(key: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| panic!("{key} must be set"))
}

#[tokio::main]
async fn main() {
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
    let allow_register = std::env::var("ALLOW_REGISTER").is_ok_and(|v| v == "1");
    eprintln!(
        "approverd: {origin}, {} passkey(s), registration {}",
        passkeys.len(),
        if allow_register { "OPEN" } else { "closed" }
    );
    let app = Arc::new(App {
        webauthn,
        host: rp_id.split('.').next().unwrap_or(&rp_id).to_string(),
        rp_id,
        origin,
        ntfy_url: env("NTFY_URL"),
        ntfy_token: env("NTFY_TOKEN"),
        keys_file,
        passkeys: Mutex::new(passkeys),
        reg: Mutex::new(None),
        pending: Mutex::new(HashMap::new()),
        allow_register,
    });

    let local = Router::new()
        .route("/local/request", post(request))
        .with_state(app.clone());
    let public = Router::new()
        .route("/ui.css", get(|| async { page("text/css; charset=utf-8", include_str!("ui.css")) }))
        .route("/approve/{id}", get(|| async { page("text/html; charset=utf-8", include_str!("approve.html")) }))
        .route("/approve/{id}/challenge", post(challenge))
        .route("/approve/{id}/verify", post(verify))
        .route("/register", get(|| async { page("text/html; charset=utf-8", include_str!("register.html")) }))
        .route("/register/status", get(reg_status))
        .route("/register/start", post(reg_start))
        .route("/register/finish", post(reg_finish))
        .with_state(app);

    // Overridable only so a test copy can run beside the installed one.
    let local_addr = std::env::var("LOCAL_ADDR").unwrap_or_else(|_| "127.0.0.1:7070".into());
    let pages_addr = std::env::var("PAGES_ADDR").unwrap_or_else(|_| "127.0.0.1:7071".into());
    let l = tokio::net::TcpListener::bind(&local_addr).await.unwrap();
    let p = tokio::net::TcpListener::bind(&pages_addr).await.unwrap();
    tokio::join!(
        async { axum::serve(l, local).await.unwrap() },
        async { axum::serve(p, public).await.unwrap() }
    );
}

/// A page or stylesheet: never cached, never framed (so the Approve button
/// can't be overlaid by another site), and only able to talk to this origin.
fn page(content_type: &'static str, body: &'static str) -> impl IntoResponse {
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

async fn request(State(s): S, Json(r): Json<Req>) -> Json<bool> {
    let (user, command) = match (r.user, r.command, r.summary) {
        (user, Some(command), _) => (user.unwrap_or_default(), command),
        (_, None, Some(summary)) => match summary.split_once(": ") {
            Some((who, cmd)) => (who.split('@').next().unwrap_or(who).to_string(), cmd.to_string()),
            None => (String::new(), summary),
        },
        _ => return Json(false),
    };
    let kind = r.kind.unwrap_or_else(|| "sudo".into());

    let id = Uuid::new_v4();
    let (tx, rx) = oneshot::channel();
    let deadline = Instant::now() + TIMEOUT;
    s.pending.lock().await.insert(id, Pending { user, kind, command, deadline, auth: None, done: Some(tx) });

    // The push carries only a link, never the decision or the command.
    let sent = reqwest::Client::new()
        .post(&s.ntfy_url)
        .bearer_auth(&s.ntfy_token)
        .header("Title", "Approval needed")
        .header("Priority", "high")
        .header("Click", format!("{}/approve/{id}", s.origin))
        .body(format!("Approval waiting on {}", s.host))
        .send()
        .await
        .and_then(|r| r.error_for_status());
    if let Err(e) = sent {
        eprintln!("approverd: ntfy push failed: {e}");
    }

    let ok = tokio::time::timeout_at(deadline, rx)
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or(false);
    s.pending.lock().await.remove(&id);
    eprintln!("approverd: {id} {}", if ok { "approved" } else { "denied or timed out" });
    Json(ok)
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

// ---------- one-time registration (run with ALLOW_REGISTER=1, then restart without) ----------

async fn reg_status(State(s): S) -> Json<Value> {
    Json(json!({
        "open": s.allow_register,
        "passkeys": s.passkeys.lock().await.len(),
        "host": s.host,
        "rp_id": s.rp_id,
    }))
}

async fn reg_start(State(s): S) -> Result<Json<CreationChallengeResponse>, StatusCode> {
    if !s.allow_register { return Err(StatusCode::FORBIDDEN); }
    let exclude: Vec<_> = s.passkeys.lock().await.iter().map(|k| k.cred_id().clone()).collect();
    let (ccr, reg) = s.webauthn
        .start_passkey_registration(Uuid::new_v4(), "approver", "Approver", Some(exclude))
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    *s.reg.lock().await = Some(reg);
    Ok(Json(ccr))
}

async fn reg_finish(State(s): S, Json(c): Json<RegisterPublicKeyCredential>) -> StatusCode {
    if !s.allow_register { return StatusCode::FORBIDDEN; }
    let Some(reg) = s.reg.lock().await.take() else { return StatusCode::BAD_REQUEST };
    let Ok(pk) = s.webauthn.finish_passkey_registration(&c, &reg) else { return StatusCode::UNAUTHORIZED };
    let mut keys = s.passkeys.lock().await;
    keys.push(pk);
    save_passkeys(&s, &keys).await;
    eprintln!("approverd: registered passkey #{}", keys.len());
    StatusCode::OK
}
