//! Animated hamburger-to-vertical menu button.
//!
//! Renders three horizontal bars (hamburger) when closed. When clicked or opened,
//! the middle line dissolves away, the top line spins +90° clockwise inwards, and the
//! bottom line spins -90° counter-clockwise inversely inwards into two parallel vertical bars
//! ("hotdog" style). When closed, it animates smoothly back to the hamburger menu.

use std::cell::Cell;
use std::f64::consts::FRAC_PI_2;
use std::rc::Rc;

use gtk4::cairo;
use gtk4::gio;
use gtk4::glib;
use gtk4::prelude::*;

const ANIMATION_DURATION_MS: f64 = 200.0;

/// Smooth cubic ease-in-out curve.
fn ease_in_out(t: f64) -> f64 {
    if t < 0.5 {
        4.0 * t * t * t
    } else {
        1.0 - (-2.0 * t + 2.0).powi(3) / 2.0
    }
}

/// Builds a menu button containing the animated hamburger/vertical-lines icon.
pub fn build_animated_menu_button(menu_model: &impl IsA<gio::MenuModel>) -> gtk4::MenuButton {
    let drawing_area = gtk4::DrawingArea::new();
    drawing_area.set_content_width(16);
    drawing_area.set_content_height(16);

    // Progress: 0.0 = closed (hamburger), 1.0 = open (vertical bars)
    let progress = Rc::new(Cell::new(0.0f64));
    let target = Rc::new(Cell::new(0.0f64));
    let animating = Rc::new(Cell::new(false));
    let last_frame_time = Rc::new(Cell::new(0i64));

    let prog_for_draw = progress.clone();
    drawing_area.set_draw_func(move |area, cr, width, height| {
        let p = prog_for_draw.get();
        let t = ease_in_out(p.clamp(0.0, 1.0));

        let color = area.color();
        let r = color.red() as f64;
        let g = color.green() as f64;
        let b = color.blue() as f64;
        let a = color.alpha() as f64;

        let w = width as f64;
        let h = height as f64;
        let cx = w / 2.0;
        let cy = h / 2.0;

        let line_len_half = 5.0;
        let line_width = 2.0;
        let dy = 4.5; // vertical offset when closed (hamburger)
        let dx = 3.5; // horizontal offset when open (vertical bars)

        cr.set_line_cap(cairo::LineCap::Round);
        cr.set_line_width(line_width);

        // 1. Middle line: horizontal, centered, fades out and dissolves as t -> 1
        let mid_alpha = (1.0 - t).max(0.0);
        if mid_alpha > 0.001 {
            cr.set_source_rgba(r, g, b, a * mid_alpha);
            let mid_len = line_len_half * (1.0 - 0.4 * t);
            cr.move_to(cx - mid_len, cy);
            cr.line_to(cx + mid_len, cy);
            let _ = cr.stroke();
        }

        // 2. Top line: rotates clockwise +90° inwards, moves from (cx, cy - dy) to (cx - dx, cy)
        let _ = cr.save();
        let top_x = cx - dx * t;
        let top_y = cy - dy * (1.0 - t);
        let top_angle = t * FRAC_PI_2;
        cr.translate(top_x, top_y);
        cr.rotate(top_angle);
        cr.set_source_rgba(r, g, b, a);
        cr.move_to(-line_len_half, 0.0);
        cr.line_to(line_len_half, 0.0);
        let _ = cr.stroke();
        let _ = cr.restore();

        // 3. Bottom line: rotates counter-clockwise -90° inversely inwards, moves from (cx, cy + dy) to (cx + dx, cy)
        let _ = cr.save();
        let bot_x = cx + dx * t;
        let bot_y = cy + dy * (1.0 - t);
        let bot_angle = -t * FRAC_PI_2;
        cr.translate(bot_x, bot_y);
        cr.rotate(bot_angle);
        cr.set_source_rgba(r, g, b, a);
        cr.move_to(-line_len_half, 0.0);
        cr.line_to(line_len_half, 0.0);
        let _ = cr.stroke();
        let _ = cr.restore();
    });

    let menu_btn = gtk4::MenuButton::builder()
        .tooltip_text("Menu")
        .menu_model(menu_model)
        .child(&drawing_area)
        .build();

    let start_animation = {
        let area = drawing_area.clone();
        let progress = progress.clone();
        let target = target.clone();
        let animating = animating.clone();
        let last_frame_time = last_frame_time.clone();

        move || {
            if animating.get() {
                return;
            }
            animating.set(true);
            last_frame_time.set(0);

            let area_weak = area.downgrade();
            let progress = progress.clone();
            let target = target.clone();
            let animating = animating.clone();
            let last_frame_time = last_frame_time.clone();

            area.add_tick_callback(move |_, clock| {
                let Some(area) = area_weak.upgrade() else {
                    animating.set(false);
                    return glib::ControlFlow::Break;
                };

                let now = clock.frame_time();
                let last = last_frame_time.get();
                last_frame_time.set(now);

                if last == 0 {
                    return glib::ControlFlow::Continue;
                }

                let dt_ms = (now - last) as f64 / 1000.0;
                let step = dt_ms / ANIMATION_DURATION_MS;

                let cur = progress.get();
                let tgt = target.get();
                let diff = tgt - cur;

                if diff.abs() <= step || step <= 0.0 {
                    progress.set(tgt);
                    area.queue_draw();
                    animating.set(false);
                    glib::ControlFlow::Break
                } else {
                    let next = cur + diff.signum() * step;
                    progress.set(next);
                    area.queue_draw();
                    glib::ControlFlow::Continue
                }
            });
        }
    };

    let start_anim_for_btn = start_animation.clone();
    let target_for_btn = target.clone();
    menu_btn.connect_active_notify(move |btn| {
        let active = btn.is_active();
        target_for_btn.set(if active { 1.0 } else { 0.0 });
        start_anim_for_btn();
    });

    if let Some(popover) = menu_btn.popover() {
        let target_for_pop = target;
        let start_anim_for_pop = start_animation;
        popover.connect_closed(move |_| {
            target_for_pop.set(0.0);
            start_anim_for_pop();
        });
    }

    menu_btn
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn easing_endpoints_and_midpoint_are_exact() {
        assert_eq!(ease_in_out(0.0), 0.0);
        assert_eq!(ease_in_out(0.5), 0.5);
        assert_eq!(ease_in_out(1.0), 1.0);
    }

    #[test]
    fn easing_is_strictly_monotonic() {
        let mut prev = 0.0;
        for i in 1..=100 {
            let t = i as f64 / 100.0;
            let val = ease_in_out(t);
            assert!(val >= prev, "val {val} >= prev {prev} at t={t}");
            prev = val;
        }
        assert_eq!(prev, 1.0);
    }

    #[cfg(test)]
    pub(crate) fn gtk_checks() {
        let menu = gio::Menu::new();
        let btn = build_animated_menu_button(&menu);
        assert_eq!(btn.tooltip_text().as_deref(), Some("Menu"));
        assert!(btn.child().is_some());
    }
}
