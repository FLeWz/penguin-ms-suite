//! In-app image viewer: a dimmed layer over the whole window with zoom (wheel, pinch, +/-,
//! buttons) and pan (drag). Esc, the close button or a click outside the image closes it.
use adw::prelude::*;
use gtk::{gdk, glib};
use std::{cell::{Cell, RefCell}, rc::Rc};

const MIN_SCALE: f64 = 0.05;
const MAX_SCALE: f64 = 16.0;
const WHEEL_STEP: f64 = 1.2;

thread_local! {
    static HOST: RefCell<Option<gtk::Overlay>> = const { RefCell::new(None) };
}

/// The overlay that covers the whole window; the viewer is added on top of it.
pub fn set_host(o: &gtk::Overlay) {
    HOST.with(|h| *h.borrow_mut() = Some(o.clone()));
}

/// Show `tex` in the viewer. False when no host exists (caller falls back to a window).
pub fn show_image(tex: &gdk::Texture, name: &str) -> bool {
    let Some(host) = HOST.with(|h| h.borrow().clone()) else { return false };
    let (tw, th) = (tex.width() as f64, tex.height() as f64);
    if tw < 1.0 || th < 1.0 {
        return false;
    }

    let backdrop = gtk::Overlay::new();
    backdrop.add_css_class("lightbox");
    backdrop.set_focusable(true);
    backdrop.set_hexpand(true);
    backdrop.set_vexpand(true);

    let pic = gtk::Picture::for_paintable(tex);
    pic.set_content_fit(gtk::ContentFit::Fill);
    pic.set_can_shrink(true);
    pic.set_halign(gtk::Align::Center);
    pic.set_valign(gtk::Align::Center);
    pic.set_tooltip_text(Some(name));
    pic.set_cursor_from_name(Some("grab"));

    let scroll = gtk::ScrolledWindow::builder().child(&pic).hexpand(true).vexpand(true).build();
    scroll.set_kinetic_scrolling(false);
    backdrop.set_child(Some(&scroll));
    let (hadj, vadj) = (scroll.hadjustment(), scroll.vadjustment());

    let pct = gtk::Button::with_label("100%");
    pct.add_css_class("flat");
    pct.set_tooltip_text(Some("Fit to window"));
    let scale = Rc::new(Cell::new(1.0f64));
    let fit = Rc::new(Cell::new(1.0f64));
    // Scroll positions to apply once the resized picture has been laid out.
    let pending: Rc<Cell<(Option<f64>, Option<f64>)>> = Rc::new(Cell::new((None, None)));

    // Set the zoom, keeping the image point under (ax, ay) (viewport coordinates) in place.
    let apply: Rc<dyn Fn(f64, f64, f64)> = {
        let (scale, pic, scroll, pct, pending) =
            (scale.clone(), pic.clone(), scroll.clone(), pct.clone(), pending.clone());
        let (hadj, vadj) = (hadj.clone(), vadj.clone());
        Rc::new(move |new, ax, ay| {
            let new = new.clamp(MIN_SCALE, MAX_SCALE);
            let old = scale.get();
            let off = |view: f64, content: f64| ((view - content) / 2.0).max(0.0);
            let (vw, vh) = (scroll.width() as f64, scroll.height() as f64);
            let cx = (hadj.value() + ax - off(vw, tw * old)) / old;
            let cy = (vadj.value() + ay - off(vh, th * old)) / old;
            scale.set(new);
            pic.set_size_request((tw * new).round().max(1.0) as i32, (th * new).round().max(1.0) as i32);
            pct.set_label(&format!("{}%", (new * 100.0).round()));
            let target = (cx * new + off(vw, tw * new) - ax, cy * new + off(vh, th * new) - ay);
            pending.set((Some(target.0), Some(target.1)));
            glib::timeout_add_local_once(std::time::Duration::from_millis(150), {
                let pending = pending.clone();
                move || pending.set((None, None))
            });
        })
    };
    {
        let p1 = pending.clone();
        hadj.connect_changed(move |a| {
            if let (Some(h), v) = p1.get() {
                a.set_value(h);
                p1.set((None, v));
            }
        });
        let pending = pending.clone();
        vadj.connect_changed(move |a| {
            if let (h, Some(v)) = pending.get() {
                a.set_value(v);
                pending.set((h, None));
            }
        });
    }
    let center = {
        let scroll = scroll.clone();
        move || (scroll.width() as f64 / 2.0, scroll.height() as f64 / 2.0)
    };
    let fit_now: Rc<dyn Fn()> = {
        let (scroll, fit, apply, hadj, vadj) = (scroll.clone(), fit.clone(), apply.clone(), hadj.clone(), vadj.clone());
        Rc::new(move || {
            let f = (scroll.width() as f64 / tw).min(scroll.height() as f64 / th).min(1.0).max(MIN_SCALE);
            fit.set(f);
            apply(f, 0.0, 0.0);
            hadj.set_value(0.0);
            vadj.set_value(0.0);
        })
    };
    {
        // The first fit needs the real viewport size, known after the first layout.
        let fit_now = fit_now.clone();
        let done = Cell::new(false);
        scroll.add_tick_callback(move |s, _| {
            if done.get() {
                return glib::ControlFlow::Break;
            }
            if s.width() > 0 && s.height() > 0 {
                fit_now();
                done.set(true);
                return glib::ControlFlow::Break;
            }
            glib::ControlFlow::Continue
        });
    }
    {
        let fit_now = fit_now.clone();
        pct.connect_clicked(move |_| fit_now());
    }

    let close: Rc<dyn Fn()> = {
        let (host, backdrop) = (host.clone(), backdrop.clone());
        Rc::new(move || host.remove_overlay(&backdrop))
    };

    // Wheel zooms around the pointer.
    let wheel = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
    wheel.set_propagation_phase(gtk::PropagationPhase::Capture);
    {
        let (scale, apply, scroll) = (scale.clone(), apply.clone(), scroll.clone());
        let pos = Rc::new(Cell::new((0.0f64, 0.0f64)));
        let motion = gtk::EventControllerMotion::new();
        {
            let pos = pos.clone();
            motion.connect_motion(move |_, x, y| pos.set((x, y)));
        }
        scroll.add_controller(motion);
        wheel.connect_scroll(move |_, _, dy| {
            let (x, y) = pos.get();
            apply(scale.get() * WHEEL_STEP.powf(-dy), x, y);
            glib::Propagation::Stop
        });
    }
    scroll.add_controller(wheel);

    // Pinch.
    let pinch = gtk::GestureZoom::new();
    {
        let start = Rc::new(Cell::new(1.0f64));
        let (scale2, start2) = (scale.clone(), start.clone());
        pinch.connect_begin(move |_, _| start2.set(scale2.get()));
        let apply = apply.clone();
        pinch.connect_scale_changed(move |g, s| {
            let (x, y) = g.bounding_box_center().unwrap_or((0.0, 0.0));
            apply(start.get() * s, x, y);
        });
    }
    scroll.add_controller(pinch);

    // Drag to pan.
    let drag = gtk::GestureDrag::new();
    drag.set_button(1);
    {
        let start = Rc::new(Cell::new((0.0f64, 0.0f64)));
        let (s2, h, v) = (start.clone(), hadj.clone(), vadj.clone());
        drag.connect_drag_begin(move |_, _, _| s2.set((h.value(), v.value())));
        let (h, v) = (hadj.clone(), vadj.clone());
        drag.connect_drag_update(move |_, dx, dy| {
            let (sx, sy) = start.get();
            h.set_value(sx - dx);
            v.set_value(sy - dy);
        });
    }
    scroll.add_controller(drag);

    // Click outside the image closes; double click on it toggles fit and 100%.
    let click = gtk::GestureClick::new();
    click.set_button(1);
    {
        let (pic, scroll, close, scale, fit, apply, fit_now) =
            (pic.clone(), scroll.clone(), close.clone(), scale.clone(), fit.clone(), apply.clone(), fit_now.clone());
        click.connect_released(move |_, n, x, y| {
            let inside = pic
                .compute_bounds(&scroll)
                .is_some_and(|r| r.contains_point(&gtk::graphene::Point::new(x as f32, y as f32)));
            if !inside {
                close();
            } else if n == 2 {
                if (scale.get() - fit.get()).abs() < 0.01 {
                    apply(1.0, x, y);
                } else {
                    fit_now();
                }
            }
        });
    }
    scroll.add_controller(click);

    // Keys.
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    {
        let (close, apply, scale, fit_now) = (close.clone(), apply.clone(), scale.clone(), fit_now.clone());
        keys.connect_key_pressed(move |_, k, _, _| {
            use gtk::gdk::Key;
            let (cx, cy) = center();
            match k {
                Key::Escape => close(),
                Key::plus | Key::equal | Key::KP_Add => apply(scale.get() * WHEEL_STEP, cx, cy),
                Key::minus | Key::KP_Subtract => apply(scale.get() / WHEEL_STEP, cx, cy),
                Key::_0 | Key::KP_0 => fit_now(),
                _ => return glib::Propagation::Proceed,
            }
            glib::Propagation::Stop
        });
    }
    backdrop.add_controller(keys);

    // Controls.
    let closeb = gtk::Button::from_icon_name("window-close-symbolic");
    closeb.add_css_class("circular");
    closeb.add_css_class("osd");
    closeb.set_halign(gtk::Align::End);
    closeb.set_valign(gtk::Align::Start);
    closeb.set_margin_top(12);
    closeb.set_margin_end(12);
    closeb.set_tooltip_text(Some("Close (Esc)"));
    {
        let close = close.clone();
        closeb.connect_clicked(move |_| close());
    }
    backdrop.add_overlay(&closeb);

    let bar = gtk::Box::new(gtk::Orientation::Horizontal, 2);
    bar.add_css_class("osd");
    bar.add_css_class("toolbar");
    bar.set_halign(gtk::Align::Center);
    bar.set_valign(gtk::Align::End);
    bar.set_margin_bottom(16);
    for (icon, tip, factor) in [("zoom-out-symbolic", "Zoom out (-)", 1.0 / WHEEL_STEP), ("zoom-in-symbolic", "Zoom in (+)", WHEEL_STEP)] {
        let b = gtk::Button::from_icon_name(icon);
        b.add_css_class("flat");
        b.set_tooltip_text(Some(tip));
        let (apply, scale, scroll) = (apply.clone(), scale.clone(), scroll.clone());
        b.connect_clicked(move |_| {
            apply(scale.get() * factor, scroll.width() as f64 / 2.0, scroll.height() as f64 / 2.0)
        });
        if factor < 1.0 {
            bar.append(&b);
            bar.append(&pct);
        } else {
            bar.append(&b);
        }
    }
    backdrop.add_overlay(&bar);

    host.add_overlay(&backdrop);
    backdrop.grab_focus();
    true
}
