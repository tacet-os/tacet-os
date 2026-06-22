//! Canvas grid rendering for tacet.
//!
//! Produces [`SolidColorRenderElement`]s for the infinite grid lines that
//! make the canvas visible at rest. Minor lines every [`GRID_SIZE`] logical
//! pixels, major lines every [`MAJOR_EVERY`] minors with a slightly
//! brighter color. Camera position is subtracted so the grid pans with
//! the viewport; lines outside the output's rectangle are skipped.

use smithay::{
    backend::renderer::{
        Color32F,
        element::{Id, Kind, solid::SolidColorRenderElement},
        utils::CommitCounter,
    },
    output::Output,
    utils::{Point, Rectangle, Size},
};

use crate::camera::Camera;

/// Logical-pixel distance between consecutive grid lines.
pub const GRID_SIZE: i32 = 64;
/// Logical-pixel thickness of each grid line.
pub const LINE_WIDTH: i32 = 1;
/// Every Nth minor line is rendered as a major line.
pub const MAJOR_EVERY: i32 = 8;

const COLOR_MINOR: Color32F = Color32F::new(0.18, 0.18, 0.24, 1.0);
const COLOR_MAJOR: Color32F = Color32F::new(0.28, 0.28, 0.36, 1.0);

pub fn grid_elements(output: &Output, camera: &Camera) -> Vec<SolidColorRenderElement> {
    let Some(mode) = output.current_mode() else {
        return Vec::new();
    };
    let scale = output.current_scale().fractional_scale();
    let transform = output.current_transform();
    let size_logical = transform.transform_size(mode.size).to_f64().to_logical(scale);
    let width_screen = size_logical.w.ceil() as i32;
    let height_screen = size_logical.h.ceil() as i32;

    // Visible canvas span = screen span ÷ zoom. With zoom < 1 we need to
    // emit lines well past the screen edge in pre-zoom coordinates, because
    // the RescaleRenderElement wrapper shrinks them on render. At zoom > 1
    // we'd emit slightly too many lines (cheap, just integer push).
    let zoom = camera.zoom.max(0.001);
    let width = ((width_screen as f64) / zoom).ceil() as i32 + GRID_SIZE;
    let height = ((height_screen as f64) / zoom).ceil() as i32 + GRID_SIZE;

    // Grid line width is specified in screen pixels, but the geometry we
    // emit is in canvas coords (pre-RescaleRenderElement). At zoom < 1
    // the rescale shrinks canvas-pixel lines below sub-pixel — they round
    // to 0 and the grid disappears. Compensate by widening the line in
    // canvas-coords so the post-rescale screen width stays ≥ LINE_WIDTH.
    // At zoom ≥ 1 we keep canvas-pixel width (lines look 1+ screen px
    // anyway after rescale-up).
    let line_w_canvas = if zoom < 1.0 {
        ((LINE_WIDTH as f64) / zoom).ceil() as i32
    } else {
        LINE_WIDTH
    };

    let cx = camera.position.x;
    let cy = camera.position.y;
    let major_step = GRID_SIZE * MAJOR_EVERY;

    let mut elements =
        Vec::with_capacity(((width / GRID_SIZE) + (height / GRID_SIZE) + 4) as usize);

    // Vertical lines — walk canvas x-coordinates from the first multiple of
    // GRID_SIZE just left of the viewport to just past its right edge.
    let first_canvas_x = (cx.div_euclid(GRID_SIZE)) * GRID_SIZE;
    let mut canvas_x = first_canvas_x;
    while canvas_x <= cx + width {
        let screen_x = canvas_x - cx;
        let color = if canvas_x.rem_euclid(major_step) == 0 {
            COLOR_MAJOR
        } else {
            COLOR_MINOR
        };
        let geometry = Rectangle::new(
            Point::from((screen_x, 0)).to_physical_precise_round(scale),
            Size::from((line_w_canvas, height)).to_physical_precise_round(scale),
        );
        elements.push(SolidColorRenderElement::new(
            Id::new(),
            geometry,
            CommitCounter::default(),
            color,
            Kind::Unspecified,
        ));
        canvas_x += GRID_SIZE;
    }

    // Horizontal lines — bounds also widened by `width / zoom` so the lines
    // span the post-rescale screen width.
    let first_canvas_y = (cy.div_euclid(GRID_SIZE)) * GRID_SIZE;
    let mut canvas_y = first_canvas_y;
    while canvas_y <= cy + height {
        let screen_y = canvas_y - cy;
        let color = if canvas_y.rem_euclid(major_step) == 0 {
            COLOR_MAJOR
        } else {
            COLOR_MINOR
        };
        let geometry = Rectangle::new(
            Point::from((0, screen_y)).to_physical_precise_round(scale),
            Size::from((width, line_w_canvas)).to_physical_precise_round(scale),
        );
        elements.push(SolidColorRenderElement::new(
            Id::new(),
            geometry,
            CommitCounter::default(),
            color,
            Kind::Unspecified,
        ));
        canvas_y += GRID_SIZE;
    }

    elements
}
