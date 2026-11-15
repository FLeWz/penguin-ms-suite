use super::Page;
use crate::{graph, util};
use adw::prelude::*;
use serde_json::{json, Value};
use std::{cell::RefCell, rc::Rc};

fn chat_title(c: &Value) -> String {
    if let Some(t) = c["topic"].as_str().filter(|t| !t.is_empty()) {
        return t.into();
    }
    let names: Vec<&str> = c["members"]
        .as_array()
        .map(|m| m.iter().filter_map(|x| x["displayName"].as_str()).collect())
        .unwrap_or_default();
    if names.is_empty() { "Chat".into() } else { names.join(", ") }
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
    right.append(&util::pane(&msgs));
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
        let (msgs, current) = (msgs.clone(), current.clone());
        Rc::new(move || {
            let Some(id) = current.borrow().clone() else { return };
            let msgs = msgs.clone();
            util::run_ok(
                move || graph::get(&format!("/me/chats/{id}/messages?$top=40")),
                move |v| {
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
            *current.borrow_mut() = ids.borrow().get(row.index() as usize).cloned();
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
            || graph::get("/me/chats?$expand=members&$top=40&$orderby=lastMessagePreview/createdDateTime%20desc"),
            move |v| {
                util::clear(&chats);
                let items = v["value"].as_array().cloned().unwrap_or_default();
                *ids.borrow_mut() = items.iter().map(|c| c["id"].as_str().unwrap_or("").to_string()).collect();
                for c in &items {
                    let prev = util::strip_html(c["lastMessagePreview"]["body"]["content"].as_str().unwrap_or(""));
                    chats.append(&util::row(&chat_title(c), &prev));
                }
            },
        );
    });
    Page { widget: paned.upcast(), refresh }
}
