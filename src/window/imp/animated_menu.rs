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

    fn configure_popover(btn: &gtk4::MenuButton, popover: &gtk4::Popover) {
        popover.set_halign(gtk4::Align::Start);
        popover.set_has_arrow(false);
        popover.set_position(gtk4::PositionType::Bottom);

        // The menu sits flush with the window's left edge and flush to the bottom of the title bar.
        // `halign(Start)` lines the popup *surface* up with the button, so shift it left by the
        // button's distance from the window edge, and vertically to meet the bottom of the title
        // bar. `inset_x` and `inset_y` track how far the menu's visible edge sits inside its surface
        // (the theme's shadow margin / padding). The insets are measured from where the popup
        // actually landed, then remembered so later opens start in place without jumping.
        let inset_x = Rc::new(Cell::new(0i32));
        let inset_y = Rc::new(Cell::new(0i32));
        let phase = Rc::new(Cell::new(Align::Measure));
        let btn_weak = btn.downgrade();
        let (inset_x_show, inset_y_show, phase_for_show) =
            (inset_x.clone(), inset_y.clone(), phase.clone());
        let btn_show = btn_weak.clone();
        popover.connect_notify_local(Some("visible"), move |pop, _| {
            if !pop.is_visible() {
                return;
            }
            phase_for_show.set(Align::Measure);
            let Some(btn) = btn_show.upgrade() else {
                return;
            };
            let Some(root) = btn.root() else {
                return;
            };
            let origin = gtk4::graphene::Point::new(0.0, 0.0);
            if let Some(at) = btn.compute_point(&root, &origin) {
                let init_y = if let Some(target_y) = target_top_in_window(&btn) {
                    let btn_bottom = at.y().round() as i32 + btn.height();
                    target_y - btn_bottom - inset_y_show.get()
                } else {
                    -inset_y_show.get()
                };
                pop.set_offset(-(at.x().round() as i32) - inset_x_show.get(), init_y);
            }
        });
        popover.connect_realize(move |pop| {
            let Some(surface) = pop.surface() else {
                return;
            };
            let pop_weak = pop.downgrade();
            let (inset_x, inset_y, phase) = (inset_x.clone(), inset_y.clone(), phase.clone());
            let btn_layout = btn_weak.clone();
            surface.connect_layout(move |_, _, _| {
                let Some(pop) = pop_weak.upgrade() else {
                    return;
                };
                if phase.get() == Align::Done {
                    return;
                }
                let Some((vis_x, vis_y)) = visible_pos_in_window(&pop) else {
                    return;
                };
                let target_y = btn_layout
                    .upgrade()
                    .and_then(|b| target_top_in_window(&b))
                    .unwrap_or(vis_y);
                let gap_x = vis_x;
                let gap_y = vis_y - target_y;
                match phase.get() {
                    Align::Measure if gap_x != 0 || gap_y != 0 => {
                        inset_x.set(inset_x.get() + gap_x);
                        inset_y.set(inset_y.get() + gap_y);
                        let (x, y) = pop.offset();
                        pop.set_offset(x - gap_x, y - gap_y);
                        phase.set(Align::Verify);
                    }
                    // A gap left after the correction is a constraint (the monitor edge of a
                    // maximized window), not the theme: forget it so the next open is not
                    // shifted by it, and stop rather than chase the edge.
                    Align::Verify => {
                        inset_x.set(inset_x.get() - gap_x);
                        inset_y.set(inset_y.get() - gap_y);
                        phase.set(Align::Done);
                    }
                    _ => phase.set(Align::Done),
                }
            });
        });
    }

    /// Progress of one opening's alignment, driven by the popup surface's `layout` signal.
    #[derive(Clone, Copy, PartialEq)]
    enum Align {
        Measure,
        Verify,
        Done,
    }

    /// Where the bottom of the title bar sits in the window's coordinates,
    /// falling back to the bottom of the button itself if no header is found.
    fn target_top_in_window(btn: &gtk4::MenuButton) -> Option<i32> {
        let root = btn.root()?;
        if let Some(header) = btn
            .ancestor(adw::HeaderBar::static_type())
            .or_else(|| btn.ancestor(gtk4::HeaderBar::static_type()))
        {
            let bottom_pt = gtk4::graphene::Point::new(0.0, header.height() as f32);
            if let Some(p) = header.compute_point(&root, &bottom_pt) {
                return Some(p.y().round() as i32);
            }
        }
        let bottom_pt = gtk4::graphene::Point::new(0.0, btn.height() as f32);
        btn.compute_point(&root, &bottom_pt)
            .map(|p| p.y().round() as i32)
    }

    /// Where the popover's visible menu box starts, in its window's coordinates.
    fn visible_pos_in_window(pop: &gtk4::Popover) -> Option<(i32, i32)> {
        let popup = pop.surface()?.downcast::<gtk4::gdk::Popup>().ok()?;
        let (pop_tx, pop_ty) = pop.surface_transform();
        let (win_tx, win_ty) = pop.parent()?.native()?.surface_transform();
        let child = pop
            .child()?
            .compute_point(pop, &gtk4::graphene::Point::new(0.0, 0.0))?;
        let x = f64::from(popup.position_x()) + pop_tx + f64::from(child.x()) - win_tx;
        let y = f64::from(popup.position_y()) + pop_ty + f64::from(child.y()) - win_ty;
        Some((x.round() as i32, y.round() as i32))
    }

    if let Some(popover) = menu_btn.popover() {
        configure_popover(&menu_btn, &popover);
        let target_for_pop = target.clone();
        let start_anim_for_pop = start_animation.clone();
        popover.connect_closed(move |_| {
            target_for_pop.set(0.0);
            start_anim_for_pop();
        });
    }

    menu_btn.connect_notify_local(Some("popover"), move |btn, _| {
        if let Some(popover) = btn.popover() {
            configure_popover(btn, &popover);
        }
    });

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
