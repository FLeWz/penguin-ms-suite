//! App icon and public name. The icon is embedded in the binary and installed per user as
//! `dev.penguin.MsSuite` in the hicolor theme with a matching desktop entry, so the window,
//! taskbar and notifications show it (Wayland resolves both through the application id).
use std::path::PathBuf;

const ICON: &[u8] = include_bytes!("../icon.png");
pub const TITLE: &str = "Penguin MS Suite";

fn data_home() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn write_if_changed(path: &std::path::Path, bytes: &[u8]) {
    if std::fs::read(path).is_ok_and(|old| old == bytes) {
        return;
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, bytes);
}

/// Install icon and desktop entry (best effort) and make GTK use the icon. Call once GTK is up.
pub fn setup(app_id: &str) {
    let home = data_home();
    write_if_changed(&home.join(format!("icons/hicolor/512x512/apps/{app_id}.png")), ICON);
    if let Ok(exe) = std::env::current_exe() {
        let entry = format!(
            "[Desktop Entry]\nType=Application\nName={TITLE}\nComment=Microsoft 365 client\n\
             Exec={}\nIcon={app_id}\nCategories=Network;Office;\nStartupWMClass={app_id}\n",
            exe.display()
        );
        write_if_changed(&home.join(format!("applications/{app_id}.desktop")), entry.as_bytes());
    }
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::IconTheme::for_display(&display).add_search_path(home.join("icons"));
    }
    gtk::Window::set_default_icon_name(app_id);
}
