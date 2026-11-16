use adw::prelude::*;
use gtk::glib;
use std::cell::RefCell;

thread_local! {
    static TOASTS: RefCell<Option<adw::ToastOverlay>> = const { RefCell::new(None) };
}

pub fn set_toaster(o: &adw::ToastOverlay) {
    TOASTS.with(|t| *t.borrow_mut() = Some(o.clone()));
}

pub fn toast(msg: &str) {
    TOASTS.with(|t| {
        if let Some(o) = t.borrow().as_ref() {
            o.add_toast(adw::Toast::new(msg));
        }
    });
}

/// Run blocking `work` on a thread; deliver result on the GTK main loop.
pub fn run<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
    done: impl FnOnce(T) + 'static,
) {
    let (tx, rx) = async_channel::bounded(1);
    std::thread::spawn(move || {
        let _ = tx.send_blocking(work());
    });
    glib::spawn_future_local(async move {
        if let Ok(v) = rx.recv().await {
            done(v);
        }
    });
}

/// Like `run` but toasts errors and only calls `done` on success.
pub fn run_ok<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, String> + Send + 'static,
    done: impl FnOnce(T) + 'static,
) {
    run(work, move |r| match r {
        Ok(v) => done(v),
        Err(e) => toast(&e),
    });
}

/// Crude HTML -> text (Teams messages).
pub fn strip_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                out.push(' ');
            }
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Message body -> text that keeps line breaks. HTML: <br>, </p>, </div>, <li>, headings
/// and table rows become newlines and source newlines count as spaces. Plain text: newlines
/// are kept. Spaces inside a line are collapsed; at most one blank line in a row.
pub fn message_text(body: &str, content_type: &str) -> String {
    convert(body, content_type, false)
}

// Private-use markers that carry <a href> through the text pipeline.
const LINK_OPEN: char = '\u{E000}';
const LINK_URL_END: char = '\u{E001}';
const LINK_CLOSE: char = '\u{E002}';

/// href of an `<a>` tag, http(s) only.
fn anchor_href(tag: &str) -> Option<String> {
    let h = tag
        .split("href=\"")
        .nth(1)
        .and_then(|r| r.split('"').next())
        .or_else(|| tag.split("href='").nth(1).and_then(|r| r.split('\'').next()))?;
    (h.starts_with("https://") || h.starts_with("http://")).then(|| h.replace(' ', "%20"))
}

fn convert(body: &str, content_type: &str, links: bool) -> String {
    let html = !content_type.eq_ignore_ascii_case("text");
    let body = &body.replace([LINK_OPEN, LINK_URL_END, LINK_CLOSE], "");
    let src = if html { body.replace(['\r', '\n'], " ") } else { body.replace('\r', "") };
    let mut out = String::with_capacity(src.len());
    let mut open_anchor = false;
    if html {
        let mut chars = src.chars();
        while let Some(c) = chars.next() {
            if c != '<' {
                out.push(c);
                continue;
            }
            let mut tag = String::new();
            for t in chars.by_ref() {
                if t == '>' {
                    break;
                }
                tag.push(t);
            }
            let closing = tag.starts_with('/');
            if links && (tag.starts_with("a ") || tag.starts_with("A ") || tag.eq_ignore_ascii_case("/a")) {
                if closing {
                    if open_anchor {
                        out.push(LINK_CLOSE);
                        open_anchor = false;
                    }
                } else if let Some(h) = anchor_href(&tag) {
                    out.push(LINK_OPEN);
                    out.push_str(&h);
                    out.push(LINK_URL_END);
                    open_anchor = true;
                }
            }
            let name: String = tag
                .trim_start_matches('/')
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric())
                .collect::<String>()
                .to_ascii_lowercase();
            if name == "emoji" {
                // Teams emoji are <emoji alt="😀">; keep the character.
                if let Some(a) = tag.split("alt=\"").nth(1).and_then(|r| r.split('"').next()) {
                    out.push_str(a);
                }
            }
            let breaks = match name.as_str() {
                "br" => true,
                "li" => !closing,
                "p" | "div" | "tr" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "blockquote" => closing,
                _ => false,
            };
            if breaks {
                out.push('\n');
            }
        }
    } else {
        out = src;
    }
    let out = out
        .replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&");
    let mut lines: Vec<String> = vec![];
    for l in out.split('\n') {
        let l = l.split_whitespace().collect::<Vec<_>>().join(" ");
        if l.is_empty() && lines.last().is_none_or(|p| p.is_empty()) {
            continue; // collapse blank runs, drop leading blanks
        }
        lines.push(l);
    }
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

/// Escape plain text for Pango markup, turning bare http(s) URLs into links.
fn linkify(seg: &str, out: &mut String) {
    let mut rest = seg;
    while let Some(i) = rest.find("http://").into_iter().chain(rest.find("https://")).min() {
        out.push_str(&glib::markup_escape_text(&rest[..i]));
        let tail = &rest[i..];
        let end = tail
            .find(|c: char| c.is_whitespace() || matches!(c, '<' | '>' | '"') || c == LINK_OPEN || c == LINK_CLOSE)
            .unwrap_or(tail.len());
        let raw = &tail[..end];
        let url = raw.trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']', '}', '\'']);
        // "https://host/path…" is Teams' shortened display text, not a real URL.
        if url.contains('…') || url.len() <= "https://".len() {
            out.push_str(&glib::markup_escape_text(raw));
        } else {
            let esc = glib::markup_escape_text(url);
            out.push_str(&format!("<a href=\"{esc}\">{esc}</a>"));
            out.push_str(&glib::markup_escape_text(&raw[url.len()..]));
        }
        rest = &tail[end..];
    }
    out.push_str(&glib::markup_escape_text(rest));
}

/// Message body as Pango markup: same text and line breaks as `message_text`, with real
/// `<a href>` links kept (label text may differ from the target) and bare URLs linked.
pub fn message_markup(body: &str, content_type: &str) -> String {
    let t = convert(body, content_type, true);
    let (mut out, mut seg, mut in_a) = (String::new(), String::new(), false);
    let mut chars = t.chars();
    while let Some(c) = chars.next() {
        match c {
            LINK_OPEN => {
                linkify(&seg, &mut out);
                seg.clear();
                let url: String = chars.by_ref().take_while(|c| *c != LINK_URL_END).collect();
                out.push_str(&format!("<a href=\"{}\">", glib::markup_escape_text(&url)));
                in_a = true;
            }
            LINK_CLOSE => {
                out.push_str(&glib::markup_escape_text(&seg));
                seg.clear();
                out.push_str("</a>");
                in_a = false;
            }
            _ => seg.push(c),
        }
    }
    if in_a {
        out.push_str(&glib::markup_escape_text(&seg));
        out.push_str("</a>");
    } else {
        linkify(&seg, &mut out);
    }
    out
}

pub fn row(title: &str, sub: &str) -> gtk::Box {
    let b = gtk::Box::new(gtk::Orientation::Vertical, 2);
    b.set_margin_top(6);
    b.set_margin_bottom(6);
    b.set_margin_start(10);
    b.set_margin_end(10);
    for (t, css) in [(title, "heading"), (sub, "dim-label")] {
        let l = gtk::Label::new(Some(t));
        l.set_xalign(0.0);
        l.set_ellipsize(gtk::pango::EllipsizeMode::End);
        l.set_single_line_mode(true);
        l.add_css_class(css);
        b.append(&l);
    }
    b
}

pub fn clear(list: &gtk::ListBox) {
    while let Some(c) = list.first_child() {
        list.remove(&c);
    }
}

pub fn pane(child: &impl IsA<gtk::Widget>) -> gtk::ScrolledWindow {
    gtk::ScrolledWindow::builder()
        .child(child)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .build()
}

#[cfg(test)]
mod tests {
    use super::{message_markup, message_text};

    #[test]
    fn markup_keeps_anchor_targets_and_links_bare_urls() {
        let m = message_markup(
            "<p>see <a href=\"https://a.com/x?q=1&amp;r=2\">this page</a> and https://b.org/y.</p>",
            "html",
        );
        assert_eq!(
            m,
            "see <a href=\"https://a.com/x?q=1&amp;r=2\">this page</a> and <a href=\"https://b.org/y\">https://b.org/y</a>."
        );
    }

    #[test]
    fn markup_escapes_text_and_skips_shortened_links() {
        assert_eq!(message_markup("1 < 2 & 3", "text"), "1 &lt; 2 &amp; 3");
        assert_eq!(message_markup("go https://x.com/very/lo…", "text"), "go https://x.com/very/lo…");
        assert_eq!(message_markup("<a href=\"javascript:alert(1)\">x</a>", "html"), "x");
    }

    #[test]
    fn html_breaks_become_newlines() {
        assert_eq!(message_text("<p>one</p><p>two<br>three</p>", "html"), "one\ntwo\nthree");
        assert_eq!(message_text("a &amp; b\n  c", "html"), "a & b c"); // source newline = space
    }

    #[test]
    fn plain_text_keeps_newlines() {
        assert_eq!(message_text("first\nsecond\n\n\n\nthird  x", "text"), "first\nsecond\n\nthird x");
    }
}
