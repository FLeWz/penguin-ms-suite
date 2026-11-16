pub mod calendar;
pub mod excel;
pub mod finder;
pub mod links;
pub mod media;
pub mod outlook;
pub mod teams;

use std::rc::Rc;

pub struct Page {
    pub widget: gtk::Widget,
    pub refresh: Rc<dyn Fn()>,
}

/// Placeholder shown when the tenant has not granted the needed permission.
pub fn locked(app: &str, perm: &str) -> Page {
    use adw::prelude::*;
    let w = adw::StatusPage::builder()
        .icon_name("dialog-password-symbolic")
        .title(format!("{app} needs admin approval"))
        .description(format!(
            "Permission {perm} has not been granted for this app.\nAsk your admin to approve it, then sign out and sign in again."
        ))
        .build();
    Page { widget: w.upcast(), refresh: Rc::new(|| {}) }
}
