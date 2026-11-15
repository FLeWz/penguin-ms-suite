use super::Page;
use crate::{graph, util};
use adw::prelude::*;
use serde_json::Value;
use std::{cell::RefCell, rc::Rc};

const MAX_ROWS: usize = 100;
const MAX_COLS: usize = 26;

fn col_name(mut i: usize) -> String {
    let mut s = String::new();
    loop {
        s.insert(0, (b'A' + (i % 26) as u8) as char);
        if i < 26 { break; }
        i = i / 26 - 1;
    }
    s
}

fn cell_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        o => o.to_string(),
    }
}

pub fn build() -> Page {
    let files = gtk::StringList::new(&[]);
    let sheets = gtk::StringList::new(&[]);
    let file_dd = gtk::DropDown::new(Some(files.clone()), gtk::Expression::NONE);
    let sheet_dd = gtk::DropDown::new(Some(sheets.clone()), gtk::Expression::NONE);
    file_dd.set_hexpand(true);
    let bar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    for w in [&file_dd, &sheet_dd] {
        bar.append(w);
    }
    bar.set_margin_top(6);
    bar.set_margin_bottom(6);
    bar.set_margin_start(6);
    bar.set_margin_end(6);

    let grid = gtk::Grid::new();
    grid.set_column_spacing(0);
    let scroll = gtk::ScrolledWindow::builder().child(&grid).vexpand(true).hexpand(true).build();
    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.append(&bar);
    root.append(&scroll);

    let ids: Rc<RefCell<Vec<String>>> = Default::default();
    let busy = Rc::new(std::cell::Cell::new(false)); // suppress handlers during programmatic updates

    let load_sheet = {
        let (ids, grid, file_dd, sheet_dd) = (ids.clone(), grid.clone(), file_dd.clone(), sheet_dd.clone());
        Rc::new(move || {
            let Some(id) = ids.borrow().get(file_dd.selected() as usize).cloned() else { return };
            let Some(name) = sheet_dd.selected_item().and_downcast::<gtk::StringObject>() else { return };
            let path = format!(
                "/me/drive/items/{id}/workbook/worksheets/{}/usedRange(valuesOnly=true)?$select=values",
                graph::enc(&name.string())
            );
            let grid = grid.clone();
            util::run_ok(move || graph::get(&path), move |v| {
                while let Some(c) = grid.first_child() {
                    grid.remove(&c);
                }
                let rows = v["values"].as_array().cloned().unwrap_or_default();
                for c in 0..rows.first().and_then(|r| r.as_array()).map_or(0, |r| r.len()).min(MAX_COLS) {
                    let h = gtk::Label::new(Some(&col_name(c)));
                    h.add_css_class("dim-label");
                    grid.attach(&h, c as i32 + 1, 0, 1, 1);
                }
                for (r, row) in rows.iter().take(MAX_ROWS).enumerate() {
                    let n = gtk::Label::new(Some(&(r + 1).to_string()));
                    n.add_css_class("dim-label");
                    n.set_margin_end(8);
                    grid.attach(&n, 0, r as i32 + 1, 1, 1);
                    for (c, val) in row.as_array().into_iter().flatten().take(MAX_COLS).enumerate() {
                        let l = gtk::Label::new(Some(&cell_text(val)));
                        l.set_xalign(0.0);
                        l.set_width_chars(12);
                        l.set_max_width_chars(24);
                        l.set_ellipsize(gtk::pango::EllipsizeMode::End);
                        l.set_margin_end(10);
                        l.set_margin_top(2);
                        l.set_margin_bottom(2);
                        grid.attach(&l, c as i32 + 1, r as i32 + 1, 1, 1);
                    }
                }
                if rows.len() > MAX_ROWS {
                    util::toast(&format!("Showing first {MAX_ROWS} of {} rows", rows.len()));
                }
            });
        })
    };

    let load_sheets = {
        let (ids, sheets, file_dd, busy, load_sheet) =
            (ids.clone(), sheets.clone(), file_dd.clone(), busy.clone(), load_sheet.clone());
        Rc::new(move || {
            let Some(id) = ids.borrow().get(file_dd.selected() as usize).cloned() else { return };
            let (sheets, busy, load_sheet) = (sheets.clone(), busy.clone(), load_sheet.clone());
            util::run_ok(
                move || graph::get(&format!("/me/drive/items/{id}/workbook/worksheets?$select=name")),
                move |v| {
                    busy.set(true);
                    sheets.splice(0, sheets.n_items(), &[]);
                    for s in v["value"].as_array().into_iter().flatten() {
                        if let Some(n) = s["name"].as_str() {
                            sheets.append(n);
                        }
                    }
                    busy.set(false);
                    load_sheet();
                },
            );
        })
    };

    {
        let (busy, f) = (busy.clone(), load_sheets.clone());
        file_dd.connect_selected_notify(move |_| if !busy.get() { f() });
    }
    {
        let (busy, f) = (busy.clone(), load_sheet.clone());
        sheet_dd.connect_selected_notify(move |_| if !busy.get() { f() });
    }

    let refresh: Rc<dyn Fn()> = Rc::new(move || {
        let (files, ids, busy, load_sheets) = (files.clone(), ids.clone(), busy.clone(), load_sheets.clone());
        util::run_ok(
            || graph::get("/me/drive/root/search(q='.xlsx')?$select=id,name&$top=50"),
            move |v| {
                busy.set(true);
                files.splice(0, files.n_items(), &[]);
                ids.borrow_mut().clear();
                for f in v["value"].as_array().into_iter().flatten() {
                    if let (Some(id), Some(n)) = (f["id"].as_str(), f["name"].as_str()) {
                        if n.ends_with(".xlsx") {
                            ids.borrow_mut().push(id.into());
                            files.append(n);
                        }
                    }
                }
                busy.set(false);
                if ids.borrow().is_empty() {
                    util::toast("No .xlsx files in OneDrive");
                } else {
                    load_sheets();
                }
            },
        );
    });
    Page { widget: root.upcast(), refresh }
}
