use std::cell::RefCell;

use smithay::{
    desktop::{WindowSurface, layer_map_for_output, space::SpaceElement},
    input::{
        pointer::{
            AxisFrame, ButtonEvent, GestureHoldBeginEvent, GestureHoldEndEvent, GesturePinchBeginEvent,
            GesturePinchEndEvent, GesturePinchUpdateEvent, GestureSwipeBeginEvent, GestureSwipeEndEvent,
            GestureSwipeUpdateEvent, GrabStartData as PointerGrabStartData, MotionEvent, PointerGrab,
            PointerInnerHandle, RelativeMotionEvent,
        },
        touch::{GrabStartData as TouchGrabStartData, TouchGrab},
    },
    reexports::wayland_protocols::xdg::shell::server::xdg_toplevel,
    utils::{IsAlive, Logical, Point, Rectangle, Serial, Size},
    wayland::{compositor::with_states, shell::xdg::SurfaceCachedState},
};
#[cfg(feature = "xwayland")]
use smithay::xwayland::xwm::ResizeEdge as X11ResizeEdge;

use super::{SurfaceData, WindowElement};
use crate::{
    focus::PointerFocusTarget,
    state::{AnvilState, Backend},
};

/// After a move grab releases, if the dropped window overlaps any
/// other element, find the nearest free slot on its output and return
/// `(start_loc, end_loc)` for the caller to register with the window
/// animator. Returns `None` if the drop is fine as-is or no slot fits.
///
/// We let drag motion proceed through overlap (feels more direct than
/// slide-along-edge — cursor and window stay locked together, no
/// "stuck against an obstacle" sensation) and clean up only at the
/// terminal moment. Animating the auto-reflow makes it look
/// intentional rather than feeling like the compositor snatched the
/// window away from the user's drop.
fn reflow_target<BackendData: Backend + 'static>(
    data: &AnvilState<BackendData>,
    window: &WindowElement,
) -> Option<(Point<i32, Logical>, Point<i32, Logical>)> {
    let space = &data.space;
    let current_loc = space.element_location(window)?;
    let my_bbox = space.element_geometry(window)?;

    let center_i = Point::from((
        my_bbox.loc.x + my_bbox.size.w / 2,
        my_bbox.loc.y + my_bbox.size.h / 2,
    ));
    // Output containing the window's center. If the drop is mostly
    // off-screen, the center is the closest single point to "where
    // they meant to put it"; better than the top-left which can sit
    // outside every output on a multi-monitor canvas.
    let output = space
        .outputs()
        .find(|o| {
            space
                .output_geometry(o)
                .map(|g| g.contains(center_i))
                .unwrap_or(false)
        })
        .cloned()?;

    let geo = space.output_geometry(&output)?;
    let zone = layer_map_for_output(&output).non_exclusive_zone();
    let bounds = Rectangle::new(geo.loc + zone.loc, zone.size);

    let existing: Vec<Rectangle<i32, Logical>> = space
        .elements()
        .filter(|e| *e != window)
        .filter_map(|e| space.element_geometry(e))
        .filter(|r| r.overlaps(bounds))
        .collect();

    // Cheap exit when the drop already fits — vast majority of drags.
    if !existing.iter().any(|r| r.overlaps(my_bbox)) {
        return None;
    }

    let anchor = Point::<f64, Logical>::from((center_i.x as f64, center_i.y as f64));
    let target_topleft =
        crate::shell::find_free_slot(bounds, my_bbox.size, &existing, anchor)?;

    // find_free_slot returns the top-left of a footprint-sized rect
    // in screen-space coords (= bbox coords). map_element takes the
    // loc that SpaceElement geometry is OFFSET from, so we apply the
    // same shift to current_loc: shift = target_topleft - bbox.loc.
    let shift = target_topleft - my_bbox.loc;
    Some((current_loc, current_loc + shift))
}

pub struct PointerMoveSurfaceGrab<BackendData: Backend + 'static> {
    pub start_data: PointerGrabStartData<AnvilState<BackendData>>,
    pub window: WindowElement,
    pub initial_window_location: Point<i32, Logical>,
}

impl<BackendData: Backend> PointerGrab<AnvilState<BackendData>> for PointerMoveSurfaceGrab<BackendData> {
    fn motion(
        &mut self,
        data: &mut AnvilState<BackendData>,
        handle: &mut PointerInnerHandle<'_, AnvilState<BackendData>>,
        _focus: Option<(PointerFocusTarget, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        // While the grab is active, no client has pointer focus
        handle.motion(data, None, event);

        // After the canvas-coord pointer-model rework, event.location and
        // start_data.location are already in canvas coords (logical
        // pixels on the infinite canvas), as is the window position. The
        // delta is therefore the canvas-space window displacement we
        // want, untouched. Dividing by camera.zoom here would re-apply
        // a transform already folded into pointer accumulation —
        // historically that did the right thing, but with the new model
        // it makes window drag run at 1/zoom² speed (broken at any
        // zoom != 1).
        let canvas_delta = event.location - self.start_data.location;
        let new_location = self.initial_window_location.to_f64() + canvas_delta;
        let new_loc_i = new_location.to_i32_round();

        // Free-overlap drag: commit the cursor-derived position with no
        // collision check. Windows can pass through each other while
        // held — on release, `unset` finds the nearest free slot and
        // glides the window there. Keeps the cursor-and-window 1:1
        // lock that feels direct, and defers the cleanup to a moment
        // the user has already mentally committed to.
        let current = data
            .space
            .element_location(&self.window)
            .unwrap_or(self.initial_window_location);
        if new_loc_i != current {
            data.space.map_element(self.window.clone(), new_loc_i, true);
        }
    }

    fn relative_motion(
        &mut self,
        data: &mut AnvilState<BackendData>,
        handle: &mut PointerInnerHandle<'_, AnvilState<BackendData>>,
        focus: Option<(PointerFocusTarget, Point<f64, Logical>)>,
        event: &RelativeMotionEvent,
    ) {
        handle.relative_motion(data, focus, event);
    }

    fn button(
        &mut self,
        data: &mut AnvilState<BackendData>,
        handle: &mut PointerInnerHandle<'_, AnvilState<BackendData>>,
        event: &ButtonEvent,
    ) {
        handle.button(data, event);
        if handle.current_pressed().is_empty() {
            // No more buttons are pressed, release the grab.
            handle.unset_grab(self, data, event.serial, event.time, true);
        }
    }

    fn axis(
        &mut self,
        data: &mut AnvilState<BackendData>,
        handle: &mut PointerInnerHandle<'_, AnvilState<BackendData>>,
        details: AxisFrame,
    ) {
        handle.axis(data, details)
    }

    fn frame(
        &mut self,
        data: &mut AnvilState<BackendData>,
        handle: &mut PointerInnerHandle<'_, AnvilState<BackendData>>,
    ) {
        handle.frame(data);
    }

    fn gesture_swipe_begin(
        &mut self,
        data: &mut AnvilState<BackendData>,
        handle: &mut PointerInnerHandle<'_, AnvilState<BackendData>>,
        event: &GestureSwipeBeginEvent,
    ) {
        handle.gesture_swipe_begin(data, event);
    }

    fn gesture_swipe_update(
        &mut self,
        data: &mut AnvilState<BackendData>,
        handle: &mut PointerInnerHandle<'_, AnvilState<BackendData>>,
        event: &GestureSwipeUpdateEvent,
    ) {
        handle.gesture_swipe_update(data, event);
    }

    fn gesture_swipe_end(
        &mut self,
        data: &mut AnvilState<BackendData>,
        handle: &mut PointerInnerHandle<'_, AnvilState<BackendData>>,
        event: &GestureSwipeEndEvent,
    ) {
        handle.gesture_swipe_end(data, event);
    }

    fn gesture_pinch_begin(
        &mut self,
        data: &mut AnvilState<BackendData>,
        handle: &mut PointerInnerHandle<'_, AnvilState<BackendData>>,
        event: &GesturePinchBeginEvent,
    ) {
        handle.gesture_pinch_begin(data, event);
    }

    fn gesture_pinch_update(
        &mut self,
        data: &mut AnvilState<BackendData>,
        handle: &mut PointerInnerHandle<'_, AnvilState<BackendData>>,
        event: &GesturePinchUpdateEvent,
    ) {
        handle.gesture_pinch_update(data, event);
    }

    fn gesture_pinch_end(
        &mut self,
        data: &mut AnvilState<BackendData>,
        handle: &mut PointerInnerHandle<'_, AnvilState<BackendData>>,
        event: &GesturePinchEndEvent,
    ) {
        handle.gesture_pinch_end(data, event);
    }

    fn gesture_hold_begin(
        &mut self,
        data: &mut AnvilState<BackendData>,
        handle: &mut PointerInnerHandle<'_, AnvilState<BackendData>>,
        event: &GestureHoldBeginEvent,
    ) {
        handle.gesture_hold_begin(data, event);
    }

    fn gesture_hold_end(
        &mut self,
        data: &mut AnvilState<BackendData>,
        handle: &mut PointerInnerHandle<'_, AnvilState<BackendData>>,
        event: &GestureHoldEndEvent,
    ) {
        handle.gesture_hold_end(data, event);
    }

    fn start_data(&self) -> &PointerGrabStartData<AnvilState<BackendData>> {
        &self.start_data
    }

    fn unset(&mut self, data: &mut AnvilState<BackendData>) {
        if let Some((from, to)) = reflow_target(data, &self.window) {
            data.window_animator.start(self.window.clone(), from, to);
        }
    }
}

pub struct TouchMoveSurfaceGrab<BackendData: Backend + 'static> {
    pub start_data: TouchGrabStartData<AnvilState<BackendData>>,
    pub window: WindowElement,
    pub initial_window_location: Point<i32, Logical>,
}

impl<BackendData: Backend> TouchGrab<AnvilState<BackendData>> for TouchMoveSurfaceGrab<BackendData> {
    fn down(
        &mut self,
        _data: &mut AnvilState<BackendData>,
        _handle: &mut smithay::input::touch::TouchInnerHandle<'_, AnvilState<BackendData>>,
        _focus: Option<(
            <AnvilState<BackendData> as smithay::input::SeatHandler>::TouchFocus,
            Point<f64, Logical>,
        )>,
        _event: &smithay::input::touch::DownEvent,
        _seq: Serial,
    ) {
    }

    fn up(
        &mut self,
        data: &mut AnvilState<BackendData>,
        handle: &mut smithay::input::touch::TouchInnerHandle<'_, AnvilState<BackendData>>,
        event: &smithay::input::touch::UpEvent,
        seq: Serial,
    ) {
        if event.slot != self.start_data.slot {
            return;
        }

        handle.up(data, event, seq);
        handle.unset_grab(self, data);
    }

    fn motion(
        &mut self,
        data: &mut AnvilState<BackendData>,
        _handle: &mut smithay::input::touch::TouchInnerHandle<'_, AnvilState<BackendData>>,
        _focus: Option<(
            <AnvilState<BackendData> as smithay::input::SeatHandler>::TouchFocus,
            Point<f64, Logical>,
        )>,
        event: &smithay::input::touch::MotionEvent,
        _seq: Serial,
    ) {
        if event.slot != self.start_data.slot {
            return;
        }

        let delta = event.location - self.start_data.location;
        let new_location = self.initial_window_location.to_f64() + delta;
        let new_loc_i = new_location.to_i32_round();
        // Same free-overlap model as the pointer grab — reflow happens
        // on grab end (`unset` below), not during motion.
        let current = data
            .space
            .element_location(&self.window)
            .unwrap_or(self.initial_window_location);
        if new_loc_i != current {
            data.space.map_element(self.window.clone(), new_loc_i, true);
        }
    }

    fn frame(
        &mut self,
        _data: &mut AnvilState<BackendData>,
        _handle: &mut smithay::input::touch::TouchInnerHandle<'_, AnvilState<BackendData>>,
        _seq: Serial,
    ) {
    }

    fn cancel(
        &mut self,
        data: &mut AnvilState<BackendData>,
        handle: &mut smithay::input::touch::TouchInnerHandle<'_, AnvilState<BackendData>>,
        seq: Serial,
    ) {
        handle.cancel(data, seq);
        handle.unset_grab(self, data);
    }

    fn shape(
        &mut self,
        data: &mut AnvilState<BackendData>,
        handle: &mut smithay::input::touch::TouchInnerHandle<'_, AnvilState<BackendData>>,
        event: &smithay::input::touch::ShapeEvent,
        seq: Serial,
    ) {
        handle.shape(data, event, seq);
    }

    fn orientation(
        &mut self,
        data: &mut AnvilState<BackendData>,
        handle: &mut smithay::input::touch::TouchInnerHandle<'_, AnvilState<BackendData>>,
        event: &smithay::input::touch::OrientationEvent,
        seq: Serial,
    ) {
        handle.orientation(data, event, seq);
    }

    fn start_data(&self) -> &smithay::input::touch::GrabStartData<AnvilState<BackendData>> {
        &self.start_data
    }

    fn unset(&mut self, data: &mut AnvilState<BackendData>) {
        if let Some((from, to)) = reflow_target(data, &self.window) {
            data.window_animator.start(self.window.clone(), from, to);
        }
    }
}

bitflags::bitflags! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    pub struct ResizeEdge: u32 {
        const NONE = 0;
        const TOP = 1;
        const BOTTOM = 2;
        const LEFT = 4;
        const TOP_LEFT = 5;
        const BOTTOM_LEFT = 6;
        const RIGHT = 8;
        const TOP_RIGHT = 9;
        const BOTTOM_RIGHT = 10;
    }
}

impl From<xdg_toplevel::ResizeEdge> for ResizeEdge {
    #[inline]
    fn from(x: xdg_toplevel::ResizeEdge) -> Self {
        Self::from_bits(x as u32).unwrap()
    }
}

impl From<ResizeEdge> for xdg_toplevel::ResizeEdge {
    #[inline]
    fn from(x: ResizeEdge) -> Self {
        Self::try_from(x.bits()).unwrap()
    }
}

#[cfg(feature = "xwayland")]
impl From<X11ResizeEdge> for ResizeEdge {
    #[inline]
    fn from(edge: X11ResizeEdge) -> Self {
        match edge {
            X11ResizeEdge::Bottom => ResizeEdge::BOTTOM,
            X11ResizeEdge::BottomLeft => ResizeEdge::BOTTOM_LEFT,
            X11ResizeEdge::BottomRight => ResizeEdge::BOTTOM_RIGHT,
            X11ResizeEdge::Left => ResizeEdge::LEFT,
            X11ResizeEdge::Right => ResizeEdge::RIGHT,
            X11ResizeEdge::Top => ResizeEdge::TOP,
            X11ResizeEdge::TopLeft => ResizeEdge::TOP_LEFT,
            X11ResizeEdge::TopRight => ResizeEdge::TOP_RIGHT,
        }
    }
}

/// Information about the resize operation.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct ResizeData {
    /// The edges the surface is being resized with.
    pub edges: ResizeEdge,
    /// The initial window location.
    pub initial_window_location: Point<i32, Logical>,
    /// The initial window size (geometry width and height).
    pub initial_window_size: Size<i32, Logical>,
}

/// State of the resize operation.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Default)]
pub enum ResizeState {
    /// The surface is not being resized.
    #[default]
    NotResizing,
    /// The surface is currently being resized.
    Resizing(ResizeData),
    /// The resize has finished, and the surface needs to ack the final configure.
    WaitingForFinalAck(ResizeData, Serial),
    /// The resize has finished, and the surface needs to commit its final state.
    WaitingForCommit(ResizeData),
}

pub struct PointerResizeSurfaceGrab<BackendData: Backend + 'static> {
    pub start_data: PointerGrabStartData<AnvilState<BackendData>>,
    pub window: WindowElement,
    pub edges: ResizeEdge,
    pub initial_window_location: Point<i32, Logical>,
    pub initial_window_size: Size<i32, Logical>,
    pub last_window_size: Size<i32, Logical>,
}

impl<BackendData: Backend> PointerGrab<AnvilState<BackendData>> for PointerResizeSurfaceGrab<BackendData> {
    fn motion(
        &mut self,
        data: &mut AnvilState<BackendData>,
        handle: &mut PointerInnerHandle<'_, AnvilState<BackendData>>,
        _focus: Option<(PointerFocusTarget, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        // While the grab is active, no client has pointer focus
        handle.motion(data, None, event);

        // It is impossible to get `min_size` and `max_size` of dead toplevel, so we return early.
        if !self.window.alive() {
            handle.unset_grab(self, data, event.serial, event.time, true);
            return;
        }

        let (mut dx, mut dy) = (event.location - self.start_data.location).into();

        let mut new_window_width = self.initial_window_size.w;
        let mut new_window_height = self.initial_window_size.h;

        let left_right = ResizeEdge::LEFT | ResizeEdge::RIGHT;
        let top_bottom = ResizeEdge::TOP | ResizeEdge::BOTTOM;

        if self.edges.intersects(left_right) {
            if self.edges.intersects(ResizeEdge::LEFT) {
                dx = -dx;
            }

            new_window_width = (self.initial_window_size.w as f64 + dx) as i32;
        }

        if self.edges.intersects(top_bottom) {
            if self.edges.intersects(ResizeEdge::TOP) {
                dy = -dy;
            }

            new_window_height = (self.initial_window_size.h as f64 + dy) as i32;
        }

        let (min_size, max_size) = if let Some(surface) = self.window.wl_surface() {
            with_states(&surface, |states| {
                let mut guard = states.cached_state.get::<SurfaceCachedState>();
                let data = guard.current();
                (data.min_size, data.max_size)
            })
        } else {
            ((0, 0).into(), (0, 0).into())
        };

        let min_width = min_size.w.max(1);
        let min_height = min_size.h.max(1);
        let max_width = if max_size.w == 0 { i32::MAX } else { max_size.w };
        let max_height = if max_size.h == 0 { i32::MAX } else { max_size.h };

        new_window_width = new_window_width.max(min_width).min(max_width);
        new_window_height = new_window_height.max(min_height).min(max_height);

        self.last_window_size = (new_window_width, new_window_height).into();

        match &self.window.0.underlying_surface() {
            WindowSurface::Wayland(xdg) => {
                xdg.with_pending_state(|state| {
                    state.states.set(xdg_toplevel::State::Resizing);
                    state.size = Some(self.last_window_size);
                });
                xdg.send_pending_configure();
            }
            #[cfg(feature = "xwayland")]
            WindowSurface::X11(x11) => {
                let location = data.space.element_location(&self.window).unwrap();
                x11.configure_with_sync(Rectangle::new(location, self.last_window_size), None)
                    .unwrap();
            }
        }
    }

    fn relative_motion(
        &mut self,
        data: &mut AnvilState<BackendData>,
        handle: &mut PointerInnerHandle<'_, AnvilState<BackendData>>,
        focus: Option<(PointerFocusTarget, Point<f64, Logical>)>,
        event: &RelativeMotionEvent,
    ) {
        handle.relative_motion(data, focus, event);
    }

    fn button(
        &mut self,
        data: &mut AnvilState<BackendData>,
        handle: &mut PointerInnerHandle<'_, AnvilState<BackendData>>,
        event: &ButtonEvent,
    ) {
        handle.button(data, event);
        if handle.current_pressed().is_empty() {
            // No more buttons are pressed, release the grab.
            handle.unset_grab(self, data, event.serial, event.time, true);

            // If toplevel is dead, we can't resize it, so we return early.
            if !self.window.alive() {
                return;
            }

            match &self.window.0.underlying_surface() {
                WindowSurface::Wayland(xdg) => {
                    xdg.with_pending_state(|state| {
                        state.states.unset(xdg_toplevel::State::Resizing);
                        state.size = Some(self.last_window_size);
                    });
                    xdg.send_pending_configure();
                    if self.edges.intersects(ResizeEdge::TOP_LEFT) {
                        let geometry = self.window.geometry();
                        let mut location = data.space.element_location(&self.window).unwrap();

                        if self.edges.intersects(ResizeEdge::LEFT) {
                            location.x = self.initial_window_location.x
                                + (self.initial_window_size.w - geometry.size.w);
                        }
                        if self.edges.intersects(ResizeEdge::TOP) {
                            location.y = self.initial_window_location.y
                                + (self.initial_window_size.h - geometry.size.h);
                        }

                        data.space.map_element(self.window.clone(), location, true);
                    }

                    with_states(&self.window.wl_surface().unwrap(), |states| {
                        let mut data = states
                            .data_map
                            .get::<RefCell<SurfaceData>>()
                            .unwrap()
                            .borrow_mut();
                        if let ResizeState::Resizing(resize_data) = data.resize_state {
                            data.resize_state = ResizeState::WaitingForFinalAck(resize_data, event.serial);
                        } else {
                            panic!("invalid resize state: {:?}", data.resize_state);
                        }
                    });
                }
                #[cfg(feature = "xwayland")]
                WindowSurface::X11(x11) => {
                    let mut location = data.space.element_location(&self.window).unwrap();
                    if self.edges.intersects(ResizeEdge::TOP_LEFT) {
                        let geometry = self.window.geometry();

                        if self.edges.intersects(ResizeEdge::LEFT) {
                            location.x = self.initial_window_location.x
                                + (self.initial_window_size.w - geometry.size.w);
                        }
                        if self.edges.intersects(ResizeEdge::TOP) {
                            location.y = self.initial_window_location.y
                                + (self.initial_window_size.h - geometry.size.h);
                        }

                        data.space.map_element(self.window.clone(), location, true);
                    }
                    x11.configure_with_sync(Rectangle::new(location, self.last_window_size), None)
                        .unwrap();

                    let Some(surface) = self.window.wl_surface() else {
                        // X11 Window got unmapped, abort
                        return;
                    };
                    with_states(&surface, |states| {
                        let mut data = states
                            .data_map
                            .get::<RefCell<SurfaceData>>()
                            .unwrap()
                            .borrow_mut();
                        if let ResizeState::Resizing(resize_data) = data.resize_state {
                            data.resize_state = ResizeState::WaitingForCommit(resize_data);
                        } else {
                            panic!("invalid resize state: {:?}", data.resize_state);
                        }
                    });
                }
            }
        }
    }

    fn axis(
        &mut self,
        data: &mut AnvilState<BackendData>,
        handle: &mut PointerInnerHandle<'_, AnvilState<BackendData>>,
        details: AxisFrame,
    ) {
        handle.axis(data, details)
    }

    fn frame(
        &mut self,
        data: &mut AnvilState<BackendData>,
        handle: &mut PointerInnerHandle<'_, AnvilState<BackendData>>,
    ) {
        handle.frame(data);
    }

    fn gesture_swipe_begin(
        &mut self,
        data: &mut AnvilState<BackendData>,
        handle: &mut PointerInnerHandle<'_, AnvilState<BackendData>>,
        event: &GestureSwipeBeginEvent,
    ) {
        handle.gesture_swipe_begin(data, event);
    }

    fn gesture_swipe_update(
        &mut self,
        data: &mut AnvilState<BackendData>,
        handle: &mut PointerInnerHandle<'_, AnvilState<BackendData>>,
        event: &GestureSwipeUpdateEvent,
    ) {
        handle.gesture_swipe_update(data, event);
    }

    fn gesture_swipe_end(
        &mut self,
        data: &mut AnvilState<BackendData>,
        handle: &mut PointerInnerHandle<'_, AnvilState<BackendData>>,
        event: &GestureSwipeEndEvent,
    ) {
        handle.gesture_swipe_end(data, event);
    }

    fn gesture_pinch_begin(
        &mut self,
        data: &mut AnvilState<BackendData>,
        handle: &mut PointerInnerHandle<'_, AnvilState<BackendData>>,
        event: &GesturePinchBeginEvent,
    ) {
        handle.gesture_pinch_begin(data, event);
    }

    fn gesture_pinch_update(
        &mut self,
        data: &mut AnvilState<BackendData>,
        handle: &mut PointerInnerHandle<'_, AnvilState<BackendData>>,
        event: &GesturePinchUpdateEvent,
    ) {
        handle.gesture_pinch_update(data, event);
    }

    fn gesture_pinch_end(
        &mut self,
        data: &mut AnvilState<BackendData>,
        handle: &mut PointerInnerHandle<'_, AnvilState<BackendData>>,
        event: &GesturePinchEndEvent,
    ) {
        handle.gesture_pinch_end(data, event);
    }

    fn gesture_hold_begin(
        &mut self,
        data: &mut AnvilState<BackendData>,
        handle: &mut PointerInnerHandle<'_, AnvilState<BackendData>>,
        event: &GestureHoldBeginEvent,
    ) {
        handle.gesture_hold_begin(data, event);
    }

    fn gesture_hold_end(
        &mut self,
        data: &mut AnvilState<BackendData>,
        handle: &mut PointerInnerHandle<'_, AnvilState<BackendData>>,
        event: &GestureHoldEndEvent,
    ) {
        handle.gesture_hold_end(data, event);
    }

    fn start_data(&self) -> &PointerGrabStartData<AnvilState<BackendData>> {
        &self.start_data
    }

    fn unset(&mut self, _data: &mut AnvilState<BackendData>) {}
}

pub struct TouchResizeSurfaceGrab<BackendData: Backend + 'static> {
    pub start_data: TouchGrabStartData<AnvilState<BackendData>>,
    pub window: WindowElement,
    pub edges: ResizeEdge,
    pub initial_window_location: Point<i32, Logical>,
    pub initial_window_size: Size<i32, Logical>,
    pub last_window_size: Size<i32, Logical>,
}

impl<BackendData: Backend> TouchGrab<AnvilState<BackendData>> for TouchResizeSurfaceGrab<BackendData> {
    fn down(
        &mut self,
        _data: &mut AnvilState<BackendData>,
        _handle: &mut smithay::input::touch::TouchInnerHandle<'_, AnvilState<BackendData>>,
        _focus: Option<(
            <AnvilState<BackendData> as smithay::input::SeatHandler>::TouchFocus,
            Point<f64, Logical>,
        )>,
        _event: &smithay::input::touch::DownEvent,
        _seq: Serial,
    ) {
    }

    fn up(
        &mut self,
        data: &mut AnvilState<BackendData>,
        handle: &mut smithay::input::touch::TouchInnerHandle<'_, AnvilState<BackendData>>,
        event: &smithay::input::touch::UpEvent,
        _seq: Serial,
    ) {
        if event.slot != self.start_data.slot {
            return;
        }
        handle.unset_grab(self, data);

        // If toplevel is dead, we can't resize it, so we return early.
        if !self.window.alive() {
            return;
        }

        match self.window.0.underlying_surface() {
            WindowSurface::Wayland(xdg) => {
                xdg.with_pending_state(|state| {
                    state.states.unset(xdg_toplevel::State::Resizing);
                    state.size = Some(self.last_window_size);
                });
                xdg.send_pending_configure();
                if self.edges.intersects(ResizeEdge::TOP_LEFT) {
                    let geometry = self.window.geometry();
                    let mut location = data.space.element_location(&self.window).unwrap();

                    if self.edges.intersects(ResizeEdge::LEFT) {
                        location.x =
                            self.initial_window_location.x + (self.initial_window_size.w - geometry.size.w);
                    }
                    if self.edges.intersects(ResizeEdge::TOP) {
                        location.y =
                            self.initial_window_location.y + (self.initial_window_size.h - geometry.size.h);
                    }

                    data.space.map_element(self.window.clone(), location, true);
                }

                with_states(&self.window.wl_surface().unwrap(), |states| {
                    let mut data = states
                        .data_map
                        .get::<RefCell<SurfaceData>>()
                        .unwrap()
                        .borrow_mut();
                    if let ResizeState::Resizing(resize_data) = data.resize_state {
                        data.resize_state = ResizeState::WaitingForFinalAck(resize_data, event.serial);
                    } else {
                        panic!("invalid resize state: {:?}", data.resize_state);
                    }
                });
            }
            #[cfg(feature = "xwayland")]
            WindowSurface::X11(x11) => {
                let mut location = data.space.element_location(&self.window).unwrap();
                if self.edges.intersects(ResizeEdge::TOP_LEFT) {
                    let geometry = self.window.geometry();

                    if self.edges.intersects(ResizeEdge::LEFT) {
                        location.x =
                            self.initial_window_location.x + (self.initial_window_size.w - geometry.size.w);
                    }
                    if self.edges.intersects(ResizeEdge::TOP) {
                        location.y =
                            self.initial_window_location.y + (self.initial_window_size.h - geometry.size.h);
                    }

                    data.space.map_element(self.window.clone(), location, true);
                }
                x11.configure_with_sync(Rectangle::new(location, self.last_window_size), None)
                    .unwrap();

                let Some(surface) = self.window.wl_surface() else {
                    // X11 Window got unmapped, abort
                    return;
                };
                with_states(&surface, |states| {
                    let mut data = states
                        .data_map
                        .get::<RefCell<SurfaceData>>()
                        .unwrap()
                        .borrow_mut();
                    if let ResizeState::Resizing(resize_data) = data.resize_state {
                        data.resize_state = ResizeState::WaitingForCommit(resize_data);
                    } else {
                        panic!("invalid resize state: {:?}", data.resize_state);
                    }
                });
            }
        }
    }

    fn motion(
        &mut self,
        data: &mut AnvilState<BackendData>,
        handle: &mut smithay::input::touch::TouchInnerHandle<'_, AnvilState<BackendData>>,
        _focus: Option<(
            <AnvilState<BackendData> as smithay::input::SeatHandler>::TouchFocus,
            Point<f64, Logical>,
        )>,
        event: &smithay::input::touch::MotionEvent,
        _seq: Serial,
    ) {
        if event.slot != self.start_data.slot {
            return;
        }

        // It is impossible to get `min_size` and `max_size` of dead toplevel, so we return early.
        if !self.window.alive() {
            handle.unset_grab(self, data);
            return;
        }

        let (mut dx, mut dy) = (event.location - self.start_data.location).into();

        let mut new_window_width = self.initial_window_size.w;
        let mut new_window_height = self.initial_window_size.h;

        let left_right = ResizeEdge::LEFT | ResizeEdge::RIGHT;
        let top_bottom = ResizeEdge::TOP | ResizeEdge::BOTTOM;

        if self.edges.intersects(left_right) {
            if self.edges.intersects(ResizeEdge::LEFT) {
                dx = -dx;
            }

            new_window_width = (self.initial_window_size.w as f64 + dx) as i32;
        }

        if self.edges.intersects(top_bottom) {
            if self.edges.intersects(ResizeEdge::TOP) {
                dy = -dy;
            }

            new_window_height = (self.initial_window_size.h as f64 + dy) as i32;
        }

        let (min_size, max_size) = if let Some(surface) = self.window.wl_surface() {
            with_states(&surface, |states| {
                let mut guard = states.cached_state.get::<SurfaceCachedState>();
                let data = guard.current();
                (data.min_size, data.max_size)
            })
        } else {
            ((0, 0).into(), (0, 0).into())
        };

        let min_width = min_size.w.max(1);
        let min_height = min_size.h.max(1);
        let max_width = if max_size.w == 0 { i32::MAX } else { max_size.w };
        let max_height = if max_size.h == 0 { i32::MAX } else { max_size.h };

        new_window_width = new_window_width.max(min_width).min(max_width);
        new_window_height = new_window_height.max(min_height).min(max_height);

        self.last_window_size = (new_window_width, new_window_height).into();

        match self.window.0.underlying_surface() {
            WindowSurface::Wayland(xdg) => {
                xdg.with_pending_state(|state| {
                    state.states.set(xdg_toplevel::State::Resizing);
                    state.size = Some(self.last_window_size);
                });
                xdg.send_pending_configure();
            }
            #[cfg(feature = "xwayland")]
            WindowSurface::X11(x11) => {
                let location = data.space.element_location(&self.window).unwrap();
                x11.configure_with_sync(Rectangle::new(location, self.last_window_size), None)
                    .unwrap();
            }
        }
    }

    fn frame(
        &mut self,
        _data: &mut AnvilState<BackendData>,
        _handle: &mut smithay::input::touch::TouchInnerHandle<'_, AnvilState<BackendData>>,
        _seq: Serial,
    ) {
    }

    fn cancel(
        &mut self,
        data: &mut AnvilState<BackendData>,
        handle: &mut smithay::input::touch::TouchInnerHandle<'_, AnvilState<BackendData>>,
        seq: Serial,
    ) {
        handle.cancel(data, seq);
        handle.unset_grab(self, data);
    }

    fn shape(
        &mut self,
        data: &mut AnvilState<BackendData>,
        handle: &mut smithay::input::touch::TouchInnerHandle<'_, AnvilState<BackendData>>,
        event: &smithay::input::touch::ShapeEvent,
        seq: Serial,
    ) {
        handle.shape(data, event, seq);
    }

    fn orientation(
        &mut self,
        data: &mut AnvilState<BackendData>,
        handle: &mut smithay::input::touch::TouchInnerHandle<'_, AnvilState<BackendData>>,
        event: &smithay::input::touch::OrientationEvent,
        seq: Serial,
    ) {
        handle.orientation(data, event, seq);
    }

    fn start_data(&self) -> &smithay::input::touch::GrabStartData<AnvilState<BackendData>> {
        &self.start_data
    }

    fn unset(&mut self, _data: &mut AnvilState<BackendData>) {}
}
