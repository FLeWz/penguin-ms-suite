use super::Page;
use crate::{graph, util};
use adw::prelude::*;
use serde_json::{json, Value};
use std::{cell::RefCell, rc::Rc};

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
    let left = util::pane(&chats);
    left.set_size_request(260, -1);
    paned.set_start_child(Some(&left));
    paned.set_end_child(Some(&right));
    paned.set_position(280);
    paned.set_shrink_start_child(false);

    let ids: Rc<RefCell<Vec<String>>> = Default::default();
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
        let (ids, current, load_msgs) = (ids.clone(), current.clone(), load_msgs.clone());
        chats.connect_row_selected(move |_, row| {
            let Some(row) = row else { return };
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

    let refresh: Rc<dyn Fn()> = Rc::new(move || {
        let (chats, ids) = (chats.clone(), ids.clone());
        util::run_ok(
            || {
                let me = graph::get("/me?$select=id")?["id"].as_str().unwrap_or("").to_string();
                let v = graph::get("/me/chats?$expand=members&$top=50&$orderby=lastMessagePreview/createdDateTime%20desc")?;
                Ok((me, v))
            },
            move |(me, v)| {
                util::clear(&chats);
                let items = v["value"].as_array().cloned().unwrap_or_default();
                let mut order: Vec<String> = vec![];
                // People = 1:1 chats. Groups = group chats and meeting chats.
                for (label, people) in [("PEOPLE", true), ("GROUPS", false)] {
                    let sel: Vec<&Value> = items
                        .iter()
                        .filter(|c| (c["chatType"].as_str() == Some("oneOnOne")) == people)
                        .collect();
                    if sel.is_empty() {
                        continue;
                    }
                    chats.append(&header(label));
                    order.push(String::new());
                    for c in sel {
                        let prev = util::strip_html(
                            c["lastMessagePreview"]["body"]["content"].as_str().unwrap_or(""),
                        );
                        chats.append(&util::row(&chat_title(c, &me), &prev));
                        order.push(c["id"].as_str().unwrap_or("").to_string());
                    }
                }
                *ids.borrow_mut() = order;
            },
        );
    });
    Page { widget: paned.upcast(), refresh }
}
