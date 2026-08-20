use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

use wren_penrose::{Canvas, Palette, Point, RadialReveal, Shading, Tiling};
#[cfg(test)]
use wren_view::CatppuccinFlavor;
use wren_view::{CatppuccinColor, CatppuccinPalette, DesiredGrid, RasterBorder, RasterOverlay, RasterQuad};

pub(super) const ANIMATION_FRAME_MILLIS: u64 = 83;
pub(super) const ANIMATION_FRAME_PERIOD: Duration = Duration::from_millis(ANIMATION_FRAME_MILLIS);
const FULL_DENSITY_SHORT_SIDE: f64 = 80.0;
const NARROW_WINDOW_TRANSITION: f64 = 28.0;
const FULL_DENSITY_TILE_EDGE: f64 = 5.0;
const NARROW_WINDOW_TILE_EDGE: f64 = 7.0;
const GRAPHICS_SAMPLES_PER_CANVAS_UNIT: usize = 6;
const TILE_BORDER_WIDTH_PIXELS: f32 = 0.7;
const DEFAULT_CELL_HEIGHT_TO_WIDTH: f64 = 2.0;
const ARC_ORIGIN_MARGIN: f64 = 0.12;
const STARTUP_SEED_STEP: u64 = 0x9e37_79b9_7f4a_7c15;

static STARTUP_RESET_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
struct CachedCanvas {
    columns: usize,
    raster_height: usize,
    tiling: Tiling,
}

#[derive(Debug)]
pub(super) struct StartupScreen {
    canvas: Option<CachedCanvas>,
    alternate_canvas: Option<CachedCanvas>,
    reveal_origin: Option<Duration>,
    arc_origin_fraction: Point,
    motion_phase: f64,
    cell_height_to_width: f64,
}

impl Default for StartupScreen {
    fn default() -> Self {
        Self::from_seed(fresh_startup_seed())
    }
}

impl StartupScreen {
    pub(super) fn from_seed(seed: u64) -> Self {
        let x_seed = mix_seed(seed);
        let y_seed = mix_seed(x_seed);
        let phase_seed = mix_seed(y_seed);
        let origin_span = 1.0 - ARC_ORIGIN_MARGIN * 2.0;
        Self {
            canvas: None,
            alternate_canvas: None,
            reveal_origin: None,
            arc_origin_fraction: Point::new(ARC_ORIGIN_MARGIN + seed_unit(x_seed) * origin_span, ARC_ORIGIN_MARGIN + seed_unit(y_seed) * origin_span),
            motion_phase: seed_unit(phase_seed) * std::f64::consts::TAU,
            cell_height_to_width: DEFAULT_CELL_HEIGHT_TO_WIDTH,
        }
    }

    pub(super) fn set_cell_height_to_width(&mut self, ratio: f64) {
        if ratio.is_finite() && ratio > 0.0 {
            self.cell_height_to_width = ratio;
        }
    }

    pub(super) fn paint(&mut self, mut grid: DesiredGrid, elapsed: Duration, theme: CatppuccinPalette) -> DesiredGrid {
        let content_rows = grid.height.saturating_sub(1);
        if grid.width == 0 || content_rows == 0 {
            return grid;
        }
        let raster_height = raster_height(content_rows, self.cell_height_to_width);
        let rebuild = self.canvas.as_ref().is_none_or(|cached| cached.columns != grid.width || cached.raster_height != raster_height);
        if rebuild {
            let alternate_matches = self.alternate_canvas.as_ref().is_some_and(|cached| cached.columns == grid.width && cached.raster_height == raster_height);
            if alternate_matches {
                std::mem::swap(&mut self.canvas, &mut self.alternate_canvas);
            } else {
                self.alternate_canvas = self.canvas.take();
                self.canvas = Some(build_canvas(grid.width, raster_height));
            }
        }
        let Some(canvas) = self.canvas.as_mut() else {
            return grid;
        };
        let reveal_origin = *self.reveal_origin.get_or_insert(elapsed);
        let elapsed = elapsed.saturating_sub(reveal_origin);
        let frame = elapsed.as_millis() / u128::from(ANIMATION_FRAME_MILLIS);
        let quantized_millis = frame.saturating_mul(u128::from(ANIMATION_FRAME_MILLIS));
        let elapsed = Duration::from_millis(u64::try_from(quantized_millis).unwrap_or(u64::MAX));
        let drawing_canvas = canvas.tiling.canvas();
        let shading = tiling_shading(theme, drawing_canvas, self.arc_origin_fraction, self.motion_phase);
        let reveal = RadialReveal::default();
        let sample_scale = GRAPHICS_SAMPLES_PER_CANVAS_UNIT as f32;
        let edge_color = theme.color(CatppuccinColor::Crust);
        let quads = canvas
            .tiling
            .tiles()
            .iter()
            .filter(|tile| reveal.tile_is_visible(tile, drawing_canvas, elapsed))
            .map(|tile| RasterQuad {
                vertices: tile.vertices().map(|point| [point.x as f32 * sample_scale, point.y as f32 * sample_scale]),
                color: shading.tile_color(tile, elapsed),
                border: Some(RasterBorder { color: edge_color, width: TILE_BORDER_WIDTH_PIXELS }),
            })
            .collect();
        grid.raster_overlay = Some(Arc::new(RasterOverlay {
            frame_id: grid.epoch,
            width: grid.width.saturating_mul(GRAPHICS_SAMPLES_PER_CANVAS_UNIT),
            height: canvas.raster_height,
            columns: grid.width,
            rows: content_rows,
            background: theme.color(CatppuccinColor::Base),
            quads: Arc::new(quads),
        }));
        grid
    }
}

fn build_canvas(columns: usize, raster_height: usize) -> CachedCanvas {
    let canvas_height = raster_height as f64 / GRAPHICS_SAMPLES_PER_CANVAS_UNIT as f64;
    let edge_length = tile_edge_length(columns, canvas_height);
    let tiling = Tiling::cover(Canvas::new(columns as f64, canvas_height), edge_length);
    CachedCanvas { columns, raster_height, tiling }
}

fn raster_height(content_rows: usize, cell_height_to_width: f64) -> usize {
    (content_rows as f64 * GRAPHICS_SAMPLES_PER_CANVAS_UNIT as f64 * cell_height_to_width).round().max(1.0) as usize
}

fn tile_edge_length(columns: usize, canvas_height: f64) -> f64 {
    let shortest_side = (columns as f64).min(canvas_height);
    let narrowness = ((FULL_DENSITY_SHORT_SIDE - shortest_side) / NARROW_WINDOW_TRANSITION).clamp(0.0, 1.0);
    FULL_DENSITY_TILE_EDGE + (NARROW_WINDOW_TILE_EDGE - FULL_DENSITY_TILE_EDGE) * narrowness
}

fn fresh_startup_seed() -> u64 {
    let elapsed = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
    let clock = elapsed as u64 ^ (elapsed >> 64) as u64;
    let sequence = STARTUP_RESET_SEQUENCE.fetch_add(STARTUP_SEED_STEP, Ordering::Relaxed);
    mix_seed(clock ^ sequence)
}

fn mix_seed(mut value: u64) -> u64 {
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

fn seed_unit(seed: u64) -> f64 {
    (seed >> 11) as f64 / (1_u64 << 53) as f64
}

fn tiling_shading(theme: CatppuccinPalette, canvas: Canvas, arc_origin_fraction: Point, motion_phase: f64) -> Shading {
    Shading {
        palette: Palette {
            background: theme.color(CatppuccinColor::Base),
            edge: theme.color(CatppuccinColor::Crust),
            shadow: theme.color(CatppuccinColor::Mantle),
            midtone: theme.color(CatppuccinColor::Surface1),
            highlight: theme.color(CatppuccinColor::Mauve),
        },
        edge_width: 0.0,
        arc_origin: Point::new(canvas.width * arc_origin_fraction.x, canvas.height * arc_origin_fraction.y),
        motion_phase,
        ..Shading::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wren_view::CellRow;

    fn blank_grid(width: usize, height: usize) -> DesiredGrid {
        DesiredGrid { epoch: 1, width, height, rows: (0..height).map(|_| Arc::new(CellRow::default())).collect(), cursor: (0, 0), raster_overlay: None }
    }

    #[test]
    fn startup_screen_rebuilds_for_the_exact_canvas_size() {
        let mut screen = StartupScreen::default();
        let first = screen.paint(blank_grid(80, 24), Duration::ZERO, CatppuccinPalette::for_flavor(CatppuccinFlavor::Mocha));
        let first_overlay = first.raster_overlay.expect("raster overlay");
        assert_eq!((first_overlay.columns, first_overlay.rows), (80, 23));
        assert_eq!((first_overlay.width, first_overlay.height), (480, 276));

        let resized = screen.paint(blank_grid(43, 17), Duration::from_millis(500), CatppuccinPalette::for_flavor(CatppuccinFlavor::Mocha));
        assert_eq!(resized.raster_overlay.as_ref().map(|overlay| (overlay.columns, overlay.rows)), Some((43, 16)));
        assert_eq!(screen.canvas.as_ref().map(|canvas| (canvas.columns, canvas.raster_height)), Some((43, 192)));

        let restored = screen.paint(blank_grid(80, 24), Duration::from_millis(700), CatppuccinPalette::for_flavor(CatppuccinFlavor::Mocha));
        assert_eq!(restored.raster_overlay.expect("restored overlay").columns, 80);
        assert_eq!(screen.canvas.as_ref().map(|canvas| (canvas.columns, canvas.raster_height)), Some((80, 276)));
    }

    #[test]
    fn startup_starts_blank_then_reveals_theme_colored_tiles() {
        let mut screen = StartupScreen::default();
        let theme = CatppuccinPalette::for_flavor(CatppuccinFlavor::Mocha);
        let first = screen.paint(blank_grid(40, 12), Duration::from_secs(7), theme);
        let later = screen.paint(blank_grid(40, 12), Duration::from_millis(7_900), theme);
        let first = first.raster_overlay.expect("first overlay");
        let later = later.raster_overlay.expect("later overlay");
        assert_eq!((first.background, later.background), (theme.color(CatppuccinColor::Base), theme.color(CatppuccinColor::Base)));
        assert!(first.quads.is_empty());
        assert!(!later.quads.is_empty());
    }

    #[test]
    fn startup_uses_exact_vectors_without_building_terminal_cells() {
        let mut screen = StartupScreen::default();
        let _blank = screen.paint(blank_grid(40, 12), Duration::ZERO, CatppuccinPalette::for_flavor(CatppuccinFlavor::Mocha));
        let rendered = screen.paint(blank_grid(40, 12), Duration::from_secs(3), CatppuccinPalette::for_flavor(CatppuccinFlavor::Mocha));
        let overlay = rendered.raster_overlay.expect("raster overlay");
        assert_eq!((overlay.columns, overlay.rows), (40, 11));
        assert_eq!((overlay.width, overlay.height), (240, 132));
        assert_eq!(overlay.background, CatppuccinPalette::for_flavor(CatppuccinFlavor::Mocha).color(CatppuccinColor::Base));
        assert!(!overlay.quads.is_empty());
        assert!(rendered.rows.iter().all(|row| row.cells.is_empty()));
    }

    #[test]
    fn narrow_tall_resizes_keep_thin_rhombi_legible() {
        let narrow_edge = tile_edge_length(52, 226.0);
        assert_eq!(narrow_edge, NARROW_WINDOW_TILE_EDGE);

        // A thin Penrose rhombus has altitude edge * sin(36 degrees). Keep at
        // least four raster samples across that altitude in the narrow layout.
        let visible_thin_face = narrow_edge * 36.0_f64.to_radians().sin();
        assert!(visible_thin_face > 4.0);

        assert_eq!(tile_edge_length(240, 158.0), FULL_DENSITY_TILE_EDGE);

        let theme = CatppuccinPalette::for_flavor(CatppuccinFlavor::Mocha);
        let mut screen = StartupScreen::default();
        let _blank = screen.paint(blank_grid(52, 114), Duration::ZERO, theme);
        let finished = screen.paint(blank_grid(52, 114), Duration::from_secs(3), theme);
        assert!(!finished.raster_overlay.expect("narrow raster overlay").quads.is_empty());
    }

    #[test]
    fn startup_keeps_every_penrose_rhomb_visually_separate() {
        let theme = CatppuccinPalette::for_flavor(CatppuccinFlavor::Mocha);
        let mut screen = StartupScreen::from_seed(7);
        let _blank = screen.paint(blank_grid(80, 24), Duration::ZERO, theme);
        let rendered = screen.paint(blank_grid(80, 24), Duration::from_secs(3), theme);
        let quads = &rendered.raster_overlay.expect("raster overlay").quads;
        let tiling = &screen.canvas.as_ref().expect("cached canvas").tiling;
        assert_eq!(quads.len(), tiling.tiles().len());
        for (quad, tile) in quads.iter().zip(tiling.tiles()) {
            assert_eq!(quad.border, Some(RasterBorder { color: theme.color(CatppuccinColor::Crust), width: TILE_BORDER_WIDTH_PIXELS }));
            for (rendered, exact) in quad.vertices.iter().zip(tile.vertices()) {
                assert!((rendered[0] - exact.x as f32 * GRAPHICS_SAMPLES_PER_CANVAS_UNIT as f32).abs() < 1.0e-4);
                assert!((rendered[1] - exact.y as f32 * GRAPHICS_SAMPLES_PER_CANVAS_UNIT as f32).abs() < 1.0e-4);
            }
        }
    }

    #[test]
    fn ghostty_pixel_metrics_preserve_penrose_rhomb_proportions_across_resizes() {
        let theme = CatppuccinPalette::for_flavor(CatppuccinFlavor::Mocha);
        for (columns, rows, cell_height_to_width) in [(43, 17, 1.45), (80, 24, 2.15), (151, 51, 2.65)] {
            let mut screen = StartupScreen::from_seed(7);
            screen.set_cell_height_to_width(cell_height_to_width);
            let _blank = screen.paint(blank_grid(columns, rows), Duration::ZERO, theme);
            let rendered = screen.paint(blank_grid(columns, rows), Duration::from_secs(3), theme);
            let overlay = rendered.raster_overlay.expect("raster overlay");
            let target_aspect = columns as f64 / ((rows - 1) as f64 * cell_height_to_width);
            let raster_aspect = overlay.width as f64 / overlay.height as f64;
            assert!((raster_aspect - target_aspect).abs() / target_aspect < 0.01);

            for quad in overlay.quads.iter() {
                let edge_lengths = std::array::from_fn::<_, 4, _>(|edge| {
                    let start = quad.vertices[edge];
                    let end = quad.vertices[(edge + 1) % 4];
                    (end[0] - start[0]).hypot(end[1] - start[1])
                });
                let shortest = edge_lengths.into_iter().fold(f32::INFINITY, f32::min);
                let longest = edge_lengths.into_iter().fold(0.0, f32::max);
                assert!((longest - shortest) / longest < 1.0e-4);
            }
        }
    }

    #[test]
    fn each_seed_selects_a_stable_random_arc_origin() {
        let first = StartupScreen::from_seed(1);
        let repeated = StartupScreen::from_seed(1);
        let second = StartupScreen::from_seed(2);
        assert_eq!(first.arc_origin_fraction, repeated.arc_origin_fraction);
        assert_eq!(first.motion_phase, repeated.motion_phase);
        assert_ne!(first.arc_origin_fraction, second.arc_origin_fraction);
        for coordinate in [first.arc_origin_fraction.x, first.arc_origin_fraction.y, second.arc_origin_fraction.x, second.arc_origin_fraction.y] {
            assert!((ARC_ORIGIN_MARGIN..=1.0 - ARC_ORIGIN_MARGIN).contains(&coordinate));
        }
    }
}
