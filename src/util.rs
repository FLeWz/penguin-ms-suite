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
