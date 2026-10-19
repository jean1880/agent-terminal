//! The closing screen's animation: the app icon packing itself up.
//!
//! The three agent dots wait on the floor beside the chat bubble. One after another they crouch,
//! hop up past its side and over its top edge, and drop into its screen (hidden for a moment
//! behind the rim, as if through a slot), landing with a squash. Then the screen closes like a lid from the
//! top down, swallowing them, and the bubble settles with a small bump. It holds shut, reopens
//! empty, the dots pop back onto the floor, and the loop starts again.
//!
//! [`frame_at`] is the whole timeline as a pure function of the loop position, so it is tested
//! without a display; [`PackAnimation`] draws it with cairo from a frame-clock tick callback. The
//! geometry is the app icon's (`assets/ca.nuvek.AgentTerminal.svg`, a 128-unit square), drawn
//! below a band of headroom for the hops and beside the dots' floor. The icon's colours are the brand's and stay fixed; the
//! widget draws no background, so the page behind it follows the theme. With animations turned
//! off (`gtk-enable-animations`), it shows the icon itself, still.

use std::cell::{Cell, RefCell};
use std::f64::consts::PI;
use std::rc::Rc;

use gtk4::cairo;
use gtk4::glib;
use gtk4::prelude::*;

use crate::brand::animations_enabled;

/// One loop, in seconds.
pub const LOOP_SECS: f64 = 1.8;

/// The canvas, in icon units: the bubble (x 12..116) and, to its lower right, the floor the dots
/// wait on; from [`TOP`] (headroom for the hops) down to [`BOTTOM`] (the floor's shadows). Wide
/// enough on both sides that the bubble itself is centred, over the step line.
const RIGHT: f64 = 166.0;
const LEFT: f64 = 64.0 - (RIGHT - 64.0);
const TOP: f64 = -22.0;
const BOTTOM: f64 = 124.0;
const WIDTH: f64 = RIGHT - LEFT;

const DOT_R: f64 = 5.5;
/// Where the dots wait (their centres' height: level with the tail's tip) and where they land.
const FLOOR_Y: f64 = 112.0;
const SLOT_Y: f64 = 66.0;
/// The top of a hop.
const PEAK_Y: f64 = -12.0;
/// Beside the bubble, nearest first: a rising dot never passes in front of the screen.
const FLOOR_X: [f64; 3] = [128.0, 142.0, 156.0];
const SLOT_X: [f64; 3] = [64.0, 80.0, 96.0];
/// How late in a hop the sideways move happens (`u^SIDEWAYS`): up beside the bubble first, then
/// across over its top.
const SIDEWAYS: f64 = 2.0;

/// The screen (the bubble's dark face) and the bubble's top edge.
const SCREEN: (f64, f64, f64, f64) = (18.0, 20.0, 92.0, 72.0);
const SCREEN_R: f64 = 11.0;
const RIM_TOP: f64 = 14.0;
/// The point the bubble's settle bump scales around.
const BUBBLE_CENTRE: (f64, f64) = (64.0, 60.0);

// The timeline, as fractions of a loop.
/// Dot `i` starts crouching at `HOP_START + i * HOP_STAGGER`.
const HOP_START: f64 = 0.02;
const HOP_STAGGER: f64 = 0.09;
const CROUCH: f64 = 0.04;
const FLIGHT: f64 = 0.20;
const LAND: f64 = 0.06;
const LID_CLOSE: (f64, f64) = (0.56, 0.68);
const BUMP: (f64, f64) = (0.68, 0.76);
const LID_OPEN: (f64, f64) = (0.80, 0.90);
/// Dot `i` pops back onto the floor from `POP_START + i * POP_STAGGER`, over `POP`.
const POP_START: f64 = 0.86;
const POP_STAGGER: f64 = 0.03;
const POP: f64 = 0.06;

/// Where a dot is in its trip, which decides how it is drawn.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DotPlace {
    /// Not drawn (packed away, before it pops back).
    Hidden,
    /// On the floor or rising: drawn in front of everything.
    Outside,
    /// Falling into the bubble: drawn only above the rim and inside the open screen, so it
    /// disappears behind the rim's top edge on the way in.
    Entering,
    /// In its slot on the screen: drawn under the lid.
    Packed,
}

/// One dot's pose: centre, squash (`sx`, `sy`), and an overall `scale` for popping in.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DotPose {
    pub x: f64,
    pub y: f64,
    pub sx: f64,
    pub sy: f64,
    pub scale: f64,
    pub place: DotPlace,
}

/// Everything that moves, at one instant.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Frame {
    pub dots: [DotPose; 3],
    /// How open the screen is: 1 open, 0 shut (the lid comes down from the top).
    pub openness: f64,
    /// The bubble's scale around its centre (the settle bump after it shuts).
    pub bubble_scale: f64,
}

impl Frame {
    /// The app icon as it ships: screen open, dots in their slots. Shown when animations are off.
    pub fn still() -> Self {
        let dot = |i: usize| DotPose {
            x: SLOT_X[i],
            y: SLOT_Y,
            sx: 1.0,
            sy: 1.0,
            scale: 1.0,
            place: DotPlace::Packed,
        };
        Self {
            dots: [dot(0), dot(1), dot(2)],
            openness: 1.0,
            bubble_scale: 1.0,
        }
    }
}

fn ease_in_out(t: f64) -> f64 {
    let t = t.clamp(0.0, 1.0);
    if t < 0.5 {
        4.0 * t * t * t
    } else {
        1.0 - (-2.0 * t + 2.0).powi(3) / 2.0
    }
}

fn ease_out_cubic(t: f64) -> f64 {
    1.0 - (1.0 - t.clamp(0.0, 1.0)).powi(3)
}

/// Overshoots a little, then settles: a pop.
fn ease_out_back(t: f64) -> f64 {
    let t = t.clamp(0.0, 1.0);
    let c1 = 1.70158;
    let c3 = c1 + 1.0;
    1.0 + c3 * (t - 1.0).powi(3) + c1 * (t - 1.0).powi(2)
}

/// How far `t` is through `[start, end)`, clamped to 0..=1.
fn progress(t: f64, start: f64, end: f64) -> f64 {
    ((t - start) / (end - start)).clamp(0.0, 1.0)
}

/// A squash that rises and dies away over `p` in 0..=1 (0 at both ends).
fn squash(p: f64, depth: f64) -> f64 {
    (PI * p).sin() * (1.0 - p) * depth * 2.0
}

/// The share of a hop spent rising: under one gravity, time goes as the square root of the drop.
fn apex_at() -> f64 {
    let rise = (FLOOR_Y - PEAK_Y).sqrt();
    let fall = (SLOT_Y - PEAK_Y).sqrt();
    rise / (rise + fall)
}

fn dot_at(i: usize, t: f64) -> DotPose {
    let crouch_at = HOP_START + i as f64 * HOP_STAGGER;
    let fly_at = crouch_at + CROUCH;
    let land_at = fly_at + FLIGHT;
    let rest = |x: f64, y: f64, place| DotPose {
        x,
        y,
        sx: 1.0,
        sy: 1.0,
        scale: 1.0,
        place,
    };
    // Squashed (flatter, wider) against the surface below it: the bottom stays put.
    let squashed = |x: f64, y: f64, s: f64, place| DotPose {
        x,
        y: y + DOT_R * s,
        sx: 1.0 + s,
        sy: 1.0 - s,
        scale: 1.0,
        place,
    };
    let pop_at = POP_START + i as f64 * POP_STAGGER;
    if t >= pop_at {
        // Back on the floor for the next loop.
        let scale = ease_out_back(progress(t, pop_at, pop_at + POP));
        return DotPose {
            scale,
            ..rest(FLOOR_X[i], FLOOR_Y, DotPlace::Outside)
        };
    }
    if t < crouch_at {
        return rest(FLOOR_X[i], FLOOR_Y, DotPlace::Outside);
    }
    if t < fly_at {
        let s = squash(progress(t, crouch_at, fly_at), 0.22);
        return squashed(FLOOR_X[i], FLOOR_Y, s, DotPlace::Outside);
    }
    if t < land_at {
        let u = progress(t, fly_at, land_at);
        let apex = apex_at();
        let x = FLOOR_X[i] + (SLOT_X[i] - FLOOR_X[i]) * u.powf(SIDEWAYS);
        let (y, place) = if u < apex {
            let k = 1.0 - u / apex;
            (
                FLOOR_Y - (FLOOR_Y - PEAK_Y) * (1.0 - k * k),
                DotPlace::Outside,
            )
        } else {
            let k = (u - apex) / (1.0 - apex);
            (PEAK_Y + (SLOT_Y - PEAK_Y) * k * k, DotPlace::Entering)
        };
        // Stretched along the motion when fast (leaving and arriving), round at the top.
        let stretch = 0.18 * (1.0 - (PI * u).sin());
        return DotPose {
            x,
            y,
            sx: 1.0 - stretch * 0.5,
            sy: 1.0 + stretch,
            scale: 1.0,
            place,
        };
    }
    if t < LID_OPEN.0 {
        let s = squash(progress(t, land_at, land_at + LAND), 0.3);
        return squashed(SLOT_X[i], SLOT_Y, s, DotPlace::Packed);
    }
    // Packed and gone: the screen reopens empty.
    DotPose {
        scale: 0.0,
        ..rest(SLOT_X[i], SLOT_Y, DotPlace::Hidden)
    }
}

/// The animation at `t` seconds since it started (any `t`; it loops every [`LOOP_SECS`]).
pub fn frame_at(t: f64) -> Frame {
    let t = (t / LOOP_SECS).rem_euclid(1.0);
    let openness = if t < LID_CLOSE.1 {
        1.0 - ease_in_out(progress(t, LID_CLOSE.0, LID_CLOSE.1))
    } else if t < LID_OPEN.0 {
        0.0
    } else {
        ease_out_cubic(progress(t, LID_OPEN.0, LID_OPEN.1))
    };
    let bump = progress(t, BUMP.0, BUMP.1);
    let bubble_scale = if bump > 0.0 && bump < 1.0 {
        1.0 + 0.035 * (PI * bump).sin()
    } else {
        1.0
    };
    Frame {
        dots: [dot_at(0, t), dot_at(1, t), dot_at(2, t)],
        openness,
        bubble_scale,
    }
}

/// The icon's speech-bubble outline (body, rim and tail), in icon units.
fn bubble_path(cr: &cairo::Context) {
    cr.new_path();
    cr.move_to(28.0, 14.0);
    cr.line_to(100.0, 14.0);
    cr.arc(100.0, 30.0, 16.0, -PI / 2.0, 0.0);
    cr.line_to(116.0, 82.0);
    cr.arc(100.0, 82.0, 16.0, 0.0, PI / 2.0);
    cr.line_to(56.0, 98.0);
    cr.line_to(32.0, 117.0);
    cr.line_to(35.0, 98.0);
    cr.line_to(28.0, 98.0);
    cr.arc(28.0, 82.0, 16.0, PI / 2.0, PI);
    cr.line_to(12.0, 30.0);
    cr.arc(28.0, 30.0, 16.0, PI, 1.5 * PI);
    cr.close_path();
}

fn rounded_rect(cr: &cairo::Context, (x, y, w, h): (f64, f64, f64, f64), r: f64) {
    cr.new_sub_path();
    cr.arc(x + w - r, y + r, r, -PI / 2.0, 0.0);
    cr.arc(x + w - r, y + h - r, r, 0.0, PI / 2.0);
    cr.arc(x + r, y + h - r, r, PI / 2.0, PI);
    cr.arc(x + r, y + r, r, PI, 1.5 * PI);
    cr.close_path();
}

fn rgb(hex: u32) -> (f64, f64, f64) {
    let c = |shift: u32| f64::from((hex >> shift) & 0xff) / 255.0;
    (c(16), c(8), c(0))
}

fn draw_dot(cr: &cairo::Context, i: usize, pose: &DotPose) {
    if pose.scale <= 0.001 || pose.place == DotPlace::Hidden {
        return;
    }
    // The icon's agent accents, in slot order (Claude, agy, Codex: `Driver::ALL`).
    let Some(&driver) = agent_core::adapter::Driver::ALL.get(i) else {
        return;
    };
    let (r, g, b) = crate::brand::rgb(driver);
    let _ = cr.save();
    cr.translate(pose.x, pose.y);
    cr.scale(pose.sx * pose.scale, pose.sy * pose.scale);
    cr.arc(0.0, 0.0, DOT_R, 0.0, 2.0 * PI);
    cr.set_source_rgb(r, g, b);
    let _ = cr.fill();
    let _ = cr.restore();
}

/// The visible part of the screen: the lid covers it from the top down.
fn open_screen(openness: f64) -> (f64, f64, f64, f64) {
    let (x, y, w, h) = SCREEN;
    let open_h = h * openness.clamp(0.0, 1.0);
    (x, y + h - open_h, w, open_h)
}

/// Draws `frame` into a `width` x `height` area, centred and scaled to fit.
pub fn draw_frame(cr: &cairo::Context, width: f64, height: f64, frame: &Frame) {
    let scale = (width / WIDTH).min(height / (BOTTOM - TOP));
    if scale <= 0.0 {
        return;
    }
    let _ = cr.save();
    cr.translate(
        (width - WIDTH * scale) / 2.0,
        (height - (BOTTOM - TOP) * scale) / 2.0,
    );
    cr.scale(scale, scale);
    cr.translate(-LEFT, -TOP);

    // Floor shadows, under dots near the floor (fading as they rise).
    for (i, pose) in frame.dots.iter().enumerate() {
        if pose.place != DotPlace::Outside || pose.scale <= 0.001 {
            continue;
        }
        let lift = ((FLOOR_Y - pose.y) / 60.0).clamp(0.0, 1.0);
        let alpha = 0.28 * (1.0 - lift) * pose.scale.min(1.0);
        if alpha <= 0.01 {
            continue;
        }
        let _ = cr.save();
        cr.translate(FLOOR_X[i], FLOOR_Y + DOT_R + 1.5);
        cr.scale(DOT_R * (1.1 - 0.4 * lift) * pose.sx, 1.6);
        cr.arc(0.0, 0.0, 1.0, 0.0, 2.0 * PI);
        cr.set_source_rgba(0.0, 0.0, 0.0, alpha);
        let _ = cr.fill();
        let _ = cr.restore();
    }

    // The bubble, with its settle bump.
    let _ = cr.save();
    let (bx, by) = BUBBLE_CENTRE;
    cr.translate(bx, by);
    cr.scale(frame.bubble_scale, frame.bubble_scale);
    cr.translate(-bx, -by);

    // Depth: the same bubble, dropped and darkened.
    let _ = cr.save();
    cr.translate(0.0, 5.0);
    bubble_path(cr);
    let (r, g, b) = rgb(0x0d0a16);
    cr.set_source_rgba(r, g, b, 0.55);
    let _ = cr.fill();
    let _ = cr.restore();

    // Body and rim.
    bubble_path(cr);
    let rim = cairo::LinearGradient::new(0.0, 14.0, 0.0, 117.0);
    let (r, g, b) = rgb(0xa08cff);
    rim.add_color_stop_rgb(0.0, r, g, b);
    let (r, g, b) = rgb(0x5a44c8);
    rim.add_color_stop_rgb(1.0, r, g, b);
    let _ = cr.set_source(&rim);
    let _ = cr.fill();

    // The screen and what is on it, under the lid.
    let visible = open_screen(frame.openness);
    if visible.3 > 0.05 {
        let _ = cr.save();
        cr.rectangle(visible.0, visible.1, visible.2, visible.3);
        cr.clip();
        rounded_rect(cr, SCREEN, SCREEN_R);
        let screen = cairo::LinearGradient::new(0.0, SCREEN.1, 0.0, SCREEN.1 + SCREEN.3);
        let (r, g, b) = rgb(0x2b2442);
        screen.add_color_stop_rgb(0.0, r, g, b);
        let (r, g, b) = rgb(0x181425);
        screen.add_color_stop_rgb(1.0, r, g, b);
        let _ = cr.set_source(&screen);
        let _ = cr.fill();
        // The soft highlight along the screen's top.
        rounded_rect(cr, (24.0, 23.0, 80.0, 2.0), 1.0);
        cr.set_source_rgba(1.0, 1.0, 1.0, 0.07);
        let _ = cr.fill();
        // The prompt chevron.
        cr.move_to(34.0, 43.0);
        cr.line_to(50.0, 56.0);
        cr.line_to(34.0, 69.0);
        let (r, g, b) = rgb(0xc8c8ff);
        cr.set_source_rgb(r, g, b);
        cr.set_line_width(8.0);
        cr.set_line_cap(cairo::LineCap::Round);
        cr.set_line_join(cairo::LineJoin::Round);
        let _ = cr.stroke();
        for (i, pose) in frame.dots.iter().enumerate() {
            if pose.place == DotPlace::Packed {
                draw_dot(cr, i, pose);
            }
        }
        let _ = cr.restore();
        // The lid's edge while it moves: a faint light line where it meets the screen.
        if frame.openness < 0.999 {
            cr.rectangle(visible.0 + 4.0, visible.1 - 0.75, visible.2 - 8.0, 1.5);
            cr.set_source_rgba(0.85, 0.8, 1.0, 0.35);
            let _ = cr.fill();
        }
    }
    let _ = cr.restore();

    // Dots in flight, in front of everything; one falling in shows only above the rim and
    // inside the open screen, so it slips in behind the rim's edge.
    for (i, pose) in frame.dots.iter().enumerate() {
        match pose.place {
            DotPlace::Outside => draw_dot(cr, i, pose),
            DotPlace::Entering => {
                let _ = cr.save();
                cr.rectangle(-WIDTH, TOP - WIDTH, 3.0 * WIDTH, RIM_TOP - (TOP - WIDTH));
                cr.rectangle(visible.0, visible.1, visible.2, visible.3);
                cr.clip();
                draw_dot(cr, i, pose);
                let _ = cr.restore();
            }
            DotPlace::Packed | DotPlace::Hidden => {}
        }
    }
    let _ = cr.restore();
}

/// The natural size: the bubble at 128 px, the icon's own size, where it is crisp.
const NATURAL_WIDTH: i32 = WIDTH as i32;
const NATURAL_HEIGHT: i32 = ((BOTTOM - TOP) * NATURAL_WIDTH as f64 / WIDTH) as i32;

/// The drawing area that plays the animation while it is mapped, at the display's frame rate.
pub struct PackAnimation {
    area: gtk4::DrawingArea,
    state: Rc<State>,
}

struct State {
    /// The frame clock's time of the first tick, in microseconds.
    started: Cell<Option<i64>>,
    frame: Cell<Frame>,
    tick: RefCell<Option<gtk4::TickCallbackId>>,
}

impl PackAnimation {
    pub fn new() -> Self {
        let area = gtk4::DrawingArea::builder()
            .content_width(NATURAL_WIDTH)
            .content_height(NATURAL_HEIGHT)
            .halign(gtk4::Align::Center)
            .valign(gtk4::Align::Center)
            // Decorative: the status line below it says what is happening.
            .accessible_role(gtk4::AccessibleRole::Presentation)
            .build();
        let state = Rc::new(State {
            started: Cell::new(None),
            frame: Cell::new(Frame::still()),
            tick: RefCell::new(None),
        });
        let draw_state = state.clone();
        area.set_draw_func(move |_, cr, w, h| {
            draw_frame(cr, f64::from(w), f64::from(h), &draw_state.frame.get());
        });

        let start = {
            let state = state.clone();
            move |area: &gtk4::DrawingArea| {
                if state.tick.borrow().is_some() || !animations_enabled() {
                    return;
                }
                state.started.set(None);
                let tick_state = state.clone();
                let id = area.add_tick_callback(move |area, clock| {
                    let now = clock.frame_time();
                    let started = tick_state.started.get().unwrap_or(now);
                    tick_state.started.set(Some(started));
                    let secs = (now - started) as f64 / 1_000_000.0;
                    tick_state.frame.set(frame_at(secs));
                    area.queue_draw();
                    glib::ControlFlow::Continue
                });
                state.tick.replace(Some(id));
            }
        };
        let stop = {
            let state = state.clone();
            move |area: &gtk4::DrawingArea| {
                if let Some(id) = state.tick.take() {
                    id.remove();
                }
                state.frame.set(Frame::still());
                area.queue_draw();
            }
        };
        area.connect_map({
            let start = start.clone();
            move |area| start(area)
        });
        area.connect_unmap({
            let stop = stop.clone();
            move |area| stop(area)
        });
        // Turning animations off (or on) applies at once.
        if let Some(settings) = gtk4::Settings::default() {
            let weak = area.downgrade();
            let handler = settings.connect_gtk_enable_animations_notify(move |_| {
                let Some(area) = weak.upgrade() else { return };
                if !area.is_mapped() {
                    return;
                }
                if animations_enabled() {
                    start(&area);
                } else {
                    stop(&area);
                }
            });
            // The settings object lives for the app; the handler goes with the widget.
            let settings_weak = settings.downgrade();
            let handler = RefCell::new(Some(handler));
            area.connect_destroy(move |_| {
                if let (Some(settings), Some(handler)) = (settings_weak.upgrade(), handler.take()) {
                    settings.disconnect(handler);
                }
            });
        }
        Self { area, state }
    }

    pub fn widget(&self) -> &gtk4::DrawingArea {
        &self.area
    }

    /// Whether it is animating (mapped, with animations on); otherwise it shows the still icon.
    pub fn is_playing(&self) -> bool {
        self.state.tick.borrow().is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(fraction: f64) -> Frame {
        frame_at(fraction * LOOP_SECS)
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-6
    }

    #[test]
    fn the_loop_starts_with_the_dots_on_the_floor_and_the_screen_open() {
        let f = at(0.0);
        assert!(close(f.openness, 1.0));
        assert!(close(f.bubble_scale, 1.0));
        for (i, d) in f.dots.iter().enumerate() {
            assert_eq!(d.place, DotPlace::Outside);
            assert!(close(d.x, FLOOR_X[i]) && close(d.y, FLOOR_Y));
            assert!(close(d.scale, 1.0) && close(d.sx, 1.0) && close(d.sy, 1.0));
        }
    }

    #[test]
    fn the_loop_is_seamless() {
        // Just before the wrap, everything is where the loop begins.
        let end = frame_at(LOOP_SECS - 1e-9);
        let start = frame_at(0.0);
        assert!(close(end.openness, start.openness));
        for (a, b) in end.dots.iter().zip(start.dots.iter()) {
            assert_eq!(a.place, b.place);
            assert!((a.x - b.x).abs() < 1e-3 && (a.y - b.y).abs() < 1e-3);
            assert!((a.scale - b.scale).abs() < 1e-3, "{a:?} vs {b:?}");
        }
        // And any number of loops later.
        let (a, b) = (frame_at(0.37), frame_at(0.37 + 3.0 * LOOP_SECS));
        assert!((a.openness - b.openness).abs() < 1e-6);
        for (a, b) in a.dots.iter().zip(b.dots.iter()) {
            assert_eq!(a.place, b.place);
            assert!((a.x - b.x).abs() < 1e-6 && (a.y - b.y).abs() < 1e-6);
            assert!((a.sx - b.sx).abs() < 1e-6 && (a.sy - b.sy).abs() < 1e-6);
        }
    }

    #[test]
    fn the_dots_hop_one_after_another_and_drop_into_their_slots() {
        // Dot 0 is in the air while dot 2 still waits on the floor.
        let f = at(HOP_START + CROUCH + FLIGHT * 0.3);
        assert!(f.dots[0].y < FLOOR_Y - 20.0, "{:?}", f.dots[0]);
        assert!(close(f.dots[2].y, FLOOR_Y));
        // Every hop clears the bubble's top edge before it drops in.
        for i in 0..3 {
            let fly = HOP_START + i as f64 * HOP_STAGGER + CROUCH;
            let top = (0..=100)
                .map(|k| at(fly + FLIGHT * k as f64 / 100.0).dots[i].y)
                .fold(f64::INFINITY, f64::min);
            assert!(top < RIM_TOP - DOT_R, "dot {i} peaks at {top}");
            // On the way down it is Entering (clipped by the rim), then lands Packed.
            let falling = at(fly + FLIGHT * 0.95).dots[i];
            assert_eq!(falling.place, DotPlace::Entering);
        }
        let packed = at(LID_CLOSE.0 - 0.01);
        for (i, d) in packed.dots.iter().enumerate() {
            assert_eq!(d.place, DotPlace::Packed);
            assert!(close(d.x, SLOT_X[i]) && close(d.y, SLOT_Y), "{d:?}");
        }
    }

    /// A dot drawn in front of the screen while rising would look as if it were already inside.
    #[test]
    fn a_rising_dot_never_crosses_in_front_of_the_screen() {
        let screen_right = SCREEN.0 + SCREEN.2;
        let mut t = 0.0;
        while t < 1.0 {
            for d in at(t).dots {
                if d.place == DotPlace::Outside && d.y + DOT_R > RIM_TOP {
                    assert!(d.x - DOT_R > screen_right, "{d:?} at {t}");
                }
            }
            t += 0.002;
        }
    }

    #[test]
    fn a_landing_squashes_and_keeps_the_dot_on_its_floor() {
        let land = HOP_START + CROUCH + FLIGHT;
        let d = at(land + LAND * 0.25).dots[0];
        assert!(d.sx > 1.0 && d.sy < 1.0, "{d:?}");
        // The bottom of the squashed dot stays where a round one's would be.
        assert!(close(d.y + DOT_R * d.sy, SLOT_Y + DOT_R), "{d:?}");
        let settled = at(land + LAND + 0.01).dots[0];
        assert!(close(settled.sx, 1.0) && close(settled.sy, 1.0));
    }

    #[test]
    fn the_lid_closes_holds_and_reopens_empty() {
        assert!(close(at(LID_CLOSE.0).openness, 1.0));
        let half = at((LID_CLOSE.0 + LID_CLOSE.1) / 2.0).openness;
        assert!(half > 0.1 && half < 0.9, "{half}");
        assert!(close(at(LID_CLOSE.1 + 0.01).openness, 0.0));
        assert!(close(at(LID_OPEN.0 - 0.01).openness, 0.0));
        // Shut, it bumps once and settles.
        assert!(at((BUMP.0 + BUMP.1) / 2.0).bubble_scale > 1.0);
        assert!(close(at(BUMP.1 + 0.01).bubble_scale, 1.0));
        // Reopened, the slots are empty until the dots pop back onto the floor.
        let reopened = at(LID_OPEN.1 - 0.02);
        assert!(reopened.openness > 0.5);
        assert!(reopened
            .dots
            .iter()
            .all(|d| matches!(d.place, DotPlace::Hidden | DotPlace::Outside)));
        // The lid only ever moves one way at a time.
        let mut last = 1.0;
        for k in 0..=100 {
            let t = LID_CLOSE.0 + (LID_CLOSE.1 - LID_CLOSE.0) * k as f64 / 100.0;
            let o = at(t).openness;
            assert!(o <= last + 1e-9);
            last = o;
        }
    }

    #[test]
    fn the_animation_moves_smoothly() {
        // No jump between neighbouring frames at 60 fps anywhere in the loop.
        let step = 1.0 / 60.0;
        let mut prev = frame_at(0.0);
        let mut t = step;
        while t <= LOOP_SECS * 2.0 {
            let f = frame_at(t);
            for (a, b) in prev.dots.iter().zip(f.dots.iter()) {
                if a.place != DotPlace::Hidden && b.place != DotPlace::Hidden {
                    let moved = ((a.x - b.x).powi(2) + (a.y - b.y).powi(2)).sqrt();
                    assert!(moved < 30.0, "dot jumped {moved} at {t}");
                }
            }
            assert!(
                (prev.openness - f.openness).abs() < 0.25,
                "lid jumped at {t}"
            );
            prev = f;
            t += step;
        }
    }

    #[test]
    fn the_still_frame_is_the_icon() {
        let f = Frame::still();
        assert!(close(f.openness, 1.0) && close(f.bubble_scale, 1.0));
        for (i, d) in f.dots.iter().enumerate() {
            assert_eq!(d.place, DotPlace::Packed);
            assert!(close(d.x, SLOT_X[i]) && close(d.y, SLOT_Y));
        }
    }

    #[test]
    fn frames_draw_at_any_size_without_error() {
        for (w, h) in [(96, 122), (128, 162), (300, 200), (0, 0)] {
            let surface =
                cairo::ImageSurface::create(cairo::Format::ARgb32, w, h).expect("surface");
            let cr = cairo::Context::new(&surface).expect("context");
            for k in 0..20 {
                draw_frame(&cr, f64::from(w), f64::from(h), &frame_at(k as f64 * 0.1));
            }
            draw_frame(&cr, f64::from(w), f64::from(h), &Frame::still());
            assert_eq!(cr.status(), Ok(()));
        }
    }
}
