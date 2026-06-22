//! Canvas viewport state for tacet.
//!
//! Each [`Camera`] owns the position + zoom level of one output viewing
//! the infinite 2D canvas. `position` is the canvas-coordinate point that
//! sits at the output's top-left corner; panning right increases `x`,
//! windows underneath appear to scroll left on screen.
//!
//! Animation model: mutating methods (`pan`, `zoom_in`, `zoom_around`, …)
//! update `target_*` fields. The compositor's render loop calls
//! [`Camera::step`] each frame to interpolate `position` / `zoom` toward
//! their targets via exponential decay. This gives smooth Figma-style
//! transitions instead of one-shot jumps without requiring an
//! animation-curve scheduler. `position` and `zoom` remain the
//! authoritative "what's currently on screen" values — every consumer
//! (RescaleRenderElement, grid layout, surface hit-test, fractional
//! scale broadcast) reads them directly.

use smithay::utils::{Logical, Point};
use tracing::info;

/// Lower bound on `Camera::zoom`. 0.25 = 4× zoom-out is the deepest
/// useful "overview" zoom for a typical canvas UI; below this, surfaces
/// and grid lines either round to sub-pixel sizes (and disappear) or
/// stop conveying useful information.
pub const MIN_ZOOM: f64 = 0.25;
/// Upper bound on `Camera::zoom` (10× native size).
pub const MAX_ZOOM: f64 = 10.0;

/// Exponential-decay time constant in seconds. With τ=80 ms the camera
/// reaches 63 % of remaining distance in 80 ms, 95 % in ~240 ms. Tuned to
/// feel responsive but obviously animated (instant would be τ→0).
const ANIM_TAU: f64 = 0.08;

/// Snap thresholds — once we're closer than these to the target, jump
/// straight to it. Prevents the camera from hovering at sub-pixel deltas
/// forever and triggering useless redraws every frame.
const ZOOM_SNAP_EPS: f64 = 0.0005;
const POS_SNAP_EPS: f64 = 0.5;

#[derive(Debug, Clone)]
pub struct Camera {
    /// Canvas-coordinate origin of the viewport — the point that sits at
    /// the output's top-left corner of the leftmost monitor.
    pub position: Point<i32, Logical>,
    /// Current zoom factor; 1.0 means a canvas pixel maps to a screen
    /// logical pixel before output HiDPI scale.
    pub zoom: f64,

    /// Where the camera is heading. Mutators (`pan`, `zoom_in`,
    /// `zoom_around`, …) write here; [`step`] interpolates `position` /
    /// `zoom` toward these targets. Position is kept as f64 to preserve
    /// sub-pixel precision during animation (the public `position` is
    /// rounded to i32 for smithay's `space.map_output`, which only
    /// accepts integer logical pixels).
    target_zoom: f64,
    target_pos_x: f64,
    target_pos_y: f64,
    pos_x_f: f64,
    pos_y_f: f64,
}

impl Default for Camera {
    fn default() -> Self {
        Self {
            position: Point::from((0, 0)),
            zoom: 1.0,
            target_zoom: 1.0,
            target_pos_x: 0.0,
            target_pos_y: 0.0,
            pos_x_f: 0.0,
            pos_y_f: 0.0,
        }
    }
}

impl Camera {
    pub const PAN_STEP: i32 = 100;
    pub const ZOOM_FACTOR: f64 = 1.2;

    /// Pan is intentionally instant — for MMB-drag and arrow-key nav users
    /// expect 1:1 direct manipulation. Smoothing it would make the canvas
    /// "lag behind" the mouse, which feels broken rather than polished.
    /// Only `zoom_*` operations animate.
    pub fn pan(&mut self, dx: i32, dy: i32) {
        self.target_pos_x += dx as f64;
        self.target_pos_y += dy as f64;
        self.pos_x_f = self.target_pos_x;
        self.pos_y_f = self.target_pos_y;
        self.position.x = self.pos_x_f.round() as i32;
        self.position.y = self.pos_y_f.round() as i32;
        info!(
            x = self.position.x,
            y = self.position.y,
            "camera pan (instant)"
        );
    }

    pub fn zoom_in(&mut self) {
        self.target_zoom = (self.target_zoom * Self::ZOOM_FACTOR).clamp(MIN_ZOOM, MAX_ZOOM);
        info!(target_zoom = self.target_zoom, "camera zoom in");
    }

    pub fn zoom_out(&mut self) {
        self.target_zoom = (self.target_zoom / Self::ZOOM_FACTOR).clamp(MIN_ZOOM, MAX_ZOOM);
        info!(target_zoom = self.target_zoom, "camera zoom out");
    }

    /// Zoom by `factor` while keeping the canvas point under `screen_pos`
    /// (in output-local logical pixels) anchored. The position shift is
    /// computed against the *target* zoom so when interpolation settles,
    /// the chosen point lands back under the cursor. Derivation: requiring
    /// `screen/zoom + cam` to be invariant gives
    /// `cam_new = cam_old + screen * (1/zoom_old - 1/zoom_new)`.
    pub fn zoom_around(&mut self, screen_pos: Point<f64, Logical>, factor: f64) {
        let old = self.target_zoom;
        let new_zoom = (old * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        if (new_zoom - old).abs() < f64::EPSILON {
            return;
        }
        let dx = screen_pos.x * (1.0 / old - 1.0 / new_zoom);
        let dy = screen_pos.y * (1.0 / old - 1.0 / new_zoom);
        self.target_pos_x += dx;
        self.target_pos_y += dy;
        self.target_zoom = new_zoom;
        info!(
            target_zoom = self.target_zoom,
            tx = self.target_pos_x,
            ty = self.target_pos_y,
            "camera zoom around cursor"
        );
    }

    pub fn reset(&mut self) {
        self.target_pos_x = 0.0;
        self.target_pos_y = 0.0;
        self.target_zoom = 1.0;
        info!("camera reset (target)");
    }

    /// Advance `position` and `zoom` toward their targets by `dt` seconds
    /// of exponential decay. The decay factor `k = 1 - exp(-dt / τ)` is
    /// framerate-independent — at 60 fps `dt ≈ 16ms` gives `k ≈ 0.18`,
    /// at 144 fps `dt ≈ 7ms` gives `k ≈ 0.084`. Both converge in roughly
    /// the same wall-clock time (~240 ms to 95 %). Should be called once
    /// per render frame before reading `position` / `zoom`.
    pub fn step(&mut self, dt: f64) {
        if dt <= 0.0 {
            return;
        }
        let k = (1.0 - (-dt / ANIM_TAU).exp()).clamp(0.0, 1.0);

        self.zoom += (self.target_zoom - self.zoom) * k;
        if (self.target_zoom - self.zoom).abs() < ZOOM_SNAP_EPS {
            self.zoom = self.target_zoom;
        }

        self.pos_x_f += (self.target_pos_x - self.pos_x_f) * k;
        self.pos_y_f += (self.target_pos_y - self.pos_y_f) * k;
        if (self.target_pos_x - self.pos_x_f).abs() < POS_SNAP_EPS {
            self.pos_x_f = self.target_pos_x;
        }
        if (self.target_pos_y - self.pos_y_f).abs() < POS_SNAP_EPS {
            self.pos_y_f = self.target_pos_y;
        }
        self.position.x = self.pos_x_f.round() as i32;
        self.position.y = self.pos_y_f.round() as i32;
    }

    /// The zoom value the camera is animating toward. Useful for callers
    /// that care about the END state of an in-progress animation —
    /// notably the fractional_scale broadcast, which sends the protocol
    /// scale to clients: if we sent the *current* zoom every frame, foot
    /// and other grid-based clients would recalculate their cell layout
    /// 15× per 240 ms animation, looking like the window is rapidly
    /// resizing. Sending target_zoom gives them a single stable value to
    /// settle on; the visual is still smooth because we render with
    /// `current` zoom independently of what we tell clients.
    pub fn target_zoom(&self) -> f64 {
        self.target_zoom
    }

    /// Map a canvas-coordinate point that lies on `output` (whose top-left
    /// is mapped to this camera's `position`) to that output's local
    /// screen-coordinate system. Layer-shell surfaces position themselves
    /// in output-local screen-coords (un-zoomed), so any pointer hit-test
    /// against a layer surface must first run canvas-coord → screen-coord.
    /// Without this conversion clicking on a layer surface misses whenever
    /// `zoom != 1.0` or the user has panned the canvas.
    pub fn canvas_to_output_local(
        &self,
        canvas: Point<f64, Logical>,
        output_canvas_origin: Point<f64, Logical>,
    ) -> Point<f64, Logical> {
        let dx = (canvas.x - output_canvas_origin.x) * self.zoom;
        let dy = (canvas.y - output_canvas_origin.y) * self.zoom;
        Point::from((dx, dy))
    }

    /// True iff `position` or `zoom` haven't reached their targets yet.
    /// The render loop checks this each frame: when true, it re-applies
    /// camera state (output remap + broadcast_fractional_scale) so
    /// damage tracking + clients see the in-progress values. When false,
    /// idle — no per-frame work needed beyond normal damage-driven repaint.
    pub fn is_animating(&self) -> bool {
        self.zoom != self.target_zoom
            || self.pos_x_f != self.target_pos_x
            || self.pos_y_f != self.target_pos_y
    }
}
