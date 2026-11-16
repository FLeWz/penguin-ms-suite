//! Link previews for Teams messages: title, description and image from the page's
//! Open Graph / HTML metadata, fetched by this app (no third-party service).
//!
//! Privacy and safety: every previewed link is requested from this machine, so the
//! linked site sees your IP. Only http(s) is fetched; localhost, private and link-local
//! addresses are refused (also for redirects); Microsoft sign-in-gated hosts are skipped;
//! responses are capped at 400 KB. Set `PENGUIN_MS_LINK_PREVIEWS=0` to turn previews off.
use crate::util;
use adw::prelude::*;
use gtk::gdk;
use serde_json::Value;
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    net::IpAddr,
    rc::Rc,
    time::Duration,
};

const MAX_PAGE_BYTES: u64 = 400 * 1024;
const MAX_IMAGE_BYTES: u64 = 5 * 1024 * 1024;
const MAX_LINKS_PER_MESSAGE: usize = 2;
const MAX_TEXTURES: usize = 40;
const UA: &str = "Mozilla/5.0 (X11; Linux x86_64) PenguinMSSuite/0.1 link-preview";

/// Hosts that need a Microsoft sign-in: a fetch would only return a login page.
const SKIP_HOSTS: [&str; 7] = [
    "sharepoint.com",
    "office.com",
    "teams.microsoft.com",
    "login.microsoftonline.com",
    "live.com",
    "onedrive.com",
    "graph.microsoft.com",
];
const MEDIA_EXT: [&str; 13] = [
    "jpg", "jpeg", "png", "gif", "webp", "bmp", "tiff", "mp4", "mov", "mkv", "webm", "m4v", "avi",
];

#[derive(Clone)]
struct Preview {
    title: String,
    description: String,
    site: String,
    image: Option<String>,
    /// A video link (YouTube): larger thumbnail with a play badge.
    video: bool,
}

// ---------- URL handling ----------

fn host_of(url: &str) -> Option<&str> {
    let rest = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://"))?;
    let auth = rest.split(['/', '?', '#']).next()?;
    let auth = auth.rsplit('@').next()?; // drop user info
    if let Some(v6) = auth.strip_prefix('[') {
        return v6.split(']').next();
    }
    auth.split(':').next().filter(|h| !h.is_empty())
}

/// Refuse local and private targets (SSRF guard).
fn host_allowed(host: &str) -> bool {
    let h = host.to_ascii_lowercase();
    if h == "localhost" || h.ends_with(".localhost") || h.ends_with(".local") || h.ends_with(".internal") || h.ends_with(".lan") {
        return false;
    }
    match h.parse::<IpAddr>() {
        Ok(IpAddr::V4(a)) => !(a.is_private() || a.is_loopback() || a.is_link_local() || a.is_unspecified() || a.is_broadcast()),
        Ok(IpAddr::V6(a)) => {
            let seg = a.segments()[0];
            !(a.is_loopback() || a.is_unspecified() || (seg & 0xfe00) == 0xfc00 || (seg & 0xffc0) == 0xfe80)
        }
        Err(_) => true,
    }
}

fn skipped_host(host: &str) -> bool {
    let h = host.to_ascii_lowercase();
    SKIP_HOSTS.iter().any(|s| h == *s || h.ends_with(&format!(".{s}")))
}

fn is_media_url(url: &str) -> bool {
    let path = url.split(['?', '#']).next().unwrap_or("");
    let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    MEDIA_EXT.contains(&ext.as_str())
}

/// Resolve a possibly relative URL against `base`.
fn resolve(base: &str, href: &str) -> Option<String> {
    if href.starts_with("http://") || href.starts_with("https://") {
        return Some(href.to_string());
    }
    let scheme_end = base.find("://")?;
    let scheme = &base[..scheme_end];
    if let Some(rest) = href.strip_prefix("//") {
        return Some(format!("{scheme}://{rest}"));
    }
    let origin_end = base[scheme_end + 3..].find(['/', '?', '#']).map_or(base.len(), |i| i + scheme_end + 3);
    let origin = &base[..origin_end];
    if href.starts_with('/') {
        return Some(format!("{origin}{href}"));
    }
    let dir = base[..base.split(['?', '#']).next()?.rfind('/').filter(|i| *i >= origin_end).unwrap_or(origin_end)].to_string();
    Some(format!("{dir}/{href}"))
}

/// Links worth previewing in one message (hrefs first, then bare URLs in the text).
fn urls_of(m: &Value, text: &str) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    let mut push = |u: &str| {
        let u = u.trim_end_matches(['.', ',', ')', ']', ';', '"', '\'', '>', '!']).replace("&amp;", "&");
        if let Some(h) = host_of(&u) {
            if !u.contains('…') && !out.contains(&u) && !skipped_host(h) && !is_media_url(&u) {
                out.push(u);
            }
        }
    };
    let html = m["body"]["content"].as_str().unwrap_or("");
    for part in html.split("href=\"").skip(1) {
        if let Some(u) = part.split('"').next() {
            push(u);
        }
    }
    for tok in text.split_whitespace() {
        // Teams shortens long link text to "https://host/path…": not a real URL.
        if (tok.starts_with("http://") || tok.starts_with("https://")) && !tok.contains('…') {
            push(tok);
        }
    }
    out.truncate(MAX_LINKS_PER_MESSAGE);
    out
}

// ---------- metadata parsing ----------

fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let Some(end) = rest.find(';').filter(|e| *e <= 10) else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let ent = &rest[1..end];
        let rep = match ent {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some(' '),
            _ => ent
                .strip_prefix("#x")
                .and_then(|h| u32::from_str_radix(h, 16).ok())
                .or_else(|| ent.strip_prefix('#').and_then(|d| d.parse().ok()))
                .and_then(char::from_u32),
        };
        match rep {
            Some(c) => out.push(c),
            None => out.push_str(&rest[..=end]),
        }
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    out
}

/// Value of attribute `name` in a tag's text (quoted or bare).
fn attr(tag: &str, lower: &str, name: &str) -> Option<String> {
    let mut from = 0;
    while let Some(i) = lower[from..].find(name) {
        let at = from + i;
        from = at + name.len();
        let before_ok = at == 0 || lower.as_bytes()[at - 1].is_ascii_whitespace();
        let rest = tag[at + name.len()..].trim_start();
        if !before_ok || !rest.starts_with('=') {
            continue;
        }
        let v = rest[1..].trim_start();
        return Some(match v.chars().next()? {
            q @ ('"' | '\'') => v[1..].split(q).next()?.to_string(),
            _ => v.split(|c: char| c.is_whitespace() || c == '>').next()?.to_string(),
        });
    }
    None
}

fn parse_page(url: &str, html: &str) -> Option<Preview> {
    let lower = html.to_ascii_lowercase(); // same byte offsets as `html`
    let head_end = lower.find("</head").unwrap_or(lower.len());
    let mut meta: HashMap<String, String> = HashMap::new();
    let mut pos = 0;
    while let Some(i) = lower[pos..head_end].find("<meta") {
        let start = pos + i;
        let end = lower[start..head_end].find('>').map_or(head_end, |e| start + e);
        let (tag, ltag) = (&html[start..end], &lower[start..end]);
        let key = attr(tag, ltag, "property").or_else(|| attr(tag, ltag, "name"));
        if let (Some(k), Some(c)) = (key, attr(tag, ltag, "content")) {
            meta.entry(k.to_ascii_lowercase()).or_insert(c);
        }
        pos = end;
    }
    let get = |ks: &[&str]| ks.iter().find_map(|k| meta.get(*k)).map(|v| decode_entities(v).trim().to_string()).filter(|v| !v.is_empty());
    let page_title = lower.find("<title").and_then(|i| {
        let open_end = lower[i..].find('>')? + i + 1;
        let close = lower[open_end..].find("</title")? + open_end;
        Some(decode_entities(html[open_end..close].trim()))
    });
    let title = get(&["og:title", "twitter:title"]).or(page_title).unwrap_or_default();
    let description = get(&["og:description", "twitter:description", "description"]).unwrap_or_default();
    if title.is_empty() && description.is_empty() {
        return None;
    }
    let site = get(&["og:site_name"]).unwrap_or_else(|| host_of(url).unwrap_or("").to_string());
    let image = get(&["og:image", "og:image:url", "twitter:image"]).and_then(|i| resolve(url, &i));
    let cut = |s: String, n: usize| if s.chars().count() > n { format!("{}…", s.chars().take(n).collect::<String>()) } else { s };
    Some(Preview { title: cut(title, 120), description: cut(description, 220), site, image, video: false })
}

/// 11-character video id of a YouTube watch, share, shorts, live or embed link.
fn youtube_id(url: &str) -> Option<String> {
    let host = host_of(url)?.to_ascii_lowercase();
    let rest = url.split_once("://")?.1;
    let after_host = rest.split_once(['/', '?', '#']).map_or("", |(_, r)| r);
    let path = after_host.split(['?', '#']).next().unwrap_or("");
    let first_seg = |p: &str| p.split('/').next().unwrap_or("").to_string();
    let id = match host.as_str() {
        "youtu.be" => first_seg(path),
        "youtube.com" | "www.youtube.com" | "m.youtube.com" | "music.youtube.com" | "youtube-nocookie.com"
        | "www.youtube-nocookie.com" => {
            if let Some(p) = ["shorts/", "embed/", "live/", "v/"].iter().find_map(|k| path.strip_prefix(k)) {
                first_seg(p)
            } else if path == "watch" {
                let q = url.split_once('?')?.1.split('#').next()?;
                q.split('&').find_map(|kv| kv.strip_prefix("v="))?.to_string()
            } else {
                return None;
            }
        }
        _ => return None,
    };
    (id.len() == 11 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')).then_some(id)
}

/// YouTube via oEmbed: the watch page itself often returns a consent screen with no
/// metadata, oEmbed returns title and channel reliably. Thumbnail comes from i.ytimg.com.
fn youtube_preview(id: &str) -> Option<(Preview, Option<Vec<u8>>)> {
    let watch = format!("https://www.youtube.com/watch?v={id}");
    let api = format!("https://www.youtube.com/oembed?format=json&url={}", crate::graph::enc(&watch));
    let (_, _, body) = fetch(&api, MAX_PAGE_BYTES).ok()?;
    let v: Value = serde_json::from_slice(&body).ok()?;
    let title = v["title"].as_str()?.to_string();
    let thumb = format!("https://i.ytimg.com/vi/{id}/mqdefault.jpg"); // 16:9, no letterbox bars
    let img = fetch(&thumb, MAX_IMAGE_BYTES)
        .ok()
        .filter(|(_, ct, _)| ct.starts_with("image/"))
        .map(|(_, _, b)| b);
    Some((
        Preview {
            title,
            description: v["author_name"].as_str().unwrap_or("").to_string(),
            site: "YouTube".into(),
            image: Some(thumb),
            video: true,
        },
        img,
    ))
}

// ---------- fetching ----------

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0) // followed by hand so every hop is checked
        .timeout_global(Some(Duration::from_secs(8)))
        .user_agent(UA)
        .build()
        .into()
}

/// GET with up to 4 manually checked redirects. Returns (final url, content type, body).
fn fetch(url: &str, max: u64) -> Result<(String, String, Vec<u8>), String> {
    let agent = agent();
    let mut cur = url.to_string();
    for _ in 0..5 {
        let host = host_of(&cur).ok_or("bad url")?;
        if !host_allowed(host) {
            return Err("blocked host".into());
        }
        let r = agent.get(&cur).header("Accept", "text/html,image/*;q=0.8").call().map_err(|e| e.to_string())?;
        let code = r.status().as_u16();
        if (300..400).contains(&code) {
            let loc = r.headers().get("location").and_then(|v| v.to_str().ok()).ok_or("redirect without location")?;
            cur = resolve(&cur, loc).ok_or("bad redirect")?;
            continue;
        }
        if code >= 400 {
            return Err(format!("HTTP {code}"));
        }
        let ct = r.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("").to_ascii_lowercase();
        let body = r.into_body().into_with_config().limit(max).read_to_vec().map_err(|e| e.to_string())?;
        return Ok((cur, ct, body));
    }
    Err("too many redirects".into())
}

fn fetch_preview(url: &str) -> Option<(Preview, Option<Vec<u8>>)> {
    if let Some(id) = youtube_id(url) {
        return youtube_preview(&id);
    }
    let (final_url, ct, body) = fetch(url, MAX_PAGE_BYTES).ok()?;
    if !ct.contains("html") {
        return None;
    }
    let p = parse_page(&final_url, &String::from_utf8_lossy(&body))?;
    let img = p.image.as_deref().and_then(|u| fetch(u, MAX_IMAGE_BYTES).ok()).filter(|(_, ct, _)| ct.starts_with("image/")).map(|(_, _, b)| b);
    Some((p, img))
}

// ---------- UI ----------

/// Height reserved for a link card while it loads; loaded cards use it as their minimum
/// height, so swapping the placeholder for the card barely moves the chat.
const CARD_H: i32 = 104;

pub struct Links {
    enabled: bool,
    /// url -> preview (None = tried, nothing to show)
    previews: RefCell<HashMap<String, Option<Preview>>>,
    textures: RefCell<HashMap<String, gdk::Texture>>,
    inflight: RefCell<HashSet<String>>,
}

impl Links {
    pub fn new() -> Rc<Self> {
        let enabled = std::env::var("PENGUIN_MS_LINK_PREVIEWS").map_or(true, |v| v != "0");
        Rc::new(Links { enabled, previews: Default::default(), textures: Default::default(), inflight: Default::default() })
    }

    /// Preview cards for links in the message; cards appear when the fetch finishes.
    pub fn widgets(self: &Rc<Self>, m: &Value, text: &str) -> Vec<gtk::Widget> {
        if !self.enabled {
            return vec![];
        }
        urls_of(m, text)
            .into_iter()
            .map(|url| {
                let holder = gtk::Box::new(gtk::Orientation::Vertical, 0);
                holder.set_halign(gtk::Align::Start);
                let cached = self.previews.borrow().get(&url).cloned();
                match cached {
                    Some(Some(p)) => self.show(&holder, &url, &p),
                    Some(None) => {}
                    None => {
                        holder.set_size_request(-1, CARD_H);
                        holder.add_css_class("thumb-placeholder");
                        if self.inflight.borrow_mut().insert(url.clone()) {
                            let (me, weak, u) = (self.clone(), holder.downgrade(), url.clone());
                            util::run(
                                // Fetch and decode (scaled to card size) off the UI thread.
                                move || {
                                    fetch_preview(&u).map(|(p, img)| {
                                        let (w, h) = if p.video { (320, 180) } else { (192, 192) };
                                        let tex = img
                                            .and_then(|b| super::media::decode_scaled(&b, w, h))
                                            .map(super::media::SendTexture);
                                        (p, tex)
                                    })
                                },
                                move |r| {
                                    me.inflight.borrow_mut().remove(&url);
                                    let Some((p, img)) = r else {
                                        me.previews.borrow_mut().insert(url, None);
                                        if let Some(h) = weak.upgrade() {
                                            h.set_size_request(-1, -1); // no card: give the space back
                                            h.remove_css_class("thumb-placeholder");
                                        }
                                        return;
                                    };
                                    if let (Some(src), Some(super::media::SendTexture(t))) = (p.image.clone(), img) {
                                        let mut c = me.textures.borrow_mut();
                                        if c.len() >= MAX_TEXTURES {
                                            c.clear();
                                        }
                                        c.insert(src, t);
                                    }
                                    me.previews.borrow_mut().insert(url.clone(), Some(p.clone()));
                                    if let Some(h) = weak.upgrade() {
                                        me.show(&h, &url, &p);
                                    }
                                },
                            );
                        }
                    }
                }
                holder.upcast()
            })
            .collect()
    }

    fn show(&self, holder: &gtk::Box, url: &str, p: &Preview) {
        let card = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        card.add_css_class("link-card");
        card.set_size_request(-1, CARD_H);
        holder.set_size_request(-1, -1);
        holder.remove_css_class("thumb-placeholder");
        if let Some(t) = p.image.as_ref().and_then(|i| self.textures.borrow().get(i).cloned()) {
            let (w, h) = (t.width() as f64, t.height() as f64);
            let (maxw, maxh) = if p.video { (160.0, 90.0) } else { (96.0, 96.0) };
            let s = (maxw / w).min(maxh / h).min(1.0);
            let pic = gtk::Picture::for_paintable(&t);
            pic.set_content_fit(gtk::ContentFit::Contain);
            pic.set_can_shrink(true);
            pic.set_size_request((w * s).max(1.0) as i32, (h * s).max(1.0) as i32);
            pic.set_valign(gtk::Align::Start);
            if p.video {
                // Play badge centred on the thumbnail.
                let ov = gtk::Overlay::new();
                ov.set_child(Some(&pic));
                let play = gtk::Image::from_icon_name("media-playback-start-symbolic");
                play.set_pixel_size(20);
                play.add_css_class("play-badge");
                play.set_halign(gtk::Align::Center);
                play.set_valign(gtk::Align::Center);
                ov.add_overlay(&play);
                ov.set_valign(gtk::Align::Start);
                card.append(&ov);
            } else {
                card.append(&pic);
            }
        }
        let col = gtk::Box::new(gtk::Orientation::Vertical, 2);
        let label = |text: &str, class: Option<&str>, lines: i32| {
            let l = gtk::Label::new(Some(text));
            l.set_xalign(0.0);
            l.set_wrap(true);
            l.set_wrap_mode(gtk::pango::WrapMode::WordChar);
            l.set_max_width_chars(48);
            l.set_lines(lines);
            l.set_ellipsize(gtk::pango::EllipsizeMode::End);
            if let Some(c) = class {
                for cls in c.split(' ') {
                    l.add_css_class(cls);
                }
            }
            l
        };
        col.append(&label(&p.site, Some("caption dim-label"), 1));
        if !p.title.is_empty() {
            col.append(&label(&p.title, Some("heading"), 2));
        }
        if !p.description.is_empty() {
            col.append(&label(&p.description, Some("caption"), 3));
        }
        card.append(&col);
        card.set_cursor_from_name(Some("pointer"));
        card.set_tooltip_text(Some(url));
        let (click, url) = (gtk::GestureClick::new(), url.to_string());
        click.connect_released(move |_, _, _, _| {
            let _ = gtk::gio::AppInfo::launch_default_for_uri(&url, None::<&gtk::gio::AppLaunchContext>);
        });
        card.add_controller(click);
        while let Some(c) = holder.first_child() {
            holder.remove(&c);
        }
        holder.append(&card);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_local_and_private_hosts() {
        for h in ["localhost", "127.0.0.1", "10.1.2.3", "192.168.0.5", "169.254.1.1", "::1", "fd00::1", "printer.local"] {
            assert!(!host_allowed(h), "{h} must be blocked");
        }
        assert!(host_allowed("example.com"));
        assert!(host_allowed("93.184.216.34"));
    }

    #[test]
    fn youtube_ids() {
        let id = Some("dQw4w9WgXcQ".to_string());
        for u in [
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
            "https://youtube.com/watch?feature=share&v=dQw4w9WgXcQ&t=10",
            "https://m.youtube.com/watch?v=dQw4w9WgXcQ#x",
            "https://youtu.be/dQw4w9WgXcQ?si=abc",
            "https://www.youtube.com/shorts/dQw4w9WgXcQ",
            "https://www.youtube.com/embed/dQw4w9WgXcQ",
            "https://www.youtube.com/live/dQw4w9WgXcQ?feature=share",
        ] {
            assert_eq!(youtube_id(u), id, "{u}");
        }
        for u in [
            "https://www.youtube.com/",
            "https://www.youtube.com/watch?v=short",
            "https://www.youtube.com/@channel",
            "https://notyoutube.com/watch?v=dQw4w9WgXcQ",
        ] {
            assert_eq!(youtube_id(u), None, "{u}");
        }
    }

    #[test]
    fn host_parsing() {
        assert_eq!(host_of("https://user:pw@example.com:8443/a?b#c"), Some("example.com"));
        assert_eq!(host_of("https://[::1]:80/x"), Some("::1"));
        assert_eq!(host_of("ftp://example.com"), None);
    }

    #[test]
    fn parses_open_graph() {
        let html = r#"<html><head><title>Fallback</title>
            <meta property="og:title" content="Hello &amp; welcome">
            <meta name="description" content='A "quoted" page'>
            <meta property="og:image" content="/img/a.png">
            <meta property="og:site_name" content="Example"></head><body></body></html>"#;
        let p = parse_page("https://example.com/post/1", html).unwrap();
        assert_eq!(p.title, "Hello & welcome");
        assert_eq!(p.description, "A \"quoted\" page");
        assert_eq!(p.site, "Example");
        assert_eq!(p.image.as_deref(), Some("https://example.com/img/a.png"));
    }

    #[test]
    fn falls_back_to_title_tag_and_resolves_relative() {
        let p = parse_page("https://example.com/a/b", "<head><TITLE> Plain </TITLE></head>").unwrap();
        assert_eq!(p.title, "Plain");
        assert_eq!(resolve("https://example.com/a/b", "c.png").as_deref(), Some("https://example.com/a/c.png"));
        assert_eq!(resolve("https://example.com/a/b", "//cdn.x.com/i.png").as_deref(), Some("https://cdn.x.com/i.png"));
        assert!(parse_page("https://example.com", "<head></head>").is_none());
    }

    #[test]
    fn extracts_links_and_skips_media_and_microsoft() {
        let m = serde_json::json!({"body": {"content": "<a href=\"https://example.com/x?a=1&amp;b=2\">x</a> <a href=\"https://contoso.sharepoint.com/f\">f</a> <a href=\"https://example.com/p.png\">i</a>"}});
        let urls = urls_of(&m, "see https://rust-lang.org/learn, ok");
        assert_eq!(urls, vec!["https://example.com/x?a=1&b=2", "https://rust-lang.org/learn"]);
    }
}
