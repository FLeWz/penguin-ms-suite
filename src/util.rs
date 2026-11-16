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
    let html = !content_type.eq_ignore_ascii_case("text");
    let src = if html { body.replace(['\r', '\n'], " ") } else { body.replace('\r', "") };
    let mut out = String::with_capacity(src.len());
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
            let name: String = tag
                .trim_start_matches('/')
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric())
                .collect::<String>()
                .to_ascii_lowercase();
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
    use super::message_text;

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
