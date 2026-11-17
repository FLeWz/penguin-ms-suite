//! On-disk cache of chat messages under ~/.cache/penguin-ms-suite/messages.
//!
//! Every message the Graph API returns is stored per chat. Opening or polling a chat then only
//! asks Graph for messages modified since the last sync (`lastModifiedDateTime gt ...`), and older
//! history is served from disk, hitting the API only for pages not yet cached. The whole cache
//! folder (media downloads included) is kept under `MAX_BYTES` by deleting the oldest files.
//!
//! Callers keep using the Graph paging shape: `get` takes the same path strings and returns
//! `{"value": [...], "@odata.nextLink": ...}`. Any cache or API error falls back to a plain
//! uncached request.
use crate::graph;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::{Duration, Instant, SystemTime},
};

/// Size limit for the whole ~/.cache/penguin-ms-suite folder.
pub const MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const PAGE: usize = 50;
const SYNC_PAGES: usize = 100;
const MEM_CHATS: usize = 8;
const TRIM_EVERY: Duration = Duration::from_secs(60);
const CURSOR: &str = "cache|";

pub fn root() -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("penguin-ms-suite")
}

fn dir() -> PathBuf {
    root().join("messages")
}

#[derive(Default)]
struct Store {
    /// Newest `lastModifiedDateTime` seen; empty = nothing cached yet.
    sync: String,
    /// `createdDateTime` of the oldest cached message (cache is contiguous up to now).
    oldest: String,
    /// Oldest cached message is the first one of the chat.
    complete: bool,
    msgs: HashMap<String, Value>,
    dirty: bool,
}

struct State {
    chats: HashMap<String, Store>,
    last_trim: Option<Instant>,
}

fn state() -> &'static Mutex<State> {
    static S: OnceLock<Mutex<State>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(State { chats: HashMap::new(), last_trim: None }))
}

fn file_of(chat: &str) -> PathBuf {
    let h: String = Sha256::digest(chat.as_bytes()).iter().map(|b| format!("{b:02x}")).collect();
    dir().join(format!("{}.json", &h[..32]))
}

fn load(chat: &str) -> Store {
    let Ok(bytes) = std::fs::read(file_of(chat)) else { return Store::default() };
    let Ok(v) = serde_json::from_slice::<Value>(&bytes) else { return Store::default() };
    let mut s = Store {
        sync: v["sync"].as_str().unwrap_or("").to_string(),
        oldest: v["oldest"].as_str().unwrap_or("").to_string(),
        complete: v["complete"].as_bool().unwrap_or(false),
        ..Default::default()
    };
    for m in v["msgs"].as_array().into_iter().flatten() {
        if let Some(id) = m["id"].as_str() {
            s.msgs.insert(id.to_string(), m.clone());
        }
    }
    if s.msgs.is_empty() {
        return Store::default();
    }
    s
}

fn save(chat: &str, s: &Store) {
    let path = file_of(chat);
    let Some(parent) = path.parent() else { return };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let body = json!({
        "sync": s.sync, "oldest": s.oldest, "complete": s.complete,
        "msgs": s.msgs.values().collect::<Vec<_>>(),
    });
    let Ok(bytes) = serde_json::to_vec(&body) else { return };
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, bytes).is_ok() && std::fs::rename(&tmp, &path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Run `f` on the chat's store (loaded from disk on first use); persist when it changed.
fn with_store<R>(chat: &str, f: impl FnOnce(&mut Store) -> R) -> R {
    let mut st = state().lock().unwrap_or_else(|e| e.into_inner());
    if !st.chats.contains_key(chat) {
        if st.chats.len() >= MEM_CHATS {
            if let Some(k) = st.chats.keys().next().cloned() {
                st.chats.remove(&k);
            }
        }
        st.chats.insert(chat.to_string(), load(chat));
    }
    let s = st.chats.get_mut(chat).expect("just inserted");
    let r = f(s);
    if s.dirty {
        s.dirty = false;
        save(chat, s);
        if st.last_trim.is_none_or(|t| t.elapsed() > TRIM_EVERY) {
            st.last_trim = Some(Instant::now());
            std::thread::spawn(trim);
        }
    }
    r
}

/// Milliseconds since the epoch for a Graph ISO timestamp (0 when unparsable). Needed because
/// fractional-second widths differ, so plain string comparison is wrong.
fn ts(s: &str) -> i64 {
    let n = |a: usize, l: usize| s.get(a..a + l).and_then(|x| x.parse::<i64>().ok()).unwrap_or(0);
    if s.len() < 19 {
        return 0;
    }
    let (y, m, d, h, mi, se) = (n(0, 4), n(5, 2), n(8, 2), n(11, 2), n(14, 2), n(17, 2));
    let mut ms = 0;
    if s.as_bytes()[19] == b'.' {
        let f: String = s[20..].chars().take_while(|c| c.is_ascii_digit()).take(3).collect();
        ms = format!("{f:0<3}").parse().unwrap_or(0);
    }
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    let days = era * 146097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719468;
    (((days * 24 + h) * 60 + mi) * 60 + se) * 1000 + ms
}

fn created(m: &Value) -> &str {
    m["createdDateTime"].as_str().unwrap_or("")
}

impl Store {
    fn put(&mut self, m: &Value) {
        let Some(id) = m["id"].as_str() else { return };
        let c = created(m);
        if self.oldest.is_empty() || ts(c) < ts(&self.oldest) {
            self.oldest = c.to_string();
        }
        let lm = m["lastModifiedDateTime"].as_str().unwrap_or(c);
        if self.sync.is_empty() || ts(lm) > ts(&self.sync) {
            self.sync = lm.to_string();
        }
        self.msgs.insert(id.to_string(), m.clone());
        self.dirty = true;
    }

    /// Cached messages strictly older than `before` (all when None), newest first.
    fn sorted(&self, before: Option<&str>) -> Vec<&Value> {
        let lim = before.map(ts);
        let mut v: Vec<&Value> =
            self.msgs.values().filter(|m| lim.is_none_or(|l| ts(created(m)) < l)).collect();
        v.sort_by(|a, b| {
            ts(created(b)).cmp(&ts(created(a))).then_with(|| b["id"].as_str().cmp(&a["id"].as_str()))
        });
        v
    }
}

fn items(v: &Value) -> Vec<Value> {
    v["value"].as_array().cloned().unwrap_or_default()
}

fn next_of(v: &Value) -> Option<String> {
    v["@odata.nextLink"].as_str().map(|u| u.strip_prefix(graph::BASE).unwrap_or(u).to_string())
}

enum Req {
    First { chat: String, top: usize },
    Older { chat: String, top: usize, before: String },
}

fn parse(path: &str) -> Option<Req> {
    if let Some(c) = path.strip_prefix(CURSOR) {
        let mut p = c.splitn(3, '|');
        let top = p.next()?.parse().ok()?;
        let before = p.next()?.to_string();
        return Some(Req::Older { chat: p.next()?.to_string(), top, before });
    }
    let rest = path.strip_prefix("/me/chats/")?;
    let (chat, q) = rest.split_once("/messages?$top=")?;
    if chat.is_empty() || chat.contains('/') || q.contains('&') {
        return None;
    }
    Some(Req::First { chat: chat.to_string(), top: q.parse::<usize>().ok()?.clamp(1, PAGE) })
}

fn older_path(chat: &str, before: &str) -> String {
    format!(
        "/me/chats/{chat}/messages?$top={PAGE}&$orderby=createdDateTime%20desc\
         &$filter=createdDateTime%20lt%20{}",
        graph::enc(before)
    )
}

/// Graph-shaped page of up to `top` cached messages older than `before`.
fn serve(chat: &str, top: usize, before: Option<&str>) -> Value {
    with_store(chat, |s| {
        let all = s.sorted(before);
        let page: Vec<Value> = all.iter().take(top).map(|m| (*m).clone()).collect();
        let more = all.len() > top || !s.complete;
        let mut out = json!({ "value": page });
        if more {
            if let Some(last) = page.last() {
                out["@odata.nextLink"] = json!(format!("{CURSOR}{top}|{}|{chat}", created(last)));
            }
        }
        out
    })
}

/// Newest messages: first fill, or fetch only what changed since the last sync.
fn first(chat: &str, top: usize) -> Result<Value, String> {
    let sync = with_store(chat, |s| s.sync.clone());
    let mut changed = false;
    if sync.is_empty() {
        let v = graph::get(&format!("/me/chats/{chat}/messages?$top={PAGE}&$orderby=createdDateTime%20desc"))?;
        let more = v["@odata.nextLink"].is_string();
        with_store(chat, |s| {
            if s.sync.is_empty() {
                s.complete = !more;
                items(&v).iter().for_each(|m| s.put(m));
            }
        });
    } else {
        let mut path = format!(
            "/me/chats/{chat}/messages?$top={PAGE}&$orderby=lastModifiedDateTime%20desc\
             &$filter=lastModifiedDateTime%20gt%20{}",
            graph::enc(&sync)
        );
        let mut got = vec![];
        let mut finished = false;
        for _ in 0..SYNC_PAGES {
            let v = graph::get(&path)?;
            got.extend(items(&v));
            match next_of(&v) {
                Some(p) => path = p,
                None => {
                    finished = true;
                    break;
                }
            }
        }
        changed = with_store(chat, |s| {
            if !finished {
                *s = Store::default(); // too far behind; refill from scratch next time
                return false;
            }
            let mut changed = false;
            for m in &got {
                // Older, never-cached messages would leave a hole in the cache; they are
                // fetched later when the user scrolls back.
                let old = m["id"].as_str().and_then(|i| s.msgs.get(i));
                if old.is_some() || s.complete || ts(created(m)) >= ts(&s.oldest) {
                    changed |= old.is_none_or(|o| o["lastModifiedDateTime"] != m["lastModifiedDateTime"]);
                    s.put(m);
                }
            }
            changed
        });
        if !finished {
            return Err("cache fell too far behind".into());
        }
    }
    let mut out = serve(chat, top, None);
    // New, edited or reacted-to messages came in with this sync (callers redraw on it).
    out["changed"] = json!(changed);
    Ok(out)
}

/// Older history: from disk when cached, otherwise one API page extends the cache.
fn older(chat: &str, top: usize, before: &str) -> Result<Value, String> {
    let (known, have, complete, oldest) = with_store(chat, |s| {
        (!s.sync.is_empty(), s.sorted(Some(before)).len(), s.complete, s.oldest.clone())
    });
    if !known {
        return Err("no cache".into());
    }
    if have < top && !complete {
        let v = graph::get(&older_path(chat, &oldest))?;
        let more = v["@odata.nextLink"].is_string();
        with_store(chat, |s| {
            if s.oldest == oldest {
                items(&v).iter().for_each(|m| s.put(m));
                s.complete = !more;
                s.dirty = true;
            }
        });
    }
    Ok(serve(chat, top, Some(before)))
}

/// Drop-in for `graph::get` on chat-message pages (first page and paging cursors).
pub fn get(path: &str) -> Result<Value, String> {
    let (res, fallback) = match parse(path) {
        Some(Req::First { chat, top }) => (first(&chat, top), path.to_string()),
        Some(Req::Older { chat, top, before }) => {
            let fb = older_path(&chat, &before);
            (older(&chat, top, &before), fb)
        }
        None => return graph::get(path),
    };
    res.or_else(|_| graph::get(&fallback))
}

/// Keep ~/.cache/penguin-ms-suite under `MAX_BYTES`, deleting the oldest files first.
pub fn trim() {
    fn walk(d: &Path, out: &mut Vec<(PathBuf, u64, SystemTime)>) {
        let Ok(rd) = std::fs::read_dir(d) else { return };
        for e in rd.flatten() {
            let Ok(m) = e.metadata() else { continue };
            if m.is_dir() {
                walk(&e.path(), out);
            } else {
                out.push((e.path(), m.len(), m.modified().unwrap_or(SystemTime::UNIX_EPOCH)));
            }
        }
    }
    let mut files = vec![];
    walk(&root(), &mut files);
    let mut total: u64 = files.iter().map(|f| f.1).sum();
    if total <= MAX_BYTES {
        return;
    }
    files.sort_by_key(|f| f.2);
    for (p, len, _) in files {
        if total <= MAX_BYTES {
            break;
        }
        if std::fs::remove_file(&p).is_ok() {
            total -= len;
        }
    }
}
