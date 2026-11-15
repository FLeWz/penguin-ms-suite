//! OAuth2 authorization-code + PKCE (loopback redirect) against the Microsoft identity platform.
//! Public client: no secret. Refresh token stored 0600 in the XDG config dir.
use serde_json::Value;
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    sync::Mutex,
    time::{Duration, Instant},
};

const DEFAULT_SCOPES: &str =
    "offline_access User.Read Mail.Read Calendars.Read Chat.ReadWrite Files.Read";

/// Space-separated scopes; override with PENGUIN_MS_SCOPES (tenants may block some).
fn scopes() -> String {
    std::env::var("PENGUIN_MS_SCOPES").unwrap_or_else(|_| DEFAULT_SCOPES.into())
}

// App registration identifiers (not secrets). Env / config.json override.
const DEFAULT_CLIENT_ID: &str = "00000000-0000-0000-0000-000000000000";
const DEFAULT_TENANT: &str = "00000000-0000-0000-0000-000000000000";

static ACCESS: Mutex<Option<(String, Instant)>> = Mutex::new(None);

fn dir() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("penguin-ms-suite")
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(30)))
        .build()
        .into()
}

/// (client_id, tenant). Env vars override config.json.
pub fn config() -> Result<(String, String), String> {
    let file: Value = fs::read_to_string(dir().join("config.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(Value::Null);
    let pick = |env: &str, key: &str| {
        std::env::var(env)
            .ok()
            .or_else(|| file[key].as_str().map(String::from))
    };
    let id = pick("PENGUIN_MS_CLIENT_ID", "client_id").unwrap_or_else(|| DEFAULT_CLIENT_ID.into());
    let tenant = pick("PENGUIN_MS_TENANT", "tenant").unwrap_or_else(|| DEFAULT_TENANT.into());
    Ok((id, tenant))
}

fn endpoint(tenant: &str, leaf: &str) -> String {
    format!("https://login.microsoftonline.com/{tenant}/oauth2/v2.0/{leaf}")
}

fn err_of(v: &Value) -> String {
    v["error_description"]
        .as_str()
        .or(v["error"].as_str())
        .unwrap_or("unknown auth error")
        .to_string()
}

fn b64url(b: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut o = String::new();
    for c in b.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..=c.len() {
            o.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
        }
    }
    o
}

fn random_b64(n: usize) -> Result<String, String> {
    use std::io::Read;
    let mut b = vec![0u8; n];
    fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut b))
        .map_err(|e| e.to_string())?;
    Ok(b64url(&b))
}

fn query_param(q: &str, key: &str) -> Option<String> {
    q.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == key).then(|| {
            let b = v.as_bytes();
            let (mut o, mut i) = (Vec::new(), 0);
            while i < b.len() {
                match b[i] {
                    b'+' => o.push(b' '),
                    b'%' if i + 2 < b.len() => {
                        let h = std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok());
                        o.push(h.unwrap_or(b'%'));
                        i += 2;
                    }
                    c => o.push(c),
                }
                i += 1;
            }
            String::from_utf8_lossy(&o).into_owned()
        })
    })
}

/// Authorization-code + PKCE with a loopback redirect. Opens the system
/// browser; blocks until the redirect arrives (5 min max).
pub fn browser_login() -> Result<(), String> {
    use sha2::{Digest, Sha256};
    use std::io::{Read, Write};
    let (id, tenant) = config()?;
    let verifier = random_b64(48)?;
    let challenge = b64url(&Sha256::digest(verifier.as_bytes()));
    let state = random_b64(16)?;

    let listener = std::net::TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    let redirect = format!("http://localhost:{}", listener.local_addr().map_err(|e| e.to_string())?.port());

    let url = format!(
        "{}?client_id={}&response_type=code&redirect_uri={}&response_mode=query&scope={}\
         &state={}&code_challenge={}&code_challenge_method=S256&prompt=select_account",
        endpoint(&tenant, "authorize"),
        crate::graph::enc(&id),
        crate::graph::enc(&redirect),
        crate::graph::enc(&scopes()),
        state,
        challenge
    );
    std::process::Command::new("xdg-open")
        .arg(&url)
        .spawn()
        .map_err(|e| format!("cannot open browser: {e}"))?;

    let deadline = Instant::now() + Duration::from_secs(300);
    let (code, got_state) = loop {
        if Instant::now() > deadline {
            return Err("Sign-in timed out".into());
        }
        match listener.accept() {
            Ok((mut s, _)) => {
                let _ = s.set_nonblocking(false);
                let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
                let mut buf = [0u8; 4096];
                let n = s.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let target = req.lines().next().unwrap_or("").split(' ').nth(1).unwrap_or("");
                let q = target.split_once('?').map_or("", |x| x.1);
                let page = |m: &str| {
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nConnection: close\r\n\r\n\
                         <html><body style=\"font-family:sans-serif;text-align:center;margin-top:20vh\">{m}</body></html>"
                    )
                };
                if let Some(e) = query_param(q, "error") {
                    let d = query_param(q, "error_description").unwrap_or(e);
                    let _ = s.write_all(page("Sign-in failed. You can close this tab.").as_bytes());
                    return Err(d);
                }
                if let Some(c) = query_param(q, "code") {
                    let _ = s.write_all(page("Signed in. You can close this tab.").as_bytes());
                    break (c, query_param(q, "state").unwrap_or_default());
                }
                let _ = s.write_all(page("Waiting for sign-in…").as_bytes()); // e.g. favicon request
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(150))
            }
            Err(e) => return Err(e.to_string()),
        }
    };
    if got_state != state {
        return Err("Sign-in state mismatch".into());
    }

    let mut r = agent()
        .post(endpoint(&tenant, "token"))
        .send_form([
            ("grant_type", "authorization_code"),
            ("client_id", id.as_str()),
            ("code", code.as_str()),
            ("redirect_uri", redirect.as_str()),
            ("code_verifier", verifier.as_str()),
            ("scope", scopes().as_str()),
        ])
        .map_err(|e| e.to_string())?;
    let v: Value = r.body_mut().read_json().map_err(|e| e.to_string())?;
    if v["error"].is_string() {
        return Err(err_of(&v));
    }
    store(&v)
}

fn store(v: &Value) -> Result<(), String> {
    let at = v["access_token"].as_str().ok_or("no access_token")?;
    let ttl = v["expires_in"].as_u64().unwrap_or(3600).saturating_sub(120);
    *ACCESS.lock().unwrap() = Some((at.into(), Instant::now() + Duration::from_secs(ttl)));
    if let Some(rt) = v["refresh_token"].as_str() {
        let d = dir();
        fs::create_dir_all(&d).map_err(|e| e.to_string())?;
        let p = d.join("refresh_token");
        fs::write(&p, rt).map_err(|e| e.to_string())?;
        fs::set_permissions(&p, fs::Permissions::from_mode(0o600)).map_err(|e| e.to_string())?;
    }
    Ok(())
}

pub fn has_session() -> bool {
    dir().join("refresh_token").exists()
}

pub fn sign_out() {
    *ACCESS.lock().unwrap() = None;
    let _ = fs::remove_file(dir().join("refresh_token"));
}

/// Valid access token; refreshes transparently.
pub fn token() -> Result<String, String> {
    if let Some((t, exp)) = ACCESS.lock().unwrap().as_ref() {
        if Instant::now() < *exp {
            return Ok(t.clone());
        }
    }
    let rt = fs::read_to_string(dir().join("refresh_token")).map_err(|_| "Not signed in")?;
    let (id, tenant) = config()?;
    let mut r = agent()
        .post(endpoint(&tenant, "token"))
        .send_form([
            ("grant_type", "refresh_token"),
            ("client_id", id.as_str()),
            ("refresh_token", rt.trim()),
            ("scope", scopes().as_str()),
        ])
        .map_err(|e| e.to_string())?;
    let v: Value = r.body_mut().read_json().map_err(|e| e.to_string())?;
    if v["error"].is_string() {
        return Err(err_of(&v));
    }
    store(&v)?;
    Ok(v["access_token"].as_str().unwrap_or_default().to_string())
}
