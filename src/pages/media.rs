//! Photo and video previews for Teams messages.
//!
//! Sources: inline images (`<img src=".../hostedContents/.../$value">` in the message
//! body) and file attachments (SharePoint/OneDrive references). Images are fetched with the
//! user's token, decoded on the UI thread and cached in memory. Videos are downloaded to
//! the cache dir on click and played in a viewer window, falling back to the system player.
use crate::{auth, graph, util};
use adw::prelude::*;
use gtk::{gdk, glib};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    path::PathBuf,
    rc::Rc,
    time::{Duration, SystemTime},
};

const MAX_IMAGE_BYTES: u64 = 25 * 1024 * 1024;
const MAX_VIDEO_BYTES: u64 = 1024 * 1024 * 1024;
const THUMB_W: f64 = 320.0;
const THUMB_H: f64 = 240.0;
const MAX_TEXTURES: usize = 40; // decoded images are large; keep the cache small
const CACHE_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 3600);

const IMAGE_EXT: [&str; 7] = ["jpg", "jpeg", "png", "gif", "webp", "bmp", "tiff"];
const VIDEO_EXT: [&str; 6] = ["mp4", "mov", "mkv", "webm", "m4v", "avi"];

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Image,
    Video,
}

#[derive(Clone)]
struct Item {
    kind: Kind,
    name: String,
    /// Hosted-content URL (graph.microsoft.com) or SharePoint/OneDrive link.
    url: String,
    shared_file: bool,
}

impl Item {
    /// Graph path (relative to the v1.0 base) that returns the bytes.
    fn api_path(&self) -> String {
        if self.shared_file {
            format!("/shares/u!{}/driveItem/content", auth::b64url(self.url.as_bytes()))
        } else {
            self.url.strip_prefix(graph::BASE).unwrap_or(&self.url).to_string()
        }
    }
    fn key(&self) -> String {
        hex(&Sha256::digest(self.url.as_bytes()))[..16].to_string()
    }
    fn cache_path(&self) -> PathBuf {
        let safe: String = self
            .name
            .chars()
            .map(|c| if c.is_alphanumeric() || c == '.' || c == '-' { c } else { '_' })
            .collect();
        cache_dir().join(format!("{}_{}", self.key(), safe))
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn cache_dir() -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("penguin-ms-suite").join("media")
}

/// Delete cached downloads older than a week.
fn prune_cache() {
    let Ok(rd) = std::fs::read_dir(cache_dir()) else { return };
    for e in rd.flatten() {
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| SystemTime::now().duration_since(t).ok())
            .is_some_and(|age| age > CACHE_MAX_AGE);
        if old {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

fn ext_of(name: &str) -> String {
    name.rsplit('.').next().unwrap_or("").to_ascii_lowercase()
}

/// Media in one message: inline images, then attachments.
fn items(m: &Value) -> Vec<Item> {
    let mut out = vec![];
    // Inline images pasted into the message.
    let html = m["body"]["content"].as_str().unwrap_or("");
    for part in html.split("<img").skip(1) {
        let tag = part.split('>').next().unwrap_or("");
        let Some(src) = tag.split("src=\"").nth(1).and_then(|r| r.split('"').next()) else { continue };
        if src.starts_with(graph::BASE) && src.contains("/hostedContents/") {
            out.push(Item { kind: Kind::Image, name: "image.png".into(), url: src.to_string(), shared_file: false });
        }
    }
    // Files shared as references (OneDrive / SharePoint).
    for a in m["attachments"].as_array().into_iter().flatten() {
        let (Some(name), Some(url)) = (a["name"].as_str(), a["contentUrl"].as_str()) else { continue };
        let ext = ext_of(name);
        let kind = if IMAGE_EXT.contains(&ext.as_str()) {
            Kind::Image
        } else if VIDEO_EXT.contains(&ext.as_str()) {
            Kind::Video
        } else {
            continue;
        };
        out.push(Item { kind, name: name.to_string(), url: url.to_string(), shared_file: true });
    }
    out
}

fn open_external(path: &std::path::Path) {
    let _ = std::process::Command::new("xdg-open").arg(path).spawn();
}

pub struct Media {
    textures: RefCell<HashMap<String, gdk::Texture>>,
    inflight: RefCell<HashSet<String>>,
}

impl Media {
    pub fn new() -> Rc<Self> {
        prune_cache();
        Rc::new(Media { textures: Default::default(), inflight: Default::default() })
    }

    /// Preview widgets for the message's photos and videos (empty if none).
    pub fn widgets(self: &Rc<Self>, m: &Value) -> Vec<gtk::Widget> {
        items(m)
            .into_iter()
            .map(|it| match it.kind {
                Kind::Image => self.image_widget(it),
                Kind::Video => self.video_widget(it),
            })
            .collect()
    }

    fn image_widget(self: &Rc<Self>, it: Item) -> gtk::Widget {
        let holder = gtk::Box::new(gtk::Orientation::Vertical, 0);
        holder.set_halign(gtk::Align::Start);
        let key = it.key();
        if let Some(t) = self.textures.borrow().get(&key).cloned() {
            show_image(&holder, &t, &it.name);
            return holder.upcast();
        }
        let spin = gtk::Spinner::new();
        spin.start();
        spin.set_size_request(THUMB_W as i32, 120);
        holder.append(&spin);
        if self.inflight.borrow_mut().insert(key.clone()) {
            let (me, weak, path, name) = (self.clone(), holder.downgrade(), it.api_path(), it.name.clone());
            util::run(
                move || graph::get_bytes(&path, MAX_IMAGE_BYTES),
                move |r| {
                    me.inflight.borrow_mut().remove(&key);
                    let tex = r.and_then(|b| {
                        gdk::Texture::from_bytes(&glib::Bytes::from_owned(b)).map_err(|e| e.to_string())
                    });
                    match tex {
                        Ok(t) => {
                            let mut c = me.textures.borrow_mut();
                            if c.len() >= MAX_TEXTURES {
                                c.clear();
                            }
                            c.insert(key, t.clone());
                            if let Some(h) = weak.upgrade() {
                                show_image(&h, &t, &name);
                            }
                        }
                        Err(e) => {
                            if let Some(h) = weak.upgrade() {
                                clear(&h);
                                let l = gtk::Label::new(Some(&format!("Image unavailable ({e})")));
                                l.add_css_class("dim-label");
                                l.add_css_class("caption");
                                h.append(&l);
                            }
                        }
                    }
                },
            );
        }
        holder.upcast()
    }

    fn video_widget(self: &Rc<Self>, it: Item) -> gtk::Widget {
        let b = gtk::Button::new();
        b.add_css_class("flat");
        b.add_css_class("quiet-icon");
        b.set_halign(gtk::Align::Start);
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        row.append(&gtk::Image::from_icon_name("media-playback-start-symbolic"));
        let l = gtk::Label::new(Some(&it.name));
        l.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        l.set_max_width_chars(40);
        row.append(&l);
        b.set_child(Some(&row));
        let me = self.clone();
        b.connect_clicked(move |btn| {
            let path = it.cache_path();
            let parent = btn.root().and_downcast::<gtk::Window>();
            if path.exists() {
                open_video(parent.as_ref(), &path, &it.name);
                return;
            }
            let key = it.key();
            if !me.inflight.borrow_mut().insert(key.clone()) {
                return util::toast("Already downloading…");
            }
            util::toast(&format!("Downloading {}…", it.name));
            let (me, name, api, dest) = (me.clone(), it.name.clone(), it.api_path(), path.clone());
            btn.set_sensitive(false);
            let btn = btn.clone();
            util::run(
                move || {
                    std::fs::create_dir_all(dest.parent().unwrap_or(&dest)).map_err(|e| e.to_string())?;
                    graph::download(&api, &dest, MAX_VIDEO_BYTES)
                },
                move |r| {
                    me.inflight.borrow_mut().remove(&key);
                    btn.set_sensitive(true);
                    match r {
                        Ok(()) => open_video(btn.root().and_downcast::<gtk::Window>().as_ref(), &path, &name),
                        Err(e) => util::toast(&format!("Could not download {name}: {e}")),
                    }
                },
            );
        });
        b.upcast()
    }
}

fn clear(b: &gtk::Box) {
    while let Some(c) = b.first_child() {
        b.remove(&c);
    }
}

/// Scaled thumbnail in the bubble; click opens the full-size viewer.
fn show_image(holder: &gtk::Box, tex: &gdk::Texture, name: &str) {
    clear(holder);
    let (w, h) = (tex.width() as f64, tex.height() as f64);
    let s = (THUMB_W / w).min(THUMB_H / h).min(1.0);
    let pic = gtk::Picture::for_paintable(tex);
    pic.set_content_fit(gtk::ContentFit::Contain);
    pic.set_can_shrink(true);
    pic.set_size_request((w * s).max(1.0) as i32, (h * s).max(1.0) as i32);
    pic.set_cursor_from_name(Some("pointer"));
    pic.set_tooltip_text(Some(name));
    let click = gtk::GestureClick::new();
    let (tex, name) = (tex.clone(), name.to_string());
    let holder_w = holder.downgrade();
    click.connect_released(move |_, _, _, _| {
        let parent = holder_w.upgrade().and_then(|h| h.root()).and_downcast::<gtk::Window>();
        open_image(parent.as_ref(), &tex, &name);
    });
    pic.add_controller(click);
    holder.append(&pic);
}

fn viewer(parent: Option<&gtk::Window>, title: &str) -> gtk::Window {
    let w = gtk::Window::builder().title(title).default_width(1000).default_height(700).build();
    w.set_transient_for(parent);
    let key = gtk::EventControllerKey::new();
    let wc = w.clone();
    key.connect_key_pressed(move |_, k, _, _| {
        if k == gtk::gdk::Key::Escape {
            wc.close();
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });
    w.add_controller(key);
    w
}

fn open_image(parent: Option<&gtk::Window>, tex: &gdk::Texture, name: &str) {
    let w = viewer(parent, name);
    let pic = gtk::Picture::for_paintable(tex);
    pic.set_content_fit(gtk::ContentFit::Contain);
    pic.set_can_shrink(true);
    let click = gtk::GestureClick::new();
    let wc = w.clone();
    click.connect_released(move |_, _, _, _| wc.close());
    pic.add_controller(click);
    w.set_child(Some(&pic));
    w.present();
}

fn open_video(parent: Option<&gtk::Window>, path: &std::path::Path, name: &str) {
    let w = viewer(parent, name);
    let header = gtk::HeaderBar::new();
    let ext_btn = gtk::Button::with_label("Open in default player");
    {
        let (p, wc) = (path.to_path_buf(), w.clone());
        ext_btn.connect_clicked(move |_| {
            open_external(&p);
            wc.close();
        });
    }
    header.pack_end(&ext_btn);
    w.set_titlebar(Some(&header));
    let video = gtk::Video::new();
    video.set_autoplay(true);
    video.set_filename(Some(path));
    // No decoder for this codec (e.g. H.264 without gst-libav): hand over to the system player.
    if let Some(stream) = video.media_stream() {
        let (p, wc) = (path.to_path_buf(), w.clone());
        stream.connect_error_notify(move |s| {
            if s.error().is_some() {
                util::toast("Cannot play in-app, opening the default player");
                open_external(&p);
                wc.close();
            }
        });
    }
    w.set_child(Some(&video));
    w.present();
}
