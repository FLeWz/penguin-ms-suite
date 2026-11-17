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

/// User id -> display name, for the people in the chat list (reactions carry only ids).
pub type Names = Rc<RefCell<HashMap<String, String>>>;

pub struct Reactor {
    chat: String,
    message: String,
    my_id: String,
    names: Names,
    chips: gtk::Box,
    spacer: gtk::Widget,
    /// Reaction shown -> the raw type to send to remove it, for the reactions I made.
    mine: RefCell<HashMap<String, String>>,
}

impl Reactor {
    /// Returns the reactor, the row with the chips and the hover "add reaction" button (it is
    /// laid over the bubble's bottom edge), and a spacer that reserves the room under the
    /// bubble for the half of the row that sticks out, only while there are chips.
    pub fn new(chat: &str, message: &str, my_id: &str, names: Names) -> (Rc<Self>, gtk::Box, gtk::Widget) {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        let spacer = gtk::Box::new(gtk::Orientation::Vertical, 0);
        spacer.set_size_request(-1, 11);
        spacer.set_visible(false);
        let chips = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        let me = Rc::new(Reactor {
            chat: chat.to_string(),
            message: message.to_string(),
            my_id: my_id.to_string(),
            names,
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
            // "+" opens the full emoji chooser, built on first use (it loads the whole emoji set).
            let more = gtk::Button::with_label("+");
            more.add_css_class("flat");
            more.set_tooltip_text(Some("Any emoji"));
            {
                let (me, pop, anchor) = (me.clone(), pop.clone(), add.clone());
                let chooser: Rc<RefCell<Option<gtk::EmojiChooser>>> = Default::default();
                let chooser_c = chooser.clone();
                more.connect_clicked(move |_| {
                    pop.popdown();
                    let mut slot = chooser_c.borrow_mut();
                    let c = slot.get_or_insert_with(|| {
                        let c = gtk::EmojiChooser::new();
                        c.set_parent(&anchor); // anchored to the row button: the strip popover is closed now
                        let me = me.clone();
                        c.connect_emoji_picked(move |_, e| {
                            // The chooser may add a skin-tone or presentation selector; strip
                            // nothing, Teams takes the emoji as given.
                            let have = me.mine.borrow().contains_key(e);
                            me.toggle(e, !have);
                        });
                        c
                    });
                    c.popup();
                });
                add.connect_destroy(move |_| {
                    if let Some(c) = chooser.borrow_mut().take() {
                        c.unparent();
                    }
                });
            }
            strip.append(&more);
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
        let mut groups: HashMap<String, (usize, Option<String>, Vec<String>)> = HashMap::new();
        let names = self.names.borrow();
        for r in reactions.as_array().into_iter().flatten() {
            let raw = r["reactionType"].as_str().unwrap_or("");
            let Some(e) = display(raw) else { continue };
            let g = groups.entry(e.clone()).or_insert_with(|| {
                order.push(e.clone());
                (0, None, vec![])
            });
            g.0 += 1;
            let uid = r["user"]["user"]["id"].as_str().unwrap_or("");
            if !self.my_id.is_empty() && uid == self.my_id {
                g.1 = Some(raw.to_string());
                g.2.insert(0, "You".to_string()); // yourself first
            } else {
                g.2.push(names.get(uid).cloned().unwrap_or_else(|| "Someone".to_string()));
            }
        }
        for e in order {
            let (n, mine, who) = groups[&e].clone();
            let b = gtk::Button::with_label(&format!("{e} {n}"));
            b.set_tooltip_text(Some(&who.join(", ")));
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

