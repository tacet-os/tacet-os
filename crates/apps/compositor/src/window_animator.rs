//! Slide-in animation for newly-spawned windows.
//!
//! When `place_new_window` picks a free slot far from where the window was
//! "summoned" (the pointer), we'd like the window to glide from the summon
//! point to its resolved position instead of popping into place. Mirrors
//! the exponential-decay approach used by [`crate::camera::Camera`] so the
//! feel is consistent: same τ, same snap epsilon, same is_animating() gate
//! that lets the render loop stop ticking once everything settles.

use smithay::{
    desktop::Space,
    utils::{IsAlive, Logical, Point},
};

use crate::shell::WindowElement;

/// Matches `camera::ANIM_TAU` — keeps window slide and camera zoom feeling
/// like one motion when they happen together (e.g. spawn-then-pan).
const ANIM_TAU: f64 = 0.08;

/// Below this distance, snap to target. Same value as the camera uses for
/// position; one logical pixel is below visual perception and animating
/// further wastes a frame's worth of damage.
const POS_SNAP_EPS: f64 = 0.5;

struct WindowAnim {
    window: WindowElement,
    cur_x: f64,
    cur_y: f64,
    target_x: f64,
    target_y: f64,
}

impl WindowAnim {
    fn is_done(&self) -> bool {
        (self.target_x - self.cur_x).abs() < POS_SNAP_EPS
            && (self.target_y - self.cur_y).abs() < POS_SNAP_EPS
    }
}

#[derive(Default)]
pub struct WindowAnimator {
    anims: Vec<WindowAnim>,
}

impl std::fmt::Debug for WindowAnimator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WindowAnimator")
            .field("active", &self.anims.len())
            .finish()
    }
}

impl WindowAnimator {
    /// Register a slide from `from` → `to` for `window`. Must be called
    /// after the caller has mapped the window at `from`; this struct does
    /// not move it the first frame, only on subsequent ticks.
    ///
    /// If an animation already exists for this window (e.g. user spammed
    /// two new-window requests fast enough that placement search ran
    /// twice), replace it. Stacked animations on one window would fight.
    pub fn start(
        &mut self,
        window: WindowElement,
        from: Point<i32, Logical>,
        to: Point<i32, Logical>,
    ) {
        self.anims.retain(|a| a.window != window);
        self.anims.push(WindowAnim {
            window,
            cur_x: from.x as f64,
            cur_y: from.y as f64,
            target_x: to.x as f64,
            target_y: to.y as f64,
        });
    }

    /// True iff there's at least one in-progress slide. The render loop
    /// checks this each frame so it can stop forcing redraws once we've
    /// settled.
    pub fn is_animating(&self) -> bool {
        !self.anims.is_empty()
    }

    /// Advance every active slide by `dt` seconds and apply the resulting
    /// position via `space.map_element`. Drops dead windows and completed
    /// animations. `activate=false` on the per-frame map_element so we
    /// don't keep re-raising the window every tick (that would steal the
    /// raise from any window the user clicked since spawn).
    pub fn step(&mut self, space: &mut Space<WindowElement>, dt: f64) {
        if dt <= 0.0 || self.anims.is_empty() {
            return;
        }
        let k = (1.0 - (-dt / ANIM_TAU).exp()).clamp(0.0, 1.0);

        self.anims.retain_mut(|a| {
            if !a.window.alive() {
                return false;
            }
            a.cur_x += (a.target_x - a.cur_x) * k;
            a.cur_y += (a.target_y - a.cur_y) * k;
            if (a.target_x - a.cur_x).abs() < POS_SNAP_EPS {
                a.cur_x = a.target_x;
            }
            if (a.target_y - a.cur_y).abs() < POS_SNAP_EPS {
                a.cur_y = a.target_y;
            }
            let pos: Point<i32, Logical> =
                (a.cur_x.round() as i32, a.cur_y.round() as i32).into();
            space.map_element(a.window.clone(), pos, false);
            !a.is_done()
        });
    }
}
