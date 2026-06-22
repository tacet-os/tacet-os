use std::{convert::TryInto, process::Command, sync::atomic::Ordering};

use crate::{AnvilState, focus::{KeyboardFocusTarget, PointerFocusTarget}, shell::FullscreenSurface};

#[cfg(feature = "udev")]
use crate::udev::UdevData;
#[cfg(feature = "udev")]
use smithay::backend::renderer::DebugFlags;

use smithay::{
    backend::input::{
        self, Axis, AxisSource, Event, InputBackend, InputEvent, KeyState, KeyboardKeyEvent,
        PointerAxisEvent, PointerButtonEvent,
    },
    desktop::{WindowSurfaceType, layer_map_for_output},
    input::{
        keyboard::{FilterResult, Keysym, ModifiersState, keysyms as xkb},
        pointer::{AxisFrame, ButtonEvent, MotionEvent},
    },
    output::Scale,
    reexports::{
        wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1,
        wayland_server::protocol::wl_pointer,
    },
    utils::{Logical, Point, SERIAL_COUNTER as SCOUNTER, Serial, Transform},
    wayland::{
        input_method::InputMethodSeat,
        keyboard_shortcuts_inhibit::KeyboardShortcutsInhibitorSeat,
        shell::wlr_layer::{KeyboardInteractivity, Layer as WlrLayer},
    },
};

#[cfg(any(feature = "winit", feature = "x11", feature = "udev"))]
use smithay::backend::input::AbsolutePositionEvent;

#[cfg(any(feature = "winit", feature = "x11"))]
use smithay::output::Output;
use tracing::{debug, error, info};

use crate::state::Backend;
#[cfg(feature = "udev")]
use smithay::{
    backend::{
        input::{
            Device, DeviceCapability, GestureBeginEvent, GestureEndEvent, GesturePinchUpdateEvent as _,
            GestureSwipeUpdateEvent as _, PointerMotionEvent, ProximityState, TabletToolButtonEvent,
            TabletToolEvent, TabletToolProximityEvent, TabletToolTipEvent, TabletToolTipState, TouchEvent,
        },
        session::Session,
    },
    input::{
        pointer::{
            GestureHoldBeginEvent, GestureHoldEndEvent, GesturePinchBeginEvent, GesturePinchEndEvent,
            GesturePinchUpdateEvent, GestureSwipeBeginEvent, GestureSwipeEndEvent, GestureSwipeUpdateEvent,
            RelativeMotionEvent,
        },
        touch::{DownEvent, UpEvent},
    },
    reexports::wayland_server::DisplayHandle,
    wayland::{
        pointer_constraints::{PointerConstraint, with_pointer_constraint},
        seat::WaylandFocus,
        tablet_manager::{TabletDescriptor, TabletSeatTrait},
    },
};

impl<BackendData: Backend> AnvilState<BackendData> {
    /// Spawn the session's autostart programs as compositor children.
    /// Each spawned process inherits `WAYLAND_DISPLAY` (and `DISPLAY` if
    /// XWayland is up), so it connects to *this* compositor and not to
    /// some other Wayland server the user happens to have running.
    ///
    /// The list comes from $TACET_AUTOSTART (colon-separated) or
    /// falls back to a small built-in set when unset. Keeping it env-
    /// driven means we don't have to rebuild the compositor every time
    /// the user wants to add a new shell-side service (notification
    /// daemon, status bar, wallpaper) — the daily-driver session can
    /// override via the systemd .desktop entry.
    ///
    /// Called once during compositor startup, after the Wayland socket
    /// is listening and XWayland has been kicked off. Children's
    /// process-group membership tracks the compositor's, so quitting
    /// the compositor reaps them automatically — no orphaned bars.
    pub fn spawn_autostart(&self) {
        // Empty by default in alpha.1 — tacet-launcher isn't yet on the
        // system PATH from this flake. Users opt in via $TACET_AUTOSTART.
        const DEFAULT_AUTOSTART: &[&str] = &[];
        let raw = std::env::var("TACET_AUTOSTART").ok();
        let from_env: Vec<&str> = raw.as_deref().map(|s| s.split(':').collect()).unwrap_or_default();
        let cmds: Vec<&str> = if from_env.is_empty() {
            DEFAULT_AUTOSTART.to_vec()
        } else {
            from_env
        };

        for cmd in cmds {
            let cmd = cmd.trim();
            if cmd.is_empty() {
                continue;
            }
            info!(cmd, "spawn_autostart");
            let result = Command::new(cmd)
                .envs(
                    self.socket_name
                        .clone()
                        .map(|v| ("WAYLAND_DISPLAY", v))
                        .into_iter()
                        .chain(
                            #[cfg(feature = "xwayland")]
                            self.xdisplay.map(|v| ("DISPLAY", format!(":{v}"))),
                            #[cfg(not(feature = "xwayland"))]
                            None,
                        ),
                )
                .spawn();
            if let Err(e) = result {
                error!(cmd, err = %e, "spawn_autostart failed");
            }
        }
    }

    /// Re-map every output's anchor on the Space to the current camera
    /// position and ask the backend for a full redraw on each one. Called
    /// after every camera change (keyboard pan/zoom, MMB drag, wheel
    /// zoom) so smithay's damage tracker doesn't leave stale pixels
    /// behind from the previous frame.
    ///
    /// Multi-monitor layout: outputs are laid out horizontally, each one
    /// offset by the cumulative width of its predecessors. This keeps a
    /// dual-monitor setup as an extended desktop instead of mirrored
    /// — without this offset, every output would be mapped at the same
    /// camera position and the second monitor would just duplicate the
    /// first.
    pub fn apply_camera_state(&mut self) {
        let base = self.camera.position;
        let outputs: Vec<_> = self.space.outputs().cloned().collect();
        let mut offset_x = 0i32;
        for output in outputs {
            let logical_w = output
                .current_mode()
                .map(|m| {
                    let scale = output.current_scale().fractional_scale();
                    let transform = output.current_transform();
                    transform.transform_size(m.size).to_f64().to_logical(scale).w as i32
                })
                .unwrap_or(0);
            self.space.map_output(
                &output,
                Point::from((base.x + offset_x, base.y)),
            );
            self.backend_data.reset_buffers(&output);
            offset_x += logical_w;
        }
        // Push the new effective HiDPI factor (output_scale × camera.zoom)
        // to every surface that speaks wp_fractional_scale_v1, so they
        // re-render at the displayed resolution and stay crisp under zoom.
        // Cheap to call from pan too — set_preferred_scale is a no-op if
        // the value hasn't actually changed.
        self.broadcast_fractional_scale();
    }

    // Allow in this method because of existing usage
    #[allow(clippy::uninlined_format_args)]
    fn process_common_key_action(&mut self, action: KeyAction) {
        match action {
            KeyAction::None => (),

            KeyAction::Quit => {
                info!("Quitting.");
                self.running.store(false, Ordering::SeqCst);
            }

            KeyAction::Run(cmd) => {
                let Some((prog, args)) = cmd.split_first() else {
                    error!("KeyAction::Run with empty argv");
                    return;
                };
                let cmd_str = cmd.join(" ");
                info!(cmd = %cmd_str, "Starting program");

                if let Err(e) = Command::new(prog)
                    .args(args)
                    .envs(
                        self.socket_name
                            .clone()
                            .map(|v| ("WAYLAND_DISPLAY", v))
                            .into_iter()
                            .chain(
                                #[cfg(feature = "xwayland")]
                                self.xdisplay.map(|v| ("DISPLAY", format!(":{v}"))),
                                #[cfg(not(feature = "xwayland"))]
                                None,
                            ),
                    )
                    .spawn()
                {
                    error!(cmd = %cmd_str, err = %e, "Failed to start program");
                }
            }

            KeyAction::TogglePreview => {
                self.show_window_preview = !self.show_window_preview;
            }

            KeyAction::ToggleDecorations => {
                for element in self.space.elements() {
                    #[allow(irrefutable_let_patterns)]
                    if let Some(toplevel) = element.0.toplevel() {
                        let mode_changed = toplevel.with_pending_state(|state| {
                            if let Some(current_mode) = state.decoration_mode {
                                let new_mode =
                                    if current_mode == zxdg_toplevel_decoration_v1::Mode::ClientSide {
                                        zxdg_toplevel_decoration_v1::Mode::ServerSide
                                    } else {
                                        zxdg_toplevel_decoration_v1::Mode::ClientSide
                                    };
                                state.decoration_mode = Some(new_mode);
                                true
                            } else {
                                false
                            }
                        });

                        if mode_changed && toplevel.is_initial_configure_sent() {
                            toplevel.send_pending_configure();
                        }
                    }
                }
            }

            // ─── canvas camera actions ─────────────────────────────────
            // Pan/zoom both re-map output anchors and force a full redraw
            // (smithay's damage tracker doesn't always notice that
            // everything moved, leading to ghost-trail artifacts).
            KeyAction::PanCanvas(dx, dy) => {
                self.camera.pan(dx, dy);
                self.apply_camera_state();
            }
            KeyAction::ZoomIn => {
                self.camera.zoom_in();
                self.apply_camera_state();
            }
            KeyAction::ZoomOut => {
                self.camera.zoom_out();
                self.apply_camera_state();
            }
            KeyAction::ResetCamera => {
                self.camera.reset();
                self.apply_camera_state();
            }

            // Super+Space — stash whatever has keyboard focus right now
            // and hand the seat to the tacet-launcher overlay. We pick
            // the first wlr_layer_shell surface on the Top layer that
            // declares it can receive keyboard focus; this session only
            // ever has one (the chat itself), so no namespace filter is
            // needed yet. Esc on the chat side restores `focus_stack`.
            KeyAction::FocusChat => {
                let Some(keyboard) = self.seat.get_keyboard() else { return };
                let chat_layer = self.space.outputs().find_map(|o| {
                    let map = layer_map_for_output(o);
                    map.layers()
                        .find(|l| {
                            l.can_receive_keyboard_focus()
                                && l.cached_state().layer == WlrLayer::Top
                        })
                        .cloned()
                });
                let Some(chat_layer) = chat_layer else {
                    info!("FocusChat: no Top-layer surface available to focus");
                    return;
                };
                // Don't stash chat-on-top-of-chat — if we're already on
                // the chat (user mashed Super+Space twice), keep the
                // existing stash so Esc still goes back to the *real*
                // previous toplevel, not chat→chat→nothing.
                let serial = SCOUNTER.next_serial();
                let current = keyboard.current_focus();
                let is_chat_already = matches!(
                    &current,
                    Some(KeyboardFocusTarget::LayerSurface(ls)) if ls == &chat_layer
                );
                if !is_chat_already {
                    self.focus_stack = current;
                }
                keyboard.set_focus(self, Some(chat_layer.into()), serial);
            }

            _ => unreachable!(
                "Common key action handler encountered backend specific action {:?}",
                action
            ),
        }
    }

    fn keyboard_key_to_action<B: InputBackend>(&mut self, evt: B::KeyboardKeyEvent) -> KeyAction {
        let keycode = evt.key_code();
        let state = evt.state();
        debug!(?keycode, ?state, "key");
        let serial = SCOUNTER.next_serial();
        let time = Event::time_msec(&evt);
        let mut suppressed_keys = self.suppressed_keys.clone();
        let keyboard = self.seat.get_keyboard().unwrap();

        for layer in self.layer_shell_state.layer_surfaces().rev() {
            let exclusive = layer.with_cached_state(|data| {
                data.keyboard_interactivity == KeyboardInteractivity::Exclusive
                    && (data.layer == WlrLayer::Top || data.layer == WlrLayer::Overlay)
            });
            if exclusive {
                let surface = self.space.outputs().find_map(|o| {
                    let map = layer_map_for_output(o);
                    map.layers().find(|l| l.layer_surface() == &layer).cloned()
                });
                if let Some(surface) = surface {
                    keyboard.set_focus(self, Some(surface.into()), serial);
                    keyboard.input::<(), _>(self, keycode, state, serial, time, |_, _, _| {
                        FilterResult::Forward
                    });
                    return KeyAction::None;
                };
            }
        }

        let inhibited = self
            .space
            .element_under(self.pointer.current_location())
            .and_then(|(window, _)| {
                let surface = window.wl_surface()?;
                self.seat.keyboard_shortcuts_inhibitor_for_surface(&surface)
            })
            .map(|inhibitor| inhibitor.is_active())
            .unwrap_or(false);

        let action = keyboard
            .input(self, keycode, state, serial, time, |data, modifiers, handle| {
                let keysym = handle.modified_sym();

                debug!(
                    ?state,
                    mods = ?modifiers,
                    keysym = ::xkbcommon::xkb::keysym_get_name(keysym),
                    "keysym"
                );

                // If the key is pressed and triggered a action
                // we will not forward the key to the client.
                // Additionally add the key to the suppressed keys
                // so that we can decide on a release if the key
                // should be forwarded to the client or not.
                if let KeyState::Pressed = state {
                    // Esc while we're holding a stashed focus = the user
                    // just summoned chat via Super+Space and is now
                    // dismissing it. Forward Esc to chat so its UI
                    // clears, and set a marker; the outer fn pops the
                    // stash back to the seat after input() returns
                    // (can't set_focus from inside input — keyboard is
                    // already borrowed).
                    if keysym == Keysym::Escape && data.focus_stack.is_some() {
                        data.pending_focus_restore = true;
                        return FilterResult::Forward;
                    }

                    if !inhibited {
                        let action = process_keyboard_shortcut(*modifiers, keysym);

                        if action.is_some() {
                            suppressed_keys.push(keysym);
                        }

                        action
                            .map(FilterResult::Intercept)
                            .unwrap_or(FilterResult::Forward)
                    } else {
                        FilterResult::Forward
                    }
                } else {
                    let suppressed = suppressed_keys.contains(&keysym);
                    if suppressed {
                        suppressed_keys.retain(|k| *k != keysym);
                        FilterResult::Intercept(KeyAction::None)
                    } else {
                        FilterResult::Forward
                    }
                }
            })
            .unwrap_or(KeyAction::None);

        self.suppressed_keys = suppressed_keys;

        // Drain the Esc-restore marker set above. We do this here
        // instead of inside the closure because `keyboard` is borrowed
        // by `keyboard.input(...)`. Forwarding Esc happened inside the
        // closure (chat's JS got it and cleared the UI); now we hand
        // the seat back to whatever toplevel was focused before
        // Super+Space.
        if self.pending_focus_restore {
            self.pending_focus_restore = false;
            if let Some(prev) = self.focus_stack.take() {
                keyboard.set_focus(self, Some(prev), serial);
            }
        }

        action
    }

    fn on_pointer_button<B: InputBackend>(&mut self, evt: B::PointerButtonEvent) {
        let serial = SCOUNTER.next_serial();
        let button = evt.button_code();

        let state = wl_pointer::ButtonState::from(evt.state());

        // Middle-mouse → canvas pan grab. Press flips canvas_panning on;
        // motion handler tracks deltas in output-local coords. Release
        // turns it back off. The previous-position field starts empty —
        // the first motion event after press just records the position
        // (no delta to apply yet).
        const BTN_MIDDLE: u32 = 0x112;
        const BTN_LEFT: u32 = 0x110;
        if button == BTN_MIDDLE {
            match state {
                wl_pointer::ButtonState::Pressed => {
                    self.canvas_panning = true;
                    self.last_pan_local_pos = None;
                    return;
                }
                wl_pointer::ButtonState::Released => {
                    self.canvas_panning = false;
                    self.last_pan_local_pos = None;
                    return;
                }
                _ => {}
            }
        }

        // Super+LMB anywhere on a window → move-window grab (sway/i3 style).
        // Anvil's xdg-shell move_request path only handles CSD title-bar
        // drags; this gives us drag-from-anywhere with the modifier.
        if button == BTN_LEFT && state == wl_pointer::ButtonState::Pressed {
            let mods = self.seat.get_keyboard().unwrap().modifier_state();
            if mods.logo {
                let pointer_loc = self.pointer.current_location();
                if let Some((window, win_loc)) = self
                    .space
                    .element_under(pointer_loc)
                    .map(|(w, p)| (w.clone(), p))
                {
                    self.space.raise_element(&window, true);
                    let start_data = smithay::input::pointer::GrabStartData {
                        focus: None,
                        button,
                        location: pointer_loc,
                    };
                    let grab = crate::shell::grabs::PointerMoveSurfaceGrab {
                        start_data,
                        window,
                        initial_window_location: win_loc,
                    };
                    let pointer = self.pointer.clone();
                    pointer.set_grab(self, grab, serial, smithay::input::pointer::Focus::Clear);
                    return;
                }
            }
        }

        if wl_pointer::ButtonState::Pressed == state {
            self.update_keyboard_focus(self.pointer.current_location(), serial);
        };
        let pointer = self.pointer.clone();
        pointer.button(
            self,
            &ButtonEvent {
                button,
                state: state.try_into().unwrap(),
                serial,
                time: evt.time_msec(),
            },
        );
        pointer.frame(self);
    }

    fn update_keyboard_focus(&mut self, location: Point<f64, Logical>, serial: Serial) {
        let keyboard = self.seat.get_keyboard().unwrap();
        let touch = self.seat.get_touch();
        let input_method = self.seat.input_method();
        // change the keyboard focus unless the pointer or keyboard is grabbed
        // We test for any matching surface type here but always use the root
        // (in case of a window the toplevel) surface for the focus.
        // So for example if a user clicks on a subsurface or popup the toplevel
        // will receive the keyboard focus. Directly assigning the focus to the
        // matching surface leads to issues with clients dismissing popups and
        // subsurface menus (for example firefox-wayland).
        // see here for a discussion about that issue:
        // https://gitlab.freedesktop.org/wayland/wayland/-/issues/294
        if !self.pointer.is_grabbed()
            && (!keyboard.is_grabbed() || input_method.keyboard_grabbed())
            && !touch.map(|touch| touch.is_grabbed()).unwrap_or(false)
        {
            let output = self.space.output_under(location).next().cloned();
            if let Some(output) = output.as_ref() {
                let output_geo = self.space.output_geometry(output).unwrap();
                if let Some(window) = output
                    .user_data()
                    .get::<FullscreenSurface>()
                    .and_then(|f| f.get())
                {
                    if let Some((_, _)) =
                        window.surface_under(location - output_geo.loc.to_f64(), WindowSurfaceType::ALL)
                    {
                        #[cfg(feature = "xwayland")]
                        if let Some(surface) = window.0.x11_surface() {
                            self.xwm.as_mut().unwrap().raise_window(surface).unwrap();
                        }
                        keyboard.set_focus(self, Some(window.into()), serial);
                        return;
                    }
                }

                // Layer surfaces are positioned in OUTPUT-LOCAL SCREEN coords
                // (not affected by canvas zoom/pan), but `location` is in
                // canvas-coords. Project the pointer through the camera to
                // output-local screen-coords before hit-testing the layer map.
                let screen_local = self.camera.canvas_to_output_local(
                    location,
                    output_geo.loc.to_f64(),
                );
                let layers = layer_map_for_output(output);
                if let Some(layer) = layers
                    .layer_under(WlrLayer::Overlay, screen_local)
                    .or_else(|| layers.layer_under(WlrLayer::Top, screen_local))
                {
                    if layer.can_receive_keyboard_focus() {
                        if let Some((_, _)) = layer.surface_under(
                            screen_local - layers.layer_geometry(layer).unwrap().loc.to_f64(),
                            WindowSurfaceType::ALL,
                        ) {
                            keyboard.set_focus(self, Some(layer.clone().into()), serial);
                            return;
                        }
                    }
                }
            }

            if let Some((window, _)) = self.space.element_under(location).map(|(w, p)| (w.clone(), p)) {
                self.space.raise_element(&window, true);
                #[cfg(feature = "xwayland")]
                if let Some(surface) = window.0.x11_surface() {
                    self.xwm.as_mut().unwrap().raise_window(surface).unwrap();
                }
                keyboard.set_focus(self, Some(window.into()), serial);
                return;
            }

            if let Some(output) = output.as_ref() {
                let output_geo = self.space.output_geometry(output).unwrap();
                let screen_local = self.camera.canvas_to_output_local(
                    location,
                    output_geo.loc.to_f64(),
                );
                let layers = layer_map_for_output(output);
                if let Some(layer) = layers
                    .layer_under(WlrLayer::Bottom, screen_local)
                    .or_else(|| layers.layer_under(WlrLayer::Background, screen_local))
                {
                    if layer.can_receive_keyboard_focus() {
                        if let Some((_, _)) = layer.surface_under(
                            screen_local - layers.layer_geometry(layer).unwrap().loc.to_f64(),
                            WindowSurfaceType::ALL,
                        ) {
                            keyboard.set_focus(self, Some(layer.clone().into()), serial);
                        }
                    }
                }
            };
        }
    }

    pub fn surface_under(
        &self,
        pos: Point<f64, Logical>,
    ) -> Option<(PointerFocusTarget, Point<f64, Logical>)> {
        let output = self.space.outputs().find(|o| {
            let geometry = self.space.output_geometry(o).unwrap();
            geometry.contains(pos.to_i32_round())
        })?;
        let output_geo = self.space.output_geometry(output).unwrap();
        let layers = layer_map_for_output(output);

        let mut under = None;
        if let Some((surface, loc)) = output
            .user_data()
            .get::<FullscreenSurface>()
            .and_then(|f| f.get())
            .and_then(|w| w.surface_under(pos - output_geo.loc.to_f64(), WindowSurfaceType::ALL))
        {
            under = Some((surface, loc + output_geo.loc));
        } else if let Some(focus) = layers
            .layer_under(WlrLayer::Overlay, pos - output_geo.loc.to_f64())
            .or_else(|| layers.layer_under(WlrLayer::Top, pos - output_geo.loc.to_f64()))
            .and_then(|layer| {
                let layer_loc = layers.layer_geometry(layer).unwrap().loc;
                layer
                    .surface_under(
                        pos - output_geo.loc.to_f64() - layer_loc.to_f64(),
                        WindowSurfaceType::ALL,
                    )
                    .map(|(surface, loc)| {
                        (
                            PointerFocusTarget::from(surface),
                            loc + layer_loc + output_geo.loc,
                        )
                    })
            })
        {
            under = Some(focus)
        } else if let Some(focus) = self.space.element_under(pos).and_then(|(window, loc)| {
            window
                .surface_under(pos - loc.to_f64(), WindowSurfaceType::ALL)
                .map(|(surface, surf_loc)| (surface, surf_loc + loc))
        }) {
            under = Some(focus);
        } else if let Some(focus) = layers
            .layer_under(WlrLayer::Bottom, pos - output_geo.loc.to_f64())
            .or_else(|| layers.layer_under(WlrLayer::Background, pos - output_geo.loc.to_f64()))
            .and_then(|layer| {
                let layer_loc = layers.layer_geometry(layer).unwrap().loc;
                layer
                    .surface_under(
                        pos - output_geo.loc.to_f64() - layer_loc.to_f64(),
                        WindowSurfaceType::ALL,
                    )
                    .map(|(surface, loc)| {
                        (
                            PointerFocusTarget::from(surface),
                            loc + layer_loc + output_geo.loc,
                        )
                    })
            })
        {
            under = Some(focus)
        };
        under.map(|(s, l)| (s, l.to_f64()))
    }

    fn on_pointer_axis<B: InputBackend>(&mut self, evt: B::PointerAxisEvent) {
        let horizontal_amount = evt
            .amount(input::Axis::Horizontal)
            .unwrap_or_else(|| evt.amount_v120(input::Axis::Horizontal).unwrap_or(0.0) * 15.0 / 120.);
        let vertical_amount = evt
            .amount(input::Axis::Vertical)
            .unwrap_or_else(|| evt.amount_v120(input::Axis::Vertical).unwrap_or(0.0) * 15.0 / 120.);
        let horizontal_amount_discrete = evt.amount_v120(input::Axis::Horizontal);
        let vertical_amount_discrete = evt.amount_v120(input::Axis::Vertical);

        // Ctrl+wheel or Super+wheel → zoom around cursor, intercept before
        // forwarding to clients. Ctrl is preferred for nested mode (Super
        // is usually grabbed by the outer compositor for its own bindings;
        // we keep both so the binding works in both nested and native
        // sessions). One notch (~ ±15) maps to one ZOOM_FACTOR step;
        // sub-notch motion (touchpad) scales proportionally.
        let mods = self.seat.get_keyboard().unwrap().modifier_state();
        if (mods.ctrl || mods.logo) && vertical_amount != 0.0 {
            let cursor = self.pointer.current_location();
            let factor = if vertical_amount > 0.0 {
                // Smithay's convention: positive wheel = scroll down → zoom out.
                1.0 / crate::camera::Camera::ZOOM_FACTOR.powf((vertical_amount / 15.0).abs())
            } else {
                crate::camera::Camera::ZOOM_FACTOR.powf((vertical_amount / 15.0).abs())
            };
            self.camera.zoom_around(cursor, factor);
            self.apply_camera_state();
            return;
        }

        {
            let mut frame = AxisFrame::new(evt.time_msec()).source(evt.source());
            if horizontal_amount != 0.0 {
                frame = frame.relative_direction(Axis::Horizontal, evt.relative_direction(Axis::Horizontal));
                frame = frame.value(Axis::Horizontal, horizontal_amount);
                if let Some(discrete) = horizontal_amount_discrete {
                    frame = frame.v120(Axis::Horizontal, discrete as i32);
                }
            }
            if vertical_amount != 0.0 {
                frame = frame.relative_direction(Axis::Vertical, evt.relative_direction(Axis::Vertical));
                frame = frame.value(Axis::Vertical, vertical_amount);
                if let Some(discrete) = vertical_amount_discrete {
                    frame = frame.v120(Axis::Vertical, discrete as i32);
                }
            }
            if evt.source() == AxisSource::Finger {
                if evt.amount(Axis::Horizontal) == Some(0.0) {
                    frame = frame.stop(Axis::Horizontal);
                }
                if evt.amount(Axis::Vertical) == Some(0.0) {
                    frame = frame.stop(Axis::Vertical);
                }
            }
            let pointer = self.pointer.clone();
            pointer.axis(self, frame);
            pointer.frame(self);
        }
    }
}

#[cfg(any(feature = "winit", feature = "x11"))]
impl<BackendData: Backend> AnvilState<BackendData> {
    pub fn process_input_event_windowed<B: InputBackend>(&mut self, event: InputEvent<B>, output_name: &str) {
        match event {
            InputEvent::Keyboard { event } => match self.keyboard_key_to_action::<B>(event) {
                KeyAction::ScaleUp => {
                    let output = self
                        .space
                        .outputs()
                        .find(|o| o.name() == output_name)
                        .unwrap()
                        .clone();

                    let current_scale = output.current_scale().fractional_scale();
                    let new_scale = current_scale + 0.25;
                    output.change_current_state(None, None, Some(Scale::Fractional(new_scale)), None);

                    crate::shell::fixup_positions(&mut self.space, self.pointer.current_location());
                    self.backend_data.reset_buffers(&output);
                }

                KeyAction::ScaleDown => {
                    let output = self
                        .space
                        .outputs()
                        .find(|o| o.name() == output_name)
                        .unwrap()
                        .clone();

                    let current_scale = output.current_scale().fractional_scale();
                    let new_scale = f64::max(1.0, current_scale - 0.25);
                    output.change_current_state(None, None, Some(Scale::Fractional(new_scale)), None);

                    crate::shell::fixup_positions(&mut self.space, self.pointer.current_location());
                    self.backend_data.reset_buffers(&output);
                }

                KeyAction::RotateOutput => {
                    let output = self
                        .space
                        .outputs()
                        .find(|o| o.name() == output_name)
                        .unwrap()
                        .clone();

                    let current_transform = output.current_transform();
                    let new_transform = match current_transform {
                        Transform::Normal => Transform::_90,
                        Transform::_90 => Transform::_180,
                        Transform::_180 => Transform::_270,
                        Transform::_270 => Transform::Flipped,
                        Transform::Flipped => Transform::Flipped90,
                        Transform::Flipped90 => Transform::Flipped180,
                        Transform::Flipped180 => Transform::Flipped270,
                        Transform::Flipped270 => Transform::Normal,
                    };
                    tracing::info!(?current_transform, ?new_transform, output = ?output.name(), "changing output transform");
                    output.change_current_state(None, Some(new_transform), None, None);
                    crate::shell::fixup_positions(&mut self.space, self.pointer.current_location());
                    self.backend_data.reset_buffers(&output);
                }

                action => match action {
                    KeyAction::None
                    | KeyAction::Quit
                    | KeyAction::Run(_)
                    | KeyAction::TogglePreview
                    | KeyAction::ToggleDecorations
                    | KeyAction::PanCanvas(..)
                    | KeyAction::ZoomIn
                    | KeyAction::ZoomOut
                    | KeyAction::ResetCamera
                    | KeyAction::FocusChat => self.process_common_key_action(action),

                    _ => tracing::warn!(
                        ?action,
                        output_name,
                        "Key action unsupported on on output backend.",
                    ),
                },
            },

            InputEvent::PointerMotionAbsolute { event } => {
                let output = self
                    .space
                    .outputs()
                    .find(|o| o.name() == output_name)
                    .unwrap()
                    .clone();
                self.on_pointer_move_absolute_windowed::<B>(event, &output)
            }
            InputEvent::PointerButton { event } => self.on_pointer_button::<B>(event),
            InputEvent::PointerAxis { event } => self.on_pointer_axis::<B>(event),
            _ => (), // other events are not handled in anvil (yet)
        }
    }

    fn on_pointer_move_absolute_windowed<B: InputBackend>(
        &mut self,
        evt: B::PointerMotionAbsoluteEvent,
        output: &Output,
    ) {
        let output_geo = self.space.output_geometry(output).unwrap();

        // Output-local logical coords: only depends on output_geo.size, which
        // doesn't shift when we call map_output. The space-absolute `pos`
        // (computed below) folds in output_geo.loc and IS our pan target,
        // so using it for pan-delta tracking creates a feedback loop.
        let local_pos = evt.position_transformed(output_geo.size);

        // Canvas pan: while MMB-grab is active, translate the local-coord
        // delta into camera shift instead of forwarding motion to surfaces.
        // First motion event after MMB press just records the position; no
        // delta to apply yet. Screen-pixel delta is divided by camera.zoom
        // to get the canvas-coord shift — at zoom 2× a 100-pixel mouse drag
        // should move the camera by 50 canvas pixels, not 100, so the
        // pointed canvas point stays under the cursor.
        if self.canvas_panning {
            if let Some(prev) = self.last_pan_local_pos {
                let delta = local_pos - prev;
                let z = self.camera.zoom.max(0.001);
                self.camera.pan(
                    -(delta.x / z).round() as i32,
                    -(delta.y / z).round() as i32,
                );
                self.apply_camera_state();
            }
            self.last_pan_local_pos = Some(local_pos);
            return;
        }

        let pos = local_pos + output_geo.loc.to_f64();
        let serial = SCOUNTER.next_serial();

        let pointer = self.pointer.clone();
        let under = self.surface_under(pos);
        pointer.motion(
            self,
            under,
            &MotionEvent {
                location: pos,
                serial,
                time: evt.time_msec(),
            },
        );
        pointer.frame(self);
    }

    pub fn release_all_keys(&mut self) {
        let keyboard = self.seat.get_keyboard().unwrap();
        for keycode in keyboard.pressed_keys() {
            keyboard.input(
                self,
                keycode,
                KeyState::Released,
                SCOUNTER.next_serial(),
                0,
                |_, _, _| FilterResult::Forward::<bool>,
            );
        }
    }
}

#[cfg(feature = "udev")]
impl AnvilState<UdevData> {
    pub fn process_input_event<B: InputBackend>(&mut self, dh: &DisplayHandle, event: InputEvent<B>) {
        match event {
            InputEvent::Keyboard { event, .. } => match self.keyboard_key_to_action::<B>(event) {
                #[cfg(feature = "udev")]
                KeyAction::VtSwitch(vt) => {
                    info!(to = vt, "Trying to switch vt");
                    if let Err(err) = self.backend_data.session.change_vt(vt) {
                        error!(vt, "Error switching vt: {}", err);
                    }
                }
                KeyAction::Screen(num) => {
                    let geometry = self
                        .space
                        .outputs()
                        .nth(num)
                        .map(|o| self.space.output_geometry(o).unwrap());

                    if let Some(geometry) = geometry {
                        let x = geometry.loc.x as f64 + geometry.size.w as f64 / 2.0;
                        let y = geometry.size.h as f64 / 2.0;
                        let location = (x, y).into();
                        let pointer = self.pointer.clone();
                        let under = self.surface_under(location);
                        pointer.motion(
                            self,
                            under,
                            &MotionEvent {
                                location,
                                serial: SCOUNTER.next_serial(),
                                time: self.clock.now().as_millis(),
                            },
                        );
                        pointer.frame(self);
                    }
                }
                KeyAction::ScaleUp => {
                    let pos = self.pointer.current_location().to_i32_round();
                    let output = self
                        .space
                        .outputs()
                        .find(|o| self.space.output_geometry(o).unwrap().contains(pos))
                        .cloned();

                    if let Some(output) = output {
                        let (output_location, scale) = (
                            self.space.output_geometry(&output).unwrap().loc,
                            output.current_scale().fractional_scale(),
                        );
                        let new_scale = scale + 0.25;
                        output.change_current_state(None, None, Some(Scale::Fractional(new_scale)), None);

                        let rescale = scale / new_scale;
                        let output_location = output_location.to_f64();
                        let mut pointer_output_location = self.pointer.current_location() - output_location;
                        pointer_output_location.x *= rescale;
                        pointer_output_location.y *= rescale;
                        let pointer_location = output_location + pointer_output_location;

                        crate::shell::fixup_positions(&mut self.space, pointer_location);
                        let pointer = self.pointer.clone();
                        let under = self.surface_under(pointer_location);
                        pointer.motion(
                            self,
                            under,
                            &MotionEvent {
                                location: pointer_location,
                                serial: SCOUNTER.next_serial(),
                                time: self.clock.now().as_millis(),
                            },
                        );
                        pointer.frame(self);
                        self.backend_data.reset_buffers(&output);
                    }
                }
                KeyAction::ScaleDown => {
                    let pos = self.pointer.current_location().to_i32_round();
                    let output = self
                        .space
                        .outputs()
                        .find(|o| self.space.output_geometry(o).unwrap().contains(pos))
                        .cloned();

                    if let Some(output) = output {
                        let (output_location, scale) = (
                            self.space.output_geometry(&output).unwrap().loc,
                            output.current_scale().fractional_scale(),
                        );
                        let new_scale = f64::max(1.0, scale - 0.25);
                        output.change_current_state(None, None, Some(Scale::Fractional(new_scale)), None);

                        let rescale = scale / new_scale;
                        let output_location = output_location.to_f64();
                        let mut pointer_output_location = self.pointer.current_location() - output_location;
                        pointer_output_location.x *= rescale;
                        pointer_output_location.y *= rescale;
                        let pointer_location = output_location + pointer_output_location;

                        crate::shell::fixup_positions(&mut self.space, pointer_location);
                        let pointer = self.pointer.clone();
                        let under = self.surface_under(pointer_location);
                        pointer.motion(
                            self,
                            under,
                            &MotionEvent {
                                location: pointer_location,
                                serial: SCOUNTER.next_serial(),
                                time: self.clock.now().as_millis(),
                            },
                        );
                        pointer.frame(self);
                        self.backend_data.reset_buffers(&output);
                    }
                }
                KeyAction::RotateOutput => {
                    let pos = self.pointer.current_location().to_i32_round();
                    let output = self
                        .space
                        .outputs()
                        .find(|o| self.space.output_geometry(o).unwrap().contains(pos))
                        .cloned();

                    if let Some(output) = output {
                        let current_transform = output.current_transform();
                        let new_transform = match current_transform {
                            Transform::Normal => Transform::_90,
                            Transform::_90 => Transform::_180,
                            Transform::_180 => Transform::_270,
                            Transform::_270 => Transform::Flipped,
                            Transform::Flipped => Transform::Flipped90,
                            Transform::Flipped90 => Transform::Flipped180,
                            Transform::Flipped180 => Transform::Flipped270,
                            Transform::Flipped270 => Transform::Normal,
                        };
                        output.change_current_state(None, Some(new_transform), None, None);
                        crate::shell::fixup_positions(&mut self.space, self.pointer.current_location());
                        self.backend_data.reset_buffers(&output);
                    }
                }
                KeyAction::ToggleTint => {
                    let mut debug_flags = self.backend_data.debug_flags();
                    debug_flags.toggle(DebugFlags::TINT);
                    self.backend_data.set_debug_flags(debug_flags);
                }

                action => match action {
                    KeyAction::None
                    | KeyAction::Quit
                    | KeyAction::Run(_)
                    | KeyAction::TogglePreview
                    | KeyAction::ToggleDecorations
                    | KeyAction::PanCanvas(..)
                    | KeyAction::ZoomIn
                    | KeyAction::ZoomOut
                    | KeyAction::ResetCamera
                    | KeyAction::FocusChat => self.process_common_key_action(action),

                    _ => unreachable!(),
                },
            },
            InputEvent::PointerMotion { event, .. } => self.on_pointer_move::<B>(dh, event),
            InputEvent::PointerMotionAbsolute { event, .. } => self.on_pointer_move_absolute::<B>(dh, event),
            InputEvent::PointerButton { event, .. } => self.on_pointer_button::<B>(event),
            InputEvent::PointerAxis { event, .. } => self.on_pointer_axis::<B>(event),
            InputEvent::TabletToolAxis { event, .. } => self.on_tablet_tool_axis::<B>(event),
            InputEvent::TabletToolProximity { event, .. } => self.on_tablet_tool_proximity::<B>(dh, event),
            InputEvent::TabletToolTip { event, .. } => self.on_tablet_tool_tip::<B>(event),
            InputEvent::TabletToolButton { event, .. } => self.on_tablet_button::<B>(event),
            InputEvent::GestureSwipeBegin { event, .. } => self.on_gesture_swipe_begin::<B>(event),
            InputEvent::GestureSwipeUpdate { event, .. } => self.on_gesture_swipe_update::<B>(event),
            InputEvent::GestureSwipeEnd { event, .. } => self.on_gesture_swipe_end::<B>(event),
            InputEvent::GesturePinchBegin { event, .. } => self.on_gesture_pinch_begin::<B>(event),
            InputEvent::GesturePinchUpdate { event, .. } => self.on_gesture_pinch_update::<B>(event),
            InputEvent::GesturePinchEnd { event, .. } => self.on_gesture_pinch_end::<B>(event),
            InputEvent::GestureHoldBegin { event, .. } => self.on_gesture_hold_begin::<B>(event),
            InputEvent::GestureHoldEnd { event, .. } => self.on_gesture_hold_end::<B>(event),

            InputEvent::TouchDown { event } => self.on_touch_down::<B>(event),
            InputEvent::TouchUp { event } => self.on_touch_up::<B>(event),
            InputEvent::TouchMotion { event } => self.on_touch_motion::<B>(event),
            InputEvent::TouchFrame { event } => self.on_touch_frame::<B>(event),
            InputEvent::TouchCancel { event } => self.on_touch_cancel::<B>(event),

            InputEvent::DeviceAdded { device } => {
                if device.has_capability(DeviceCapability::TabletTool) {
                    self.seat
                        .tablet_seat()
                        .add_tablet::<Self>(dh, &TabletDescriptor::from(&device));
                }
                if device.has_capability(DeviceCapability::Touch) && self.seat.get_touch().is_none() {
                    self.seat.add_touch();
                }
            }
            InputEvent::DeviceRemoved { device } => {
                if device.has_capability(DeviceCapability::TabletTool) {
                    let tablet_seat = self.seat.tablet_seat();

                    tablet_seat.remove_tablet(&TabletDescriptor::from(&device));

                    // If there are no tablets in seat we can remove all tools
                    if tablet_seat.count_tablets() == 0 {
                        tablet_seat.clear_tools();
                    }
                }
            }
            _ => {
                // other events are not handled in anvil (yet)
            }
        }
    }

    fn on_pointer_move<B: InputBackend>(&mut self, _dh: &DisplayHandle, evt: B::PointerMotionEvent) {
        // Canvas pan on udev backend: relative-motion path. libinput delta is
        // already in accelerated logical pixels, so divide by camera.zoom to
        // keep the canvas point under the cursor at any zoom level. We also
        // skip forwarding the motion to surfaces (return early) — MMB-drag
        // belongs to the compositor while panning.
        if self.canvas_panning {
            let d = evt.delta();
            let z = self.camera.zoom.max(0.001);
            self.camera.pan(
                -(d.x / z).round() as i32,
                -(d.y / z).round() as i32,
            );
            self.apply_camera_state();
            return;
        }

        let mut pointer_location = self.pointer.current_location();
        let serial = SCOUNTER.next_serial();

        let pointer = self.pointer.clone();
        let under = self.surface_under(pointer_location);

        let mut pointer_locked = false;
        let mut pointer_confined = false;
        let mut confine_region = None;
        if let Some((surface, surface_loc)) = under
            .as_ref()
            .and_then(|(target, l)| Some((target.wl_surface()?, l)))
        {
            with_pointer_constraint(&surface, &pointer, |constraint| match constraint {
                Some(constraint) if constraint.is_active() => {
                    // Constraint does not apply if not within region
                    if !constraint
                        .region()
                        .is_none_or(|x| x.contains((pointer_location - *surface_loc).to_i32_round()))
                    {
                        return;
                    }
                    match &*constraint {
                        PointerConstraint::Locked(_locked) => {
                            pointer_locked = true;
                        }
                        PointerConstraint::Confined(confine) => {
                            pointer_confined = true;
                            confine_region = confine.region().cloned();
                        }
                    }
                }
                _ => {}
            });
        }

        // Canvas-coord pointer model: libinput delta is in screen-pixel
        // units, but the space (and everything indexed by it: surface
        // hit-tests, client-side motion events, drag grabs) is in canvas
        // logical units. They coincide at zoom=1; at zoom=2 a 100-px
        // physical mouse move corresponds to 50 canvas units. Divide once
        // here so every consumer downstream (clients, surface_under,
        // clamp_coords) sees a single consistent coord system.
        let z = self.camera.zoom.max(0.001);
        let canvas_delta = smithay::utils::Point::<f64, smithay::utils::Logical>::from((
            evt.delta().x / z,
            evt.delta().y / z,
        ));
        let canvas_delta_unaccel = smithay::utils::Point::<f64, smithay::utils::Logical>::from((
            evt.delta_unaccel().x / z,
            evt.delta_unaccel().y / z,
        ));

        pointer.relative_motion(
            self,
            under.clone(),
            &RelativeMotionEvent {
                delta: canvas_delta,
                delta_unaccel: canvas_delta_unaccel,
                utime: evt.time(),
            },
        );

        // If pointer is locked, only emit relative motion
        if pointer_locked {
            pointer.frame(self);
            return;
        }

        pointer_location += canvas_delta;

        // clamp to screen limits
        // this event is never generated by winit
        pointer_location = self.clamp_coords(pointer_location);

        let new_under = self.surface_under(pointer_location);

        // If confined, don't move pointer if it would go outside surface or region
        if pointer_confined {
            if let Some((surface, surface_loc)) = &under {
                if new_under.as_ref().and_then(|(under, _)| under.wl_surface()) != surface.wl_surface() {
                    pointer.frame(self);
                    return;
                }
                if let Some(region) = confine_region {
                    if !region.contains((pointer_location - *surface_loc).to_i32_round()) {
                        pointer.frame(self);
                        return;
                    }
                }
            }
        }

        pointer.motion(
            self,
            under,
            &MotionEvent {
                location: pointer_location,
                serial,
                time: evt.time_msec(),
            },
        );
        pointer.frame(self);

        // If pointer is now in a constraint region, activate it
        // TODO Anywhere else pointer is moved needs to do this
        if let Some((under, surface_location)) =
            new_under.and_then(|(target, loc)| Some((target.wl_surface()?.into_owned(), loc)))
        {
            with_pointer_constraint(&under, &pointer, |constraint| match constraint {
                Some(constraint) if !constraint.is_active() => {
                    let point = (pointer_location - surface_location).to_i32_round();
                    if constraint.region().is_none_or(|region| region.contains(point)) {
                        constraint.activate();
                    }
                }
                _ => {}
            });
        }
    }

    fn on_pointer_move_absolute<B: InputBackend>(
        &mut self,
        _dh: &DisplayHandle,
        evt: B::PointerMotionAbsoluteEvent,
    ) {
        let serial = SCOUNTER.next_serial();

        let max_x = self
            .space
            .outputs()
            .fold(0, |acc, o| acc + self.space.output_geometry(o).unwrap().size.w);

        let max_h_output = self
            .space
            .outputs()
            .max_by_key(|o| self.space.output_geometry(o).unwrap().size.h)
            .unwrap();

        let max_y = self.space.output_geometry(max_h_output).unwrap().size.h;

        let mut pointer_location = (evt.x_transformed(max_x), evt.y_transformed(max_y)).into();

        // clamp to screen limits
        pointer_location = self.clamp_coords(pointer_location);

        let pointer = self.pointer.clone();
        let under = self.surface_under(pointer_location);

        pointer.motion(
            self,
            under,
            &MotionEvent {
                location: pointer_location,
                serial,
                time: evt.time_msec(),
            },
        );
        pointer.frame(self);
    }

    fn on_tablet_tool_axis<B: InputBackend>(&mut self, evt: B::TabletToolAxisEvent) {
        let tablet_seat = self.seat.tablet_seat();

        if let Some(pointer_location) = self.touch_location_transformed(&evt) {
            let pointer = self.pointer.clone();
            let under = self.surface_under(pointer_location);
            let tablet = tablet_seat.get_tablet(&TabletDescriptor::from(&evt.device()));
            let tool = tablet_seat.get_tool(&evt.tool());

            pointer.motion(
                self,
                under.clone(),
                &MotionEvent {
                    location: pointer_location,
                    serial: SCOUNTER.next_serial(),
                    time: self.clock.now().as_millis(),
                },
            );

            if let (Some(tablet), Some(tool)) = (tablet, tool) {
                if evt.pressure_has_changed() {
                    tool.pressure(evt.pressure());
                }
                if evt.distance_has_changed() {
                    tool.distance(evt.distance());
                }
                if evt.tilt_has_changed() {
                    tool.tilt(evt.tilt());
                }
                if evt.slider_has_changed() {
                    tool.slider_position(evt.slider_position());
                }
                if evt.rotation_has_changed() {
                    tool.rotation(evt.rotation());
                }
                if evt.wheel_has_changed() {
                    tool.wheel(evt.wheel_delta(), evt.wheel_delta_discrete());
                }

                tool.motion(
                    pointer_location,
                    under.and_then(|(f, loc)| f.wl_surface().map(|s| (s.into_owned(), loc))),
                    &tablet,
                    SCOUNTER.next_serial(),
                    evt.time_msec(),
                );
            }

            pointer.frame(self);
        }
    }

    fn on_tablet_tool_proximity<B: InputBackend>(
        &mut self,
        dh: &DisplayHandle,
        evt: B::TabletToolProximityEvent,
    ) {
        let tablet_seat = self.seat.tablet_seat();

        if let Some(pointer_location) = self.touch_location_transformed(&evt) {
            let tool = evt.tool();
            tablet_seat.add_tool::<Self>(self, dh, &tool);

            let pointer = self.pointer.clone();
            let under = self.surface_under(pointer_location);
            let tablet = tablet_seat.get_tablet(&TabletDescriptor::from(&evt.device()));
            let tool = tablet_seat.get_tool(&tool);

            pointer.motion(
                self,
                under.clone(),
                &MotionEvent {
                    location: pointer_location,
                    serial: SCOUNTER.next_serial(),
                    time: evt.time_msec(),
                },
            );
            pointer.frame(self);

            if let (Some(under), Some(tablet), Some(tool)) = (
                under.and_then(|(f, loc)| f.wl_surface().map(|s| (s.into_owned(), loc))),
                tablet,
                tool,
            ) {
                match evt.state() {
                    ProximityState::In => tool.proximity_in(
                        pointer_location,
                        under,
                        &tablet,
                        SCOUNTER.next_serial(),
                        evt.time_msec(),
                    ),
                    ProximityState::Out => tool.proximity_out(evt.time_msec()),
                }
            }
        }
    }

    fn on_tablet_tool_tip<B: InputBackend>(&mut self, evt: B::TabletToolTipEvent) {
        let tool = self.seat.tablet_seat().get_tool(&evt.tool());

        if let Some(tool) = tool {
            match evt.tip_state() {
                TabletToolTipState::Down => {
                    let serial = SCOUNTER.next_serial();
                    tool.tip_down(serial, evt.time_msec());

                    // change the keyboard focus
                    self.update_keyboard_focus(self.pointer.current_location(), serial);
                }
                TabletToolTipState::Up => {
                    tool.tip_up(evt.time_msec());
                }
            }
        }
    }

    fn on_tablet_button<B: InputBackend>(&mut self, evt: B::TabletToolButtonEvent) {
        let tool = self.seat.tablet_seat().get_tool(&evt.tool());

        if let Some(tool) = tool {
            tool.button(
                evt.button(),
                evt.button_state(),
                SCOUNTER.next_serial(),
                evt.time_msec(),
            );
        }
    }

    fn on_gesture_swipe_begin<B: InputBackend>(&mut self, evt: B::GestureSwipeBeginEvent) {
        let serial = SCOUNTER.next_serial();
        let pointer = self.pointer.clone();
        pointer.gesture_swipe_begin(
            self,
            &GestureSwipeBeginEvent {
                serial,
                time: evt.time_msec(),
                fingers: evt.fingers(),
            },
        );
    }

    fn on_gesture_swipe_update<B: InputBackend>(&mut self, evt: B::GestureSwipeUpdateEvent) {
        let pointer = self.pointer.clone();
        pointer.gesture_swipe_update(
            self,
            &GestureSwipeUpdateEvent {
                time: evt.time_msec(),
                delta: evt.delta(),
            },
        );
    }

    fn on_gesture_swipe_end<B: InputBackend>(&mut self, evt: B::GestureSwipeEndEvent) {
        let serial = SCOUNTER.next_serial();
        let pointer = self.pointer.clone();
        pointer.gesture_swipe_end(
            self,
            &GestureSwipeEndEvent {
                serial,
                time: evt.time_msec(),
                cancelled: evt.cancelled(),
            },
        );
    }

    fn on_gesture_pinch_begin<B: InputBackend>(&mut self, evt: B::GesturePinchBeginEvent) {
        let serial = SCOUNTER.next_serial();
        let pointer = self.pointer.clone();
        pointer.gesture_pinch_begin(
            self,
            &GesturePinchBeginEvent {
                serial,
                time: evt.time_msec(),
                fingers: evt.fingers(),
            },
        );
    }

    fn on_gesture_pinch_update<B: InputBackend>(&mut self, evt: B::GesturePinchUpdateEvent) {
        let pointer = self.pointer.clone();
        pointer.gesture_pinch_update(
            self,
            &GesturePinchUpdateEvent {
                time: evt.time_msec(),
                delta: evt.delta(),
                scale: evt.scale(),
                rotation: evt.rotation(),
            },
        );
    }

    fn on_gesture_pinch_end<B: InputBackend>(&mut self, evt: B::GesturePinchEndEvent) {
        let serial = SCOUNTER.next_serial();
        let pointer = self.pointer.clone();
        pointer.gesture_pinch_end(
            self,
            &GesturePinchEndEvent {
                serial,
                time: evt.time_msec(),
                cancelled: evt.cancelled(),
            },
        );
    }

    fn on_gesture_hold_begin<B: InputBackend>(&mut self, evt: B::GestureHoldBeginEvent) {
        let serial = SCOUNTER.next_serial();
        let pointer = self.pointer.clone();
        pointer.gesture_hold_begin(
            self,
            &GestureHoldBeginEvent {
                serial,
                time: evt.time_msec(),
                fingers: evt.fingers(),
            },
        );
    }

    fn on_gesture_hold_end<B: InputBackend>(&mut self, evt: B::GestureHoldEndEvent) {
        let serial = SCOUNTER.next_serial();
        let pointer = self.pointer.clone();
        pointer.gesture_hold_end(
            self,
            &GestureHoldEndEvent {
                serial,
                time: evt.time_msec(),
                cancelled: evt.cancelled(),
            },
        );
    }

    fn touch_location_transformed<B: InputBackend, E: AbsolutePositionEvent<B>>(
        &self,
        evt: &E,
    ) -> Option<Point<f64, Logical>> {
        let output = self
            .space
            .outputs()
            .find(|output| output.name().starts_with("eDP"))
            .or_else(|| self.space.outputs().next());

        let output = output?;
        let output_geometry = self.space.output_geometry(output)?;

        let transform = output.current_transform();
        let size = transform.invert().transform_size(output_geometry.size);
        Some(
            transform.transform_point_in(evt.position_transformed(size), &size.to_f64())
                + output_geometry.loc.to_f64(),
        )
    }

    fn on_touch_down<B: InputBackend>(&mut self, evt: B::TouchDownEvent) {
        let Some(handle) = self.seat.get_touch() else {
            return;
        };

        let Some(touch_location) = self.touch_location_transformed(&evt) else {
            return;
        };

        let serial = SCOUNTER.next_serial();
        self.update_keyboard_focus(touch_location, serial);

        let under = self.surface_under(touch_location);
        handle.down(
            self,
            under,
            &DownEvent {
                slot: evt.slot(),
                location: touch_location,
                serial,
                time: evt.time_msec(),
            },
        );
    }
    fn on_touch_up<B: InputBackend>(&mut self, evt: B::TouchUpEvent) {
        let Some(handle) = self.seat.get_touch() else {
            return;
        };
        let serial = SCOUNTER.next_serial();
        handle.up(
            self,
            &UpEvent {
                slot: evt.slot(),
                serial,
                time: evt.time_msec(),
            },
        )
    }
    fn on_touch_motion<B: InputBackend>(&mut self, evt: B::TouchMotionEvent) {
        let Some(handle) = self.seat.get_touch() else {
            return;
        };
        let Some(touch_location) = self.touch_location_transformed(&evt) else {
            return;
        };

        let under = self.surface_under(touch_location);
        handle.motion(
            self,
            under,
            &smithay::input::touch::MotionEvent {
                slot: evt.slot(),
                location: touch_location,
                time: evt.time_msec(),
            },
        );
    }
    fn on_touch_frame<B: InputBackend>(&mut self, _evt: B::TouchFrameEvent) {
        let Some(handle) = self.seat.get_touch() else {
            return;
        };
        handle.frame(self);
    }
    fn on_touch_cancel<B: InputBackend>(&mut self, _evt: B::TouchCancelEvent) {
        let Some(handle) = self.seat.get_touch() else {
            return;
        };
        handle.cancel(self);
    }

    fn clamp_coords(&self, pos: Point<f64, Logical>) -> Point<f64, Logical> {
        if self.space.outputs().next().is_none() {
            return pos;
        }

        // The pointer lives in canvas-Logical coords. `apply_camera_state`
        // sets each output's `.loc` to `camera.position` (+ per-output
        // x-offset), while `.size` stays in screen-Logical px — so the
        // canvas region currently visible on output `o` is
        //     [loc, loc + size / camera.zoom).
        // The previous clamp pinned the upper-left to (0,0) and the
        // lower-right to Σ output.size.w, mixing both coordinate systems
        // and (under non-zero camera pan) leaving a strip of unreachable
        // canvas at the visible upper-left of the screen — the "left+top
        // wall" symptom.
        let (pos_x, pos_y) = pos.into();
        let zoom = self.camera.zoom.max(0.0001);
        let visible: Vec<(f64, f64, f64, f64)> = self
            .space
            .outputs()
            .filter_map(|o| {
                let g = self.space.output_geometry(o)?;
                Some((
                    g.loc.x as f64,
                    g.loc.y as f64,
                    g.size.w as f64 / zoom,
                    g.size.h as f64 / zoom,
                ))
            })
            .collect();
        if visible.is_empty() {
            return pos;
        }

        let min_x = visible.iter().map(|(x, _, _, _)| *x).fold(f64::INFINITY, f64::min);
        let max_x = visible.iter().map(|(x, _, w, _)| x + w).fold(f64::NEG_INFINITY, f64::max);
        let clamped_x = pos_x.clamp(min_x, max_x);

        if let Some((_, y, _, h)) = visible
            .iter()
            .find(|(x, _, w, _)| clamped_x >= *x && clamped_x < x + w)
        {
            let clamped_y = pos_y.clamp(*y, y + h);
            (clamped_x, clamped_y).into()
        } else {
            (clamped_x, pos_y).into()
        }
    }
}

/// Possible results of a keyboard action
#[allow(dead_code)] // some of these are only read if udev is enabled
#[derive(Debug)]
enum KeyAction {
    /// Quit the compositor
    Quit,
    /// Trigger a vt-switch
    VtSwitch(i32),
    /// Run a command. First element is the program; rest are argv.
    /// (No shell parsing — values flow straight into `Command::args`.)
    Run(Vec<String>),
    /// Switch the current screen
    Screen(usize),
    ScaleUp,
    ScaleDown,
    TogglePreview,
    RotateOutput,
    ToggleTint,
    ToggleDecorations,
    /// Pan canvas viewport by (dx, dy) logical pixels.
    PanCanvas(i32, i32),
    /// Zoom canvas in (× [`Camera::ZOOM_FACTOR`]).
    ZoomIn,
    /// Zoom canvas out (÷ [`Camera::ZOOM_FACTOR`]).
    ZoomOut,
    /// Reset camera: position (0,0), zoom 1.0.
    ResetCamera,
    /// Stash current keyboard focus and hand it to the tacet-launcher
    /// layer-shell surface. Bound to Super+Space — Spotlight/Raycast
    /// summon gesture. Paired with [`KeyAction::None`]-via-Esc-intercept
    /// in `keyboard_key_to_action` that pops the stash back.
    FocusChat,
    /// Do nothing more
    None,
}

fn process_keyboard_shortcut(modifiers: ModifiersState, keysym: Keysym) -> Option<KeyAction> {
    if modifiers.ctrl && modifiers.alt && keysym == Keysym::BackSpace || modifiers.logo && keysym == Keysym::q
    {
        // ctrl+alt+backspace = quit
        // logo + q = quit
        Some(KeyAction::Quit)
    } else if (xkb::KEY_XF86Switch_VT_1..=xkb::KEY_XF86Switch_VT_12).contains(&keysym.raw()) {
        // VTSwitch
        Some(KeyAction::VtSwitch(
            (keysym.raw() - xkb::KEY_XF86Switch_VT_1 + 1) as i32,
        ))
    } else if modifiers.logo && keysym == Keysym::Return {
        // Super+Enter → spawn a terminal. Resolution order:
        //   1. $TACET_TERMINAL — our explicit override (highest priority)
        //   2. $TERMINAL           — Unix convention; respect what the user
        //                            already configured as their default
        //   3. xdg-terminal-exec   — XDG spec command that defers to the
        //                            user's desktop's default-app setting
        //   4. PATH-search a list of known Wayland-friendly terminals
        //   5. "foot" as last resort, even if absent (logs error on spawn)
        let in_path = |cmd: &str| {
            std::env::var_os("PATH")
                .map(|paths| std::env::split_paths(&paths).any(|d| d.join(cmd).is_file()))
                .unwrap_or(false)
        };
        let cmd = std::env::var("TACET_TERMINAL")
            .ok()
            .or_else(|| std::env::var("TERMINAL").ok())
            .or_else(|| in_path("xdg-terminal-exec").then(|| "xdg-terminal-exec".into()))
            .unwrap_or_else(|| {
                const CANDIDATES: &[&str] = &["foot", "alacritty", "kitty", "wezterm", "xterm"];
                CANDIDATES
                    .iter()
                    .find(|c| in_path(c))
                    .copied()
                    .unwrap_or("foot")
                    .to_string()
            });
        // TACET_TERMINAL may contain args (e.g. "foot --hold"). Split
        // on whitespace so they flow into Command::args rather than
        // being mistaken for the program name.
        Some(KeyAction::Run(
            cmd.split_whitespace().map(str::to_owned).collect(),
        ))
    } else if modifiers.logo && keysym == Keysym::b {
        // Super+B → open tacet-browser on a self-identifying debug
        // page. Override the target by setting $TACET_BROWSER_DEBUG_URL
        // (handy when iterating on a local generative UI).
        let url = std::env::var("TACET_BROWSER_DEBUG_URL")
            .unwrap_or_else(|_| "data:text/html,<h1>tacet-browser</h1>".to_string());
        Some(KeyAction::Run(vec!["tacet-browser".into(), url]))
    } else if modifiers.logo && (xkb::KEY_1..=xkb::KEY_9).contains(&keysym.raw()) {
        Some(KeyAction::Screen((keysym.raw() - xkb::KEY_1) as usize))
    } else if modifiers.logo && modifiers.shift && keysym == Keysym::M {
        Some(KeyAction::ScaleDown)
    } else if modifiers.logo && modifiers.shift && keysym == Keysym::P {
        Some(KeyAction::ScaleUp)
    } else if modifiers.logo && modifiers.shift && keysym == Keysym::W {
        Some(KeyAction::TogglePreview)
    } else if modifiers.logo && modifiers.shift && keysym == Keysym::R {
        Some(KeyAction::RotateOutput)
    } else if modifiers.logo && modifiers.shift && keysym == Keysym::T {
        Some(KeyAction::ToggleTint)
    } else if modifiers.logo && modifiers.shift && keysym == Keysym::D {
        Some(KeyAction::ToggleDecorations)

    // ─── canvas pan/zoom (tacet) ─────────────────────────────────
    // Pan via Mod+Ctrl+arrows. Step size lives in [`Camera::PAN_STEP`].
    } else if modifiers.logo && modifiers.ctrl && keysym == Keysym::Left {
        Some(KeyAction::PanCanvas(-crate::camera::Camera::PAN_STEP, 0))
    } else if modifiers.logo && modifiers.ctrl && keysym == Keysym::Right {
        Some(KeyAction::PanCanvas(crate::camera::Camera::PAN_STEP, 0))
    } else if modifiers.logo && modifiers.ctrl && keysym == Keysym::Up {
        Some(KeyAction::PanCanvas(0, -crate::camera::Camera::PAN_STEP))
    } else if modifiers.logo && modifiers.ctrl && keysym == Keysym::Down {
        Some(KeyAction::PanCanvas(0, crate::camera::Camera::PAN_STEP))
    // Zoom via Mod+= / Mod+- (Keysym::equal because Shift isn't required;
    // Keysym::plus catches the shifted variant too).
    } else if modifiers.logo && (keysym == Keysym::equal || keysym == Keysym::plus) {
        Some(KeyAction::ZoomIn)
    } else if modifiers.logo && (keysym == Keysym::minus || keysym == Keysym::underscore) {
        Some(KeyAction::ZoomOut)
    } else if modifiers.logo && keysym == Keysym::_0 {
        Some(KeyAction::ResetCamera)

    // Super+Space → focus the tacet-launcher command-bar overlay.
    // Spotlight/Raycast summon gesture; paired with the Esc-intercept
    // in keyboard_key_to_action() that pops the stashed previous focus.
    } else if modifiers.logo && keysym == Keysym::space {
        Some(KeyAction::FocusChat)

    } else {
        None
    }
}
