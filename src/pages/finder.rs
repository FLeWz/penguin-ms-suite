//! Fuzzy message finder (telescope/fzf style) for Teams: one chat or all chats.
//! Matching is client-side over messages fetched from Graph and held in memory.
use crate::{graph, util};
use adw::prelude::*;
use gtk::glib;
use serde_json::Value;
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    rc::Rc,
    time::{Duration, Instant},
};

const MAX_HITS: usize = 60;
const MATCH_CHARS: usize = 400; // only the first N chars of a message are matched
const REINDEX_AFTER: Duration = Duration::from_secs(120);

#[derive(Clone)]
pub struct Msg {
    pub id: String,
    pub chat_id: String,
    pub chat_title: String,
    pub who: String,
    pub text: String,
    pub when: String,
    pub ts: i64,
}

/// chat id -> messages (newest first).
pub type Index = Rc<RefCell<HashMap<String, Vec<Msg>>>>;

pub fn parse_time(iso: &str) -> (String, i64) {
    let s = format!("{}Z", iso.trim_end_matches('Z').split('.').next().unwrap_or(iso));
    match glib::DateTime::from_iso8601(&s, None).and_then(|d| d.to_local()) {
        Ok(d) => (
            d.format("%d %b %H:%M").map(|g| g.to_string()).unwrap_or_default(),
            d.to_unix(),
        ),
        Err(_) => (iso.to_string(), 0),
    }
}

/// Graph chat messages -> searchable records. System/empty messages are skipped.
pub fn parse(chat_id: &str, title: &str, items: &[Value]) -> Vec<Msg> {
    items
        .iter()
        .filter_map(|m| {
            let text = util::strip_html(m["body"]["content"].as_str().unwrap_or(""));
            if text.is_empty() || m["messageType"].as_str().is_some_and(|t| t != "message") {
                return None;
            }
            let who = m["from"]["user"]["displayName"]
                .as_str()
                .or(m["from"]["application"]["displayName"].as_str())
                .unwrap_or("system");
            let (when, ts) = parse_time(m["createdDateTime"].as_str().unwrap_or(""));
            Some(Msg {
                id: m["id"].as_str()?.to_string(),
                chat_id: chat_id.to_string(),
                chat_title: title.to_string(),
                who: who.to_string(),
                text,
                when,
                ts,
            })
        })
        .collect()
}

/// Merge into the index, de-duplicating by message id.
pub fn add(index: &Index, msgs: Vec<Msg>) {
    let Some(chat) = msgs.first().map(|m| m.chat_id.clone()) else { return };
    let mut idx = index.borrow_mut();
    let list = idx.entry(chat).or_default();
    let have: HashSet<String> = list.iter().map(|m| m.id.clone()).collect();
    list.extend(msgs.into_iter().filter(|m| !have.contains(&m.id)));
    list.sort_by(|a, b| b.ts.cmp(&a.ts));
}

// ---------- fuzzy matching ----------

fn bonus(prev: Option<char>, cur: char) -> i32 {
    match prev {
        None => 8,
        Some(p) if !p.is_alphanumeric() => 8,
        Some(p) if p.is_lowercase() && cur.is_uppercase() => 6,
        _ => 0,
    }
}

/// Subsequence match, case-insensitive. Rewards consecutive runs and word
/// starts, penalises gaps. Returns (score, matched char indices).
pub fn fuzzy(query: &str, text: &str) -> Option<(i32, Vec<usize>)> {
    const NEG: i32 = i32::MIN / 2;
    let q: Vec<char> = query.chars().flat_map(|c| c.to_lowercase()).collect();
    if q.is_empty() {
        return Some((0, vec![]));
    }
    let orig: Vec<char> = text.chars().take(MATCH_CHARS).collect();
    let lt: Vec<char> = orig.iter().map(|c| c.to_lowercase().next().unwrap_or(*c)).collect();
    let (n, m) = (lt.len(), q.len());
    if m > n {
        return None;
    }
    // Cheap reject: must be a subsequence at all.
    let mut qi = 0;
    for &c in &lt {
        if qi < m && c == q[qi] {
            qi += 1;
        }
    }
    if qi < m {
        return None;
    }
    let bon: Vec<i32> = (0..n)
        .map(|j| bonus(if j == 0 { None } else { Some(orig[j - 1]) }, orig[j]))
        .collect();

    let mut dp = vec![NEG; m * n];
    let mut par = vec![usize::MAX; m * n];
    for i in 0..m {
        let (mut best, mut best_k) = (NEG, usize::MAX); // max of dp[i-1][k] + k over k <= j-2
        for j in 0..n {
            if i > 0 && j >= 2 {
                let k = j - 2;
                let v = dp[(i - 1) * n + k];
                if v > NEG && v + k as i32 > best {
                    best = v + k as i32;
                    best_k = k;
                }
            }
            if lt[j] != q[i] {
                continue;
            }
            let base = 16 + bon[j];
            if i == 0 {
                dp[j] = base - (j.min(6) as i32);
                continue;
            }
            let (mut cand, mut pk) = (NEG, usize::MAX);
            if j >= 1 {
                let v = dp[(i - 1) * n + j - 1];
                if v > NEG {
                    cand = v + 10; // consecutive
                    pk = j - 1;
                }
            }
            if best > NEG {
                let v = best - j as i32 + 1 - 3; // gap of g chars costs 3 + g
                if v > cand {
                    cand = v;
                    pk = best_k;
                }
            }
            if pk != usize::MAX {
                dp[i * n + j] = cand + base;
                par[i * n + j] = pk;
            }
        }
    }
    let (mut j, mut score) = (usize::MAX, NEG);
    for jj in 0..n {
        if dp[(m - 1) * n + jj] > score {
            score = dp[(m - 1) * n + jj];
            j = jj;
        }
    }
    if j == usize::MAX {
        return None;
    }
    let mut pos = vec![0; m];
    for i in (0..m).rev() {
        pos[i] = j;
        j = par[i * n + j];
    }
    Some((score, pos))
}

/// All whitespace-separated tokens must match (fzf-style AND). A token may
/// match the message text (highlighted) or, at half score, the sender/chat name.
fn score_msg(tokens: &[&str], m: &Msg) -> Option<(i32, Vec<usize>)> {
    let (mut total, mut pos) = (0, vec![]);
    for t in tokens {
        if let Some((s, p)) = fuzzy(t, &m.text) {
            total += s;
            pos.extend(p);
        } else if let Some((s, _)) = fuzzy(t, &m.who).or_else(|| fuzzy(t, &m.chat_title)) {
            total += s / 2;
        } else {
            return None;
        }
    }
    pos.sort_unstable();
    pos.dedup();
    Some((total, pos))
}

fn highlight(text: &str, pos: &[usize]) -> String {
    let chars: Vec<char> = text.chars().collect();
    let first = pos.first().copied().unwrap_or(0);
    let start = if first > 30 { first - 20 } else { 0 };
    let end = (start + 240).min(chars.len());
    let hit: HashSet<usize> = pos.iter().copied().collect();
    let mut out = String::new();
    if start > 0 {
        out.push('…');
    }
    let (mut buf, mut on) = (String::new(), false);
    let flush = |out: &mut String, buf: &mut String, on: bool| {
        if buf.is_empty() {
            return;
        }
        let esc = glib::markup_escape_text(buf);
        if on {
            out.push_str(&format!("<b><u>{esc}</u></b>"));
        } else {
            out.push_str(&esc);
        }
        buf.clear();
    };
    for i in start..end {
        let h = hit.contains(&i);
        if h != on {
            flush(&mut out, &mut buf, on);
            on = h;
        }
        buf.push(chars[i]);
    }
    flush(&mut out, &mut buf, on);
    out
}

// ---------- UI ----------

pub struct Hooks {
    pub index: Index,
    pub current: Rc<dyn Fn() -> Option<String>>,
    /// (chat id, title) for every known chat.
    pub chats: Rc<dyn Fn() -> Vec<(String, String)>>,
    /// Open chat + message.
    pub jump: Rc<dyn Fn(String, String)>,
}

pub struct Finder {
    pub panel: gtk::Box,
    open: Rc<dyn Fn(bool)>,
}

impl Finder {
    /// `all`: search every chat; otherwise only the open chat.
    pub fn open(&self, all: bool) {
        (self.open)(all)
    }
}

fn move_selection(list: &gtk::ListBox, scroll: &gtk::ScrolledWindow, delta: i32) {
    let cur = list.selected_row().map_or(-1, |r| r.index());
    let Some(row) = list.row_at_index((cur + delta).max(0)) else { return };
    list.select_row(Some(&row));
    if let Some(b) = row.compute_bounds(list) {
        let adj = scroll.vadjustment();
        let (y, h) = (b.y() as f64, b.height() as f64);
        if y < adj.value() {
            adj.set_value(y);
        } else if y + h > adj.value() + adj.page_size() {
            adj.set_value(y + h - adj.page_size());
        }
    }
}

pub fn build(h: Hooks) -> Finder {
    // `panel` is the dimmed full-window layer (click it to close); `card` is the solid box in it.
    let panel = gtk::Box::new(gtk::Orientation::Vertical, 0);
    panel.add_css_class("finder-scrim");
    panel.set_visible(false);
    let card = gtk::Box::new(gtk::Orientation::Vertical, 6);
    card.add_css_class("finder-card");
    card.set_width_request(640);
    card.set_halign(gtk::Align::Center);
    card.set_valign(gtk::Align::Start);
    card.set_margin_top(30);
    panel.append(&card);
    {
        let click = gtk::GestureClick::new();
        click.set_button(1);
        let (panel_c, card_c) = (panel.clone(), card.clone());
        click.connect_released(move |_, _, x, y| {
            let inside = card_c
                .compute_bounds(&panel_c)
                .is_some_and(|r| r.contains_point(&gtk::graphene::Point::new(x as f32, y as f32)));
            if !inside {
                panel_c.set_visible(false);
            }
        });
        panel.add_controller(click);
    }

    let scope_chat = gtk::ToggleButton::with_label("This chat");
    let scope_all = gtk::ToggleButton::with_label("All chats");
    scope_all.set_group(Some(&scope_chat));
    let linked = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    linked.add_css_class("linked");
    linked.append(&scope_chat);
    linked.append(&scope_all);
    let status = gtk::Label::new(None);
    status.add_css_class("dim-label");
    status.set_hexpand(true);
    status.set_xalign(1.0);
    let top = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    top.set_margin_top(8);
    top.set_margin_start(8);
    top.set_margin_end(8);
    top.append(&linked);
    top.append(&status);

    let entry = gtk::SearchEntry::new();
    entry.set_placeholder_text(Some("Fuzzy find…   Tab: switch scope · ↑↓: move · Enter: open · Esc: close"));
    entry.set_margin_start(8);
    entry.set_margin_end(8);
    let results = gtk::ListBox::new();
    results.add_css_class("navigation-sidebar");
    let scroll = gtk::ScrolledWindow::builder()
        .child(&results)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .max_content_height(380)
        .propagate_natural_height(true)
        .build();
    scroll.set_margin_bottom(8);
    card.append(&top);
    card.append(&entry);
    card.append(&scroll);

    let hits: Rc<RefCell<Vec<Msg>>> = Default::default();
    let note: Rc<RefCell<String>> = Default::default(); // indexing progress text

    // Re-run the query against the index and redraw results.
    let run: Rc<dyn Fn()> = {
        let (entry, results, status, scope_all, hits, note) = (
            entry.clone(), results.clone(), status.clone(), scope_all.clone(), hits.clone(), note.clone(),
        );
        let (index, current) = (h.index.clone(), h.current.clone());
        Rc::new(move || {
            let q = entry.text().to_string();
            let tokens: Vec<&str> = q.split_whitespace().collect();
            let all = scope_all.is_active();
            let cur = current();

            let mut scored: Vec<(i32, i64, Msg, Vec<usize>)> = vec![];
            {
                let idx = index.borrow();
                let lists: Vec<&Vec<Msg>> = if all {
                    idx.values().collect()
                } else {
                    cur.as_ref().and_then(|c| idx.get(c)).into_iter().collect()
                };
                if !tokens.is_empty() || !all {
                    for m in lists.into_iter().flatten() {
                        if let Some((s, p)) = score_msg(&tokens, m) {
                            scored.push((s, m.ts, m.clone(), p));
                        }
                    }
                }
            }
            scored.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
            scored.truncate(MAX_HITS);

            util::clear(&results);
            let mut shown = vec![];
            for (_, _, m, pos) in scored {
                let b = gtk::Box::new(gtk::Orientation::Vertical, 2);
                b.set_margin_top(4);
                b.set_margin_bottom(4);
                b.set_margin_start(8);
                b.set_margin_end(8);
                let head = if all {
                    format!("{}  ·  {}  ·  {}", m.chat_title, m.who, m.when)
                } else {
                    format!("{}  ·  {}", m.who, m.when)
                };
                let l1 = gtk::Label::new(Some(&head));
                l1.set_xalign(0.0);
                l1.add_css_class("caption");
                l1.add_css_class("dim-label");
                l1.set_ellipsize(gtk::pango::EllipsizeMode::End);
                let l2 = gtk::Label::new(None);
                l2.set_markup(&highlight(&m.text, &pos));
                l2.set_xalign(0.0);
                l2.set_ellipsize(gtk::pango::EllipsizeMode::End);
                l2.set_single_line_mode(true);
                b.append(&l1);
                b.append(&l2);
                results.append(&b);
                shown.push(m);
            }
            let n = shown.len();
            *hits.borrow_mut() = shown;
            if let Some(r) = results.row_at_index(0) {
                results.select_row(Some(&r));
            }
            let base = if all && tokens.is_empty() {
                "Type to search all chats".to_string()
            } else if !all && cur.is_none() {
                "Open a chat first".to_string()
            } else {
                format!("{n} match{}", if n == 1 { "" } else { "es" })
            };
            let extra = note.borrow().clone();
            status.set_text(&if extra.is_empty() { base } else { format!("{base}  ·  {extra}") });
        })
    };

    // Index job: pages back through the given chats (4 worker threads) into the index,
    // adding each page as it arrives. `pages` is the most 50-message pages fetched per chat.
    let fetched: Rc<RefCell<HashMap<String, Instant>>> = Default::default();
    let deep: Rc<RefCell<HashMap<String, Instant>>> = Default::default();
    let index_job: Rc<dyn Fn(Vec<(String, String)>, usize, Rc<RefCell<HashMap<String, Instant>>>, bool)> = {
        let (index, run, note, panel) = (h.index.clone(), run.clone(), note.clone(), panel.clone());
        Rc::new(move |targets, pages, done_map, quiet_count| {
            if targets.is_empty() {
                return;
            }
            for (id, _) in &targets {
                done_map.borrow_mut().insert(id.clone(), Instant::now());
            }
            let total = targets.len();
            // (chat, a page of messages or an error, last page for this chat)
            let (tx, rx) = async_channel::unbounded::<(String, Result<Vec<Msg>, String>, bool)>();
            for t in 0..4 {
                let tx = tx.clone();
                let mine: Vec<(String, String)> = targets.iter().skip(t).step_by(4).cloned().collect();
                std::thread::spawn(move || {
                    for (id, title) in mine {
                        let mut path = format!("/me/chats/{id}/messages?$top=50");
                        for n in 0..pages {
                            match crate::msgcache::get(&path) {
                                Ok(v) => {
                                    let msgs = parse(&id, &title, v["value"].as_array().map_or(&[][..], |a| a.as_slice()));
                                    let next = v["@odata.nextLink"].as_str().map(|u| u.strip_prefix(graph::BASE).unwrap_or(u).to_string());
                                    let last = n + 1 == pages || next.is_none();
                                    let _ = tx.send_blocking((id.clone(), Ok(msgs), last));
                                    match next {
                                        Some(p) => path = p,
                                        None => break,
                                    }
                                }
                                Err(e) => {
                                    let _ = tx.send_blocking((id.clone(), Err(e), true));
                                    break;
                                }
                            }
                        }
                    }
                });
            }
            drop(tx);
            *note.borrow_mut() = if quiet_count { "loading history…".into() } else { format!("indexing 0/{total}") };
            let (index, run, note, done_map, panel) =
                (index.clone(), run.clone(), note.clone(), done_map.clone(), panel.clone());
            glib::spawn_future_local(async move {
                let (mut done, mut count) = (0, 0usize);
                while let Ok((id, r, last)) = rx.recv().await {
                    match r {
                        Ok(m) => {
                            count += m.len();
                            add(&index, m);
                        }
                        Err(_) => {
                            done_map.borrow_mut().remove(&id); // retry next time
                        }
                    }
                    if last {
                        done += 1;
                    }
                    *note.borrow_mut() = if done == total {
                        String::new()
                    } else if quiet_count {
                        format!("loading history… {count} messages")
                    } else {
                        format!("indexing {done}/{total}")
                    };
                    if panel.is_visible() {
                        run();
                    }
                }
            });
        })
    };
    // All chats: the 4 newest pages (200 messages) of each.
    let ensure_all: Rc<dyn Fn()> = {
        let (chats, fetched, index_job) = (h.chats.clone(), fetched.clone(), index_job.clone());
        Rc::new(move || {
            let targets: Vec<(String, String)> = chats()
                .into_iter()
                .filter(|(id, _)| fetched.borrow().get(id).is_none_or(|t| t.elapsed() > REINDEX_AFTER))
                .collect();
            index_job(targets, 4, fetched.clone(), false);
        })
    };
    // This chat: its whole loaded history, up to 40 pages (2000 messages).
    let ensure_deep: Rc<dyn Fn()> = {
        let (chats, current, deep, index_job) = (h.chats.clone(), h.current.clone(), deep.clone(), index_job.clone());
        Rc::new(move || {
            let Some(cur) = current() else { return };
            if deep.borrow().get(&cur).is_some_and(|t| t.elapsed() <= REINDEX_AFTER * 5) {
                return;
            }
            let title = chats().into_iter().find(|(id, _)| *id == cur).map(|(_, t)| t).unwrap_or_default();
            index_job(vec![(cur, title)], 40, deep.clone(), true);
        })
    };

    // Input wiring.
    {
        let run = run.clone();
        entry.connect_search_changed(move |_| run());
    }
    {
        let (run, ensure_all, ensure_deep, scope_all) =
            (run.clone(), ensure_all.clone(), ensure_deep.clone(), scope_all.clone());
        scope_all.connect_toggled(move |b| {
            if b.is_active() {
                ensure_all();
            } else {
                ensure_deep();
            }
            run();
        });
    }
    {
        let panel = panel.clone();
        entry.connect_stop_search(move |_| panel.set_visible(false));
    }
    let activate = {
        let (hits, panel, jump) = (hits.clone(), panel.clone(), h.jump.clone());
        Rc::new(move |row: Option<gtk::ListBoxRow>| {
            let Some(row) = row else { return };
            let Some(m) = hits.borrow().get(row.index() as usize).cloned() else { return };
            panel.set_visible(false);
            jump(m.chat_id, m.id);
        })
    };
    {
        let (activate, results) = (activate.clone(), results.clone());
        entry.connect_activate(move |_| activate(results.selected_row()));
    }
    {
        let activate = activate.clone();
        results.connect_row_activated(move |_, row| activate(Some(row.clone())));
    }
    // Capture-phase keys so Tab/arrows work while the entry has focus.
    {
        let key = gtk::EventControllerKey::new();
        key.set_propagation_phase(gtk::PropagationPhase::Capture);
        let (panel_k, results, scroll, scope_chat, scope_all) =
            (panel.clone(), results.clone(), scroll.clone(), scope_chat.clone(), scope_all.clone());
        key.connect_key_pressed(move |_, k, _, state| {
            let panel = &panel_k;
            use gtk::gdk::Key;
            let ctrl = state.contains(gtk::gdk::ModifierType::CONTROL_MASK);
            match k {
                Key::Escape => panel.set_visible(false),
                Key::Tab | Key::ISO_Left_Tab => {
                    if scope_all.is_active() {
                        scope_chat.set_active(true)
                    } else {
                        scope_all.set_active(true)
                    }
                }
                Key::Down => move_selection(&results, &scroll, 1),
                Key::Up => move_selection(&results, &scroll, -1),
                Key::n if ctrl => move_selection(&results, &scroll, 1),
                Key::p if ctrl => move_selection(&results, &scroll, -1),
                _ => return glib::Propagation::Proceed,
            }
            glib::Propagation::Stop
        });
        panel.add_controller(key);
    }

    let open: Rc<dyn Fn(bool)> = {
        let (panel, entry, scope_all, scope_chat, run, ensure_all, ensure_deep) = (
            panel.clone(), entry.clone(), scope_all.clone(), scope_chat.clone(), run.clone(), ensure_all.clone(),
            ensure_deep.clone(),
        );
        Rc::new(move |all: bool| {
            panel.set_visible(true);
            if all {
                scope_all.set_active(true); // toggled handler indexes + runs
            } else {
                scope_chat.set_active(true);
            }
            if all {
                ensure_all();
            } else {
                ensure_deep();
            }
            // Focus once the panel is mapped, otherwise Esc and typing go to the page behind.
            let entry = entry.clone();
            glib::idle_add_local_once(move || {
                entry.grab_focus();
                entry.select_region(0, -1);
            });
            run();
        })
    };
    Finder { panel, open }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subsequence_matches_and_positions() {
        let (_, p) = fuzzy("dpl", "deploy now").unwrap();
        assert_eq!(p, vec![0, 2, 3]);
        assert!(fuzzy("xyz", "deploy now").is_none());
        assert!(fuzzy("DEP", "deploy").is_some()); // case-insensitive
    }

    #[test]
    fn consecutive_beats_scattered() {
        let a = fuzzy("dep", "deploy").unwrap().0;
        let b = fuzzy("dep", "d e p x").unwrap().0;
        assert!(a > b);
    }

    #[test]
    fn word_start_beats_mid_word() {
        let a = fuzzy("rel", "the release").unwrap().0;
        let b = fuzzy("rel", "the prelude").unwrap().0;
        assert!(a > b);
    }

    #[test]
    fn tokens_are_anded() {
        let m = Msg {
            id: "1".into(), chat_id: "c".into(), chat_title: "Ops".into(),
            who: "Ana".into(), text: "deploy finished".into(), when: String::new(), ts: 0,
        };
        assert!(score_msg(&["deploy", "fin"], &m).is_some());
        assert!(score_msg(&["deploy", "zzz"], &m).is_none());
        assert!(score_msg(&["ana", "deploy"], &m).is_some()); // sender matches
    }
}
