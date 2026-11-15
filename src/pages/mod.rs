pub mod calendar;
pub mod excel;
pub mod outlook;
pub mod teams;

use std::rc::Rc;

pub struct Page {
    pub widget: gtk::Widget,
    pub refresh: Rc<dyn Fn()>,
}
