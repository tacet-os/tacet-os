use smithay::{
    backend::renderer::{
        Color32F, ImportAll, ImportMem, Renderer,
        damage::{Error as OutputDamageTrackerError, OutputDamageTracker, RenderOutputResult},
        element::{
            AsRenderElements, RenderElement, Wrap,
            surface::WaylandSurfaceRenderElement,
            utils::{
                ConstrainAlign, ConstrainScaleBehavior, CropRenderElement, RelocateRenderElement,
                RescaleRenderElement,
            },
        },
    },
    desktop::{
        layer_map_for_output,
        space::{
            ConstrainBehavior, ConstrainReference, Space, SpaceRenderElements, constrain_space_element,
        },
    },
    output::Output,
    utils::{Point, Rectangle, Scale, Size},
    wayland::shell::wlr_layer::Layer as WlrLayer,
};

#[cfg(feature = "debug")]
use crate::drawing::FpsElement;
use crate::{
    drawing::{CLEAR_COLOR, CLEAR_COLOR_FULLSCREEN, PointerRenderElement},
    shell::{FullscreenSurface, WindowElement, WindowRenderElement},
};

smithay::backend::renderer::element::render_elements! {
    pub CustomRenderElements<R> where
        R: ImportAll + ImportMem;
    Pointer=PointerRenderElement<R>,
    Surface=WaylandSurfaceRenderElement<R>,
    Grid=smithay::backend::renderer::element::solid::SolidColorRenderElement,
    #[cfg(feature = "debug")]
    // Note: We would like to borrow this element instead, but that would introduce
    // a feature-dependent lifetime, which introduces a lot more feature bounds
    // as the whole type changes and we can't have an unused lifetime (for when "debug" is disabled)
    // in the declaration.
    Fps=FpsElement<R::TextureId>,
}

impl<R: Renderer> std::fmt::Debug for CustomRenderElements<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pointer(arg0) => f.debug_tuple("Pointer").field(arg0).finish(),
            Self::Surface(arg0) => f.debug_tuple("Surface").field(arg0).finish(),
            Self::Grid(arg0) => f.debug_tuple("Grid").field(arg0).finish(),
            #[cfg(feature = "debug")]
            Self::Fps(arg0) => f.debug_tuple("Fps").field(arg0).finish(),
            Self::_GenericCatcher(arg0) => f.debug_tuple("_GenericCatcher").field(arg0).finish(),
        }
    }
}

smithay::backend::renderer::element::render_elements! {
    pub OutputRenderElements<R, E> where R: ImportAll + ImportMem;
    Space=SpaceRenderElements<R, E>,
    Window=Wrap<E>,
    Custom=CustomRenderElements<R>,
    Preview=CropRenderElement<RelocateRenderElement<RescaleRenderElement<WindowRenderElement<R>>>>,
    // Canvas zoom: per-window rescale around screen-origin so toplevels
    // scale together with the canvas grid. Layer-shell surfaces are
    // rendered OUTSIDE this wrapper — they're owned by the compositor
    // (anchor + exclusive_zone semantics), not by the canvas, so they
    // must stay 1:1 with the physical screen regardless of canvas zoom.
    //
    // Concrete WindowRenderElement<R> (not the generic `E`) to keep the
    // macro-generated `From` impls non-overlapping with ZoomedGrid's
    // RescaleRenderElement<SolidColorRenderElement>. In practice E is
    // always WindowRenderElement<R> at our call sites, so we lose
    // nothing by being explicit.
    ZoomedWindow=RescaleRenderElement<WindowRenderElement<R>>,
    // Layer-shell surface: emitted in screen-space, no zoom transform.
    // This is what makes panels / chat-bar / notifications stable while
    // the user pans + zooms the canvas underneath.
    LayerSurface=WaylandSurfaceRenderElement<R>,
    // Grid lines scaled to match zoom so spacing reads consistent at any zoom.
    ZoomedGrid=RescaleRenderElement<smithay::backend::renderer::element::solid::SolidColorRenderElement>,
}

impl<R: Renderer + ImportAll + ImportMem, E: RenderElement<R> + std::fmt::Debug> std::fmt::Debug
    for OutputRenderElements<R, E>
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Space(arg0) => f.debug_tuple("Space").field(arg0).finish(),
            Self::Window(arg0) => f.debug_tuple("Window").field(arg0).finish(),
            Self::Custom(arg0) => f.debug_tuple("Custom").field(arg0).finish(),
            Self::Preview(arg0) => f.debug_tuple("Preview").field(arg0).finish(),
            Self::ZoomedWindow(arg0) => f.debug_tuple("ZoomedWindow").field(arg0).finish(),
            Self::LayerSurface(arg0) => f.debug_tuple("LayerSurface").field(arg0).finish(),
            Self::ZoomedGrid(arg0) => f.debug_tuple("ZoomedGrid").field(arg0).finish(),
            Self::_GenericCatcher(arg0) => f.debug_tuple("_GenericCatcher").field(arg0).finish(),
        }
    }
}

pub fn space_preview_elements<'a, R, C>(
    renderer: &'a mut R,
    space: &'a Space<WindowElement>,
    output: &'a Output,
) -> impl Iterator<Item = C> + 'a
where
    R: Renderer + ImportAll + ImportMem,
    R::TextureId: Clone + 'static,
    C: From<CropRenderElement<RelocateRenderElement<RescaleRenderElement<WindowRenderElement<R>>>>> + 'a,
{
    let constrain_behavior = ConstrainBehavior {
        reference: ConstrainReference::BoundingBox,
        behavior: ConstrainScaleBehavior::Fit,
        align: ConstrainAlign::CENTER,
    };

    let preview_padding = 10;

    let elements_on_space = space.elements_for_output(output).count();
    let output_scale = output.current_scale().fractional_scale();
    let output_transform = output.current_transform();
    let output_size = output
        .current_mode()
        .map(|mode| {
            output_transform
                .transform_size(mode.size)
                .to_f64()
                .to_logical(output_scale)
        })
        .unwrap_or_default();

    let max_elements_per_row = 4;
    let elements_per_row = usize::min(elements_on_space, max_elements_per_row);
    let rows = f64::ceil(elements_on_space as f64 / elements_per_row as f64);

    let preview_size = Size::from((
        f64::round(output_size.w / elements_per_row as f64) as i32 - preview_padding * 2,
        f64::round(output_size.h / rows) as i32 - preview_padding * 2,
    ));

    space
        .elements_for_output(output)
        .enumerate()
        .flat_map(move |(element_index, window)| {
            let column = element_index % elements_per_row;
            let row = element_index / elements_per_row;
            let preview_location = Point::from((
                preview_padding + (preview_padding + preview_size.w) * column as i32,
                preview_padding + (preview_padding + preview_size.h) * row as i32,
            ));
            let constrain = Rectangle::new(preview_location, preview_size);
            constrain_space_element(
                renderer,
                window,
                preview_location,
                1.0,
                output_scale,
                constrain,
                constrain_behavior,
            )
        })
}

#[profiling::function]
pub fn output_elements<R>(
    output: &Output,
    space: &Space<WindowElement>,
    custom_elements: impl IntoIterator<Item = CustomRenderElements<R>>,
    renderer: &mut R,
    show_window_preview: bool,
    camera: &crate::camera::Camera,
) -> (Vec<OutputRenderElements<R, WindowRenderElement<R>>>, Color32F)
where
    R: Renderer + ImportAll + ImportMem,
    R::TextureId: Clone + 'static,
{
    if let Some(window) = output
        .user_data()
        .get::<FullscreenSurface>()
        .and_then(|f| f.get())
    {
        let scale = output.current_scale().fractional_scale().into();
        let window_render_elements: Vec<WindowRenderElement<R>> =
            AsRenderElements::<R>::render_elements(&window, renderer, (0, 0).into(), scale, 1.0);

        let elements = custom_elements
            .into_iter()
            .map(OutputRenderElements::from)
            .chain(
                window_render_elements
                    .into_iter()
                    .map(|e| OutputRenderElements::Window(Wrap::from(e))),
            )
            .collect::<Vec<_>>();
        (elements, CLEAR_COLOR_FULLSCREEN)
    } else {
        let mut output_render_elements = custom_elements
            .into_iter()
            .map(OutputRenderElements::from)
            .collect::<Vec<_>>();

        if show_window_preview && space.elements_for_output(output).count() > 0 {
            output_render_elements.extend(space_preview_elements(renderer, space, output));
        }

        // Render-element order in smithay is TOP → BOTTOM (first emitted
        // is drawn last, i.e. on top). The three layer-classes need this
        // stacking on the canvas-OS:
        //
        //   1. Overlay  +  Top    layer-shell  — screen-space, unzoomed
        //   2. xdg toplevels                    — canvas-space, zoomed
        //   3. Grid                             — canvas-space, zoomed
        //   4. Bottom   +  Background layer-shell — screen-space, unzoomed
        //
        // We replace anvil's `space_render_elements` (which mixes layer-
        // shell INTO the same vec as toplevels) with two separate passes
        // so we can apply RescaleRenderElement only to the toplevel/grid
        // elements. Chat-bar, panels, notifications stay 1:1 with the
        // physical pixel grid regardless of canvas zoom.
        let output_scale = output.current_scale().fractional_scale();
        let scale = Scale::from(output_scale);
        let zoom_scale = Scale::from(camera.zoom);
        let zoom_origin = Point::<i32, smithay::utils::Physical>::from((0, 0));

        // ── 1. Upper layers (Overlay + Top) — screen-space, on top ──
        {
            let layer_map = layer_map_for_output(output);
            for layer in layer_map
                .layers_on(WlrLayer::Overlay)
                .chain(layer_map.layers_on(WlrLayer::Top))
            {
                let Some(geo) = layer_map.layer_geometry(layer) else { continue };
                let phys_loc = geo.loc.to_physical_precise_round(output_scale);
                let surf_elements: Vec<WaylandSurfaceRenderElement<R>> =
                    layer.render_elements(renderer, phys_loc, scale, 1.0);
                output_render_elements.extend(
                    surf_elements
                        .into_iter()
                        .map(OutputRenderElements::LayerSurface),
                );
            }
        }

        // ── 2. xdg toplevels — canvas-space, wrapped in zoom ──
        if let Some(output_geo) = space.output_geometry(output) {
            let window_elements = space
                .render_elements_for_region(renderer, &output_geo, output_scale, 1.0);
            output_render_elements.extend(window_elements.into_iter().map(|e| {
                OutputRenderElements::ZoomedWindow(
                    RescaleRenderElement::from_element(e, zoom_origin, zoom_scale),
                )
            }));
        }

        // ── 3. Canvas grid — canvas-space, zoomed, below windows ──
        output_render_elements.extend(
            crate::grid::grid_elements(output, camera).into_iter().map(|e| {
                OutputRenderElements::ZoomedGrid(RescaleRenderElement::from_element(
                    e,
                    zoom_origin,
                    zoom_scale,
                ))
            }),
        );

        // ── 4. Lower layers (Bottom + Background) — screen-space, behind ──
        {
            let layer_map = layer_map_for_output(output);
            for layer in layer_map
                .layers_on(WlrLayer::Bottom)
                .chain(layer_map.layers_on(WlrLayer::Background))
            {
                let Some(geo) = layer_map.layer_geometry(layer) else { continue };
                let phys_loc = geo.loc.to_physical_precise_round(output_scale);
                let surf_elements: Vec<WaylandSurfaceRenderElement<R>> =
                    layer.render_elements(renderer, phys_loc, scale, 1.0);
                output_render_elements.extend(
                    surf_elements
                        .into_iter()
                        .map(OutputRenderElements::LayerSurface),
                );
            }
        }

        (output_render_elements, CLEAR_COLOR)
    }
}

#[allow(clippy::too_many_arguments)]
pub fn render_output<'a, 'd, R>(
    output: &'a Output,
    space: &'a Space<WindowElement>,
    custom_elements: impl IntoIterator<Item = CustomRenderElements<R>>,
    renderer: &'a mut R,
    framebuffer: &'a mut R::Framebuffer<'_>,
    damage_tracker: &'d mut OutputDamageTracker,
    age: usize,
    show_window_preview: bool,
    camera: &crate::camera::Camera,
) -> Result<RenderOutputResult<'d>, OutputDamageTrackerError<R::Error>>
where
    R: Renderer + ImportAll + ImportMem,
    R::TextureId: Clone + 'static,
{
    let (elements, clear_color) =
        output_elements(output, space, custom_elements, renderer, show_window_preview, camera);
    damage_tracker.render_output(renderer, framebuffer, age, &elements, clear_color)
}
