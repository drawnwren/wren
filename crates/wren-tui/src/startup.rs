use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

use wren_penrose::{Canvas, Palette, Point, RadialReveal, Shading, Tiling};
use wren_view::{CatppuccinPalette, DesiredGrid, RasterOverlay, RasterQuad, RasterSource};

pub(super) const ANIMATION_FRAME_MILLIS: u64 = 83;
pub(super) const ANIMATION_FRAME_PERIOD: Duration = Duration::from_millis(ANIMATION_FRAME_MILLIS);
const FULL_DENSITY_SHORT_SIDE: f64 = 80.0;
const NARROW_WINDOW_TRANSITION: f64 = 28.0;
const FULL_DENSITY_TILE_EDGE: f64 = 5.0;
const NARROW_WINDOW_TILE_EDGE: f64 = 7.0;
const GRAPHICS_SAMPLES_PER_CANVAS_UNIT: usize = 6;
const ARC_ORIGIN_MARGIN: f64 = 0.12;
const STARTUP_SEED_STEP: u64 = 0x9e37_79b9_7f4a_7c15;

static STARTUP_RESET_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
struct CachedCanvas {
    columns: usize,
    sample_rows: usize,
    tiling: Tiling,
}

#[derive(Debug)]
pub(super) struct StartupScreen {
    canvas: Option<CachedCanvas>,
    alternate_canvas: Option<CachedCanvas>,
    reveal_origin: Option<Duration>,
    arc_origin_fraction: Point,
    motion_phase: f64,
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
        }
    }

    pub(super) fn paint(&mut self, mut grid: DesiredGrid, elapsed: Duration, theme: CatppuccinPalette) -> DesiredGrid {
        let content_rows = grid.height.saturating_sub(1);
        if grid.width == 0 || content_rows == 0 {
            return grid;
        }
        let sample_rows = content_rows.saturating_mul(2);
        let rebuild = self.canvas.as_ref().is_none_or(|cached| cached.columns != grid.width || cached.sample_rows != sample_rows);
        if rebuild {
            let alternate_matches = self.alternate_canvas.as_ref().is_some_and(|cached| cached.columns == grid.width && cached.sample_rows == sample_rows);
            if alternate_matches {
                std::mem::swap(&mut self.canvas, &mut self.alternate_canvas);
            } else {
                self.alternate_canvas = self.canvas.take();
                self.canvas = Some(build_canvas(grid.width, sample_rows));
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
        let quads = canvas
            .tiling
            .tiles()
            .iter()
            .filter(|tile| reveal.tile_is_visible(tile, canvas.tiling.canvas(), elapsed))
            .map(|tile| RasterQuad {
                vertices: tile.vertices().map(|point| [point.x as f32 * sample_scale, point.y as f32 * sample_scale]),
                color: shading.tile_color(tile, elapsed),
            })
            .collect();
        grid.raster_overlay = Some(Arc::new(RasterOverlay {
            frame_id: grid.epoch,
            width: grid.width.saturating_mul(GRAPHICS_SAMPLES_PER_CANVAS_UNIT),
            height: canvas.sample_rows.saturating_mul(GRAPHICS_SAMPLES_PER_CANVAS_UNIT),
            columns: grid.width,
            rows: content_rows,
            source: RasterSource::Quads { background: theme.base, quads: Arc::new(quads) },
        }));
        grid
    }
}

fn build_canvas(columns: usize, sample_rows: usize) -> CachedCanvas {
    let edge_length = tile_edge_length(columns, sample_rows);
    let canvas = Canvas::new(columns as f64, sample_rows as f64);
    let tiling = Tiling::cover(canvas, edge_length);
    CachedCanvas { columns, sample_rows, tiling }
}

fn tile_edge_length(columns: usize, sample_rows: usize) -> f64 {
    let shortest_side = columns.min(sample_rows) as f64;
    let narrowness = ((FULL_DENSITY_SHORT_SIDE - shortest_side) / NARROW_WINDOW_TRANSITION).clamp(0.0, 1.0);
    FULL_DENSITY_TILE_EDGE + (NARROW_WINDOW_TILE_EDGE - FULL_DENSITY_TILE_EDGE) * narrowness
}

fn tiling_shading(theme: CatppuccinPalette, canvas: Canvas, arc_origin_fraction: Point, motion_phase: f64) -> Shading {
    Shading {
        palette: tiling_palette(theme),
        // Adjacent flat facets are already discrete. An extra outline consumes
        // most of a thin rhombus in a small window and turns the tessellation
        // into disconnected bars.
        edge_width: 0.0,
        arc_origin: Point::new(canvas.width * arc_origin_fraction.x, canvas.height * arc_origin_fraction.y),
        motion_phase,
        ..Shading::default()
    }
}

const fn tiling_palette(theme: CatppuccinPalette) -> Palette {
    Palette { background: theme.base, edge: theme.crust, shadow: theme.mantle, midtone: theme.surface1, highlight: theme.mauve }
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
        let first = screen.paint(blank_grid(80, 24), Duration::ZERO, CatppuccinPalette::MOCHA);
        let first_overlay = first.raster_overlay.expect("raster overlay");
        assert_eq!((first_overlay.columns, first_overlay.rows), (80, 23));
        assert_eq!((first_overlay.width, first_overlay.height), (480, 276));

        let resized = screen.paint(blank_grid(43, 17), Duration::from_millis(500), CatppuccinPalette::MOCHA);
        assert_eq!(resized.raster_overlay.as_ref().map(|overlay| (overlay.columns, overlay.rows)), Some((43, 16)));
        assert_eq!(screen.canvas.as_ref().map(|canvas| (canvas.columns, canvas.sample_rows)), Some((43, 32)));

        let restored = screen.paint(blank_grid(80, 24), Duration::from_millis(700), CatppuccinPalette::MOCHA);
        assert_eq!(restored.raster_overlay.expect("restored overlay").columns, 80);
        assert_eq!(screen.canvas.as_ref().map(|canvas| (canvas.columns, canvas.sample_rows)), Some((80, 46)));
    }

    #[test]
    fn startup_starts_blank_then_reveals_theme_colored_tiles() {
        let mut screen = StartupScreen::default();
        let theme = CatppuccinPalette::MOCHA;
        let first = screen.paint(blank_grid(40, 12), Duration::from_secs(7), theme);
        let later = screen.paint(blank_grid(40, 12), Duration::from_millis(7_900), theme);
        let RasterSource::Quads { background: first_background, quads: first_quads } = &first.raster_overlay.expect("first overlay").source else {
            panic!("startup uses vector quads");
        };
        let RasterSource::Quads { background: later_background, quads: later_quads } = &later.raster_overlay.expect("later overlay").source else {
            panic!("startup uses vector quads");
        };
        assert_eq!((*first_background, *later_background), (theme.base, theme.base));
        assert!(first_quads.is_empty());
        assert!(!later_quads.is_empty());
    }

    #[test]
    fn startup_uses_exact_vectors_without_building_terminal_cells() {
        let mut screen = StartupScreen::default();
        let _blank = screen.paint(blank_grid(40, 12), Duration::ZERO, CatppuccinPalette::MOCHA);
        let rendered = screen.paint(blank_grid(40, 12), Duration::from_secs(3), CatppuccinPalette::MOCHA);
        let overlay = rendered.raster_overlay.expect("raster overlay");
        assert_eq!((overlay.columns, overlay.rows), (40, 11));
        assert_eq!((overlay.width, overlay.height), (240, 132));
        let RasterSource::Quads { background, quads } = &overlay.source else {
            panic!("graphics path should preserve exact vector quads");
        };
        assert_eq!(*background, CatppuccinPalette::MOCHA.base);
        assert!(!quads.is_empty());
        assert!(rendered.rows.iter().all(|row| row.cells.is_empty()));
    }

    #[test]
    fn narrow_tall_resizes_keep_thin_rhombi_legible() {
        let narrow_edge = tile_edge_length(52, 226);
        assert_eq!(narrow_edge, NARROW_WINDOW_TILE_EDGE);

        // A thin Penrose rhombus has altitude edge * sin(36 degrees). Keep at
        // least four raster samples across that altitude in the narrow layout.
        let visible_thin_face = narrow_edge * 36.0_f64.to_radians().sin();
        assert!(visible_thin_face > 4.0);

        assert_eq!(tile_edge_length(240, 158), FULL_DENSITY_TILE_EDGE);

        let theme = CatppuccinPalette::MOCHA;
        assert_eq!(tiling_shading(theme, Canvas::new(52.0, 226.0), Point::new(0.5, 0.5), 0.0).edge_width, 0.0);
        let mut screen = StartupScreen::default();
        let _blank = screen.paint(blank_grid(52, 114), Duration::ZERO, theme);
        let finished = screen.paint(blank_grid(52, 114), Duration::from_secs(3), theme);
        let RasterSource::Quads { quads, .. } = &finished.raster_overlay.expect("narrow raster overlay").source else {
            panic!("startup uses vector quads");
        };
        assert!(!quads.is_empty());
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
