use super::Page;
use crate::{graph, util};
use adw::prelude::*;
use serde_json::{json, Value};
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    rc::Rc,
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
    l.set_margin_top(10);
    l.set_margin_bottom(4);
    l.set_margin_start(10);
    let r = gtk::ListBoxRow::new();
    r.set_child(Some(&l));
    r.set_selectable(false);
    r.set_activatable(false);
    r
}

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

fn icon_button(icon: &str, tip: &str) -> gtk::Button {
    let b = gtk::Button::from_icon_name(icon);
    b.add_css_class("flat");
    b.add_css_class("circular");
    b.set_valign(gtk::Align::Center);
    b.set_tooltip_text(Some(tip));
    b
}

pub fn build() -> Page {
    let chats = gtk::ListBox::new();
    chats.add_css_class("navigation-sidebar");
    let msgs = gtk::ListBox::new();
    msgs.set_selection_mode(gtk::SelectionMode::None);
    let entry = gtk::Entry::new();
    entry.set_placeholder_text(Some("Type a message…"));
    entry.set_margin_top(6);
    entry.set_margin_bottom(6);
    entry.set_margin_start(6);
    entry.set_margin_end(6);

    let right = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let msg_scroll = util::pane(&msgs);
    // While `stick` is set, keep the view pinned to the newest message as
    // wrapped labels settle their final height.
    let stick = Rc::new(std::cell::Cell::new(false));
    {
        let stick = stick.clone();
        msg_scroll.vadjustment().connect_changed(move |a| {
            if stick.get() {
                a.set_value(a.upper() - a.page_size());
            }
        });
    }
    right.append(&msg_scroll);
    right.append(&entry);
    right.set_hexpand(true);
    let paned = gtk::Paned::new(gtk::Orientation::Horizontal);
    let show_hidden = gtk::ToggleButton::with_label("Show hidden");
    show_hidden.add_css_class("flat");
    show_hidden.set_margin_start(6);
    show_hidden.set_margin_end(6);
    show_hidden.set_margin_top(4);
    let left = gtk::Box::new(gtk::Orientation::Vertical, 0);
    left.append(&show_hidden);
    left.append(&util::pane(&chats));
    left.set_size_request(260, -1);
    paned.set_start_child(Some(&left));
    paned.set_end_child(Some(&right));
    paned.set_position(280);
    paned.set_shrink_start_child(false);

    let ids: Rc<RefCell<Vec<String>>> = Default::default();
    // Suppresses the selection handler while the list is rebuilt programmatically.
    let quiet = Rc::new(std::cell::Cell::new(false));
    let current: Rc<RefCell<Option<String>>> = Default::default();

    let load_msgs = {
        let (msgs, current, stick) = (msgs.clone(), current.clone(), stick.clone());
        Rc::new(move || {
            let Some(id) = current.borrow().clone() else { return };
            let (msgs, stick) = (msgs.clone(), stick.clone());
            util::run_ok(
                move || graph::get(&format!("/me/chats/{id}/messages?$top=40")),
                move |v| {
                    stick.set(true);
                    gtk::glib::timeout_add_local_once(std::time::Duration::from_millis(600), move || {
                        stick.set(false)
                    });
                    util::clear(&msgs);
                    let mut items = v["value"].as_array().cloned().unwrap_or_default();
                    items.reverse(); // API returns newest first
                    for m in items {
                        let who = m["from"]["user"]["displayName"].as_str().unwrap_or("system");
                        let text = util::strip_html(m["body"]["content"].as_str().unwrap_or(""));
                        if text.is_empty() {
                            continue;
                        }
                        let r = util::row(who, "");
                        if let Some(l) = r.last_child().and_downcast::<gtk::Label>() {
                            l.set_text(&text);
                            l.set_wrap(true);
                            l.set_single_line_mode(false);
                            l.set_ellipsize(gtk::pango::EllipsizeMode::None);
                            l.set_selectable(true);
                        }
                        msgs.append(&r);
                    }
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
        let (current, load_msgs) = (current.clone(), load_msgs.clone());
        entry.connect_activate(move |e| {
            let text = e.text().to_string();
            let Some(id) = current.borrow().clone() else { return };
            if text.trim().is_empty() {
                return;
            }
            e.set_text("");
            let load_msgs = load_msgs.clone();
            util::run_ok(
                move || graph::post(&format!("/me/chats/{id}/messages"), &json!({"body": {"content": text}})),
                move |_| load_msgs(),
            );
        });
    }

    // Latest message id seen per chat, to detect new arrivals. Empty until first load.
    let seen: Rc<RefCell<HashMap<String, String>>> = Default::default();
    let me: Rc<RefCell<String>> = Default::default();
    let signature: Rc<RefCell<String>> = Default::default();
    let cache: Rc<RefCell<(String, Vec<Value>)>> = Default::default(); // (my id, chats)
    let prefs = Rc::new(RefCell::new(Prefs::load()));
    // Lets row buttons re-render the list (render refers to itself through this cell).
    let render_cell: Rc<RefCell<Option<Rc<dyn Fn()>>>> = Default::default();

    let render: Rc<dyn Fn()> = {
        let (chats, ids, current, quiet, cache, prefs, render_cell, show_hidden) = (
            chats.clone(), ids.clone(), current.clone(), quiet.clone(), cache.clone(),
            prefs.clone(), render_cell.clone(), show_hidden.clone(),
        );
        Rc::new(move || {
            let (my_id, items) = cache.borrow().clone();
            let p = prefs.borrow();
            let one = |c: &Value| c["chatType"].as_str() == Some("oneOnOne");
            let id_of = |c: &Value| c["id"].as_str().unwrap_or("").to_string();
            let vis = |c: &&Value| !p.hidden.contains(&id_of(c));
            let sections: Vec<(&str, Vec<&Value>)> = vec![
                ("FAVOURITES", items.iter().filter(vis).filter(|c| p.fav.contains(&id_of(c))).collect()),
                ("PEOPLE", items.iter().filter(vis).filter(|c| !p.fav.contains(&id_of(c)) && one(c)).collect()),
                ("GROUPS", items.iter().filter(vis).filter(|c| !p.fav.contains(&id_of(c)) && !one(c)).collect()),
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

                    let line = gtk::Box::new(gtk::Orientation::Horizontal, 0);
                    let info = util::row(&chat_title(c, &my_id), &prev);
                    info.set_hexpand(true);
                    line.append(&info);

                    let fav_btn = icon_button(
                        if is_fav { "starred-symbolic" } else { "non-starred-symbolic" },
                        if is_fav { "Remove from favourites" } else { "Add to favourites" },
                    );
                    let hide_btn = icon_button(
                        if is_hidden { "view-reveal-symbolic" } else { "view-conceal-symbolic" },
                        if is_hidden { "Unhide chat" } else { "Hide chat" },
                    );
                    for (btn, is_fav_btn) in [(&fav_btn, true), (&hide_btn, false)] {
                        let (prefs, render_cell, cid) = (prefs.clone(), render_cell.clone(), cid.clone());
                        btn.connect_clicked(move |_| {
                            {
                                let mut p = prefs.borrow_mut();
                                if is_fav_btn {
                                    Prefs::toggle(&mut p.fav, &cid);
                                } else {
                                    Prefs::toggle(&mut p.hidden, &cid);
                                }
                                p.save();
                            }
                            let r = render_cell.borrow().clone();
                            if let Some(r) = r {
                                // Defer: we are inside a widget that render() destroys.
                                gtk::glib::idle_add_local_once(move || r());
                            }
                        });
                        line.append(btn);
                    }
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
        let (chats, current, load_msgs, seen, me, signature, cache, prefs, render) = (
            chats.clone(), current.clone(), load_msgs.clone(), seen.clone(), me.clone(),
            signature.clone(), cache.clone(), prefs.clone(), render.clone(),
        );
        Rc::new(move |polling: bool| {
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
                    let (my_id, v) = match r {
                        Ok(x) => x,
                        Err(e) => {
                            if !polling {
                                util::toast(&e); // polling stays silent
                            }
                            return;
                        }
                    };
                    *me2.borrow_mut() = my_id.clone();
                    let items = v["value"].as_array().cloned().unwrap_or_default();

                    // New-message detection.
                    let first_load = seen.borrow().is_empty();
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

    // Live updates: poll every 10 s while the page exists.
    {
        let do_refresh = do_refresh.clone();
        let weak = paned.downgrade();
        gtk::glib::timeout_add_seconds_local(10, move || {
            if weak.upgrade().and_then(|p| p.parent()).is_none() || !crate::auth::has_session() {
                return gtk::glib::ControlFlow::Break; // page removed or signed out
            }
            do_refresh(true);
            gtk::glib::ControlFlow::Continue
        });
    }

    let refresh: Rc<dyn Fn()> = Rc::new(move || do_refresh(false));
    Page { widget: paned.upcast(), refresh }
}
