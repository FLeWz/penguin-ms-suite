//! Sending attachments in Teams chats: file picker, drag and drop, clipboard paste,
//! a staging tray above the message bar, and the Graph upload/send sequence.
//!
//! Sending (worker thread):
//! - small images (<= 4 MB) go inline as `hostedContents`, like a pasted image in Teams;
//! - everything else is uploaded to OneDrive ("Microsoft Teams Chat Files", simple upload,
//!   max 250 MB), shared "view" with the chat's other members only (scope `users`), and
//!   attached to the message as a reference. If sharing fails the file is not sent.
use crate::{graph, util};
use adw::prelude::*;
use base64::Engine;
use gtk::{gdk, gio, glib};
use serde_json::{json, Value};
use std::{cell::RefCell, path::PathBuf, rc::Rc};

const INLINE_IMAGE_MAX: u64 = 4_000_000;
const UPLOAD_MAX: u64 = 250 * 1024 * 1024;
const FOLDER: &str = "Microsoft Teams Chat Files";

pub enum Src {
    Path(PathBuf),
    Bytes(Vec<u8>),
}

pub struct Staged {
    pub name: String,
    pub src: Src,
    pub size: u64,
}

impl Staged {
    fn mime(&self) -> &'static str {
        match self.name.rsplit('.').next().unwrap_or("").to_ascii_lowercase().as_str() {
            "png" => "image/png",
            "jpg" | "jpeg" => "image/jpeg",
            "gif" => "image/gif",
            "webp" => "image/webp",
            _ => "application/octet-stream",
        }
    }
    fn is_image(&self) -> bool {
        self.mime().starts_with("image/")
    }
    fn inline(&self) -> bool {
        self.is_image() && self.size <= INLINE_IMAGE_MAX
    }
    fn bytes(&self) -> Result<Vec<u8>, String> {
        match &self.src {
            Src::Bytes(b) => Ok(b.clone()),
            Src::Path(p) => std::fs::read(p).map_err(|e| format!("{}: {e}", self.name)),
        }
    }
}

pub fn human_size(n: u64) -> String {
    match n {
        0..=1023 => format!("{n} B"),
        1024..=1048575 => format!("{:.0} KB", n as f64 / 1024.0),
        _ => format!("{:.1} MB", n as f64 / 1048576.0),
    }
}

// ---------- staging tray ----------

/// Files queued for the next message, shown as removable chips above the message bar.
pub struct Tray {
    pub widget: gtk::ScrolledWindow,
    row: gtk::Box,
    items: Rc<RefCell<Vec<Staged>>>,
}

fn rebuild(items: &Rc<RefCell<Vec<Staged>>>, row: &gtk::Box, widget: &gtk::ScrolledWindow) {
    while let Some(c) = row.first_child() {
        row.remove(&c);
    }
    for (i, f) in items.borrow().iter().enumerate() {
        let chip = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        chip.add_css_class("attach-chip");
        if f.is_image() && f.size < 10_000_000 {
            let tex = match &f.src {
                Src::Path(p) => gdk::Texture::from_filename(p).ok(),
                Src::Bytes(b) => gdk::Texture::from_bytes(&glib::Bytes::from(b.as_slice())).ok(),
            };
            if let Some(t) = tex {
                let pic = gtk::Picture::for_paintable(&t);
                pic.set_content_fit(gtk::ContentFit::Cover);
                pic.set_size_request(36, 36);
                chip.append(&pic);
            }
        } else {
            chip.append(&gtk::Image::from_icon_name("text-x-generic-symbolic"));
        }
        let l = gtk::Label::new(Some(&format!("{} ({})", f.name, human_size(f.size))));
        l.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        l.set_max_width_chars(28);
        chip.append(&l);
        let x = gtk::Button::from_icon_name("window-close-symbolic");
        x.add_css_class("flat");
        x.add_css_class("circular");
        x.set_tooltip_text(Some("Remove"));
        let (items2, row2, widget2) = (items.clone(), row.clone(), widget.clone());
        x.connect_clicked(move |_| {
            let mut v = items2.borrow_mut();
            if i < v.len() {
                v.remove(i);
            }
            drop(v);
            rebuild(&items2, &row2, &widget2);
        });
        chip.append(&x);
        row.append(&chip);
    }
    widget.set_visible(!items.borrow().is_empty());
}

impl Tray {
    pub fn new() -> Rc<Self> {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        row.set_margin_start(6);
        row.set_margin_end(6);
        row.set_margin_top(4);
        let widget = gtk::ScrolledWindow::builder()
            .child(&row)
            .vscrollbar_policy(gtk::PolicyType::Never)
            .propagate_natural_height(true)
            .visible(false)
            .build();
        Rc::new(Tray { widget, row, items: Default::default() })
    }

    pub fn is_empty(&self) -> bool {
        self.items.borrow().is_empty()
    }

    pub fn take(&self) -> Vec<Staged> {
        let v = std::mem::take(&mut *self.items.borrow_mut());
        rebuild(&self.items, &self.row, &self.widget);
        v
    }

    /// Put files back after a failed send (before anything newly added).
    pub fn restore(&self, mut files: Vec<Staged>) {
        files.append(&mut self.items.borrow_mut());
        *self.items.borrow_mut() = files;
        rebuild(&self.items, &self.row, &self.widget);
    }

    fn push(&self, f: Staged) {
        self.items.borrow_mut().push(f);
        rebuild(&self.items, &self.row, &self.widget);
    }

    pub fn add_path(&self, p: PathBuf) {
        let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        match std::fs::metadata(&p) {
            Ok(m) if m.is_dir() => util::toast(&format!("{name} is a folder; attach files only")),
            Ok(m) if m.len() > UPLOAD_MAX => {
                util::toast(&format!("{name} is too large (max {})", human_size(UPLOAD_MAX)))
            }
            Ok(m) => self.push(Staged { name, src: Src::Path(p), size: m.len() }),
            Err(e) => util::toast(&format!("Cannot read {name}: {e}")),
        }
    }

    pub fn add_texture(&self, t: &gdk::Texture) {
        let png = t.save_to_png_bytes();
        let stamp = glib::DateTime::now_local()
            .ok()
            .and_then(|d| d.format("%Y%m%d-%H%M%S").ok())
            .map(|g| g.to_string())
            .unwrap_or_else(|| "image".into());
        self.push(Staged {
            name: format!("pasted-image-{stamp}.png"),
            size: png.len() as u64,
            src: Src::Bytes(png.to_vec()),
        });
    }

    fn add_files(&self, files: &[gio::File]) {
        for f in files {
            match f.path() {
                Some(p) => self.add_path(p),
                None => util::toast("That item is not a local file"),
            }
        }
    }
}

// ---------- input: picker, drop, paste ----------

pub fn pick_files(tray: &Rc<Tray>, parent: Option<gtk::Window>) {
    let tray = tray.clone();
    glib::spawn_future_local(async move {
        let dialog = gtk::FileDialog::builder().title("Attach files").build();
        if let Ok(list) = dialog.open_multiple_future(parent.as_ref()).await {
            let files: Vec<gio::File> = (0..list.n_items())
                .filter_map(|i| list.item(i).and_downcast::<gio::File>())
                .collect();
            tray.add_files(&files);
        }
    });
}

/// Accept files and images dropped onto `target`.
pub fn install_drop(target: &impl IsA<gtk::Widget>, tray: &Rc<Tray>) {
    let t = target.as_ref().clone();
    let files = gtk::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY);
    {
        let (t, tray) = (t.clone(), tray.clone());
        files.connect_drop(move |_, v, _, _| {
            t.remove_css_class("drop-active");
            match v.get::<gdk::FileList>() {
                Ok(fl) => {
                    tray.add_files(&fl.files());
                    true
                }
                Err(_) => false,
            }
        });
    }
    let image = gtk::DropTarget::new(gdk::Texture::static_type(), gdk::DragAction::COPY);
    {
        let (t, tray) = (t.clone(), tray.clone());
        image.connect_drop(move |_, v, _, _| {
            t.remove_css_class("drop-active");
            match v.get::<gdk::Texture>() {
                Ok(tex) => {
                    tray.add_texture(&tex);
                    true
                }
                Err(_) => false,
            }
        });
    }
    for dt in [&files, &image] {
        let t1 = t.clone();
        dt.connect_enter(move |_, _, _| {
            t1.add_css_class("drop-active");
            gdk::DragAction::COPY
        });
        let t2 = t.clone();
        dt.connect_leave(move |_| t2.remove_css_class("drop-active"));
    }
    t.add_controller(files);
    t.add_controller(image);
}

/// Ctrl+V / Shift+Insert in the message bar: pasted files or images are staged; plain text
/// still pastes normally. Files win over text (file managers also put paths on the clipboard);
/// an image is taken only when there is no text (a copied spreadsheet range has both).
pub fn install_paste(entry: &gtk::Entry, tray: &Rc<Tray>) {
    use gtk::gdk::Key;
    let key = gtk::EventControllerKey::new();
    key.set_propagation_phase(gtk::PropagationPhase::Capture);
    let (entry2, tray) = (entry.clone(), tray.clone());
    key.connect_key_pressed(move |_, k, _, state| {
        let ctrl = state.contains(gdk::ModifierType::CONTROL_MASK);
        let shift = state.contains(gdk::ModifierType::SHIFT_MASK);
        let paste = (ctrl && (k == Key::v || k == Key::V)) || (shift && k == Key::Insert);
        if !paste {
            return glib::Propagation::Proceed;
        }
        let cb = entry2.clipboard();
        let formats = cb.formats();
        if formats.contains_type(gdk::FileList::static_type()) {
            let tray = tray.clone();
            glib::spawn_future_local(async move {
                if let Ok(v) = cb.read_value_future(gdk::FileList::static_type(), glib::Priority::DEFAULT).await {
                    if let Ok(fl) = v.get::<gdk::FileList>() {
                        tray.add_files(&fl.files());
                    }
                }
            });
            glib::Propagation::Stop
        } else if formats.contains_type(gdk::Texture::static_type())
            && !formats.contains_type(String::static_type())
        {
            let tray = tray.clone();
            glib::spawn_future_local(async move {
                if let Ok(t) = cb.read_texture_future().await {
                    if let Some(t) = t {
                        tray.add_texture(&t);
                    }
                }
            });
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });
    entry.add_controller(key);
}

// ---------- sending (worker thread) ----------

/// Message text as Teams HTML: escaped, line breaks kept, and bare `http(s)://` and `www.`
/// addresses turned into `<a href>` links. A plain-text body is not linked by Teams.
pub fn html_body(text: &str) -> String {
    let mut out = String::from("<p>");
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            out.push_str("<br>");
        }
        let mut first = true;
        for word in line.split(' ') {
            if !first {
                out.push(' ');
            }
            first = false;
            let url_start = word.starts_with("http://") || word.starts_with("https://") || word.starts_with("www.");
            if !url_start {
                out.push_str(&esc(word));
                continue;
            }
            let url = word.trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']', '}', '\'', '"']);
            let host_ok = url.split_once("://").map_or(url, |(_, r)| r).len() > 3;
            if !host_ok {
                out.push_str(&esc(word));
                continue;
            }
            let href = if url.starts_with("www.") { format!("https://{url}") } else { url.to_string() };
            out.push_str(&format!("<a href=\"{}\">{}</a>{}", esc(&href), esc(url), esc(&word[url.len()..])));
        }
    }
    out.push_str("</p>");
    out
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// Teams attachment id: the driveItem eTag GUID without dashes, else random hex.
fn attachment_id(item: &Value) -> String {
    let from_etag = item["eTag"]
        .as_str()
        .and_then(|e| e.split('{').nth(1))
        .and_then(|e| e.split('}').next())
        .map(|g| g.replace('-', "").to_ascii_lowercase());
    from_etag.unwrap_or_else(|| {
        let mut b = [0u8; 16];
        let _ = std::io::Read::read_exact(&mut std::fs::File::open("/dev/urandom").unwrap(), &mut b);
        b.iter().map(|x| format!("{x:02x}")).collect()
    })
}

fn upload(f: &Staged) -> Result<Value, String> {
    let path = format!(
        "/me/drive/root:/{}/{}:/content?@microsoft.graph.conflictBehavior=rename",
        graph::enc(FOLDER),
        graph::enc(&f.name)
    );
    match &f.src {
        Src::Path(p) => graph::put_file(&path, p),
        Src::Bytes(b) => graph::put_bytes(&path, b.clone()),
    }
}

/// Upload (if needed), share with the other chat members, and post one message.
pub fn send(chat_id: &str, text: &str, files: &[Staged], recipients: &[String]) -> Result<(), String> {
    let mut html = String::new();
    if !text.trim().is_empty() {
        html.push_str(&html_body(text));
    }
    let (mut hosted, mut atts) = (vec![], vec![]);
    for (i, f) in files.iter().enumerate() {
        if f.inline() {
            let n = hosted.len() + 1;
            hosted.push(json!({
                "@microsoft.graph.temporaryId": n.to_string(),
                "contentBytes": base64::engine::general_purpose::STANDARD.encode(f.bytes()?),
                "contentType": f.mime(),
            }));
            html.push_str(&format!(
                "<p><img src=\"../hostedContents/{n}/$value\" alt=\"{}\"></p>",
                esc(&f.name)
            ));
            continue;
        }
        let _ = i;
        let item = upload(f).map_err(|e| format!("Upload of {} failed: {e}", f.name))?;
        let id = item["id"].as_str().ok_or("upload returned no id")?;
        if recipients.is_empty() {
            return Err("No chat members to share the file with".into());
        }
        let link = graph::post(
            &format!("/me/drive/items/{id}/createLink"),
            &json!({
                "type": "view",
                "scope": "users",
                "recipients": recipients.iter().map(|r| json!({"objectId": r})).collect::<Vec<_>>(),
            }),
        )
        .map_err(|e| format!("Could not share {}: {e}", f.name))?;
        let url = link["link"]["webUrl"].as_str().or(item["webUrl"].as_str()).ok_or("no link returned")?;
        let aid = attachment_id(&item);
        html.push_str(&format!("<attachment id=\"{aid}\"></attachment>"));
        atts.push(json!({"id": aid, "contentType": "reference", "contentUrl": url, "name": f.name}));
    }
    let mut body = json!({"body": {"contentType": "html", "content": html}});
    if !hosted.is_empty() {
        body["hostedContents"] = Value::Array(hosted);
    }
    if !atts.is_empty() {
        body["attachments"] = Value::Array(atts);
    }
    graph::post(&format!("/chats/{chat_id}/messages"), &body).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn staged(name: &str, size: u64) -> Staged {
        Staged { name: name.into(), src: Src::Bytes(vec![]), size }
    }

    #[test]
    fn small_images_go_inline_everything_else_is_uploaded() {
        assert!(staged("a.PNG", 1000).inline());
        assert!(staged("a.jpg", INLINE_IMAGE_MAX).inline());
        assert!(!staged("a.jpg", INLINE_IMAGE_MAX + 1).inline()); // too big for hosted content
        assert!(!staged("a.pdf", 10).inline());
        assert!(!staged("noext", 10).inline());
    }

    #[test]
    fn attachment_id_comes_from_etag_guid() {
        let item = json!({"eTag": "\"{1F2E3D4C-0000-4AAA-8BBB-0123456789AB},3\""});
        assert_eq!(attachment_id(&item), "1f2e3d4c00004aaa8bbb0123456789ab");
        assert_eq!(attachment_id(&json!({})).len(), 32); // random fallback
    }

    #[test]
    fn urls_become_links_and_text_is_escaped() {
        assert_eq!(
            html_body("see https://a.com/x?q=1&r=2. ok\nwww.b.org, <b>"),
            "<p>see <a href=\"https://a.com/x?q=1&amp;r=2\">https://a.com/x?q=1&amp;r=2</a>. ok<br>\
             <a href=\"https://www.b.org\">www.b.org</a>, &lt;b&gt;</p>"
        );
        assert_eq!(html_body("https:// alone"), "<p>https:// alone</p>");
    }

    #[test]
    fn message_text_is_escaped() {
        assert_eq!(esc("a<b> & \"c\""), "a&lt;b&gt; &amp; &quot;c&quot;");
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(2048), "2 KB");
        assert_eq!(human_size(3 * 1048576), "3.0 MB");
    }
}
