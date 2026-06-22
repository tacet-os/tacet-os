//! softbuffer + cosmic-text renderer.
//!
//! Per frame:
//!   1. Lock the Term, snapshot every visible cell into a `Vec<RowSpan>`
//!      grouped by background colour runs (so we can paint backgrounds
//!      cheaply with rectangles before any text).
//!   2. Release the lock.
//!   3. Paint the framebuffer:
//!        a. Clear to default-bg.
//!        b. Per row: paint background-runs as horizontal stripes.
//!        c. Per cell with a non-default fg-colour char: rasterize the
//!           glyph via the SwashCache and blit on top of the bg pixels.
//!        d. Cursor: invert fg/bg of the cursor cell (block cursor only).
//!
//! Why not use cosmic-text's `Buffer::set_rich_text` + `Buffer::draw`?
//! We want STRICT cell positioning — glyph X = col * cell_width — and
//! letting cosmic-text shape a 200-col line introduces tiny rounding
//! drift that breaks the grid look. Per-cell rasterization through
//! the `SwashCache` is what alacritty/wezterm also do, and the
//! per-glyph cache keeps it fast.

use std::num::NonZeroU32;
use std::sync::Arc;

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::Point;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::Term;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::vte::ansi::{Color as AnsiColor, NamedColor, Rgb};
use cosmic_text::fontdb::{Family, Query, Stretch, Style, Weight};
use cosmic_text::{
    Attrs, Buffer as TextBuffer, CacheKey, CacheKeyFlags, FamilyOwned, FontSystem, Metrics,
    Shaping, SubpixelBin, SwashCache,
};
use softbuffer::{Buffer as PixelBuffer, Surface};
use winit::window::Window;

use crate::term::EventProxy;

/// Default RGB foreground for cells with `Color::Named(Foreground)`.
const DEFAULT_FG: Rgb = Rgb { r: 0xeb, g: 0xeb, b: 0xeb };
/// Default RGB background.
const DEFAULT_BG: Rgb = Rgb { r: 0x10, g: 0x10, b: 0x14 };

/// Tango/xterm-ish 16 colour palette for `NamedColor::{Red,Green,…}`
/// and the first 16 entries of the 256-colour table. Order matches
/// `NamedColor as u8`.
const PALETTE_16: [Rgb; 16] = [
    Rgb { r: 0x1d, g: 0x1f, b: 0x21 }, // Black
    Rgb { r: 0xcc, g: 0x66, b: 0x66 }, // Red
    Rgb { r: 0xb5, g: 0xbd, b: 0x68 }, // Green
    Rgb { r: 0xf0, g: 0xc6, b: 0x74 }, // Yellow
    Rgb { r: 0x81, g: 0xa2, b: 0xbe }, // Blue
    Rgb { r: 0xb2, g: 0x94, b: 0xbb }, // Magenta
    Rgb { r: 0x8a, g: 0xbe, b: 0xb7 }, // Cyan
    Rgb { r: 0xc5, g: 0xc8, b: 0xc6 }, // White
    Rgb { r: 0x66, g: 0x66, b: 0x66 }, // BrightBlack
    Rgb { r: 0xff, g: 0x33, b: 0x34 }, // BrightRed
    Rgb { r: 0xb9, g: 0xca, b: 0x4a }, // BrightGreen
    Rgb { r: 0xe7, g: 0xc5, b: 0x47 }, // BrightYellow
    Rgb { r: 0x70, g: 0x9a, b: 0xc8 }, // BrightBlue
    Rgb { r: 0x9b, g: 0x64, b: 0xfb }, // BrightMagenta
    Rgb { r: 0x54, g: 0xce, b: 0xd6 }, // BrightCyan
    Rgb { r: 0xff, g: 0xff, b: 0xff }, // BrightWhite
];

/// Glyph metrics chosen at startup. JetBrains Mono 14pt at 1.0 scale.
pub const FONT_SIZE: f32 = 14.0;
pub const LINE_HEIGHT: f32 = 18.0;

/// Renderer owning the font + glyph + window state.
pub struct Renderer {
    pub window: Arc<Window>,
    surface: Surface<Arc<Window>, Arc<Window>>,
    font_system: FontSystem,
    swash_cache: SwashCache,
    /// fontdb ID for the monospace regular face we picked at startup.
    regular_font_id: cosmic_text::fontdb::ID,
    /// Optional bold face. Falls back to `regular_font_id` if no bold
    /// face was discoverable in the system font db.
    bold_font_id: cosmic_text::fontdb::ID,
    /// Width of one cell in device pixels (advance width of 'M' at
    /// the chosen size). Integer because the grid is integer-aligned.
    pub cell_width: u32,
    /// Height of one cell — line_height rounded up.
    pub cell_height: u32,
    /// Y-offset within a cell for the glyph baseline.
    baseline: u32,
}

impl Renderer {
    pub fn new(window: Arc<Window>) -> anyhow::Result<Self> {
        let context = softbuffer::Context::new(window.clone())
            .map_err(|e| anyhow::anyhow!("softbuffer context: {e}"))?;
        let surface = Surface::new(&context, window.clone())
            .map_err(|e| anyhow::anyhow!("softbuffer surface: {e}"))?;

        let mut font_system = FontSystem::new();

        // Pick a monospace face. Prefer named fallbacks (JetBrains
        // Mono / DejaVu Sans Mono) since `Family::Monospace` resolves
        // through `db.set_monospace_family()` which cosmic-text
        // hard-codes to "Noto Sans Mono" — which may not be present.
        let regular_font_id = pick_monospace(&font_system, Weight::NORMAL)
            .ok_or_else(|| anyhow::anyhow!("no monospace font found in system font db"))?;
        let bold_font_id =
            pick_monospace(&font_system, Weight::BOLD).unwrap_or(regular_font_id);

        // Measure cell size by shaping 'M' through cosmic-text once.
        // We touch font_system mutably here, so this has to be done
        // before storing the field.
        let (cell_width, cell_height, baseline) =
            measure_cell(&mut font_system, regular_font_id);

        Ok(Self {
            window,
            surface,
            font_system,
            swash_cache: SwashCache::new(),
            regular_font_id,
            bold_font_id,
            cell_width: cell_width.max(1),
            cell_height: cell_height.max(1),
            baseline,
        })
    }

    /// Recompute (cols, rows) for a given pixel size.
    pub fn grid_size(&self, width: u32, height: u32) -> (u32, u32) {
        let cols = (width / self.cell_width).max(1);
        let rows = (height / self.cell_height).max(1);
        (cols, rows)
    }

    /// Render one frame. Caller is responsible for redraw scheduling.
    pub fn paint(&mut self, term: &FairMutex<Term<EventProxy>>) -> anyhow::Result<()> {
        let size = self.window.inner_size();
        let (width, height) = (size.width.max(1), size.height.max(1));
        let nz_w = NonZeroU32::new(width).unwrap();
        let nz_h = NonZeroU32::new(height).unwrap();
        self.surface
            .resize(nz_w, nz_h)
            .map_err(|e| anyhow::anyhow!("softbuffer resize: {e}"))?;
        let mut framebuf: PixelBuffer<'_, Arc<Window>, Arc<Window>> = self
            .surface
            .buffer_mut()
            .map_err(|e| anyhow::anyhow!("softbuffer buffer_mut: {e}"))?;

        // 1. Clear to default background.
        let default_bg_u32 = rgb_to_u32(DEFAULT_BG);
        for px in framebuf.iter_mut() {
            *px = default_bg_u32;
        }

        // 2. Snapshot cells. Hold the lock only as long as needed.
        let snapshot = {
            let term = term.lock();
            CellSnapshot::from_term(&term)
        };

        // 3. Paint backgrounds (row stripes per run of same-bg cells).
        for (row, cells) in snapshot.cells.iter().enumerate() {
            let y0 = row as u32 * self.cell_height;
            if y0 >= height {
                break;
            }
            let mut run_start_col: u32 = 0;
            let mut run_bg: Rgb = cells.first().map(|c| c.bg).unwrap_or(DEFAULT_BG);
            for (col_idx, cell) in cells.iter().enumerate() {
                let col = col_idx as u32;
                if cell.bg != run_bg {
                    fill_rect(
                        &mut framebuf,
                        width,
                        height,
                        run_start_col * self.cell_width,
                        y0,
                        (col - run_start_col) * self.cell_width,
                        self.cell_height,
                        run_bg,
                    );
                    run_start_col = col;
                    run_bg = cell.bg;
                }
            }
            // Tail run.
            let cols_in_row = cells.len() as u32;
            if cols_in_row > run_start_col {
                fill_rect(
                    &mut framebuf,
                    width,
                    height,
                    run_start_col * self.cell_width,
                    y0,
                    (cols_in_row - run_start_col) * self.cell_width,
                    self.cell_height,
                    run_bg,
                );
            }
        }

        // 4. Cursor block: invert at the cursor position. Paint the
        // background here, before glyphs — the glyph blit on top uses
        // the inverted fg so it stays legible.
        if let Some(cursor) = snapshot.cursor {
            let x = cursor.col as u32 * self.cell_width;
            let y = cursor.row as u32 * self.cell_height;
            fill_rect(
                &mut framebuf,
                width,
                height,
                x,
                y,
                self.cell_width,
                self.cell_height,
                cursor.fg,
            );
        }

        // 5. Glyphs. We separate this from `Self` methods so the
        // `&mut framebuf` borrow doesn't conflict with `&mut self`.
        let Self {
            font_system,
            swash_cache,
            regular_font_id,
            bold_font_id,
            cell_width,
            cell_height,
            baseline,
            ..
        } = self;
        let cursor = snapshot.cursor;
        for (row, cells) in snapshot.cells.iter().enumerate() {
            let y_baseline = (row as u32 * *cell_height) as i32 + *baseline as i32;
            for (col_idx, cell) in cells.iter().enumerate() {
                if cell.ch == ' ' || cell.ch == '\0' {
                    continue;
                }
                let x = (col_idx as u32 * *cell_width) as i32;
                let glyph_color = if cursor
                    .map(|c| c.row as usize == row && c.col as usize == col_idx)
                    .unwrap_or(false)
                {
                    // At cursor: use cell's BG as glyph colour so the
                    // char sits readable on the inverted block.
                    cell.bg
                } else {
                    cell.fg
                };
                draw_glyph(
                    &mut framebuf,
                    width,
                    height,
                    font_system,
                    swash_cache,
                    *regular_font_id,
                    *bold_font_id,
                    x,
                    y_baseline,
                    cell.ch,
                    cell.bold,
                    glyph_color,
                );
            }
        }

        framebuf
            .present()
            .map_err(|e| anyhow::anyhow!("softbuffer present: {e}"))?;

        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_glyph(
    framebuf: &mut PixelBuffer<'_, Arc<Window>, Arc<Window>>,
    fb_w: u32,
    fb_h: u32,
    font_system: &mut FontSystem,
    swash_cache: &mut SwashCache,
    regular_font_id: cosmic_text::fontdb::ID,
    bold_font_id: cosmic_text::fontdb::ID,
    x: i32,
    y_baseline: i32,
    ch: char,
    bold: bool,
    color: Rgb,
) {
    let font_id = if bold { bold_font_id } else { regular_font_id };

    // Map char → glyph_id via the font's rustybuzz face. Returns None
    // if this font doesn't have a glyph for this codepoint; the cell
    // is then drawn empty (background-only). cosmic-text's higher-
    // level shaper would fall back to another face here — out of
    // scope for MVP, but we'd plumb that through later for things like
    // emoji rendering.
    let glyph_id = match font_system.get_font(font_id).and_then(|f| {
        let rb = f.rustybuzz();
        rb.glyph_index(ch).map(|g| g.0)
    }) {
        Some(g) => g,
        None => return,
    };

    let cache_key = CacheKey {
        font_id,
        glyph_id,
        font_size_bits: FONT_SIZE.to_bits(),
        x_bin: SubpixelBin::Zero,
        y_bin: SubpixelBin::Zero,
        flags: CacheKeyFlags::empty(),
    };

    let image = swash_cache.get_image(font_system, cache_key);
    let Some(image) = image.as_ref() else {
        return;
    };

    let glyph_x = x + image.placement.left;
    let glyph_y = y_baseline - image.placement.top;
    let w = image.placement.width as i32;
    let h = image.placement.height as i32;

    match image.content {
        cosmic_text::SwashContent::Mask => {
            for j in 0..h {
                let py = glyph_y + j;
                if py < 0 || py as u32 >= fb_h {
                    continue;
                }
                for i in 0..w {
                    let px = glyph_x + i;
                    if px < 0 || px as u32 >= fb_w {
                        continue;
                    }
                    let coverage = image.data[(j * w + i) as usize];
                    if coverage == 0 {
                        continue;
                    }
                    let idx = (py as u32 * fb_w + px as u32) as usize;
                    let dst = framebuf[idx];
                    framebuf[idx] = blend_over(dst, color, coverage);
                }
            }
        }
        cosmic_text::SwashContent::Color => {
            for j in 0..h {
                let py = glyph_y + j;
                if py < 0 || py as u32 >= fb_h {
                    continue;
                }
                for i in 0..w {
                    let px = glyph_x + i;
                    if px < 0 || px as u32 >= fb_w {
                        continue;
                    }
                    let base = ((j * w + i) * 4) as usize;
                    let r = image.data[base];
                    let g = image.data[base + 1];
                    let b = image.data[base + 2];
                    let a = image.data[base + 3];
                    if a == 0 {
                        continue;
                    }
                    let idx = (py as u32 * fb_w + px as u32) as usize;
                    let dst = framebuf[idx];
                    framebuf[idx] = blend_over(dst, Rgb { r, g, b }, a);
                }
            }
        }
        cosmic_text::SwashContent::SubpixelMask => {
            // Treat as plain mask using the green channel — we don't
            // do subpixel-AA in MVP.
            for j in 0..h {
                let py = glyph_y + j;
                if py < 0 || py as u32 >= fb_h {
                    continue;
                }
                for i in 0..w {
                    let px = glyph_x + i;
                    if px < 0 || px as u32 >= fb_w {
                        continue;
                    }
                    let base = ((j * w + i) * 3) as usize;
                    let coverage = image.data[base + 1];
                    if coverage == 0 {
                        continue;
                    }
                    let idx = (py as u32 * fb_w + px as u32) as usize;
                    let dst = framebuf[idx];
                    framebuf[idx] = blend_over(dst, color, coverage);
                }
            }
        }
    }
}

/// Pick the first monospace font face with the requested weight from
/// the loaded font database. Tries `Family::Monospace` (which
/// resolves to cosmic-text's configured monospace alias) and then
/// scans `db.faces()` for any face marked `monospaced`.
fn pick_monospace(font_system: &FontSystem, weight: Weight) -> Option<cosmic_text::fontdb::ID> {
    let db = font_system.db();
    if let Some(id) = db.query(&Query {
        families: &[Family::Monospace],
        weight,
        stretch: Stretch::Normal,
        style: Style::Normal,
    }) {
        return Some(id);
    }
    // Fallback: first face marked monospaced. Prefer exact weight
    // match, otherwise any face from a monospaced family.
    let mut any_mono: Option<cosmic_text::fontdb::ID> = None;
    for face in db.faces() {
        if !face.monospaced {
            continue;
        }
        if face.weight == weight {
            return Some(face.id);
        }
        any_mono.get_or_insert(face.id);
    }
    any_mono
}

/// Measure (cell_width, cell_height, baseline_y_within_cell) by
/// shaping a single 'M' through cosmic-text and reading back the
/// glyph's advance. For a monospace face every cell takes the same
/// width, so 'M' is representative.
fn measure_cell(
    font_system: &mut FontSystem,
    font_id: cosmic_text::fontdb::ID,
) -> (u32, u32, u32) {
    let metrics = Metrics::new(FONT_SIZE, LINE_HEIGHT);
    let family_owned: FamilyOwned = font_system
        .db()
        .face(font_id)
        .and_then(|face| face.families.first().map(|(n, _)| n.clone()))
        .map(|name| FamilyOwned::Name(name.into()))
        .unwrap_or(FamilyOwned::Monospace);
    let attrs = Attrs::new().family(family_owned.as_family());

    let mut buffer = TextBuffer::new(font_system, metrics);
    buffer.set_size(font_system, Some(1024.0), Some(LINE_HEIGHT * 2.0));
    buffer.set_text(font_system, "M", &attrs, Shaping::Advanced);
    buffer.shape_until_scroll(font_system, false);

    let mut max_advance: f32 = FONT_SIZE * 0.6; // fallback
    for run in buffer.layout_runs() {
        if let Some(g) = run.glyphs.first() {
            if g.w > 0.0 {
                max_advance = max_advance.max(g.w);
            } else if run.line_w > 0.0 {
                max_advance = max_advance.max(run.line_w);
            }
        }
    }

    let cell_width = max_advance.ceil() as u32;
    let cell_height = LINE_HEIGHT.ceil() as u32;
    // Baseline ≈ font_size + a little leading. The exact font ascent
    // is harder to extract here without going through swash directly;
    // this approximation looks correct for typical 14/18 metrics.
    let baseline = (FONT_SIZE * 1.05).round() as u32;
    (cell_width, cell_height, baseline)
}

/// Snapshot of the cells in a single frame, in row-major order.
struct CellSnapshot {
    /// Outer = rows, inner = columns.
    cells: Vec<Vec<RenderCell>>,
    cursor: Option<CursorPos>,
}

#[derive(Copy, Clone, Debug)]
struct RenderCell {
    ch: char,
    fg: Rgb,
    bg: Rgb,
    bold: bool,
}

#[derive(Copy, Clone, Debug)]
struct CursorPos {
    row: i32,
    col: i32,
    fg: Rgb,
}

impl CellSnapshot {
    fn from_term(term: &Term<EventProxy>) -> Self {
        let grid = term.grid();
        let rows = grid.screen_lines();
        let cols = grid.columns();

        let mut out: Vec<Vec<RenderCell>> = (0..rows)
            .map(|_| Vec::with_capacity(cols))
            .collect();

        for indexed in grid.display_iter() {
            let line = indexed.point.line.0;
            let col = indexed.point.column.0;
            let row_idx = (line + grid.display_offset() as i32) as i32;
            if row_idx < 0 || row_idx as usize >= rows {
                continue;
            }
            let row_vec = &mut out[row_idx as usize];
            // The iterator skips cells the iterator considers "outside"
            // — pad row vector with spaces as needed to keep column
            // alignment.
            while row_vec.len() < col {
                row_vec.push(RenderCell {
                    ch: ' ',
                    fg: DEFAULT_FG,
                    bg: DEFAULT_BG,
                    bold: false,
                });
            }
            let cell = indexed.cell;
            let flags = cell.flags;
            let inverse = flags.contains(Flags::INVERSE);
            let mut fg = resolve_color(cell.fg, false);
            let mut bg = resolve_color(cell.bg, true);
            if inverse {
                std::mem::swap(&mut fg, &mut bg);
            }
            row_vec.push(RenderCell {
                ch: cell.c,
                fg,
                bg,
                bold: flags.contains(Flags::BOLD),
            });
        }

        // Pad short rows out to the grid width (so background fill
        // covers the whole window).
        for row in out.iter_mut() {
            while row.len() < cols {
                row.push(RenderCell {
                    ch: ' ',
                    fg: DEFAULT_FG,
                    bg: DEFAULT_BG,
                    bold: false,
                });
            }
        }

        // Cursor (only when it's in the visible viewport, which it is
        // unless the user is scrolled back — out of scope for MVP).
        let cursor_point: Point = term.grid().cursor.point;
        let cursor = Some(CursorPos {
            row: cursor_point.line.0,
            col: cursor_point.column.0 as i32,
            fg: DEFAULT_FG,
        });

        Self { cells: out, cursor }
    }
}

/// Resolve an alacritty Color into RGB. `is_bg` controls how the
/// `Color::Named(Foreground)`/`(Background)` defaults are filled in.
fn resolve_color(color: AnsiColor, is_bg: bool) -> Rgb {
    match color {
        AnsiColor::Spec(rgb) => rgb,
        AnsiColor::Indexed(i) => indexed_to_rgb(i),
        AnsiColor::Named(named) => named_to_rgb(named, is_bg),
    }
}

fn named_to_rgb(named: NamedColor, is_bg: bool) -> Rgb {
    match named {
        NamedColor::Foreground | NamedColor::BrightForeground | NamedColor::DimForeground => {
            DEFAULT_FG
        }
        NamedColor::Background => DEFAULT_BG,
        NamedColor::Cursor => DEFAULT_FG,
        other => {
            let idx = other as u8 as usize;
            if idx < 16 {
                PALETTE_16[idx]
            } else if is_bg {
                DEFAULT_BG
            } else {
                DEFAULT_FG
            }
        }
    }
}

/// xterm-256 indexed → RGB. First 16 are the palette above; 16–231 is
/// a 6×6×6 colour cube; 232–255 is a 24-step greyscale ramp.
fn indexed_to_rgb(i: u8) -> Rgb {
    if (i as usize) < 16 {
        return PALETTE_16[i as usize];
    }
    if (16..=231).contains(&i) {
        let i = i - 16;
        let r = (i / 36) % 6;
        let g = (i / 6) % 6;
        let b = i % 6;
        let scale = |v: u8| -> u8 {
            if v == 0 {
                0
            } else {
                (v * 40 + 55) as u8
            }
        };
        return Rgb {
            r: scale(r),
            g: scale(g),
            b: scale(b),
        };
    }
    // 232..=255: grayscale.
    let v = 8 + (i - 232) * 10;
    Rgb { r: v, g: v, b: v }
}

#[inline]
fn rgb_to_u32(c: Rgb) -> u32 {
    // softbuffer pixel layout on every supported platform is 0xRRGGBB
    // in u32 (the top 8 bits are unused/alpha-ignored). See
    // https://docs.rs/softbuffer.
    ((c.r as u32) << 16) | ((c.g as u32) << 8) | (c.b as u32)
}

#[inline]
fn blend_over(dst: u32, src: Rgb, coverage: u8) -> u32 {
    if coverage == 255 {
        return rgb_to_u32(src);
    }
    let a = coverage as u32;
    let inv = 255 - a;
    let dr = (dst >> 16) & 0xff;
    let dg = (dst >> 8) & 0xff;
    let db = dst & 0xff;
    let r = ((src.r as u32 * a) + dr * inv) / 255;
    let g = ((src.g as u32 * a) + dg * inv) / 255;
    let b = ((src.b as u32 * a) + db * inv) / 255;
    (r << 16) | (g << 8) | b
}

fn fill_rect(
    framebuf: &mut PixelBuffer<'_, Arc<Window>, Arc<Window>>,
    fb_w: u32,
    fb_h: u32,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    color: Rgb,
) {
    let color = rgb_to_u32(color);
    let x_end = (x + w).min(fb_w);
    let y_end = (y + h).min(fb_h);
    if x >= fb_w || y >= fb_h {
        return;
    }
    for py in y..y_end {
        let row_start = (py * fb_w + x) as usize;
        let row_end = (py * fb_w + x_end) as usize;
        for slot in &mut framebuf[row_start..row_end] {
            *slot = color;
        }
    }
}
