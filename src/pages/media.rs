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
// Bubbles keep only a thumbnail decoded at up to this size (2x the displayed size, for
// HiDPI); the original stays on disk and is decoded when the viewer opens it.
const DECODE_W: i32 = 640;
const DECODE_H: i32 = 480;
const MAX_TEXTURES: usize = 40; // decoded images are large; keep the cache small
const CACHE_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 3600);

const IMAGE_EXT: [&str; 7] = ["jpg", "jpeg", "png", "gif", "webp", "bmp", "tiff"];
const VIDEO_EXT: [&str; 6] = ["mp4", "mov", "mkv", "webm", "m4v", "avi"];

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Image,
    Video,
    /// Any other attachment: a card that opens the link in the browser.
    File,
}

#[derive(Clone)]
struct Item {
    kind: Kind,
    name: String,
    /// Hosted-content URL (graph.microsoft.com) or SharePoint/OneDrive link.
    url: String,
    shared_file: bool,
    /// Pixel size from the message HTML (`width`/`height`), to reserve space before loading.
    dims: Option<(f64, f64)>,
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

/// Number after `name` in an HTML tag: `width="320"` or `style="width: 320px"`.
fn tag_dim(tag: &str, name: &str) -> Option<f64> {
    let mut from = 0;
    while let Some(i) = tag[from..].find(name) {
        let at = from + i;
        from = at + name.len();
        let prev_ok = at == 0 || !(tag.as_bytes()[at - 1].is_ascii_alphanumeric() || tag.as_bytes()[at - 1] == b'-');
        if !prev_ok {
            continue;
        }
        let rest = tag[at + name.len()..].trim_start();
        let rest = rest.strip_prefix('=').or_else(|| rest.strip_prefix(':'))?.trim_start().trim_start_matches('"');
        let num: String = rest.chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
        if let Ok(v) = num.parse::<f64>() {
            if v > 0.0 {
                return Some(v);
            }
        }
    }
    None
}

fn tag_dims(tag: &str) -> Option<(f64, f64)> {
    Some((tag_dim(tag, "width")?, tag_dim(tag, "height")?))
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
            out.push(Item { kind: Kind::Image, name: "image.png".into(), url: src.to_string(), shared_file: false, dims: tag_dims(tag) });
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
        } else if a["contentType"].as_str() == Some("reference") {
            Kind::File
        } else {
            continue;
        };
        out.push(Item { kind, name: name.to_string(), url: url.to_string(), shared_file: true, dims: None });
    }
    out
}

/// What the context menu and forwarding need to know about one attachment.
#[derive(Clone)]
pub struct Attachment {
    pub name: String,
    /// Graph path that returns the bytes.
    pub api: String,
    /// Local copy location (used if already downloaded).
    pub cache: PathBuf,
}

pub type ForwardFn = Rc<dyn Fn(&gtk::Widget, Attachment)>;

impl Item {
    fn attachment(&self) -> Attachment {
        Attachment { name: self.name.clone(), api: self.api_path(), cache: self.cache_path() }
    }
}

/// Download into the cache unless it is already there. Blocking: call from a worker thread.
pub fn fetch_to_cache(a: &Attachment) -> Result<(), String> {
    if a.cache.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(a.cache.parent().unwrap_or(&a.cache)).map_err(|e| e.to_string())?;
    graph::download(&a.api, &a.cache, MAX_VIDEO_BYTES)
}

/// Open with the default app: download to the cache first when needed.
fn open_cached(a: Attachment) {
    util::toast(&format!("Opening {}…", a.name));
    util::run(
        move || fetch_to_cache(&a).map(|_| a.cache),
        |r| match r {
            Ok(p) => open_external(&p),
            Err(e) => util::toast(&format!("Could not open: {e}")),
        },
    );
}

fn save_as(w: &gtk::Widget, a: Attachment) {
    let parent = w.root().and_downcast::<gtk::Window>();
    glib::spawn_future_local(async move {
        let dlg = gtk::FileDialog::builder().title("Save attachment").initial_name(&a.name).build();
        let Ok(file) = dlg.save_future(parent.as_ref()).await else { return };
        let Some(dest) = file.path() else {
            return util::toast("Choose a local folder");
        };
        util::toast(&format!("Saving {}…", a.name));
        let name = a.name.clone();
        util::run(
            move || {
                if a.cache.exists() {
                    std::fs::copy(&a.cache, &dest).map(|_| ()).map_err(|e| e.to_string())
                } else {
                    graph::download(&a.api, &dest, MAX_VIDEO_BYTES)
                }
            },
            move |r| match r {
                Ok(()) => util::toast(&format!("Saved {name}")),
                Err(e) => util::toast(&format!("Could not save {name}: {e}")),
            },
        );
    });
}

/// Right-click menu (Open / Save as… / Forward…) at the pointer for an attachment widget.
fn attach_menu(w: &gtk::Widget, a: Attachment, open: Rc<dyn Fn()>, fwd: Rc<RefCell<Option<ForwardFn>>>) {
    let pop = gtk::Popover::new();
    pop.set_parent(w);
    pop.set_has_arrow(false);
    pop.set_halign(gtk::Align::Start);
    let col = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let entry = |label: &str, cb: Rc<dyn Fn()>| {
        let b = gtk::Button::with_label(label);
        b.add_css_class("flat");
        if let Some(l) = b.child().and_downcast::<gtk::Label>() {
            l.set_xalign(0.0);
        }
        let pop = pop.clone();
        b.connect_clicked(move |_| {
            pop.popdown();
            cb();
        });
        b
    };
    col.append(&entry("Open", open));
    let (w_save, a_save) = (w.downgrade(), a.clone());
    col.append(&entry(
        "Save as…",
        Rc::new(move || {
            if let Some(w) = w_save.upgrade() {
                save_as(&w, a_save.clone());
            }
        }),
    ));
    let w_fwd = w.downgrade();
    col.append(&entry(
        "Forward…",
        Rc::new(move || {
            let f = fwd.borrow().clone();
            match (f, w_fwd.upgrade()) {
                (Some(f), Some(w)) => f(&w, a.clone()),
                _ => util::toast("Forwarding is not available here"),
            }
        }),
    ));
    pop.set_child(Some(&col));
    let rc = gtk::GestureClick::new();
    rc.set_button(3);
    rc.set_propagation_phase(gtk::PropagationPhase::Capture);
    {
        let pop = pop.clone();
        rc.connect_pressed(move |g, _, x, y| {
            g.set_state(gtk::EventSequenceState::Claimed);
            pop.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
            pop.popup();
        });
    }
    w.add_controller(rc);
    w.connect_destroy(move |_| pop.unparent());
}

/// Thumbnail-sized texture from an image file. The decoder scales while loading, so a large
/// photo never exists in memory at full size. None when no pixbuf loader handles the format.
fn decode_thumb(path: &std::path::Path) -> Option<gdk::Texture> {
    let (_, w, h) = gtk::gdk_pixbuf::Pixbuf::file_info(path)?;
    let s = (DECODE_W as f64 / w as f64).min(DECODE_H as f64 / h as f64).min(1.0);
    let (tw, th) = (((w as f64 * s).round() as i32).max(1), ((h as f64 * s).round() as i32).max(1));
    let pb = gtk::gdk_pixbuf::Pixbuf::from_file_at_scale(path, tw, th, true).ok()?;
    let format = if pb.has_alpha() { gdk::MemoryFormat::R8g8b8a8 } else { gdk::MemoryFormat::R8g8b8 };
    Some(
        gdk::MemoryTexture::new(pb.width(), pb.height(), format, &pb.read_pixel_bytes(), pb.rowstride() as usize)
            .upcast(),
    )
}

/// Texture from encoded image bytes, scaled down to fit `maxw` x `maxh` while decoding
/// (never enlarged). None if no pixbuf loader handles the format.
pub(crate) fn decode_scaled(bytes: &[u8], maxw: i32, maxh: i32) -> Option<gdk::Texture> {
    let loader = gtk::gdk_pixbuf::PixbufLoader::new();
    loader.connect_size_prepared(move |l, w, h| {
        let s = (maxw as f64 / w as f64).min(maxh as f64 / h as f64).min(1.0);
        if s < 1.0 {
            l.set_size(((w as f64 * s).round() as i32).max(1), ((h as f64 * s).round() as i32).max(1));
        }
    });
    let ok = loader.write(bytes).is_ok();
    let closed = loader.close().is_ok();
    let pb = loader.pixbuf().filter(|_| ok && closed)?;
    let format = if pb.has_alpha() { gdk::MemoryFormat::R8g8b8a8 } else { gdk::MemoryFormat::R8g8b8 };
    Some(gdk::MemoryTexture::new(pb.width(), pb.height(), format, &pb.read_pixel_bytes(), pb.rowstride() as usize).upcast())
}

const MAX_GIF_BYTES: u64 = 6 * 1024 * 1024; // bigger files stay a still frame (all frames are decoded)

fn is_gif(path: &std::path::Path) -> bool {
    use std::io::Read;
    let mut head = [0u8; 6];
    std::fs::File::open(path).and_then(|mut f| f.read_exact(&mut head)).is_ok()
        && (&head == b"GIF89a" || &head == b"GIF87a")
        && std::fs::metadata(path).is_ok_and(|m| m.len() <= MAX_GIF_BYTES)
}

/// Play an animated GIF in `pic`, frames scaled to `w` x `h`. Only advances while the picture
/// is inside the message viewport and the window is active; stops when the picture is gone.
pub(crate) fn animate_gif(pic: &gtk::Picture, path: PathBuf, size: Option<(i32, i32)>) {
    let Ok(anim) = gtk::gdk_pixbuf::PixbufAnimation::from_file(&path) else { return };
    if anim.is_static_image() {
        return;
    }
    let iter = anim.iter(None);
    let weak = pic.downgrade();
    glib::spawn_future_local(async move {
        let _keep = &anim; // the iterator reads frames from the animation
        loop {
            let Some(pic) = weak.upgrade() else { break };
            let on_screen = pic.is_mapped()
                && pic.root().and_downcast::<gtk::Window>().is_some_and(|w| w.is_active())
                && pic
                    .ancestor(gtk::ScrolledWindow::static_type())
                    .and_downcast::<gtk::ScrolledWindow>()
                    .and_then(|sw| pic.compute_bounds(&sw).map(|b| (b, sw.height() as f32)))
                    .is_some_and(|(b, vh)| b.y() + b.height() > 0.0 && b.y() < vh);
            if !on_screen {
                drop(pic);
                glib::timeout_future(std::time::Duration::from_millis(300)).await;
                continue;
            }
            if iter.advance(std::time::SystemTime::now()) {
                let frame = iter.pixbuf();
                let frame = match size {
                    Some((w, h)) => frame.scale_simple(w, h, gtk::gdk_pixbuf::InterpType::Bilinear),
                    None => Some(frame),
                };
                if let Some(f) = frame {
                    pic.set_paintable(Some(&gdk::Texture::for_pixbuf(&f)));
                }
            }
            let delay = iter.delay_time().unwrap_or(std::time::Duration::from_millis(100));
            drop(pic);
            glib::timeout_future(delay.max(std::time::Duration::from_millis(30))).await;
        }
    });
}

fn open_external(path: &std::path::Path) {
    let _ = std::process::Command::new("xdg-open").arg(path).spawn();
}

/// Decoded textures are immutable and GDK documents them as usable from any thread; this
/// only lets a finished decode travel back from the worker thread.
pub(crate) struct SendTexture(pub(crate) gdk::Texture);
// SAFETY: GdkTexture is immutable and thread-safe; it is only used on the main thread afterwards.
unsafe impl Send for SendTexture {}

const MAX_PARALLEL: usize = 3; // image downloads at once
const LOAD_MARGIN: f32 = 300.0; // px beyond the viewport that still count as "near"
const PLACEHOLDER: (f64, f64) = (240.0, 160.0); // used when the size is unknown

pub struct Media {
    textures: RefCell<HashMap<String, gdk::Texture>>,
    inflight: RefCell<HashSet<String>>,
    /// Images waiting to be loaded (lazy): key -> item, and the holders showing it.
    wanted: RefCell<HashMap<String, Item>>,
    waiting: RefCell<HashMap<String, Vec<glib::WeakRef<gtk::Box>>>>,
    loading: RefCell<HashSet<String>>,
    failed: RefCell<HashMap<String, String>>,
    scroll: RefCell<Option<gtk::ScrolledWindow>>,
    kick_queued: std::cell::Cell<bool>,
    forward: Rc<RefCell<Option<ForwardFn>>>,
}

impl Media {
    pub fn new() -> Rc<Self> {
        prune_cache();
        Rc::new(Media {
            textures: Default::default(),
            inflight: Default::default(),
            wanted: Default::default(),
            waiting: Default::default(),
            loading: Default::default(),
            failed: Default::default(),
            scroll: Default::default(),
            kick_queued: Default::default(),
            forward: Default::default(),
        })
    }

    /// Whether the message has any photo, video or file attachment to show.
    pub fn has_media(m: &Value) -> bool {
        !items(m).is_empty()
    }

    /// Handler for the "Forward…" menu entry (owner shows a chat chooser and sends).
    pub fn set_forward(&self, f: ForwardFn) {
        *self.forward.borrow_mut() = Some(f);
    }

    /// Preview widgets for the message's photos and videos (empty if none).
    pub fn widgets(self: &Rc<Self>, m: &Value) -> Vec<gtk::Widget> {
        items(m)
            .into_iter()
            .map(|it| match it.kind {
                Kind::Image => self.image_widget(it),
                Kind::Video => self.video_widget(it),
                Kind::File => file_widget(it, self.forward.clone()),
            })
            .collect()
    }

    /// Lazy loading: images are fetched only when near the viewport, nearest first, a few at
    /// a time. Call once with the message list's scroll window.
    pub fn attach_viewport(self: &Rc<Self>, scroll: &gtk::ScrolledWindow) {
        *self.scroll.borrow_mut() = Some(scroll.clone());
        let adj = scroll.vadjustment();
        let me = self.clone();
        adj.connect_value_changed(move |_| me.schedule_kick());
        let me = self.clone();
        adj.connect_changed(move |_| me.schedule_kick()); // layout settled or size changed
    }

    fn schedule_kick(self: &Rc<Self>) {
        if self.kick_queued.replace(true) {
            return;
        }
        let me = self.clone();
        glib::idle_add_local_once(move || {
            me.kick_queued.set(false);
            me.kick();
        });
    }

    /// Start loading the waiting images closest to the viewport.
    fn kick(self: &Rc<Self>) {
        let Some(scroll) = self.scroll.borrow().clone() else { return };
        let view_h = scroll.height() as f32;
        let mut near: Vec<(f32, String)> = vec![];
        {
            let mut waiting = self.waiting.borrow_mut();
            waiting.retain(|_, hs| {
                hs.retain(|h| h.upgrade().is_some_and(|b| b.parent().is_some()));
                !hs.is_empty()
            });
            for (key, hs) in waiting.iter() {
                if self.loading.borrow().contains(key) {
                    continue;
                }
                let dist = hs
                    .iter()
                    .filter_map(|h| h.upgrade())
                    .filter_map(|b| b.compute_bounds(&scroll))
                    .map(|r| {
                        if r.y() + r.height() < 0.0 {
                            -(r.y() + r.height())
                        } else if r.y() > view_h {
                            r.y() - view_h
                        } else {
                            0.0 // on screen
                        }
                    })
                    .fold(f32::MAX, f32::min);
                if dist <= LOAD_MARGIN {
                    near.push((dist, key.clone()));
                }
            }
        }
        near.sort_by(|a, b| a.0.total_cmp(&b.0));
        for (_, key) in near {
            if self.loading.borrow().len() >= MAX_PARALLEL {
                break;
            }
            self.start_load(&key);
        }
    }

    fn start_load(self: &Rc<Self>, key: &str) {
        let Some(it) = self.wanted.borrow().get(key).cloned() else { return };
        self.loading.borrow_mut().insert(key.to_string());
        // Show a spinner on the placeholders now that loading has begun.
        for h in self.waiting.borrow().get(key).into_iter().flatten().filter_map(|h| h.upgrade()) {
            let spin = gtk::Spinner::new();
            spin.start();
            spin.set_halign(gtk::Align::Center);
            spin.set_valign(gtk::Align::Center);
            spin.set_hexpand(true);
            spin.set_vexpand(true);
            spin.set_size_request(24, 24);
            h.append(&spin);
        }
        let (me, key, path, name) = (self.clone(), key.to_string(), it.api_path(), it.name.clone());
        let file = it.cache_path();
        let it_file = file.clone();
        util::run(
            // Fetch and decode off the UI thread so big photos do not stall scrolling.
            move || {
                // Already downloaded (it was shown before): decode from disk, no request.
                if let Some(t) = file.exists().then(|| decode_thumb(&file)).flatten() {
                    return Ok(SendTexture(t));
                }
                let bytes = graph::get_bytes(&path, MAX_IMAGE_BYTES)?;
                // Keep the original on disk (for the viewer) and decode only a thumbnail.
                let on_disk = std::fs::create_dir_all(file.parent().unwrap_or(&file)).is_ok()
                    && std::fs::write(&file, &bytes).is_ok();
                if on_disk {
                    if let Some(t) = decode_thumb(&file) {
                        return Ok(SendTexture(t));
                    }
                }
                gdk::Texture::from_bytes(&glib::Bytes::from_owned(bytes))
                    .map(SendTexture)
                    .map_err(|e| e.to_string())
            },
            move |r| {
                me.loading.borrow_mut().remove(&key);
                me.wanted.borrow_mut().remove(&key);
                let holders = me.waiting.borrow_mut().remove(&key).unwrap_or_default();
                let file2 = it_file;
                match r {
                    Ok(SendTexture(t)) => {
                        let mut c = me.textures.borrow_mut();
                        // Evict one, not all: clearing made every image on screen load again
                        // (placeholder, then image) the next time its row was built.
                        if c.len() >= MAX_TEXTURES {
                            if let Some(k) = c.keys().next().cloned() {
                                c.remove(&k);
                            }
                        }
                        c.insert(key.clone(), t.clone());
                        drop(c);
                        for h in holders.iter().filter_map(|h| h.upgrade()) {
                            show_image(&h, &t, &name, &file2, true);
                        }
                    }
                    Err(e) => {
                        for h in holders.iter().filter_map(|h| h.upgrade()) {
                            show_error(&h, &e);
                        }
                        me.failed.borrow_mut().insert(key, e);
                    }
                }
                me.schedule_kick(); // next in line
            },
        );
    }

    fn image_widget(self: &Rc<Self>, it: Item) -> gtk::Widget {
        let holder = gtk::Box::new(gtk::Orientation::Vertical, 0);
        holder.set_halign(gtk::Align::Start);
        let key = it.key();
        {
            let (me, key, it2) = (self.clone(), key.clone(), it.clone());
            let hw = holder.downgrade();
            let open: Rc<dyn Fn()> = Rc::new(move || {
                let tex = me.textures.borrow().get(&key).cloned();
                match (tex, hw.upgrade()) {
                    (Some(t), Some(h)) => {
                        open_image(h.root().and_downcast::<gtk::Window>().as_ref(), &t, &it2.name, &it2.cache_path())
                    }
                    _ => open_cached(it2.attachment()),
                }
            });
            attach_menu(holder.upcast_ref(), it.attachment(), open, self.forward.clone());
        }
        if let Some(t) = self.textures.borrow().get(&key).cloned() {
            show_image(&holder, &t, &it.name, &it.cache_path(), false);
            return holder.upcast();
        }
        if let Some(e) = self.failed.borrow().get(&key) {
            show_error(&holder, e);
            return holder.upcast();
        }
        // Reserve the final space now so nothing shifts when the image arrives.
        let (w, h) = fit(it.dims.unwrap_or(PLACEHOLDER));
        holder.set_size_request(w, h);
        holder.add_css_class("thumb-placeholder");
        self.waiting.borrow_mut().entry(key.clone()).or_default().push(holder.downgrade());
        self.wanted.borrow_mut().entry(key).or_insert(it);
        self.schedule_kick();
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
        {
            let bw = b.downgrade();
            let open: Rc<dyn Fn()> = Rc::new(move || {
                if let Some(b) = bw.upgrade() {
                    b.emit_clicked();
                }
            });
            attach_menu(b.upcast_ref(), it.attachment(), open, self.forward.clone());
        }
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

/// Card for a non-media file attachment; click opens its link in the browser.
fn file_widget(it: Item, fwd: Rc<RefCell<Option<ForwardFn>>>) -> gtk::Widget {
    let b = gtk::Button::new();
    b.add_css_class("flat");
    b.add_css_class("quiet-icon");
    b.set_halign(gtk::Align::Start);
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    row.append(&gtk::Image::from_icon_name("text-x-generic-symbolic"));
    let l = gtk::Label::new(Some(&it.name));
    l.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    l.set_max_width_chars(40);
    row.append(&l);
    b.set_child(Some(&row));
    b.set_tooltip_text(Some(&it.url));
    {
        let url = it.url.clone();
        let open: Rc<dyn Fn()> = Rc::new(move || {
            let _ = gtk::gio::AppInfo::launch_default_for_uri(&url, None::<&gtk::gio::AppLaunchContext>);
        });
        attach_menu(b.upcast_ref(), it.attachment(), open, fwd);
    }
    b.connect_clicked(move |_| {
        let _ = gtk::gio::AppInfo::launch_default_for_uri(&it.url, None::<&gtk::gio::AppLaunchContext>);
    });
    b.upcast()
}

fn clear(b: &gtk::Box) {
    // Keep popovers (the right-click menu is parented to the holder).
    let mut child = b.first_child();
    while let Some(c) = child {
        child = c.next_sibling();
        if !c.is::<gtk::Popover>() {
            b.remove(&c);
        }
    }
}

/// Thumbnail size for an image of `dims` pixels: fits the thumbnail box, never upscaled.
fn fit(dims: (f64, f64)) -> (i32, i32) {
    let (w, h) = dims;
    let s = (THUMB_W / w).min(THUMB_H / h).min(1.0);
    ((w * s).max(1.0) as i32, (h * s).max(1.0) as i32)
}

fn show_error(holder: &gtk::Box, e: &str) {
    clear(holder);
    holder.set_size_request(-1, -1);
    holder.remove_css_class("thumb-placeholder");
    let l = gtk::Label::new(Some(&format!("Image unavailable ({e})")));
    l.add_css_class("dim-label");
    l.add_css_class("caption");
    holder.append(&l);
}

/// Scaled thumbnail in the bubble; click opens the full-size viewer. `fade`: fade it in.
fn show_image(holder: &gtk::Box, tex: &gdk::Texture, name: &str, full: &std::path::Path, fade: bool) {
    clear(holder);
    holder.remove_css_class("thumb-placeholder");
    let (pw, ph) = fit((tex.width() as f64, tex.height() as f64));
    holder.set_size_request(pw, ph);
    let pic = gtk::Picture::for_paintable(tex);
    pic.set_content_fit(gtk::ContentFit::Contain);
    pic.set_can_shrink(true);
    pic.set_size_request(pw, ph);
    pic.set_cursor_from_name(Some("pointer"));
    pic.set_tooltip_text(Some(name));
    pic.add_css_class("thumb");
    if is_gif(full) {
        animate_gif(&pic, full.to_path_buf(), Some((tex.width(), tex.height())));
    }
    if fade {
        pic.set_opacity(0.0);
        let p = pic.clone();
        glib::idle_add_local_once(move || p.set_opacity(1.0));
    }
    let click = gtk::GestureClick::new();
    let (tex, name, full) = (tex.clone(), name.to_string(), full.to_path_buf());
    let holder_w = holder.downgrade();
    click.connect_released(move |_, _, _, _| {
        let parent = holder_w.upgrade().and_then(|h| h.root()).and_downcast::<gtk::Window>();
        open_image(parent.as_ref(), &tex, &name, &full);
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

/// Open the viewer. The bubble only holds a thumbnail, so decode the original from the
/// cache file (off the UI thread) and show that; fall back to the thumbnail.
fn open_image(parent: Option<&gtk::Window>, tex: &gdk::Texture, name: &str, full: &std::path::Path) {
    if !full.exists() {
        show_viewer(parent, tex, name);
        return;
    }
    let (parent, tex, name, path) = (parent.cloned(), tex.clone(), name.to_string(), full.to_path_buf());
    let gif_path = path.clone();
    util::run(
        move || gdk::Texture::from_filename(&path).ok().map(SendTexture),
        move |r| match r {
            Some(SendTexture(full_tex)) => {
                let img = show_viewer(parent.as_ref(), &full_tex, &name);
                if let (Some(img), true) = (img, is_gif(&gif_path)) {
                    animate_gif(&img, gif_path, None);
                }
            }
            None => {
                show_viewer(parent.as_ref(), &tex, &name);
            }
        },
    );
}

/// Returns the in-app viewer's picture (for animation); None for the fallback window.
fn show_viewer(parent: Option<&gtk::Window>, tex: &gdk::Texture, name: &str) -> Option<gtk::Picture> {
    if let Some(img) = super::lightbox::show_image(tex, name) {
        return Some(img);
    }
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
    Some(pic)
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
