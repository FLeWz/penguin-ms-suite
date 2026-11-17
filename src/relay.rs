//! Push-relay client. Design and wire protocol: docs/relay-architecture.md, and the
//! penguin-ms-suite-relay project.
//!
//! Contract with the Teams page:
//! - `healthy()` true  => relay connected, heartbeating and the Graph subscription is active.
//!   Polling drops to a slow safety-net interval and relay events trigger immediate refreshes.
//! - `healthy()` false => relay absent, down or silent. Adaptive polling runs at full speed
//!   (fallback). Flipping back needs no other change.
//!
//! What runs here (one background thread, started by `start()`):
//! 1. register once with the relay (`POST /register`, Bearer invite secret) and keep
//!    `{deviceId, deviceSecret, clientState}` in `relay_device.json` (mode 0600);
//! 2. connect `wss://RELAY/ws?deviceId=..` with `Authorization: Bearer <deviceSecret>`;
//! 3. create (or renew) the Graph subscription `/users/{me}/chats/getAllMessages` whose
//!    notificationUrl is the relay's `/hook/{deviceId}`; renew with PATCH every 25 minutes;
//! 4. turn relay events into `PushEvent`s. On any failure: unhealthy, backoff, retry.
use crate::{auth, graph};
use serde_json::{json, Value};
use std::{
    net::{TcpStream, ToSocketAddrs},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tungstenite::{client::IntoClientRequest, Message};

static HEALTHY: AtomicBool = AtomicBool::new(false);

const TICK: Duration = Duration::from_secs(5); // read timeout: how often timers are checked
const DEAD_AFTER: Duration = Duration::from_secs(45); // no frame at all (heartbeat is 20 s)
const SUB_LIFETIME: Duration = Duration::from_secs(55 * 60); // < 1 h: no lifecycle URL needed
const SUB_RENEW: Duration = Duration::from_secs(25 * 60);

/// Events the relay can push. They carry ids only, never message content.
pub enum PushEvent {
    /// A message was created or changed in this chat.
    Message { chat_id: String },
    /// Notifications may have been missed (reconnect, lifecycle "missed"): refresh everything.
    Resync,
}

/// Relay base URL: `PENGUIN_MS_RELAY_URL` at run time, else the one compiled in from `.env`.
pub fn url() -> Option<String> {
    std::env::var("PENGUIN_MS_RELAY_URL")
        .ok()
        .filter(|u| !u.is_empty())
        .or_else(|| Some(env!("BUILTIN_PENGUIN_MS_RELAY_URL").to_string()).filter(|u| !u.is_empty()))
        .map(|u| u.trim_end_matches('/').to_string())
}

/// Invite secret: `PENGUIN_MS_RELAY_INVITE`, else `relay_invite` in the config dir, else the
/// one compiled in.
fn invite() -> Option<String> {
    std::env::var("PENGUIN_MS_RELAY_INVITE")
        .ok()
        .or_else(|| std::fs::read_to_string(auth::dir().join("relay_invite")).ok())
        .or_else(|| Some(env!("BUILTIN_PENGUIN_MS_RELAY_INVITE").to_string()))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// True while the relay connection is alive and the subscription is active.
pub fn healthy() -> bool {
    HEALTHY.load(Ordering::Relaxed)
}

fn set_healthy(ok: bool) {
    HEALTHY.store(ok, Ordering::Relaxed);
}

/// Start the relay client and return its event stream; None when no relay is configured.
pub fn start() -> Option<async_channel::Receiver<PushEvent>> {
    let url = url()?;
    let (tx, rx) = async_channel::unbounded();
    std::thread::spawn(move || run(url, tx));
    Some(rx)
}

// ---------- device registration ----------

struct Device {
    id: String,
    secret: String,
    client_state: String,
}

fn device_path() -> std::path::PathBuf {
    auth::dir().join("relay_device.json")
}

fn load_device(url: &str) -> Option<Device> {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(device_path()).ok()?).ok()?;
    if v["url"].as_str()? != url {
        return None; // registered with another relay
    }
    Some(Device {
        id: v["deviceId"].as_str()?.into(),
        secret: v["deviceSecret"].as_str()?.into(),
        client_state: v["clientState"].as_str()?.into(),
    })
}

fn save_device(url: &str, d: &Device) {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let body = json!({"url": url, "deviceId": d.id, "deviceSecret": d.secret, "clientState": d.client_state});
    let _ = std::fs::create_dir_all(auth::dir());
    if let Ok(mut f) =
        std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(device_path())
    {
        let _ = f.write_all(body.to_string().as_bytes());
    }
}

fn register(url: &str) -> Result<Device, String> {
    let mut req = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(20)))
        .build()
        .new_agent()
        .post(format!("{url}/register"));
    if let Some(i) = invite() {
        req = req.header("Authorization", format!("Bearer {i}"));
    }
    let mut r = req.send_empty().map_err(|e| e.to_string())?;
    match r.status().as_u16() {
        200 => {}
        401 => return Err("relay rejected the invite secret (PENGUIN_MS_RELAY_INVITE)".into()),
        c => return Err(format!("register failed: HTTP {c}")),
    }
    let v: Value = r.body_mut().read_json().map_err(|e| e.to_string())?;
    let get = |k: &str| v[k].as_str().map(String::from).ok_or_else(|| format!("register reply lacks {k}"));
    let d = Device { id: get("deviceId")?, secret: get("deviceSecret")?, client_state: get("clientState")? };
    save_device(url, &d);
    Ok(d)
}

// ---------- Graph subscription ----------

/// ISO 8601 UTC, `secs` after the Unix epoch.
fn iso(secs: u64) -> String {
    let (days, rem) = (secs / 86400, secs % 86400);
    // civil-from-days (Howard Hinnant)
    let z = days as i64 + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

fn expiry() -> String {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    iso(now + SUB_LIFETIME.as_secs())
}

/// Make sure exactly one subscription points at this device's hook; returns its id. Other
/// subscriptions of ours on the same relay (stale devices) are deleted.
fn ensure_subscription(url: &str, dev: &Device) -> Result<String, String> {
    if !auth::granted("Chat.ReadWrite") && !auth::granted("Chat.Read") {
        return Err("Chat.Read/Chat.ReadWrite not granted: no push".into());
    }
    let hook = format!("{url}/hook/{}", dev.id);
    let mut mine: Option<String> = None;
    let list = graph::get("/subscriptions")?;
    for s in list["value"].as_array().into_iter().flatten() {
        let (Some(id), Some(n)) = (s["id"].as_str(), s["notificationUrl"].as_str()) else { continue };
        if n == hook && mine.is_none() {
            mine = Some(id.to_string());
        } else if n.starts_with(&format!("{url}/hook/")) {
            let _ = graph::delete(&format!("/subscriptions/{id}"));
        }
    }
    if let Some(id) = mine {
        if graph::patch(&format!("/subscriptions/{id}"), &json!({"expirationDateTime": expiry()})).is_ok() {
            return Ok(id);
        }
        let _ = graph::delete(&format!("/subscriptions/{id}"));
    }
    let me = graph::get("/me?$select=id")?;
    let me = me["id"].as_str().ok_or("no user id")?;
    let v = graph::post(
        "/subscriptions",
        &json!({
            "changeType": "created,updated",
            "notificationUrl": hook,
            "lifecycleNotificationUrl": format!("{url}/lifecycle/{}", dev.id),
            "resource": format!("/users/{me}/chats/getAllMessages"),
            "includeResourceData": false,
            "expirationDateTime": expiry(),
            "clientState": dev.client_state,
        }),
    )?;
    v["id"].as_str().map(String::from).ok_or_else(|| "subscription reply lacks id".into())
}

// ---------- WebSocket session ----------

type Socket = tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<TcpStream>>;

fn connect(url: &str, dev: &Device) -> Result<Socket, tungstenite::Error> {
    let ws_url = format!(
        "{}/ws?deviceId={}",
        url.replacen("https://", "wss://", 1).replacen("http://", "ws://", 1),
        dev.id
    );
    let mut req = ws_url.as_str().into_client_request()?;
    req.headers_mut().insert(
        "Authorization",
        format!("Bearer {}", dev.secret).parse().map_err(|_| tungstenite::Error::Utf8("bad secret".into()))?,
    );
    let uri = req.uri().clone();
    let host = uri.host().unwrap_or_default().to_string();
    let port = uri.port_u16().unwrap_or(if uri.scheme_str() == Some("wss") { 443 } else { 80 });
    let addr = (host.as_str(), port)
        .to_socket_addrs()
        .map_err(tungstenite::Error::Io)?
        .next()
        .ok_or_else(|| tungstenite::Error::Io(std::io::Error::other("no address")))?;
    let tcp = TcpStream::connect_timeout(&addr, Duration::from_secs(15)).map_err(tungstenite::Error::Io)?;
    tcp.set_read_timeout(Some(TICK)).map_err(tungstenite::Error::Io)?;
    tcp.set_write_timeout(Some(Duration::from_secs(15))).map_err(tungstenite::Error::Io)?;
    let (ws, _) = tungstenite::client_tls(req, tcp).map_err(|e| match e {
        tungstenite::HandshakeError::Failure(e) => e,
        tungstenite::HandshakeError::Interrupted(_) => tungstenite::Error::Io(std::io::ErrorKind::TimedOut.into()),
    })?;
    Ok(ws)
}

enum End {
    /// Registration is no longer valid: forget it and register again.
    Reregister,
    Retry(String),
}

/// One connected session; returns when the connection or the subscription is lost.
fn session(url: &str, dev: &Device, tx: &async_channel::Sender<PushEvent>) -> End {
    let mut ws = match connect(url, dev) {
        Ok(w) => w,
        Err(tungstenite::Error::Http(r)) if r.status().as_u16() == 401 => return End::Reregister,
        Err(e) => return End::Retry(format!("connect: {e}")),
    };
    let mut last_seen = Instant::now();
    let mut sub: Option<(String, Instant)> = None; // (id, renew at)
    let mut next_try = Instant::now();
    loop {
        // Subscription: create or renew when due; retry a failure after a minute.
        if sub.as_ref().is_none_or(|(_, at)| Instant::now() >= *at) && Instant::now() >= next_try {
            match ensure_subscription(url, dev) {
                Ok(id) => {
                    if sub.is_none() {
                        eprintln!("relay: connected, subscription {id}");
                        let _ = tx.send_blocking(PushEvent::Resync); // catch up on anything missed
                    }
                    sub = Some((id, Instant::now() + SUB_RENEW));
                    set_healthy(true);
                }
                Err(e) => {
                    set_healthy(false);
                    sub = None;
                    next_try = Instant::now() + Duration::from_secs(60);
                    eprintln!("relay: subscription failed: {e}");
                }
            }
        }
        match ws.read() {
            Ok(Message::Text(t)) => {
                last_seen = Instant::now();
                let v: Value = serde_json::from_str(t.as_str()).unwrap_or(Value::Null);
                match v["type"].as_str() {
                    Some("message") => {
                        if let Some(c) = v["chatId"].as_str() {
                            let _ = tx.send_blocking(PushEvent::Message { chat_id: c.to_string() });
                        }
                    }
                    Some("resync") => {
                        let _ = tx.send_blocking(PushEvent::Resync);
                    }
                    Some("reauth") => {
                        sub = None; // recreate right away
                        next_try = Instant::now();
                    }
                    _ => {} // hello, heartbeat
                }
            }
            Ok(Message::Close(_)) => return End::Retry("closed by relay".into()),
            Ok(_) => last_seen = Instant::now(), // ping, pong, binary
            Err(tungstenite::Error::Io(e))
                if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) =>
            {
                if last_seen.elapsed() > DEAD_AFTER {
                    return End::Retry("no heartbeat".into());
                }
            }
            Err(e) => return End::Retry(format!("read: {e}")),
        }
        if !auth::has_session() {
            return End::Retry("signed out".into());
        }
    }
}

fn run(url: String, tx: async_channel::Sender<PushEvent>) {
    let mut backoff = 2u64;
    let mut last_err = String::new();
    loop {
        if !auth::has_session() {
            std::thread::sleep(Duration::from_secs(5)); // waiting for sign-in
            continue;
        }
        let dev = match load_device(&url).map_or_else(|| register(&url), Ok) {
            Ok(d) => d,
            Err(e) => {
                if e != last_err {
                    eprintln!("relay: {e}; using polling");
                    last_err = e;
                }
                set_healthy(false);
                std::thread::sleep(Duration::from_secs(backoff));
                backoff = (backoff * 2).min(60);
                continue;
            }
        };
        let started = Instant::now();
        let end = session(&url, &dev, &tx);
        set_healthy(false);
        match end {
            End::Reregister => {
                let _ = std::fs::remove_file(device_path());
                eprintln!("relay: device no longer known to the relay; registering again");
            }
            End::Retry(e) => {
                if e != last_err {
                    eprintln!("relay: {e}; using polling, will retry");
                    last_err = e;
                }
            }
        }
        // A session that lasted a while was healthy: reconnect quickly.
        backoff = if started.elapsed() > Duration::from_secs(120) { 2 } else { (backoff * 2).min(60) };
        std::thread::sleep(Duration::from_secs(backoff));
    }
}

#[cfg(test)]
mod tests {
    use super::iso;

    #[test]
    fn iso_is_utc_calendar_time() {
        assert_eq!(iso(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso(951_782_400), "2000-02-29T00:00:00Z"); // leap day
        assert_eq!(iso(1_794_911_647), "2026-11-17T10:34:07Z");
    }
}
