mod auth;
mod graph;
mod pages;
mod relay;
mod util;

use adw::prelude::*;
use pages::Page;
use std::{cell::RefCell, rc::Rc};

const APP_ID: &str = "dev.penguin.MsSuite";

type Builder = fn() -> Page;
// (name, icon, builder, required Graph permission)
const APPS: [(&str, &str, Builder, &str); 4] = [
    ("Teams", "user-available-symbolic", pages::teams::build, "Chat.ReadWrite"),
    ("Outlook", "mail-unread-symbolic", pages::outlook::build, "Mail.Read"),
    ("Calendar", "x-office-calendar-symbolic", pages::calendar::build, "Calendars.Read"),
    ("Excel", "x-office-spreadsheet-symbolic", pages::excel::build, "Files.Read"),
];

fn main() -> gtk::glib::ExitCode {
    // Cairo renderer: ~3x lower RSS than GL/Vulkan for this list/text UI.
    // Respect an explicit user choice.
    if std::env::var_os("GSK_RENDERER").is_none() {
        // SAFETY: single-threaded here, before GTK init or any thread spawn.
        unsafe { std::env::set_var("GSK_RENDERER", "cairo") };
    }
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_activate(build_ui);
    app.run()
}

fn build_ui(app: &adw::Application) {
    // Pages are built lazily on first visit: unused apps cost no memory.
    let pages_stack = gtk::Stack::new();
    pages_stack.set_hexpand(true);
    let built: Rc<RefCell<[Option<Rc<dyn Fn()>>; 4]>> = Default::default();
    let current = Rc::new(std::cell::Cell::new(0usize));

    let select = {
        let (stack, built, current) = (pages_stack.clone(), built.clone(), current.clone());
        Rc::new(move |i: usize| {
            current.set(i);
            let existing = built.borrow()[i].clone();
            match existing {
                Some(_) => stack.set_visible_child_name(APPS[i].0),
                None => {
                    let p = if auth::granted(APPS[i].3) {
                        (APPS[i].2)()
                    } else {
                        pages::locked(APPS[i].0, APPS[i].3)
                    };
                    stack.add_named(&p.widget, Some(APPS[i].0));
                    stack.set_visible_child_name(APPS[i].0);
                    built.borrow_mut()[i] = Some(p.refresh.clone());
                    (p.refresh)();
                }
            }
        })
    };

    // Thin sidebar.
    let sidebar = gtk::Box::new(gtk::Orientation::Vertical, 4);
    sidebar.set_margin_top(6);
    sidebar.set_margin_bottom(6);
    sidebar.set_margin_start(4);
    sidebar.set_margin_end(4);
    let mut first: Option<gtk::ToggleButton> = None;
    let mut buttons = vec![];
    for (i, (name, icon, _, _)) in APPS.iter().enumerate() {
        let b = gtk::ToggleButton::new();
        b.set_icon_name(icon);
        b.set_tooltip_text(Some(name));
        b.add_css_class("flat");
        match &first {
            Some(f) => b.set_group(Some(f)),
            None => first = Some(b.clone()),
        }
        let select = select.clone();
        b.connect_toggled(move |b| {
            if b.is_active() {
                select(i)
            }
        });
        sidebar.append(&b);
        buttons.push(b);
    }

    let app_view = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    app_view.append(&sidebar);
    app_view.append(&gtk::Separator::new(gtk::Orientation::Vertical));
    app_view.append(&pages_stack);

    // Login view.
    let login = adw::StatusPage::builder()
        .icon_name("avatar-default-symbolic")
        .title("Penguin MS Suite")
        .description("Sign in with your Microsoft 365 account")
        .build();
    let sign_in = gtk::Button::with_label("Sign in");
    sign_in.add_css_class("suggested-action");
    sign_in.add_css_class("pill");
    sign_in.set_halign(gtk::Align::Center);
    login.set_child(Some(&sign_in));

    let root = gtk::Stack::new();
    root.add_named(&login, Some("login"));
    root.add_named(&app_view, Some("app"));

    let title = adw::WindowTitle::new("Penguin MS Suite", "");
    let show_app = {
        let (root, buttons, select, title) =
            (root.clone(), buttons.clone(), select.clone(), title.clone());
        Rc::new(move || {
            let title = title.clone();
            util::run_ok(
                || graph::get("/me?$select=displayName,userPrincipalName"),
                move |v| {
                    let n = v["displayName"].as_str().unwrap_or("");
                    let m = v["userPrincipalName"].as_str().unwrap_or("");
                    title.set_subtitle(&format!("{n} · {m}"));
                },
            );
            root.set_visible_child_name("app");
            if buttons[0].is_active() {
                select(0);
            } else {
                buttons[0].set_active(true);
            }
        })
    };

    {
        let (login, show_app) = (login.clone(), show_app.clone());
        sign_in.connect_clicked(move |btn| {
            btn.set_sensitive(false);
            let (login, btn, show_app) = (login.clone(), btn.clone(), show_app.clone());
            login.set_description(Some("Complete sign-in in your browser…"));
            util::run(auth::browser_login, move |r| match r {
                Ok(()) => show_app(),
                Err(e) => {
                    login.set_description(Some(&e));
                    btn.set_sensitive(true);
                }
            });
        });
    }

    // Header.
    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&title));
    let refresh_btn = gtk::Button::from_icon_name("view-refresh-symbolic");
    refresh_btn.set_tooltip_text(Some("Refresh"));
    {
        let (built, current) = (built.clone(), current.clone());
        refresh_btn.connect_clicked(move |_| {
            let r = built.borrow()[current.get()].clone();
            if let Some(r) = r {
                r();
            }
        });
    }
    let out_btn = gtk::Button::from_icon_name("system-log-out-symbolic");
    out_btn.set_tooltip_text(Some("Sign out"));
    {
        let (root, stack, built, sign_in) =
            (root.clone(), pages_stack.clone(), built.clone(), sign_in.clone());
        out_btn.connect_clicked(move |_| {
            auth::sign_out();
            for (i, slot) in built.borrow_mut().iter_mut().enumerate() {
                if slot.take().is_some() {
                    if let Some(w) = stack.child_by_name(APPS[i].0) {
                        stack.remove(&w);
                    }
                }
            }
            sign_in.set_sensitive(true);
            root.set_visible_child_name("login");
        });
    }
    header.pack_start(&refresh_btn);
    header.pack_end(&out_btn);

    let toasts = adw::ToastOverlay::new();
    util::set_toaster(&toasts);
    let tv = adw::ToolbarView::new();
    tv.add_top_bar(&header);
    tv.set_content(Some(&root));
    toasts.set_child(Some(&tv));

    let win = adw::ApplicationWindow::builder()
        .application(app)
        .title("Penguin MS Suite")
        .default_width(1100)
        .default_height(720)
        .content(&toasts)
        .build();
    win.present();
    // Target of notification clicks.
    let raise = gtk::gio::SimpleAction::new("raise", None);
    {
        let win = win.clone();
        raise.connect_activate(move |_, _| win.present());
    }
    app.add_action(&raise);

    if auth::has_session() {
        show_app();
    } else {
        root.set_visible_child_name("login");
        if let Err(e) = auth::config() {
            login.set_description(Some(&e));
        }
    }
}
