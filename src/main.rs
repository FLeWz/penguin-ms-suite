mod appicon;
mod auth;
mod graph;
mod msgcache;
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

/// Softer light palette: off-white, warm greys instead of pure white. Applied only
/// while the effective scheme is light.
const SOFT_LIGHT: &str = "
@define-color window_bg_color #e6e4df;
@define-color window_fg_color #2e2e30;
@define-color view_bg_color #edece8;
@define-color view_fg_color #2e2e30;
@define-color headerbar_bg_color #dad8d2;
@define-color headerbar_fg_color #2e2e30;
@define-color card_bg_color #f0efeb;
@define-color popover_bg_color #edece8;
@define-color dialog_bg_color #e6e4df;
";

/// Softer dark palette: light-grey text instead of pure white, muted accent and
/// status colours (blue bubbles, green unread dot) instead of saturated ones.
const SOFT_DARK: &str = "
@define-color window_fg_color #cfd0d3;
@define-color view_fg_color #cfd0d3;
@define-color headerbar_fg_color #cfd0d3;
@define-color card_fg_color #cfd0d3;
@define-color popover_fg_color #cfd0d3;
@define-color accent_bg_color #3d5a80;
@define-color accent_color #8aa8cc;
@define-color accent_fg_color #e4e6ea;
@define-color success_color #6c9a78;
@define-color success_bg_color #3f6b4c;
@define-color warning_color #c2a35e;
@define-color error_color #c9777a;
@define-color destructive_color #c9777a;
@define-color destructive_bg_color #8a4a4d;
";

/// Applies to both schemes: the icon theme (e.g. Breeze) ships coloured icons for some of
/// the names we use; render them neutral so they match the text colour.
const ICON_CSS: &str = ".quiet-icon image { filter: grayscale(1) opacity(0.85); }";

#[derive(Clone, Copy, PartialEq)]
enum Theme {
    Dark,
    Light,
    System,
}

impl Theme {
    fn path() -> std::path::PathBuf {
        auth::dir().join("theme")
    }
    /// Default is dark: easiest on the eyes.
    fn load() -> Self {
        match std::fs::read_to_string(Self::path()).unwrap_or_default().trim() {
            "light" => Theme::Light,
            "system" => Theme::System,
            _ => Theme::Dark,
        }
    }
    fn save(self) {
        let v = match self {
            Theme::Dark => "dark",
            Theme::Light => "light",
            Theme::System => "system",
        };
        let _ = std::fs::create_dir_all(auth::dir());
        let _ = std::fs::write(Self::path(), v);
    }
    fn next(self) -> Self {
        match self {
            Theme::Dark => Theme::Light,
            Theme::Light => Theme::System,
            Theme::System => Theme::Dark,
        }
    }
    fn apply(self) {
        adw::StyleManager::default().set_color_scheme(match self {
            Theme::Dark => adw::ColorScheme::ForceDark,
            Theme::Light => adw::ColorScheme::ForceLight,
            Theme::System => adw::ColorScheme::Default,
        });
    }
    fn label(self) -> &'static str {
        match self {
            Theme::Dark => "Theme: dark (click for light)",
            Theme::Light => "Theme: light, soft (click for system)",
            Theme::System => "Theme: follow system (click for dark)",
        }
    }
}

/// Install the soft palettes and keep the right one active for the effective scheme.
fn install_palettes() {
    let Some(display) = gtk::gdk::Display::default() else { return };
    let (light, dark) = (gtk::CssProvider::new(), gtk::CssProvider::new());
    light.load_from_data(&format!("{SOFT_LIGHT}{ICON_CSS}"));
    dark.load_from_data(&format!("{SOFT_DARK}{ICON_CSS}"));
    let sync = move |sm: &adw::StyleManager| {
        let (on, off) = if sm.is_dark() { (&dark, &light) } else { (&light, &dark) };
        gtk::style_context_remove_provider_for_display(&display, off);
        gtk::style_context_add_provider_for_display(&display, on, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
    };
    let sm = adw::StyleManager::default();
    sync(&sm);
    sm.connect_dark_notify(sync);
}

fn saved_size(key: &str, default: i32) -> i32 {
    util::ui_get(key)
        .and_then(|v| v.as_i64())
        .map(|v| v as i32)
        .filter(|v| (400..=10000).contains(v))
        .unwrap_or(default)
}

/// Save the window size and maximized state (size only while not maximized or fullscreen),
/// shortly after changes and on close.
fn remember_window(win: &adw::ApplicationWindow) {
    let save = {
        let win = win.clone();
        move || {
            let maximized = win.is_maximized();
            util::ui_set("window_maximized", serde_json::json!(maximized));
            if !maximized && !win.is_fullscreen() && win.width() >= 400 && win.height() >= 400 {
                util::ui_set("window_width", serde_json::json!(win.width()));
                util::ui_set("window_height", serde_json::json!(win.height()));
            }
        }
    };
    let epoch = Rc::new(std::cell::Cell::new(0u32));
    let schedule = {
        let (save, epoch) = (save.clone(), epoch.clone());
        move || {
            epoch.set(epoch.get() + 1);
            let (g, epoch, save) = (epoch.get(), epoch.clone(), save.clone());
            gtk::glib::timeout_add_local_once(std::time::Duration::from_millis(600), move || {
                if epoch.get() == g {
                    save();
                }
            });
        }
    };
    {
        let s = schedule.clone();
        win.connect_default_width_notify(move |_| s());
        let s = schedule.clone();
        win.connect_default_height_notify(move |_| s());
        let s = schedule.clone();
        win.connect_maximized_notify(move |_| s());
    }
    win.connect_close_request(move |_| {
        save();
        gtk::glib::Propagation::Proceed
    });
}

/// glibc malloc tuning. Worker threads each get their own arena and large freed buffers
/// (image bytes, decoded pixels) stay mapped, which cost 30-60 MB RSS in a busy chat. One
/// arena, early trimming and mmap for big blocks return the memory to the system.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn tune_malloc() {
    unsafe extern "C" {
        fn mallopt(param: i32, value: i32) -> i32;
    }
    const M_TRIM_THRESHOLD: i32 = -1;
    const M_MMAP_THRESHOLD: i32 = -3;
    const M_ARENA_MAX: i32 = -8;
    // SAFETY: plain glibc call, made before any other thread exists.
    unsafe {
        mallopt(M_ARENA_MAX, 1);
        mallopt(M_TRIM_THRESHOLD, 64 * 1024);
        mallopt(M_MMAP_THRESHOLD, 128 * 1024);
    }
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn tune_malloc() {}

fn main() -> gtk::glib::ExitCode {
    tune_malloc();
    std::thread::spawn(msgcache::trim);
    // Cairo renderer: ~3x lower RSS than GL/Vulkan for this list/text UI.
    // Respect an explicit user choice.
    if std::env::var_os("GSK_RENDERER").is_none() {
        // SAFETY: single-threaded here, before GTK init or any thread spawn.
        unsafe { std::env::set_var("GSK_RENDERER", "cairo") };
    }
    gtk::glib::set_application_name(appicon::TITLE);
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_startup(|_| appicon::setup(APP_ID));
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
        b.add_css_class("quiet-icon");
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
        .icon_name(APP_ID)
        .title(appicon::TITLE)
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

    let title = adw::WindowTitle::new(appicon::TITLE, "");
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
    let theme = Rc::new(std::cell::Cell::new(Theme::load()));
    let theme_btn = gtk::Button::from_icon_name("weather-clear-night-symbolic");
    theme_btn.add_css_class("quiet-icon");
    theme_btn.set_tooltip_text(Some(theme.get().label()));
    {
        let theme = theme.clone();
        theme_btn.connect_clicked(move |b| {
            let t = theme.get().next();
            theme.set(t);
            t.apply();
            t.save();
            b.set_tooltip_text(Some(t.label()));
        });
    }
    install_palettes();
    theme.get().apply();
    header.pack_start(&refresh_btn);
    header.pack_end(&theme_btn);
    header.pack_end(&out_btn);

    let toasts = adw::ToastOverlay::new();
    util::set_toaster(&toasts);
    let tv = adw::ToolbarView::new();
    tv.add_top_bar(&header);
    tv.set_content(Some(&root));
    // Full-window layer for the in-app image viewer, below the toasts.
    let layer = gtk::Overlay::new();
    layer.set_child(Some(&tv));
    pages::lightbox::set_host(&layer);
    toasts.set_child(Some(&layer));

    let win = adw::ApplicationWindow::builder()
        .application(app)
        .title(appicon::TITLE)
        .default_width(saved_size("window_width", 1100))
        .default_height(saved_size("window_height", 720))
        .content(&toasts)
        .build();
    if util::ui_get("window_maximized").and_then(|v| v.as_bool()) == Some(true) {
        win.maximize();
    }
    remember_window(&win);
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
