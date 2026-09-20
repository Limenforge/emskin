//! Jelly cursor — spring-animated synthetic text cursor.
//!
//! Emacs hides its native caret while this effect is enabled. The compositor
//! therefore keeps drawing the synthetic caret after it settles instead of
//! rendering only a temporary trail. Each corner owns a critically damped
//! spring; new reports retarget all four without discarding their velocities.
//!
//! Caret rects arrive via IPC (`SetCursorRect`), computed by the elisp
//! client in `post-command-hook`. The compositor owns only the animation
//! timing and rendering.

use std::time::Duration;

use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            element::{
                memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement},
                Kind,
            },
            gles::GlesRenderer,
            utils::CommitCounter,
        },
    },
    utils::{Buffer as SBuffer, Logical, Physical, Point, Rectangle, Scale, Size, Transform},
};

use effect_core::paint_buffer;

// ---------------------------------------------------------------------------
// Tuning
// ---------------------------------------------------------------------------

/// Centre frequency of the critically damped corner springs, in radians/sec.
const SPRING_OMEGA: f64 = 26.0;
/// Leading corners respond faster and trailing corners slower. The projection
/// along the movement direction interpolates across this range.
const SPRING_DEFORMATION: f64 = 6.0;
const POSITION_EPSILON: f64 = 0.02;
const VELOCITY_EPSILON: f64 = 0.5;
/// Default cursor color (BGRA) — Catppuccin Mocha sky #89dceb.
const DEFAULT_COLOR_SOLID: [u8; 4] = [0xeb, 0xdc, 0x89, 0xc8];
/// Alpha applied to all jelly colors (0..=255).
const COLOR_ALPHA: u8 = 0xc8;
/// Safety margin around the polygon bbox, in logical pixels. Prevents
/// rounding from clipping the edge at fractional scale.
const BBOX_PAD: i32 = 2;
const EPS: f64 = 1e-9;

// ---------------------------------------------------------------------------
// Animation state
// ---------------------------------------------------------------------------

type RectF = Rectangle<f64, Logical>;
type Corners = [Point<f64, Logical>; 4];
type CornerVelocities = [[f64; 2]; 4];

struct SpringState {
    current: Corners,
    target: Corners,
    velocity: CornerVelocities,
    omega: [f64; 4],
    last_tick: Duration,
    moving: bool,
}

impl SpringState {
    fn new(rect: RectF, now: Duration) -> Self {
        let corners = rect_corners(rect);
        Self {
            current: corners,
            target: corners,
            velocity: [[0.0; 2]; 4],
            omega: [SPRING_OMEGA; 4],
            last_tick: now,
            moving: false,
        }
    }

    fn advance_to(&mut self, now: Duration) {
        let dt = now.saturating_sub(self.last_tick).as_secs_f64();
        self.last_tick = now;
        if !self.moving || dt <= 0.0 {
            return;
        }

        for i in 0..4 {
            step_critically_damped(
                &mut self.current[i].x,
                &mut self.velocity[i][0],
                self.target[i].x,
                self.omega[i],
                dt,
            );
            step_critically_damped(
                &mut self.current[i].y,
                &mut self.velocity[i][1],
                self.target[i].y,
                self.omega[i],
                dt,
            );
        }

        let settled = self
            .current
            .iter()
            .zip(self.target.iter())
            .all(|(value, target)| {
                (value.x - target.x).abs() <= POSITION_EPSILON
                    && (value.y - target.y).abs() <= POSITION_EPSILON
            })
            && self
                .velocity
                .iter()
                .flatten()
                .all(|velocity| velocity.abs() <= VELOCITY_EPSILON);
        if settled {
            self.current = self.target;
            self.velocity = [[0.0; 2]; 4];
            self.moving = false;
        }
    }

    fn retarget(&mut self, rect: RectF, now: Duration) {
        self.advance_to(now);
        let target = rect_corners(rect);
        if corners_equal(&self.target, &target) {
            return;
        }
        self.omega = corner_frequencies(&self.current, &target);
        self.target = target;
        self.moving = !corners_equal(&self.current, &self.target)
            || self
                .velocity
                .iter()
                .flatten()
                .any(|velocity| velocity.abs() > VELOCITY_EPSILON);
    }
}

// ---------------------------------------------------------------------------
// Plugin
// ---------------------------------------------------------------------------

pub struct JellyCursor {
    enabled: bool,
    state: Option<SpringState>,
    buf: MemoryRenderBuffer,
    commit: CommitCounter,
    color_solid: [u8; 4],
}

impl Default for JellyCursor {
    fn default() -> Self {
        Self::new()
    }
}

impl JellyCursor {
    pub fn new() -> Self {
        Self {
            enabled: false,
            state: None,
            buf: MemoryRenderBuffer::new(Fourcc::Argb8888, (1, 1), 1, Transform::Normal, None),
            commit: CommitCounter::default(),
            color_solid: DEFAULT_COLOR_SOLID,
        }
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        if !enabled {
            self.state = None;
        }
    }

    /// Parse a CSS-style hex color (`#RRGGBB` or `#RRGGBBAA`) and use it as
    /// the jelly color. Silently ignores malformed input.
    pub fn set_color_hex(&mut self, hex: &str) {
        if let Some(bgra) = parse_hex_color(hex) {
            self.color_solid = bgra;
        }
    }

    /// Push the current caret rect (in canvas coordinates) into the
    /// animation state machine. `None` resets to Idle.
    pub fn update(&mut self, rect: Option<Rectangle<i32, Logical>>, now: Duration) {
        if !self.enabled {
            return;
        }
        let Some(r) = rect else {
            self.state = None;
            return;
        };
        let target = rect_i32_to_f64(r);
        match &mut self.state {
            Some(state) => state.retarget(target, now),
            None => self.state = Some(SpringState::new(target, now)),
        }
    }
}

// ---------------------------------------------------------------------------
// Effect impl
// ---------------------------------------------------------------------------

impl effect_core::Effect for JellyCursor {
    fn name(&self) -> &'static str {
        "jelly_cursor"
    }

    fn is_active(&self) -> bool {
        self.enabled
    }

    fn chain_position(&self) -> u8 {
        77
    }

    fn pre_paint(&mut self, ctx: &effect_core::EffectCtx) {
        if let Some(state) = &mut self.state {
            state.advance_to(ctx.present_time);
        }
    }

    fn paint(
        &mut self,
        renderer: &mut GlesRenderer,
        ctx: &effect_core::EffectCtx,
    ) -> Vec<effect_core::CustomElement<GlesRenderer>> {
        let Some(state) = &self.state else {
            return Vec::new();
        };
        let pts = state.current;
        let (min_x, min_y, max_x, max_y) = bounds(&pts);
        let bbox_x = min_x.floor() as i32 - BBOX_PAD;
        let bbox_y = min_y.floor() as i32 - BBOX_PAD;
        let bbox_w = (max_x.ceil() as i32 - bbox_x + BBOX_PAD).max(1);
        let bbox_h = (max_y.ceil() as i32 - bbox_y + BBOX_PAD).max(1);

        let origin = Point::<f64, Logical>::from((bbox_x as f64, bbox_y as f64));
        let pts_local: [Point<f64, Logical>; 4] = std::array::from_fn(|i| pts[i] - origin);
        let gradient = Gradient {
            from: pts[0] - origin,
            to: pts[0] - origin,
            c_start: self.color_solid,
            c_end: self.color_solid,
        };

        let buf_size: Size<i32, SBuffer> = (bbox_w, bbox_h).into();
        paint_buffer(&mut self.buf, buf_size, |data| {
            fill_polygon_bgra(
                PixelBuffer {
                    data,
                    w: bbox_w,
                    h: bbox_h,
                },
                &pts_local,
                &gradient,
            );
        });
        self.commit.increment();

        let s: Scale<f64> = Scale::from(ctx.scale);
        let loc_phys: Point<f64, Physical> = origin.to_physical(s);
        MemoryRenderBufferRenderElement::from_buffer(
            renderer,
            loc_phys,
            &self.buf,
            None,
            None,
            None,
            Kind::Unspecified,
        )
        .ok()
        .map(|e| vec![effect_core::CustomElement::Label(e)])
        .unwrap_or_default()
    }

    fn post_paint(&mut self) -> bool {
        self.state.as_ref().is_some_and(|state| state.moving)
    }
}

// ---------------------------------------------------------------------------
// Spring and rectangle geometry
// ---------------------------------------------------------------------------

fn step_critically_damped(
    position: &mut f64,
    velocity: &mut f64,
    target: f64,
    omega: f64,
    dt: f64,
) {
    let displacement = *position - target;
    let coefficient = *velocity + omega * displacement;
    let decay = (-omega * dt).exp();
    *position = target + (displacement + coefficient * dt) * decay;
    *velocity = (*velocity - omega * coefficient * dt) * decay;
}

fn rect_corners(rect: RectF) -> Corners {
    let loc = rect.loc;
    let w = rect.size.w;
    let h = rect.size.h;
    [
        loc,
        loc + Point::from((w, 0.0)),
        loc + Point::from((w, h)),
        loc + Point::from((0.0, h)),
    ]
}

fn corners_equal(a: &Corners, b: &Corners) -> bool {
    const E: f64 = 0.5;
    a.iter()
        .zip(b.iter())
        .all(|(a, b)| (a.x - b.x).abs() < E && (a.y - b.y).abs() < E)
}

fn corners_center(corners: &Corners) -> Point<f64, Logical> {
    let (x, y) = corners
        .iter()
        .fold((0.0, 0.0), |(x, y), corner| (x + corner.x, y + corner.y));
    Point::from((x * 0.25, y * 0.25))
}

fn corner_frequencies(current: &Corners, target: &Corners) -> [f64; 4] {
    let from = corners_center(current);
    let to = corners_center(target);
    let dx = to.x - from.x;
    let dy = to.y - from.y;
    let distance = dx.hypot(dy);
    if distance < POSITION_EPSILON {
        return [SPRING_OMEGA; 4];
    }

    let direction = (dx / distance, dy / distance);
    let max_projection = target
        .iter()
        .map(|corner| ((corner.x - to.x) * direction.0 + (corner.y - to.y) * direction.1).abs())
        .fold(0.0_f64, f64::max)
        .max(EPS);

    std::array::from_fn(|i| {
        let projection = ((target[i].x - to.x) * direction.0 + (target[i].y - to.y) * direction.1)
            / max_projection;
        SPRING_OMEGA + SPRING_DEFORMATION * projection.clamp(-1.0, 1.0)
    })
}

fn rect_i32_to_f64(r: Rectangle<i32, Logical>) -> RectF {
    Rectangle::new(
        Point::from((r.loc.x as f64, r.loc.y as f64)),
        Size::from((r.size.w as f64, r.size.h as f64)),
    )
}

/// Single-pass x/y min/max over 4 points.
fn bounds(pts: &[Point<f64, Logical>; 4]) -> (f64, f64, f64, f64) {
    let mut min_x = f64::INFINITY;
    let mut min_y = f64::INFINITY;
    let mut max_x = f64::NEG_INFINITY;
    let mut max_y = f64::NEG_INFINITY;
    for p in pts {
        if p.x < min_x {
            min_x = p.x;
        }
        if p.x > max_x {
            max_x = p.x;
        }
        if p.y < min_y {
            min_y = p.y;
        }
        if p.y > max_y {
            max_y = p.y;
        }
    }
    (min_x, min_y, max_x, max_y)
}

// ---------------------------------------------------------------------------
// Polygon rasterization
// ---------------------------------------------------------------------------

struct PixelBuffer<'a> {
    data: &'a mut [u8],
    w: i32,
    h: i32,
}

struct Gradient {
    from: Point<f64, Logical>,
    to: Point<f64, Logical>,
    c_start: [u8; 4],
    c_end: [u8; 4],
}

/// Scanline-fill a convex quad into a BGRA buffer with a linear gradient.
///
/// Pixels outside the polygon are cleared to transparent in the same pass
/// (merging clear + fill saves a full-buffer zeroing sweep).
fn fill_polygon_bgra(buf: PixelBuffer<'_>, pts: &[Point<f64, Logical>; 4], grad: &Gradient) {
    let PixelBuffer {
        data,
        w: buf_w,
        h: buf_h,
    } = buf;
    let stride = buf_w * 4;

    let (_, min_yf, _, max_yf) = bounds(pts);
    let poly_min_y = (min_yf.floor() as i32).max(0);
    let poly_max_y = (max_yf.ceil() as i32).min(buf_h - 1);

    let gx = grad.to.x - grad.from.x;
    let gy = grad.to.y - grad.from.y;
    let g_len2 = gx * gx + gy * gy;
    let has_grad = g_len2 > EPS;
    let inv_len2 = if has_grad { 1.0 / g_len2 } else { 0.0 };
    let dt = gx * inv_len2;

    for py in 0..buf_h {
        let row_off = (py * stride) as usize;
        // Rows outside polygon bbox are fully transparent.
        if py < poly_min_y || py > poly_max_y {
            data[row_off..row_off + stride as usize].fill(0);
            continue;
        }

        // Scan at pixel centre so a horizontal edge (two verts at exact
        // integer y) doesn't emit spurious intersections.
        let y = py as f64 + 0.5;
        let mut x_lo = f64::NAN;
        let mut x_hi = f64::NAN;
        for i in 0..4 {
            let a = pts[i];
            let b = pts[(i + 1) % 4];
            let (lo, hi) = if a.y <= b.y { (a, b) } else { (b, a) };
            // Half-open [lo.y, hi.y) avoids double-counting at shared verts.
            if y < lo.y || y >= hi.y {
                continue;
            }
            let dy = hi.y - lo.y;
            if dy.abs() < EPS {
                continue;
            }
            let x = lo.x + (y - lo.y) / dy * (hi.x - lo.x);
            if x_lo.is_nan() {
                x_lo = x;
            } else {
                x_hi = x;
            }
        }
        if x_lo.is_nan() || x_hi.is_nan() {
            data[row_off..row_off + stride as usize].fill(0);
            continue;
        }
        if x_lo > x_hi {
            std::mem::swap(&mut x_lo, &mut x_hi);
        }
        let x0 = (x_lo.ceil() as i32).clamp(0, buf_w);
        let x1 = ((x_hi - EPS).floor() as i32 + 1).clamp(0, buf_w);
        if x1 <= x0 {
            data[row_off..row_off + stride as usize].fill(0);
            continue;
        }

        // Left gap → transparent.
        if x0 > 0 {
            data[row_off..row_off + (x0 * 4) as usize].fill(0);
        }

        // Interior → gradient; recurrence `t += dt` replaces per-pixel
        // dot product.
        let mut t = if has_grad {
            ((x0 as f64 + 0.5 - grad.from.x) * gx + (y - grad.from.y) * gy) * inv_len2
        } else {
            1.0
        };
        let mut off = row_off + (x0 * 4) as usize;
        for _ in x0..x1 {
            let tc = t.clamp(0.0, 1.0) as f32;
            data[off] = mix(grad.c_start[0], grad.c_end[0], tc);
            data[off + 1] = mix(grad.c_start[1], grad.c_end[1], tc);
            data[off + 2] = mix(grad.c_start[2], grad.c_end[2], tc);
            data[off + 3] = mix(grad.c_start[3], grad.c_end[3], tc);
            off += 4;
            t += dt;
        }

        // Right gap → transparent.
        if x1 < buf_w {
            data[row_off + (x1 * 4) as usize..row_off + stride as usize].fill(0);
        }
    }
}

fn mix(a: u8, b: u8, t: f32) -> u8 {
    (a as f32 * (1.0 - t) + b as f32 * t).round() as u8
}

// ---------------------------------------------------------------------------
// Color helpers
// ---------------------------------------------------------------------------

/// Parse `#RRGGBB` or `#RRGGBBAA` (case-insensitive, `#` optional) as BGRA.
fn parse_hex_color(s: &str) -> Option<[u8; 4]> {
    let s = s.trim().trim_start_matches('#');
    let (r, g, b, a) = match s.len() {
        6 => (
            u8::from_str_radix(&s[0..2], 16).ok()?,
            u8::from_str_radix(&s[2..4], 16).ok()?,
            u8::from_str_radix(&s[4..6], 16).ok()?,
            COLOR_ALPHA,
        ),
        8 => (
            u8::from_str_radix(&s[0..2], 16).ok()?,
            u8::from_str_radix(&s[2..4], 16).ok()?,
            u8::from_str_radix(&s[4..6], 16).ok()?,
            u8::from_str_radix(&s[6..8], 16).ok()?,
        ),
        _ => return None,
    };
    Some([b, g, r, a])
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f64, y: f64, w: f64, h: f64) -> RectF {
        Rectangle::new(Point::from((x, y)), Size::from((w, h)))
    }

    #[test]
    fn critical_spring_converges_without_overshoot() {
        let mut position = 0.0;
        let mut velocity = 0.0;
        for _ in 0..60 {
            step_critically_damped(&mut position, &mut velocity, 20.0, SPRING_OMEGA, 1.0 / 60.0);
            assert!((0.0..=20.0).contains(&position));
        }
        assert!((position - 20.0).abs() < POSITION_EPSILON);
    }

    #[test]
    fn fill_polygon_writes_interior_pixels() {
        let mut data = vec![0u8; 16 * 16 * 4];
        let pts: [Point<f64, Logical>; 4] = [
            Point::from((2.0, 2.0)),
            Point::from((14.0, 2.0)),
            Point::from((14.0, 14.0)),
            Point::from((2.0, 14.0)),
        ];
        let grad = Gradient {
            from: Point::from((2.0, 8.0)),
            to: Point::from((14.0, 8.0)),
            c_start: [0xff, 0, 0, 0xff],
            c_end: [0, 0, 0xff, 0xff],
        };
        fill_polygon_bgra(
            PixelBuffer {
                data: &mut data,
                w: 16,
                h: 16,
            },
            &pts,
            &grad,
        );
        let center = (8 * 16 + 8) * 4;
        assert!(data[center + 3] > 0, "interior pixel not painted");
        // Outside bbox pixels cleared by the merged pass.
        assert_eq!(data[0..4], [0, 0, 0, 0]);
        let bottom_left = (15 * 16) * 4;
        assert_eq!(data[bottom_left..bottom_left + 4], [0, 0, 0, 0]);
    }

    #[test]
    fn parses_hex_colors() {
        assert_eq!(
            parse_hex_color("#cba6f7"),
            Some([0xf7, 0xa6, 0xcb, COLOR_ALPHA])
        );
        assert_eq!(
            parse_hex_color("cba6f7"),
            Some([0xf7, 0xa6, 0xcb, COLOR_ALPHA])
        );
        assert_eq!(parse_hex_color("#cba6f780"), Some([0xf7, 0xa6, 0xcb, 0x80]));
        assert_eq!(parse_hex_color("not-a-color"), None);
        assert_eq!(parse_hex_color("#abc"), None);
    }

    #[test]
    fn set_color_hex_updates_solid_color() {
        let mut jc = JellyCursor::new();
        jc.set_color_hex("#000000");
        assert_eq!(jc.color_solid, [0, 0, 0, COLOR_ALPHA]);
    }

    #[test]
    fn update_with_none_sets_idle() {
        let mut jc = JellyCursor::new();
        jc.set_enabled(true);
        jc.update(
            Some(Rectangle::new((10, 10).into(), (8, 16).into())),
            Duration::ZERO,
        );
        assert!(jc.state.is_some());
        jc.update(None, Duration::from_millis(50));
        assert!(jc.state.is_none());
        // Re-entry after Idle starts settled at the new position.
        jc.update(
            Some(Rectangle::new((30, 30).into(), (8, 16).into())),
            Duration::from_millis(100),
        );
        let state = jc.state.as_ref().unwrap();
        assert!(!state.moving);
        assert!(corners_equal(
            &state.current,
            &rect_corners(rect(30.0, 30.0, 8.0, 16.0))
        ));
    }

    #[test]
    fn update_with_changed_rect_seeds_animation() {
        let mut jc = JellyCursor::new();
        jc.set_enabled(true);
        jc.update(
            Some(Rectangle::new((0, 0).into(), (8, 16).into())),
            Duration::ZERO,
        );
        jc.update(
            Some(Rectangle::new((20, 0).into(), (8, 16).into())),
            Duration::from_millis(10),
        );
        let state = jc.state.as_ref().unwrap();
        assert!(state.moving);
        assert!(corners_equal(
            &state.current,
            &rect_corners(rect(0.0, 0.0, 8.0, 16.0))
        ));
        assert!(corners_equal(
            &state.target,
            &rect_corners(rect(20.0, 0.0, 8.0, 16.0))
        ));
    }

    #[test]
    fn update_mid_flight_retargets_without_stalling() {
        let mut jc = JellyCursor::new();
        jc.set_enabled(true);
        jc.update(
            Some(Rectangle::new((0, 0).into(), (8, 16).into())),
            Duration::ZERO,
        );
        jc.update(
            Some(Rectangle::new((20, 0).into(), (8, 16).into())),
            Duration::from_millis(10),
        );
        jc.update(
            Some(Rectangle::new((40, 0).into(), (8, 16).into())),
            Duration::from_millis(15),
        );
        let state = jc.state.as_ref().unwrap();
        let current_x = corners_center(&state.current).x;
        assert!(current_x > 0.0 && current_x < 20.0);
        assert!((corners_center(&state.target).x - 44.0).abs() < 1e-6);
        assert!(
            state.velocity.iter().any(|velocity| velocity[0] > 0.0),
            "velocity = {:?}",
            state.velocity
        );
    }

    #[test]
    fn leading_corners_move_faster_than_trailing_corners() {
        let mut state = SpringState::new(rect(0.0, 0.0, 8.0, 16.0), Duration::ZERO);
        state.retarget(rect(20.0, 0.0, 8.0, 16.0), Duration::ZERO);
        state.advance_to(Duration::from_millis(50));

        let trailing_displacement = state.current[0].x;
        let leading_displacement = state.current[1].x - 8.0;
        assert!(
            leading_displacement > trailing_displacement,
            "leading={leading_displacement}, trailing={trailing_displacement}"
        );
        assert!(state.omega[1] > state.omega[0]);
    }

    #[test]
    fn settled_cursor_stays_present() {
        let mut jc = JellyCursor::new();
        jc.set_enabled(true);
        jc.update(
            Some(Rectangle::new((0, 0).into(), (8, 16).into())),
            Duration::ZERO,
        );
        jc.update(
            Some(Rectangle::new((20, 0).into(), (8, 16).into())),
            Duration::from_millis(10),
        );
        jc.state
            .as_mut()
            .unwrap()
            .advance_to(Duration::from_secs(1));
        let state = jc.state.as_ref().unwrap();
        assert!(!state.moving);
        assert!(corners_equal(&state.current, &state.target));
    }
}
