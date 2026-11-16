use super::Page;
use crate::{graph, util};
use adw::prelude::*;
use serde_json::Value;
use std::rc::Rc;

pub fn build() -> Page {
    let list = gtk::ListBox::new();
    list.add_css_class("navigation-sidebar");
    let body = gtk::Label::new(Some("Select a message"));
    body.set_wrap(true);
    body.set_selectable(true);
    body.set_xalign(0.0);
    body.set_yalign(0.0);
    body.set_margin_top(12);
    body.set_margin_bottom(12);
    body.set_margin_start(16);
    body.set_margin_end(16);

    let paned = gtk::Paned::new(gtk::Orientation::Horizontal);
    let left = util::pane(&list);
    left.set_size_request(320, -1);
    paned.set_start_child(Some(&left));
    paned.set_end_child(Some(&util::pane(&body)));
    paned.set_position(340);
    paned.set_shrink_start_child(false);
    // Fixed-width list: window resizing changes only the right pane. The divider can still be dragged.
    paned.set_resize_start_child(false);
    paned.set_resize_end_child(true);

    // Message bodies, index-aligned with list rows.
    let msgs: Rc<std::cell::RefCell<Vec<Value>>> = Default::default();
    {
        let msgs = msgs.clone();
        let body = body.clone();
        list.connect_row_selected(move |_, row| {
            let Some(row) = row else { return };
            if let Some(m) = msgs.borrow().get(row.index() as usize) {
                let subj = m["subject"].as_str().unwrap_or("");
                let text = m["body"]["content"].as_str().unwrap_or("");
                body.set_text(&format!("{subj}\n\n{text}"));
            }
        });
    }

    let refresh: Rc<dyn Fn()> = Rc::new(move || {
        let (list, msgs) = (list.clone(), msgs.clone());
        util::run_ok(
            || {
                graph::get_h(
                    "/me/mailFolders/inbox/messages?$top=50&$orderby=receivedDateTime%20desc\
                     &$select=subject,from,receivedDateTime,isRead,body",
                    &[("Prefer", "outlook.body-content-type=\"text\"")],
                )
            },
            move |v| {
                util::clear(&list);
                let items = v["value"].as_array().cloned().unwrap_or_default();
                for m in &items {
                    let from = m["from"]["emailAddress"]["name"].as_str().unwrap_or("?");
                    let subj = m["subject"].as_str().unwrap_or("(no subject)");
                    let r = util::row(subj, from);
                    if m["isRead"] == false {
                        r.add_css_class("accent");
                    }
                    list.append(&r);
                }
                *msgs.borrow_mut() = items;
            },
        );
    });
    Page { widget: paned.upcast(), refresh }
}
