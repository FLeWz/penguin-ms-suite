use super::{attach, finder, links, media, Page};
use crate::relay;
use crate::{graph, util};
use adw::prelude::*;
use serde_json::{json, Value};
use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    rc::Rc,
    time::{Duration, Instant},
};

fn chat_title(c: &Value, me: &str) -> String {
    if let Some(t) = c["topic"].as_str().filter(|t| !t.is_empty()) {
        return t.into();
    }
    let names: Vec<&str> = c["members"]
        .as_array()
        .map(|m| {
            m.iter()
                .filter(|x| x["userId"].as_str() != Some(me))
                .filter_map(|x| x["displayName"].as_str())
                .collect()
        })
        .unwrap_or_default();
    if names.is_empty() {
        "Chat".into()
    } else {
        names.join(", ")
    }
}

fn header(text: &str) -> gtk::ListBoxRow {
    let l = gtk::Label::new(Some(text));
    l.set_xalign(0.0);
    l.add_css_class("caption-heading");
    l.add_css_class("dim-label");
    l.set_margin_top(6);
    l.set_margin_bottom(1);
    l.set_margin_start(8);
    let r = gtk::ListBoxRow::new();
    r.set_child(Some(&l));
    r.set_selectable(false);
    r.set_activatable(false);
    r
}

// Adaptive polling (used while no relay is pushing events).
const POLL_BACKGROUND: Duration = Duration::from_secs(10); // unfocused, or another app tab shown
const POLL_FOCUSED: Duration = Duration::from_secs(5); // Teams visible and window focused: chat list
const POLL_ACTIVE_CHAT: Duration = Duration::from_secs(2); // open chat: newest messages only
const POLL_SAFETY_NET: Duration = Duration::from_secs(60); // chat list while the relay is healthy

/// Local (per-machine) chat preferences, stored in the app config dir.
#[derive(Default)]
struct Prefs {
    fav: HashSet<String>,
    hidden: HashSet<String>,
}

impl Prefs {
    fn path() -> std::path::PathBuf {
        crate::auth::dir().join("teams_prefs.json")
    }
    fn load() -> Self {
        let v: Value = std::fs::read_to_string(Self::path())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or(Value::Null);
        let set = |k: &str| -> HashSet<String> {
            v[k].as_array()
                .into_iter()
                .flatten()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        };
        Prefs { fav: set("favourites"), hidden: set("hidden") }
    }
    fn save(&self) {
        let v = json!({"favourites": self.fav, "hidden": self.hidden});
        let _ = std::fs::create_dir_all(crate::auth::dir());
        let _ = std::fs::write(Self::path(), v.to_string());
    }
    fn toggle(set: &mut HashSet<String>, id: &str) {
        if !set.remove(id) {
            set.insert(id.to_string());
        }
    }
}

/// Compact two-line chat entry: title, then a one-line preview.
fn chat_info(title: &str, preview: &str, unread: bool) -> gtk::Box {
    let b = gtk::Box::new(gtk::Orientation::Vertical, 0);
    b.set_margin_top(2);
    b.set_margin_bottom(2);
    b.set_margin_start(6);
    b.set_margin_end(4);
    for (i, (text, classes)) in [(title, "chat-title"), (preview, "chat-preview dim-label")].into_iter().enumerate() {
        let l = gtk::Label::new(Some(text));
        l.set_xalign(0.0);
        l.set_ellipsize(gtk::pango::EllipsizeMode::End);
        l.set_single_line_mode(true);
        for c in classes.split(' ') {
            l.add_css_class(c);
        }
        if unread && i == 0 {
            l.add_css_class("chat-title-unread");
        }
        b.append(&l);
    }
    b
}

/// Popover with the favourite and hide actions for one chat.
fn menu_popover(
    is_fav: bool,
    is_hidden: bool,
    on_fav: Rc<dyn Fn()>,
    on_hide: Rc<dyn Fn()>,
) -> gtk::Popover {
    let pop = gtk::Popover::new();
    let col = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let item = |label: &str, cb: Rc<dyn Fn()>| {
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
    col.append(&item(if is_fav { "Remove from favourites" } else { "Add to favourites" }, on_fav));
    col.append(&item(if is_hidden { "Unhide chat" } else { "Hide chat" }, on_hide));
    pop.set_child(Some(&col));
    pop
}

/// Small "⋮" button that opens `pop`.
fn chat_menu(pop: &gtk::Popover) -> gtk::MenuButton {
    let mb = gtk::MenuButton::new();
    mb.set_icon_name("view-more-symbolic");
    mb.set_has_frame(false);
    for c in ["chat-menu", "quiet-icon"] {
        mb.add_css_class(c);
    }
    mb.set_valign(gtk::Align::Center);
    mb.set_tooltip_text(Some("Chat actions"));
    mb.set_popover(Some(pop));
    mb
}

/// Unread = the last message is from someone else, is newer than the read marker
/// Teams stores for me (`viewpoint`), and has not been opened here. `fresh` covers
/// chats that got a new message during this session even if the marker is missing.
fn is_unread(
    c: &Value,
    my_id: &str,
    read_local: &HashMap<String, String>,
    fresh: &HashSet<String>,
) -> bool {
    let cid = c["id"].as_str().unwrap_or("");
    let prev = &c["lastMessagePreview"];
    let Some(pid) = prev["id"].as_str() else { return false };
    if prev["from"]["user"]["id"].as_str() == Some(my_id)
        || read_local.get(cid).is_some_and(|r| r == pid)
    {
        return false;
    }
    let server = match c["viewpoint"]["lastMessageReadDateTime"].as_str() {
        Some(read) => {
            let when = |t: &str| finder::parse_time(t).1;
            when(prev["createdDateTime"].as_str().unwrap_or("")) > when(read)
        }
        None => false,
    };
    server || fresh.contains(cid)
}

/// One chat message as a bubble: mine on the right (accent colour), others on the left.
fn bubble(
    who: &str,
    text: &str,
    markup: &str,
    when: &str,
    mine: bool,
    media: &[gtk::Widget],
    link_cards: &[gtk::Widget],
) -> gtk::Box {
    let b = gtk::Box::new(gtk::Orientation::Vertical, 2);
    b.add_css_class("bubble");
    b.add_css_class(if mine { "bubble-me" } else { "bubble-them" });
    b.set_halign(if mine { gtk::Align::End } else { gtk::Align::Start });
    if !mine {
        let n = gtk::Label::new(Some(who));
        n.set_xalign(0.0);
        n.add_css_class("heading");
        b.append(&n);
    }
    for w in media {
        b.append(w);
    }
    if !text.is_empty() {
        let t = gtk::Label::new(None);
        t.set_markup(markup);
        t.set_wrap(true);
        t.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        t.set_max_width_chars(60);
        t.set_xalign(0.0);
        t.set_selectable(true);
        b.append(&t);
    }
    for w in link_cards {
        b.append(w);
    }
    let w = gtk::Label::new(Some(when));
    w.add_css_class("caption");
    w.add_css_class("dim-label");
    w.add_css_class("bubble-time");
    w.set_xalign(1.0);
    b.append(&w);

    let outer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    outer.set_margin_top(3);
    outer.set_margin_bottom(3);
    outer.set_margin_start(12);
    outer.set_margin_end(12);
    // Spacer keeps bubbles off the far edge on the opposite side.
    outer.set_halign(gtk::Align::Fill);
    b.set_hexpand(true);
    outer.append(&b);
    outer
}

/// Scroll the message list so widget `w` (a message row) is near the top third.
fn scroll_to(list: &gtk::ListBox, adj: &gtk::Adjustment, w: &gtk::Widget) -> bool {
    let Some(row) = w.parent() else { return false };
    let Some(b) = row.compute_bounds(list) else { return false };
    adj.set_value((b.y() as f64 - adj.page_size() / 3.0).max(0.0));
    true
}

/// First open of a chat with images or link cards: keep the list invisible while they
/// arrive and the layout settles (placeholders resize, cards appear), then fade it in.
/// Without this the first view visibly jumps around. Gives up waiting after 900 ms.
fn reveal_when_settled(
    scroll: &gtk::ScrolledWindow,
    media: &Rc<media::Media>,
    links: &Rc<links::Links>,
    epoch: &Rc<Cell<u32>>,
) {
    scroll.set_opacity(0.0);
    epoch.set(epoch.get() + 1);
    let g = epoch.get();
    let start = Instant::now();
    let (scroll, media, links, epoch) = (scroll.clone(), media.clone(), links.clone(), epoch.clone());
    gtk::glib::timeout_add_local(Duration::from_millis(50), move || {
        if epoch.get() != g {
            return gtk::glib::ControlFlow::Break; // another chat took over
        }
        let t = start.elapsed();
        let settled = t >= Duration::from_millis(150) && !media.busy() && !links.busy();
        if settled || t >= Duration::from_millis(900) {
            scroll.set_opacity(1.0);
            return gtk::glib::ControlFlow::Break;
        }
        gtk::glib::ControlFlow::Continue
    });
}

pub fn build() -> Page {
    static CSS: std::sync::Once = std::sync::Once::new();
    CSS.call_once(|| {
        let p = gtk::CssProvider::new();
        p.load_from_data(
            ".search-hit { background: alpha(@accent_bg_color, 0.25); border-radius: 6px; }
             .message-list row, .message-list row:hover { background: none; }
             .bubble { border-radius: 14px; padding: 6px 10px; }
             .bubble-me { background: @accent_bg_color; color: @accent_fg_color; }
             .bubble-them { background: alpha(currentColor, 0.09); }
             .msg-scroll { transition: opacity 150ms ease-in; }
             .chat-list row { min-height: 0; padding: 0 2px; margin: 2px 4px; border-radius: 6px; }
             .chat-title { font-size: 0.92em; }
             .chat-preview { font-size: 0.75em; }
             .chat-title-unread { font-weight: 700; }
             .chat-menu { opacity: 0; margin-right: 14px; } /* clear of the overlay scrollbar */
             .chat-menu button { min-height: 0; min-width: 0; padding: 2px; border-radius: 6px; }
             .chat-menu image { -gtk-icon-size: 12px; }
             .chat-list row:hover .chat-menu, .chat-list row:selected .chat-menu, .chat-menu:checked { opacity: 1; }
             .thumb { transition: opacity 250ms ease-in; }
             .thumb-placeholder { background: alpha(currentColor, 0.07); border-radius: 8px; }
             .attach-chip { background: alpha(currentColor, 0.09); border-radius: 8px; padding: 2px 4px 2px 8px; }
             .drop-active { background: alpha(@accent_bg_color, 0.18); }
             .bubble-me link, .bubble-me link:visited { color: @accent_fg_color; text-decoration: underline; }
             .play-badge { background: alpha(black, 0.55); color: white; border-radius: 999px; padding: 8px; }
             .link-card { background: alpha(currentColor, 0.08); border-left: 3px solid @accent_color; border-radius: 8px; padding: 6px 10px; margin-top: 4px; }
             .bubble-me .bubble-time { color: alpha(@accent_fg_color, 0.75); }",
        );
        if let Some(d) = gtk::gdk::Display::default() {
            gtk::style_context_add_provider_for_display(&d, &p, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
        }
    });
    let chats = gtk::ListBox::new();
    chats.add_css_class("navigation-sidebar");
    chats.add_css_class("chat-list");
    let msgs = gtk::ListBox::new();
    msgs.set_selection_mode(gtk::SelectionMode::None);
    msgs.add_css_class("message-list");
    let entry = gtk::Entry::new();
    entry.set_placeholder_text(Some("Type a message…"));
    entry.set_margin_top(6);
    entry.set_margin_bottom(6);
    entry.set_margin_start(6);
    entry.set_margin_end(6);
    entry.set_hexpand(true);
    // Type ":smile" for emoji completion; Ctrl+. also opens GTK's chooser in the entry.
    entry.set_property("enable-emoji-completion", true);
    let emoji_btn = gtk::MenuButton::new();
    emoji_btn.set_icon_name("face-smile-symbolic");
    emoji_btn.add_css_class("flat");
    emoji_btn.add_css_class("quiet-icon");
    emoji_btn.set_tooltip_text(Some("Insert emoji"));
    emoji_btn.set_valign(gtk::Align::Center);
    let chooser = gtk::EmojiChooser::new();
    emoji_btn.set_popover(Some(&chooser));
    {
        let entry = entry.clone();
        chooser.connect_emoji_picked(move |_, emoji| {
            let mut pos = entry.position();
            entry.insert_text(emoji, &mut pos);
            entry.set_position(pos);
            entry.grab_focus_without_selecting();
        });
    }
    let compose = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    let tray = attach::Tray::new();
    let attach_btn = gtk::Button::from_icon_name("mail-attachment-symbolic");
    attach_btn.add_css_class("flat");
    attach_btn.add_css_class("quiet-icon");
    attach_btn.set_tooltip_text(Some("Attach files (or drop / paste them)"));
    attach_btn.set_valign(gtk::Align::Center);
    {
        let (tray, entry) = (tray.clone(), entry.clone());
        attach_btn.connect_clicked(move |_| {
            attach::pick_files(&tray, entry.root().and_downcast::<gtk::Window>());
        });
    }
    attach::install_paste(&entry, &tray);
    compose.append(&attach_btn);
    compose.append(&entry);
    compose.append(&emoji_btn);
    compose.set_margin_end(6);

    let right = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let msg_scroll = util::pane(&msgs);
    msg_scroll.add_css_class("msg-scroll");
    // Follow mode: while set, the view stays pinned to the newest message every time the
    // content or viewport size changes (wrapped labels and the final viewport height settle
    // over several layout passes, so a timed pin can land short). It turns off when the user
    // scrolls up and back on when they return to the bottom. `settling` ignores the scroll
    // changes caused by rebuilding the list.
    let stick = Rc::new(Cell::new(true));
    let settling = Rc::new(Cell::new(false));
    // Message id to bring into view after loading (from the finder), and the rows by id.
    let jump: Rc<RefCell<Option<String>>> = Default::default();
    let rows: Rc<RefCell<HashMap<String, gtk::Widget>>> = Default::default();
    // Scroll position held across a live reload when the user is reading history.
    let hold: Rc<Cell<Option<f64>>> = Default::default();
    {
        let (stick, jump, rows, msgs, hold) =
            (stick.clone(), jump.clone(), rows.clone(), msgs.clone(), hold.clone());
        let programmatic = Rc::new(Cell::new(false));
        {
            let (stick, settling, programmatic) = (stick.clone(), settling.clone(), programmatic.clone());
            msg_scroll.vadjustment().connect_value_changed(move |a| {
                if !programmatic.get() && !settling.get() {
                    stick.set(a.upper() - a.page_size() - a.value() < 24.0);
                }
            });
        }
        // The scroll position is applied from an idle callback, never inside the adjustment's
        // `changed` signal: that signal fires while the viewport is allocating, so a value set
        // there updates the adjustment but the viewport keeps drawing the stale offset until
        // something else triggers a layout (the "latest message hidden until I scroll" bug).
        let pending = Rc::new(Cell::new(false));
        msg_scroll.vadjustment().connect_changed(move |a| {
            if pending.replace(true) {
                return; // one deferred update covers any number of size changes
            }
            let (a, pending, stick, jump, rows, msgs, hold, programmatic) = (
                a.clone(), pending.clone(), stick.clone(), jump.clone(), rows.clone(),
                msgs.clone(), hold.clone(), programmatic.clone(),
            );
            gtk::glib::idle_add_local_once(move || {
                pending.set(false);
                let target = jump.borrow().clone();
                if let Some(w) = target.and_then(|id| rows.borrow().get(&id).cloned()) {
                    scroll_to(&msgs, &a, &w); // keep it in view while labels settle
                } else if let Some(v) = hold.get() {
                    programmatic.set(true);
                    a.set_value(v); // keep the reading position across a live reload
                    programmatic.set(false);
                } else if stick.get() {
                    programmatic.set(true);
                    a.set_value(a.upper() - a.page_size());
                    programmatic.set(false);
                }
            });
        });
    }
    let apply_jump: Rc<dyn Fn()> = {
        let (jump, rows, msgs, adj) = (jump.clone(), rows.clone(), msgs.clone(), msg_scroll.vadjustment());
        Rc::new(move || {
            let Some(id) = jump.borrow().clone() else { return };
            let Some(w) = rows.borrow().get(&id).cloned() else { return };
            if let Some(r) = w.parent() {
                r.add_css_class("search-hit");
            }
            scroll_to(&msgs, &adj, &w);
            let jump = jump.clone();
            gtk::glib::timeout_add_local_once(std::time::Duration::from_millis(700), move || {
                *jump.borrow_mut() = None;
            });
        })
    };
    // Newest message id per chat as last loaded; the fast active-chat poll compares against it.
    let live_last: Rc<RefCell<HashMap<String, String>>> = Default::default();
    // Chat whose messages are currently shown (to tell a chat switch from a live reload).
    let shown: Rc<RefCell<Option<String>>> = Default::default();
    let reveal_gen: Rc<Cell<u32>> = Default::default();
    // Backoff (seconds) added to poll intervals after errors, e.g. throttling.
    let penalty: Rc<Cell<u32>> = Default::default();
    let list_busy = Rc::new(Cell::new(false));
    let chat_busy = Rc::new(Cell::new(false));
    let me: Rc<RefCell<String>> = Default::default(); // my user id, set by the chat list poll
    let media = media::Media::new();
    media.attach_viewport(&msg_scroll);
    let link_previews = links::Links::new();
    let index: finder::Index = Default::default();
    let titles: Rc<RefCell<HashMap<String, String>>> = Default::default();
    // chat id -> member user ids other than me (recipients when sharing uploaded files)
    let chat_members: Rc<RefCell<HashMap<String, Vec<String>>>> = Default::default();
    // "Jump to newest" button, shown while scrolled up. Animates to the bottom and turns
    // follow mode back on.
    let jump_btn = gtk::Button::from_icon_name("go-down-symbolic");
    jump_btn.add_css_class("circular");
    jump_btn.add_css_class("osd");
    jump_btn.set_tooltip_text(Some("Scroll to newest message"));
    let jump_rev = gtk::Revealer::new();
    jump_rev.set_child(Some(&jump_btn));
    jump_rev.set_transition_type(gtk::RevealerTransitionType::Crossfade);
    jump_rev.set_transition_duration(150);
    jump_rev.set_halign(gtk::Align::End);
    jump_rev.set_valign(gtk::Align::End);
    jump_rev.set_margin_end(16);
    jump_rev.set_margin_bottom(12);
    {
        let adj = msg_scroll.vadjustment();
        let rev = jump_rev.clone();
        let update = move |a: &gtk::Adjustment| {
            rev.set_reveal_child(a.upper() - a.page_size() - a.value() > 200.0);
        };
        adj.connect_value_changed(update.clone());
        adj.connect_changed(update);
    }
    {
        let (adj, scroll, stick) = (msg_scroll.vadjustment(), msg_scroll.clone(), stick.clone());
        jump_btn.connect_clicked(move |_| {
            let target = adw::CallbackAnimationTarget::new({
                let adj = adj.clone();
                move |v| adj.set_value(v)
            });
            let anim = adw::TimedAnimation::new(
                &scroll,
                adj.value(),
                adj.upper() - adj.page_size(),
                250,
                target,
            );
            anim.set_easing(adw::Easing::EaseOutCubic);
            let (adj, stick) = (adj.clone(), stick.clone());
            anim.connect_done(move |_| {
                adj.set_value(adj.upper() - adj.page_size()); // content may have grown meanwhile
                stick.set(true);
            });
            anim.play();
        });
    }
    let msg_overlay = gtk::Overlay::new();
    msg_overlay.set_child(Some(&msg_scroll));
    msg_overlay.add_overlay(&jump_rev);
    msg_overlay.set_vexpand(true);
    right.append(&msg_overlay);
    right.append(&tray.widget);
    right.append(&compose);
    attach::install_drop(&right, &tray);
    right.set_hexpand(true);
    let paned = gtk::Paned::new(gtk::Orientation::Horizontal);
    let show_hidden = gtk::ToggleButton::with_label("Show hidden");
    show_hidden.add_css_class("flat");
    show_hidden.set_margin_start(6);
    show_hidden.set_margin_end(6);
    show_hidden.set_margin_top(4);
    let search = gtk::SearchEntry::new();
    search.set_placeholder_text(Some("Find a person to chat with…"));
    search.set_margin_start(6);
    search.set_margin_end(6);
    search.set_margin_top(6);
    let results = gtk::ListBox::new();
    results.add_css_class("navigation-sidebar");
    results.set_selection_mode(gtk::SelectionMode::None);
    let results_scroll = gtk::ScrolledWindow::builder()
        .child(&results)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .max_content_height(220)
        .propagate_natural_height(true)
        .visible(false)
        .build();
    let find_btn = gtk::Button::with_label("Search messages  (Ctrl+K)");
    find_btn.add_css_class("flat");
    find_btn.set_margin_start(6);
    find_btn.set_margin_end(6);
    let left = gtk::Box::new(gtk::Orientation::Vertical, 0);
    left.append(&find_btn);
    left.append(&search);
    left.append(&results_scroll);
    left.append(&show_hidden);
    left.append(&util::pane(&chats));
    left.set_size_request(260, -1);
    paned.set_start_child(Some(&left));
    paned.set_end_child(Some(&right));
    util::remember_paned(&paned, "teams_list_width", 280);
    paned.set_shrink_start_child(false);
    // Fixed-width list: window resizing changes only the right pane. The divider can still be dragged.
    paned.set_resize_start_child(false);
    paned.set_resize_end_child(true);

    let ids: Rc<RefCell<Vec<String>>> = Default::default();
    // Suppresses the selection handler while the list is rebuilt programmatically.
    let quiet = Rc::new(std::cell::Cell::new(false));
    let current: Rc<RefCell<Option<String>>> = Default::default();

    let load_msgs = {
        let (msgs, current, stick, index, titles, jump, rows, apply_jump) = (
            msgs.clone(), current.clone(), stick.clone(), index.clone(), titles.clone(),
            jump.clone(), rows.clone(), apply_jump.clone(),
        );
        let (live_last, hold, shown, adj) =
            (live_last.clone(), hold.clone(), shown.clone(), msg_scroll.vadjustment());
        let settling_c = settling.clone();
        let me = me.clone();
        let media_c = media.clone();
        let links_c = link_previews.clone();
        let (scroll_c, gen_c) = (msg_scroll.clone(), reveal_gen.clone());
        Rc::new(move || {
            let Some(id) = current.borrow().clone() else { return };
            let want = jump.borrow().clone();
            let title = titles.borrow().get(&id).cloned().unwrap_or_default();
            let (msgs, stick, index, jump, rows, apply_jump, current) = (
                msgs.clone(), stick.clone(), index.clone(), jump.clone(), rows.clone(),
                apply_jump.clone(), current.clone(),
            );
            let (live_last, hold, shown, adj) =
                (live_last.clone(), hold.clone(), shown.clone(), adj.clone());
            let settling = settling_c.clone();
            let my_id = me.borrow().clone();
            let media = media_c.clone();
            let link_previews = links_c.clone();
            let (scroll_w, reveal_gen) = (scroll_c.clone(), gen_c.clone());
            let cid = id.clone();
            util::run_ok(
                move || {
                    // One page normally; when jumping to an older message, page back until found.
                    let mut path = format!("/me/chats/{id}/messages?$top=50");
                    let mut all: Vec<Value> = vec![];
                    for _ in 0..6 {
                        let v = graph::get(&path)?;
                        all.extend(v["value"].as_array().cloned().unwrap_or_default());
                        let found = want.as_ref().is_none_or(|w| all.iter().any(|m| m["id"].as_str() == Some(w)));
                        match v["@odata.nextLink"].as_str() {
                            Some(u) if !found => path = u.strip_prefix(graph::BASE).unwrap_or(u).to_string(),
                            _ => break,
                        }
                    }
                    Ok(all)
                },
                move |all| {
                    if current.borrow().as_deref() != Some(cid.as_str()) {
                        return; // user switched chats meanwhile
                    }
                    // Follow new messages only if the user is at the bottom or just opened this chat;
                    // otherwise keep the reading position.
                    let switched = shown.borrow().as_deref() != Some(cid.as_str());
                    let at_bottom = adj.upper() - adj.page_size() - adj.value() < 40.0;
                    settling.set(true);
                    gtk::glib::timeout_add_local_once(Duration::from_millis(600), {
                        let settling = settling.clone();
                        move || settling.set(false)
                    });
                    if jump.borrow().is_some() {
                        stick.set(false); // the jump positions the view
                    } else if switched || stick.get() || at_bottom {
                        stick.set(true);
                    } else {
                        stick.set(false);
                        hold.set(Some(adj.value()));
                        gtk::glib::timeout_add_local_once(Duration::from_millis(600), move || {
                            hold.set(None)
                        });
                    }
                    *shown.borrow_mut() = Some(cid.clone());
                    if let Some(newest) = all.first().and_then(|m| m["id"].as_str()) {
                        live_last.borrow_mut().insert(cid.clone(), newest.to_string());
                    }
                    finder::add(&index, finder::parse(&cid, &title, &all));
                    util::clear(&msgs);
                    rows.borrow_mut().clear();
                    let mut async_content = false; // images or link cards that load after the rows exist
                    let mut items = all;
                    // The API orders by last-modified (edited messages jump ahead), so sort by
                    // creation time, oldest first, for display.
                    items.sort_by(|a, b| {
                        a["createdDateTime"].as_str().unwrap_or("").cmp(b["createdDateTime"].as_str().unwrap_or(""))
                    });
                    for m in items {
                        let who = m["from"]["user"]["displayName"].as_str().unwrap_or("system");
                        let text = util::message_text(
                            m["body"]["content"].as_str().unwrap_or(""),
                            m["body"]["contentType"].as_str().unwrap_or("html"),
                        );
                        let media_widgets = media.widgets(&m);
                        let link_cards = link_previews.widgets(&m, &text);
                        if text.is_empty() && media_widgets.is_empty() {
                            continue;
                        }
                        async_content |= !media_widgets.is_empty() || !link_cards.is_empty();
                        let mine = !my_id.is_empty()
                            && m["from"]["user"]["id"].as_str() == Some(my_id.as_str());
                        let when = finder::parse_time(m["createdDateTime"].as_str().unwrap_or("")).0;
                        let markup = util::message_markup(
                            m["body"]["content"].as_str().unwrap_or(""),
                            m["body"]["contentType"].as_str().unwrap_or("html"),
                        );
                        let r = bubble(who, &text, &markup, &when, mine, &media_widgets, &link_cards);
                        if let Some(mid) = m["id"].as_str() {
                            rows.borrow_mut().insert(mid.to_string(), r.clone().upcast());
                        }
                        msgs.append(&r);
                    }
                    if switched {
                        if async_content {
                            reveal_when_settled(&scroll_w, &media, &link_previews, &reveal_gen);
                        } else {
                            reveal_gen.set(reveal_gen.get() + 1); // cancel a pending reveal
                            scroll_w.set_opacity(1.0);
                        }
                    }
                    apply_jump();
                },
            );
        })
    };
    {
        let (ids, current, load_msgs, quiet) =
            (ids.clone(), current.clone(), load_msgs.clone(), quiet.clone());
        chats.connect_row_selected(move |_, row| {
            let Some(row) = row else { return };
            if quiet.get() {
                return;
            }
            let id = ids.borrow().get(row.index() as usize).cloned().filter(|i| !i.is_empty());
            if id.is_none() {
                return;
            }
            *current.borrow_mut() = id;
            load_msgs();
        });
    }
    {
        let (current, load_msgs, stick, tray, chat_members, attach_btn) = (
            current.clone(), load_msgs.clone(), stick.clone(), tray.clone(),
            chat_members.clone(), attach_btn.clone(),
        );
        let sending = Rc::new(Cell::new(false));
        entry.connect_activate(move |e| {
            if sending.get() {
                return;
            }
            let text = e.text().to_string();
            let Some(id) = current.borrow().clone() else { return };
            if text.trim().is_empty() && tray.is_empty() {
                return;
            }
            stick.set(true); // sending always follows to the newest message
            if tray.is_empty() {
                e.set_text("");
                let load_msgs = load_msgs.clone();
                util::run_ok(
                    move || graph::post(&format!("/me/chats/{id}/messages"), &json!({"body": {"content": text}})),
                    move |_| load_msgs(),
                );
                return;
            }
            // With attachments: upload, share, send. Keep text and files if it fails.
            sending.set(true);
            e.set_sensitive(false);
            attach_btn.set_sensitive(false);
            util::toast("Sending attachments…");
            let files = tray.take();
            let recipients = chat_members.borrow().get(&id).cloned().unwrap_or_default();
            let (e, attach_btn, tray, load_msgs, sending) =
                (e.clone(), attach_btn.clone(), tray.clone(), load_msgs.clone(), sending.clone());
            util::run(
                move || {
                    let r = attach::send(&id, &text, &files, &recipients);
                    (r, files)
                },
                move |(r, files)| {
                    sending.set(false);
                    e.set_sensitive(true);
                    attach_btn.set_sensitive(true);
                    match r {
                        Ok(()) => {
                            e.set_text("");
                            load_msgs();
                        }
                        Err(err) => {
                            tray.restore(files);
                            util::toast(&err);
                        }
                    }
                    e.grab_focus();
                },
            );
        });
    }

    // Latest message id seen per chat, to detect new arrivals. Empty until first load.
    let seen: Rc<RefCell<HashMap<String, String>>> = Default::default();
    // False until the first successful poll: that one only records state, no notifications.
    let primed = Rc::new(std::cell::Cell::new(false));
    let signature: Rc<RefCell<String>> = Default::default();
    let cache: Rc<RefCell<(String, Vec<Value>)>> = Default::default(); // (my id, chats)
    let prefs = Rc::new(RefCell::new(Prefs::load()));
    // Chats just created from search: they have no messages yet, so the chat
    // list API may not return them. Kept until the server lists them.
    let pending: Rc<RefCell<Vec<Value>>> = Default::default();
    // Lets row buttons re-render the list (render refers to itself through this cell).
    let render_cell: Rc<RefCell<Option<Rc<dyn Fn()>>>> = Default::default();
    // Unread tracking: chat -> last message id opened here; chats with new messages this session.
    let read_local: Rc<RefCell<HashMap<String, String>>> = Default::default();
    let fresh: Rc<RefCell<HashSet<String>>> = Default::default();

    let render: Rc<dyn Fn()> = {
        let (chats, ids, current, quiet, cache, prefs, render_cell, show_hidden, titles) = (
            chats.clone(), ids.clone(), current.clone(), quiet.clone(), cache.clone(),
            prefs.clone(), render_cell.clone(), show_hidden.clone(), titles.clone(),
        );
        let (read_local, fresh) = (read_local.clone(), fresh.clone());
        let members_c = chat_members.clone();
        Rc::new(move || {
            let (my_id, items) = cache.borrow().clone();
            {
                let mut m = members_c.borrow_mut();
                for c in &items {
                    let ids: Vec<String> = c["members"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|x| x["userId"].as_str())
                        .filter(|u| *u != my_id)
                        .map(String::from)
                        .collect();
                    m.insert(c["id"].as_str().unwrap_or("").to_string(), ids);
                }
            }
            {
                let mut t = titles.borrow_mut();
                for c in &items {
                    t.insert(c["id"].as_str().unwrap_or("").to_string(), chat_title(c, &my_id));
                }
            }
            let p = prefs.borrow();
            let one = |c: &Value| c["chatType"].as_str() == Some("oneOnOne");
            let id_of = |c: &Value| c["id"].as_str().unwrap_or("").to_string();
            let vis = |c: &&Value| !p.hidden.contains(&id_of(c));
            let sections: Vec<(&str, Vec<&Value>)> = vec![
                ("FAVOURITES", items.iter().filter(vis).filter(|c| p.fav.contains(&id_of(c))).collect()),
                ("GROUPS", items.iter().filter(vis).filter(|c| !p.fav.contains(&id_of(c)) && !one(c)).collect()),
                ("PEOPLE", items.iter().filter(vis).filter(|c| !p.fav.contains(&id_of(c)) && one(c)).collect()),
                (
                    "HIDDEN",
                    if show_hidden.is_active() {
                        items.iter().filter(|c| p.hidden.contains(&id_of(c))).collect()
                    } else {
                        vec![]
                    },
                ),
            ];

            quiet.set(true);
            util::clear(&chats);
            let mut order: Vec<String> = vec![];
            for (label, sel) in sections {
                if sel.is_empty() {
                    continue;
                }
                chats.append(&header(label));
                order.push(String::new());
                for c in sel {
                    let cid = id_of(c);
                    let prev = util::strip_html(c["lastMessagePreview"]["body"]["content"].as_str().unwrap_or(""));
                    let is_fav = p.fav.contains(&cid);
                    let is_hidden = p.hidden.contains(&cid);

                    let unread = is_unread(c, &my_id, &read_local.borrow(), &fresh.borrow());
                    let line = gtk::Box::new(gtk::Orientation::Horizontal, 0);
                    // Green dot; kept (transparent) when read so titles do not shift.
                    let dot = gtk::Image::from_icon_name("media-record-symbolic");
                    dot.add_css_class("success");
                    dot.set_margin_start(6);
                    dot.set_opacity(if unread { 1.0 } else { 0.0 });
                    if unread {
                        dot.set_tooltip_text(Some("Unread"));
                    }
                    line.append(&dot);
                    let info = chat_info(&chat_title(c, &my_id), &prev, unread);
                    info.set_hexpand(true);
                    info.set_valign(gtk::Align::Center);
                    dot.set_valign(gtk::Align::Center);
                    line.set_valign(gtk::Align::Center);
                    line.append(&info);

                    let toggle = {
                        let (prefs, render_cell, cid) = (prefs.clone(), render_cell.clone(), cid.clone());
                        Rc::new(move |fav: bool| {
                            {
                                let mut p = prefs.borrow_mut();
                                if fav {
                                    Prefs::toggle(&mut p.fav, &cid);
                                } else {
                                    Prefs::toggle(&mut p.hidden, &cid);
                                }
                                p.save();
                            }
                            let r = render_cell.borrow().clone();
                            if let Some(r) = r {
                                // Defer: the menu lives in a row that render() destroys.
                                gtk::glib::idle_add_local_once(move || r());
                            }
                        })
                    };
                    let (t1, t2) = (toggle.clone(), toggle);
                    let fav_cb: Rc<dyn Fn()> = Rc::new(move || t1(true));
                    let hide_cb: Rc<dyn Fn()> = Rc::new(move || t2(false));
                    let menu = chat_menu(&menu_popover(is_fav, is_hidden, fav_cb.clone(), hide_cb.clone()));
                    line.append(&menu);
                    // Right click: the same menu, opened at the pointer.
                    let ctx = menu_popover(is_fav, is_hidden, fav_cb, hide_cb);
                    ctx.set_parent(&line);
                    ctx.set_has_arrow(false);
                    ctx.set_halign(gtk::Align::Start);
                    let rc = gtk::GestureClick::new();
                    rc.set_button(3);
                    {
                        let ctx = ctx.clone();
                        rc.connect_pressed(move |_, _, x, y| {
                            ctx.set_pointing_to(Some(&gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
                            ctx.popup();
                        });
                    }
                    line.add_controller(rc);
                    // A popover parented to a widget must be unparented before the widget dies.
                    line.connect_destroy(move |_| ctx.unparent());
                    chats.append(&line);
                    order.push(cid);
                }
            }
            // Keep the open chat selected.
            let cur = current.borrow().clone();
            if let Some(i) = cur.and_then(|c| order.iter().position(|o| *o == c)) {
                chats.select_row(chats.row_at_index(i as i32).as_ref());
            }
            *ids.borrow_mut() = order;
            quiet.set(false);
        })
    };
    *render_cell.borrow_mut() = Some(render.clone());
    {
        let render = render.clone();
        show_hidden.connect_toggled(move |_| render());
    }

    let do_refresh: Rc<dyn Fn(bool)> = {
        let pending_c = pending.clone();
        let primed_c = primed.clone();
        let fresh_c = fresh.clone();
        let busy_c = list_busy.clone();
        let pen_c = penalty.clone();
        let (chats, current, load_msgs, seen, me, signature, cache, prefs, render) = (
            chats.clone(), current.clone(), load_msgs.clone(), seen.clone(), me.clone(),
            signature.clone(), cache.clone(), prefs.clone(), render.clone(),
        );
        Rc::new(move |polling: bool| {
            let pending = pending_c.clone();
            let primed = primed_c.clone();
            let fresh = fresh_c.clone();
            let (list_busy, penalty) = (busy_c.clone(), pen_c.clone());
            list_busy.set(true);
            let (chats, current, load_msgs, seen, me2, signature, cache, prefs, render) = (
                chats.clone(), current.clone(), load_msgs.clone(), seen.clone(), me.clone(),
                signature.clone(), cache.clone(), prefs.clone(), render.clone(),
            );
            let known_me = me.borrow().clone();
            util::run(
                move || {
                    let me = if known_me.is_empty() {
                        graph::get("/me?$select=id")?["id"].as_str().unwrap_or("").to_string()
                    } else {
                        known_me
                    };
                    let v = graph::get("/me/chats?$expand=members&$top=50&$orderby=lastMessagePreview/createdDateTime%20desc")?;
                    Ok::<_, String>((me, v))
                },
                move |r| {
                    list_busy.set(false);
                    let (my_id, v) = match r {
                        Ok(x) => x,
                        Err(e) => {
                            penalty.set((penalty.get() * 2).clamp(5, 60)); // back off, e.g. on 429
                            if !polling {
                                util::toast(&e); // polling stays silent
                            }
                            return;
                        }
                    };
                    penalty.set(0);
                    *me2.borrow_mut() = my_id.clone();
                    let mut items = v["value"].as_array().cloned().unwrap_or_default();
                    {
                        let mut pend = pending.borrow_mut();
                        pend.retain(|p| !items.iter().any(|c| c["id"] == p["id"]));
                        for p in pend.iter() {
                            items.insert(0, p.clone());
                        }
                    }

                    // New-message detection.
                    let first_load = !primed.get();
                    let mut new_in_current = false;
                    for c in &items {
                        let cid = c["id"].as_str().unwrap_or("");
                        let mid = c["lastMessagePreview"]["id"].as_str().unwrap_or("");
                        if mid.is_empty() {
                            continue;
                        }
                        let changed = seen.borrow().get(cid).map(|o| o != mid).unwrap_or(!first_load);
                        seen.borrow_mut().insert(cid.to_string(), mid.to_string());
                        if !changed {
                            continue;
                        }
                        let from = &c["lastMessagePreview"]["from"]["user"];
                        if from["id"].as_str() == Some(my_id.as_str()) {
                            continue; // my own message
                        }
                        let is_current = current.borrow().as_deref() == Some(cid);
                        new_in_current |= is_current;
                        if !is_current {
                            fresh.borrow_mut().insert(cid.to_string());
                        }
                        if prefs.borrow().hidden.contains(cid) {
                            continue; // hidden chats stay silent
                        }
                        let focused = chats
                            .root()
                            .and_downcast::<gtk::Window>()
                            .map(|w| w.is_active())
                            .unwrap_or(false);
                        if !(is_current && focused) {
                            let n = gtk::gio::Notification::new(&chat_title(c, &my_id));
                            let who = from["displayName"].as_str().unwrap_or("");
                            let text = util::strip_html(
                                c["lastMessagePreview"]["body"]["content"].as_str().unwrap_or(""),
                            );
                            n.set_body(Some(&if who.is_empty() { text } else { format!("{who}: {text}") }));
                            n.set_default_action("app.raise");
                            if let Some(app) = gtk::gio::Application::default() {
                                app.send_notification(Some(&format!("chat-{cid}")), &n);
                            }
                        }
                    }

                    primed.set(true);

                    // Re-render only when something visible changed.
                    let sig: String = items
                        .iter()
                        .map(|c| format!("{}|{}", c["id"], c["lastMessagePreview"]["id"]))
                        .collect::<Vec<_>>()
                        .join(",");
                    if *signature.borrow() != sig {
                        *signature.borrow_mut() = sig;
                        *cache.borrow_mut() = (my_id, items);
                        render();
                    }
                    if new_in_current {
                        load_msgs();
                    }
                },
            );
        })
    };

    // People search: type a name, pick a result, a 1:1 chat is created (or reused).
    let found: Rc<RefCell<Vec<Value>>> = Default::default();
    let generation = Rc::new(std::cell::Cell::new(0u32));
    {
        let (results, results_scroll, found, generation, me) = (
            results.clone(), results_scroll.clone(), found.clone(), generation.clone(), me.clone(),
        );
        search.connect_search_changed(move |e| {
            let q = e.text().trim().to_string();
            generation.set(generation.get() + 1);
            let g = generation.get();
            if q.chars().count() < 2 {
                util::clear(&results);
                results_scroll.set_visible(false);
                return;
            }
            let (results, results_scroll, found, generation, me) = (
                results.clone(), results_scroll.clone(), found.clone(), generation.clone(), me.clone(),
            );
            gtk::glib::timeout_add_local_once(std::time::Duration::from_millis(300), move || {
                if generation.get() != g {
                    return; // typed more since; a newer search is coming
                }
                let lit = q.replace('\'', "''");
                let path = format!(
                    "/users?$filter={}&$top=10&$select=id,displayName,mail",
                    graph::enc(&format!(
                        "startswith(displayName,'{lit}') or startswith(mail,'{lit}') or startswith(surname,'{lit}')"
                    ))
                );
                util::run(
                    move || graph::get(&path),
                    move |r| {
                        if generation.get() != g {
                            return;
                        }
                        let v = match r {
                            Ok(v) => v,
                            Err(e) => return util::toast(&e),
                        };
                        let my_id = me.borrow().clone();
                        let users: Vec<Value> = v["value"]
                            .as_array()
                            .cloned()
                            .unwrap_or_default()
                            .into_iter()
                            .filter(|u| u["id"].as_str() != Some(my_id.as_str()))
                            .collect();
                        util::clear(&results);
                        if users.is_empty() {
                            let row = util::row("No people found", "");
                            results.append(&row);
                        }
                        for u in &users {
                            results.append(&util::row(
                                u["displayName"].as_str().unwrap_or("?"),
                                u["mail"].as_str().unwrap_or(""),
                            ));
                        }
                        *found.borrow_mut() = users;
                        results_scroll.set_visible(true);
                    },
                );
            });
        });
    }
    let results_conn = results.clone();
    {
        let (found, me, cache, pending, prefs, current, render, load_msgs, search, results, results_scroll) = (
            found.clone(), me.clone(), cache.clone(), pending.clone(), prefs.clone(), current.clone(),
            render.clone(), load_msgs.clone(), search.clone(), results.clone(), results_scroll.clone(),
        );
        results_conn.connect_row_activated(move |_, row| {
            let Some(user) = found.borrow().get(row.index() as usize).cloned() else { return };
            let uid = user["id"].as_str().unwrap_or("").to_string();
            let name = user["displayName"].as_str().unwrap_or("").to_string();
            let my_id = me.borrow().clone();
            if uid.is_empty() || my_id.is_empty() {
                return util::toast("Not ready yet, try again in a moment");
            }
            let bind = |id: &str| format!("https://graph.microsoft.com/v1.0/users('{id}')");
            let body = json!({
                "chatType": "oneOnOne",
                "members": [
                    {"@odata.type": "#microsoft.graph.aadUserConversationMember",
                     "roles": ["owner"], "user@odata.bind": bind(&my_id)},
                    {"@odata.type": "#microsoft.graph.aadUserConversationMember",
                     "roles": ["owner"], "user@odata.bind": bind(&uid)},
                ]
            });
            let (cache, pending, prefs, current, render, load_msgs, search, results, results_scroll) = (
                cache.clone(), pending.clone(), prefs.clone(), current.clone(), render.clone(),
                load_msgs.clone(), search.clone(), results.clone(), results_scroll.clone(),
            );
            util::run_ok(
                move || graph::post("/chats", &body),
                move |c| {
                    let Some(cid) = c["id"].as_str().map(String::from) else {
                        return util::toast("Chat created but no id returned");
                    };
                    // Graph returns the existing chat if there already is one.
                    let exists = cache.borrow().1.iter().any(|x| x["id"].as_str() == Some(cid.as_str()));
                    if !exists {
                        let chat = json!({
                            "id": cid, "chatType": "oneOnOne",
                            "members": [{"userId": uid, "displayName": name}],
                        });
                        pending.borrow_mut().push(chat.clone());
                        cache.borrow_mut().1.insert(0, chat);
                    }
                    if prefs.borrow_mut().hidden.remove(&cid) {
                        prefs.borrow().save();
                    }
                    *current.borrow_mut() = Some(cid);
                    render();
                    load_msgs();
                    search.set_text("");
                    util::clear(&results);
                    results_scroll.set_visible(false);
                },
            );
        });
    }

    // Fuzzy message finder (Ctrl+F: this chat, Ctrl+K: all chats).
    let fnd = Rc::new(finder::build(finder::Hooks {
        index: index.clone(),
        current: {
            let current = current.clone();
            Rc::new(move || current.borrow().clone())
        },
        chats: {
            let titles = titles.clone();
            Rc::new(move || titles.borrow().iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        },
        jump: {
            let (jump, rows, current, render, load_msgs, apply_jump) =
                (jump.clone(), rows.clone(), current.clone(), render.clone(), load_msgs.clone(), apply_jump.clone());
            Rc::new(move |chat: String, msg: String| {
                *jump.borrow_mut() = Some(msg.clone());
                let same = current.borrow().as_deref() == Some(chat.as_str());
                if same && rows.borrow().contains_key(&msg) {
                    apply_jump();
                } else {
                    *current.borrow_mut() = Some(chat);
                    render();
                    load_msgs();
                }
            })
        },
    }));
    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&paned));
    overlay.add_overlay(&fnd.panel);
    {
        let fnd = fnd.clone();
        find_btn.connect_clicked(move |_| fnd.open(true));
    }
    {
        let sc = gtk::ShortcutController::new();
        sc.set_propagation_phase(gtk::PropagationPhase::Capture);
        for (keys, all) in [("<Control>f", false), ("<Control>k", true), ("<Control><Shift>f", true)] {
            let fnd = fnd.clone();
            sc.add_shortcut(gtk::Shortcut::new(
                gtk::ShortcutTrigger::parse_string(keys),
                Some(gtk::CallbackAction::new(move |_, _| {
                    fnd.open(all);
                    gtk::glib::Propagation::Stop
                })),
            ));
        }
        overlay.add_controller(sc);
    }

    // Fast poll of the open chat: only its newest messages, reloading on change.
    let poll_chat: Rc<dyn Fn()> = {
        let (current, live_last, seen, load_msgs, do_refresh, chat_busy, penalty) = (
            current.clone(), live_last.clone(), seen.clone(), load_msgs.clone(),
            do_refresh.clone(), chat_busy.clone(), penalty.clone(),
        );
        Rc::new(move || {
            let Some(cid) = current.borrow().clone() else { return };
            chat_busy.set(true);
            let (current, live_last, seen, load_msgs, do_refresh, chat_busy, penalty) = (
                current.clone(), live_last.clone(), seen.clone(), load_msgs.clone(),
                do_refresh.clone(), chat_busy.clone(), penalty.clone(),
            );
            let path = format!("/me/chats/{cid}/messages?$top=3");
            util::run(
                move || graph::get(&path),
                move |r| {
                    chat_busy.set(false);
                    let v = match r {
                        Ok(v) => v,
                        Err(_) => {
                            penalty.set((penalty.get() * 2).clamp(5, 60));
                            return;
                        }
                    };
                    penalty.set(0);
                    if current.borrow().as_deref() != Some(cid.as_str()) {
                        return;
                    }
                    let Some(newest) = v["value"][0]["id"].as_str() else { return };
                    let known = live_last.borrow().get(&cid).cloned();
                    if known.is_some_and(|k| k != newest) {
                        // Mark seen so the list poll does not notify for the chat on screen.
                        seen.borrow_mut().insert(cid.clone(), newest.to_string());
                        load_msgs();
                        do_refresh(true); // refresh previews / ordering
                    }
                },
            );
        })
    };

    // Mark the open chat read once it is on screen in the focused window: locally, and on the
    // Teams server (best effort) so other devices clear it too.
    let mark_read: Rc<dyn Fn(String)> = {
        let (cache, read_local, fresh, render, me) =
            (cache.clone(), read_local.clone(), fresh.clone(), render.clone(), me.clone());
        Rc::new(move |cid: String| {
            let pid = cache
                .borrow()
                .1
                .iter()
                .find(|c| c["id"].as_str() == Some(cid.as_str()))
                .and_then(|c| c["lastMessagePreview"]["id"].as_str().map(String::from));
            let Some(pid) = pid else { return };
            if read_local.borrow().get(&cid) == Some(&pid) {
                return;
            }
            read_local.borrow_mut().insert(cid.clone(), pid);
            fresh.borrow_mut().remove(&cid);
            let render = render.clone();
            gtk::glib::idle_add_local_once(move || render());
            let my = me.borrow().clone();
            if let Ok((_, tenant)) = crate::auth::config() {
                if tenant.len() == 36 && !my.is_empty() {
                    let body = json!({"user": {"id": my, "tenantId": tenant}});
                    util::run(move || graph::post(&format!("/chats/{cid}/markChatReadForUser"), &body), |_| {});
                }
            }
        })
    };

    // Scheduler, 500 ms resolution. With a healthy relay: slow safety-net poll only.
    // Without: 10 s background, 5 s focused, 2 s for the open chat.
    {
        let (do_refresh, poll_chat, current, list_busy, chat_busy, penalty) = (
            do_refresh.clone(), poll_chat.clone(), current.clone(), list_busy.clone(),
            chat_busy.clone(), penalty.clone(),
        );
        let mark_read = mark_read.clone();
        let weak = overlay.downgrade();
        let (mut last_list, mut last_chat) = (Instant::now(), Instant::now());
        gtk::glib::timeout_add_local(Duration::from_millis(500), move || {
            let Some(ov) = weak.upgrade().filter(|o| o.parent().is_some()) else {
                return gtk::glib::ControlFlow::Break; // page removed
            };
            if !crate::auth::has_session() {
                return gtk::glib::ControlFlow::Break; // signed out
            }
            let focused = ov.root().and_downcast::<gtk::Window>().is_some_and(|w| w.is_active());
            let watching = focused && ov.is_mapped(); // Teams on screen in the focused window
            let pushing = relay::healthy();
            let extra = Duration::from_secs(penalty.get() as u64);
            if watching {
                let open = current.borrow().clone();
                if let Some(c) = open {
                    mark_read(c);
                }
            }

            let list_every = if pushing {
                POLL_SAFETY_NET
            } else if watching {
                POLL_FOCUSED
            } else {
                POLL_BACKGROUND
            };
            if !list_busy.get() && last_list.elapsed() >= list_every + extra {
                last_list = Instant::now();
                do_refresh(true);
            }
            if !pushing
                && watching
                && current.borrow().is_some()
                && !chat_busy.get()
                && last_chat.elapsed() >= POLL_ACTIVE_CHAT + extra
            {
                last_chat = Instant::now();
                poll_chat();
            }
            gtk::glib::ControlFlow::Continue
        });
    }

    // Relay push events (none until the relay client exists, see relay.rs).
    if let Some(rx) = relay::start() {
        let (do_refresh, current, load_msgs) = (do_refresh.clone(), current.clone(), load_msgs.clone());
        gtk::glib::spawn_future_local(async move {
            while let Ok(ev) = rx.recv().await {
                if let relay::PushEvent::Message { chat_id } = &ev {
                    if current.borrow().as_deref() == Some(chat_id.as_str()) {
                        load_msgs();
                    }
                }
                do_refresh(true);
            }
        });
    }

    let refresh: Rc<dyn Fn()> = Rc::new(move || do_refresh(false));
    Page { widget: overlay.upcast(), refresh }
}
