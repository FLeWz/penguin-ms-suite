//! Message reactions: chips under a message (emoji + count, yours highlighted), a hover
//! "add reaction" button with the six standard Teams reactions, and set/unset through
//! `POST /chats/{chat}/messages/{message}/setReaction` and `unsetReaction` (Chat.ReadWrite).
use crate::{graph, util};
use adw::prelude::*;
use serde_json::{json, Value};
use std::{
    cell::RefCell,
    collections::{hash_map::DefaultHasher, HashMap},
    hash::{Hash, Hasher},
    rc::Rc,
};

/// The standard Teams reactions, in picker order.
const QUICK: [&str; 6] = ["👍", "❤️", "😆", "😮", "🙁", "😠"];

/// Older messages carry reaction names instead of emoji.
fn display(kind: &str) -> Option<String> {
    Some(
        match kind {
            "like" => "👍",
            "heart" => "❤️",
            "laugh" => "😆",
            "surprised" => "😮",
            "sad" => "🙁",
            "angry" => "😠",
            "custom" | "" => return None,
            other => other,
        }
        .to_string(),
    )
}

/// Fingerprint of a message's reactions, to notice changes without rebuilding the row.
pub fn sig(reactions: &Value) -> u64 {
    let mut h = DefaultHasher::new();
    for r in reactions.as_array().into_iter().flatten() {
        r["reactionType"].as_str().hash(&mut h);
        r["user"]["user"]["id"].as_str().hash(&mut h);
    }
    h.finish()
}

pub struct Reactor {
    chat: String,
    message: String,
    my_id: String,
    chips: gtk::Box,
    spacer: gtk::Widget,
    /// Reaction shown -> the raw type to send to remove it, for the reactions I made.
    mine: RefCell<HashMap<String, String>>,
}

impl Reactor {
    /// Returns the reactor, the row with the chips and the hover "add reaction" button (it is
    /// laid over the bubble's bottom edge), and a spacer that reserves the room under the
    /// bubble for the half of the row that sticks out, only while there are chips.
    pub fn new(chat: &str, message: &str, my_id: &str) -> (Rc<Self>, gtk::Box, gtk::Widget) {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        let spacer = gtk::Box::new(gtk::Orientation::Vertical, 0);
        spacer.set_size_request(-1, 11);
        spacer.set_visible(false);
        let chips = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        let me = Rc::new(Reactor {
            chat: chat.to_string(),
            message: message.to_string(),
            my_id: my_id.to_string(),
            chips: chips.clone(),
            spacer: spacer.clone().upcast(),
            mine: Default::default(),
        });
        row.append(&chips);
        if !message.is_empty() {
            let add = gtk::MenuButton::new();
            add.set_icon_name("face-smile-symbolic");
            add.add_css_class("react-add");
            add.add_css_class("quiet-icon");
            add.set_has_frame(false);
            add.set_tooltip_text(Some("Add reaction"));
            let pop = gtk::Popover::new();
            let strip = gtk::Box::new(gtk::Orientation::Horizontal, 2);
            for e in QUICK {
                let b = gtk::Button::with_label(e);
                b.add_css_class("flat");
                let (me, pop) = (me.clone(), pop.clone());
                b.connect_clicked(move |_| {
                    pop.popdown();
                    let have = me.mine.borrow().contains_key(e);
                    me.toggle(e, !have);
                });
                strip.append(&b);
            }
            pop.set_child(Some(&strip));
            add.set_popover(Some(&pop));
            row.append(&add);
        }
        (me, row, spacer.upcast())
    }

    /// Redraw the chips from the message's `reactions` array.
    pub fn refresh(self: &Rc<Self>, reactions: &Value) {
        while let Some(c) = self.chips.first_child() {
            self.chips.remove(&c);
        }
        self.mine.borrow_mut().clear();
        // emoji -> (count, my raw type), in first-seen order
        let mut order: Vec<String> = vec![];
        let mut groups: HashMap<String, (usize, Option<String>)> = HashMap::new();
        for r in reactions.as_array().into_iter().flatten() {
            let raw = r["reactionType"].as_str().unwrap_or("");
            let Some(e) = display(raw) else { continue };
            let g = groups.entry(e.clone()).or_insert_with(|| {
                order.push(e.clone());
                (0, None)
            });
            g.0 += 1;
            if !self.my_id.is_empty() && r["user"]["user"]["id"].as_str() == Some(self.my_id.as_str()) {
                g.1 = Some(raw.to_string());
            }
        }
        for e in order {
            let (n, mine) = groups[&e].clone();
            let b = gtk::Button::with_label(&format!("{e} {n}"));
            b.add_css_class("react-chip");
            if mine.is_some() {
                b.add_css_class("react-mine");
                self.mine.borrow_mut().insert(e.clone(), mine.unwrap_or_default());
            }
            let me = self.clone();
            let was_mine = self.mine.borrow().contains_key(&e);
            b.connect_clicked(move |_| me.toggle(&e, !was_mine));
            self.chips.append(&b);
        }
        let any = self.chips.first_child().is_some();
        self.chips.set_visible(any);
        self.spacer.set_visible(any);
    }

    fn toggle(self: &Rc<Self>, emoji: &str, set: bool) {
        let (chat, msg) = (self.chat.clone(), self.message.clone());
        let kind = if set {
            emoji.to_string()
        } else {
            self.mine.borrow().get(emoji).cloned().unwrap_or_else(|| emoji.to_string())
        };
        let me = self.clone();
        util::run(
            move || {
                let action = if set { "setReaction" } else { "unsetReaction" };
                graph::post(&format!("/chats/{chat}/messages/{msg}/{action}"), &json!({"reactionType": kind}))?;
                graph::get(&format!("/me/chats/{chat}/messages/{msg}"))
            },
            move |r| match r {
                Ok(v) => me.refresh(&v["reactions"]),
                Err(e) => util::toast(&format!("Could not change the reaction: {e}")),
            },
        );
    }
}

