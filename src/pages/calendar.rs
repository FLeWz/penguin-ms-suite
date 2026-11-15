use super::Page;
use crate::{graph, util};
use adw::prelude::*;
use gtk::glib;
use std::rc::Rc;

fn local_time(iso_utc: &str) -> String {
    let s = format!("{}Z", iso_utc.split('.').next().unwrap_or(iso_utc));
    glib::DateTime::from_iso8601(&s, None)
        .and_then(|d| d.to_local())
        .and_then(|d| d.format("%H:%M"))
        .map(|g| g.to_string())
        .unwrap_or_else(|_| iso_utc.into())
}

pub fn build() -> Page {
    let cal = gtk::Calendar::new();
    cal.set_margin_top(8);
    cal.set_margin_start(8);
    cal.set_margin_end(8);
    let list = gtk::ListBox::new();
    list.add_css_class("navigation-sidebar");
    list.set_selection_mode(gtk::SelectionMode::None);

    let left = gtk::Box::new(gtk::Orientation::Vertical, 0);
    left.append(&cal);
    left.set_valign(gtk::Align::Start);
    let root = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    root.append(&left);
    root.append(&gtk::Separator::new(gtk::Orientation::Vertical));
    let right = util::pane(&list);
    right.set_hexpand(true);
    root.append(&right);

    let load = {
        let (cal, list) = (cal.clone(), list.clone());
        move || {
            let d = cal.date();
            let Ok(start) = glib::DateTime::new(
                &glib::TimeZone::local(), d.year(), d.month(), d.day_of_month(), 0, 0, 0.0,
            ) else { return };
            let Ok(end) = start.add_days(1) else { return };
            let f = |t: &glib::DateTime| {
                t.to_utc()
                    .and_then(|u| u.format("%Y-%m-%dT%H:%M:%SZ"))
                    .map(|g| g.to_string())
                    .unwrap_or_default()
            };
            let path = format!(
                "/me/calendarView?startDateTime={}&endDateTime={}&$orderby=start/dateTime\
                 &$top=100&$select=subject,start,end,location,isAllDay",
                f(&start),
                f(&end)
            );
            let list = list.clone();
            util::run_ok(
                move || graph::get_h(&path, &[("Prefer", "outlook.timezone=\"UTC\"")]),
                move |v| {
                    util::clear(&list);
                    let items = v["value"].as_array().cloned().unwrap_or_default();
                    if items.is_empty() {
                        list.append(&util::row("No events", ""));
                    }
                    for e in items {
                        let when = if e["isAllDay"] == true {
                            "All day".to_string()
                        } else {
                            format!(
                                "{} – {}",
                                local_time(e["start"]["dateTime"].as_str().unwrap_or("")),
                                local_time(e["end"]["dateTime"].as_str().unwrap_or(""))
                            )
                        };
                        let loc = e["location"]["displayName"].as_str().unwrap_or("");
                        let sub = if loc.is_empty() { when } else { format!("{when}  ·  {loc}") };
                        list.append(&util::row(e["subject"].as_str().unwrap_or("(no title)"), &sub));
                    }
                },
            );
        }
    };
    let load = Rc::new(load);
    {
        let load = load.clone();
        cal.connect_day_selected(move |_| load());
    }
    Page { widget: root.upcast(), refresh: load }
}
