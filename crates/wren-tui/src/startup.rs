use std::f64::consts::TAU;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

use glam::DVec2 as Point;
use wren_types::RgbColor;
#[cfg(test)]
use wren_view::CatppuccinFlavor;
use wren_view::{CatppuccinColor, CatppuccinPalette, DesiredGrid, RasterOverlay, RasterQuad};

pub(super) const ANIMATION_FRAME_MILLIS: u64 = 83;
pub(super) const ANIMATION_FRAME_PERIOD: Duration = Duration::from_millis(ANIMATION_FRAME_MILLIS);
const FULL_DENSITY_SHORT_SIDE: f64 = 80.0;
const NARROW_WINDOW_TRANSITION: f64 = 28.0;
const FULL_DENSITY_TILE_EDGE: f64 = 5.0;
const NARROW_WINDOW_TILE_EDGE: f64 = 7.0;
const GRAPHICS_SAMPLES_PER_CANVAS_UNIT: usize = 6;
const ARC_ORIGIN_MARGIN: f64 = 0.12;
const STARTUP_SEED_STEP: u64 = 0x9e37_79b9_7f4a_7c15;
const FAMILY_COUNT: usize = 5;
const GEOMETRY_EPSILON: f64 = 1.0e-9;
const REVEAL_DELAY: Duration = Duration::from_millis(120);
const REVEAL_DURATION: Duration = Duration::from_millis(1_650);
const LIGHT_MEAN_ROTATION_RADIANS_PER_SECOND: f64 = 0.21;
const LIGHT_EASING_AMPLITUDE: f64 = 0.14;
const LIGHT_EASING_FREQUENCY: f64 = 0.40;
const ARC_SPATIAL_FREQUENCY: f64 = 0.032;
const ARC_LIGHT_BEND: f64 = 0.24;
const HIGHLIGHT_FADE_START: f64 = 0.90;
const HIGHLIGHT_PEAK_START: f64 = 0.993;
const GRID_OFFSETS: [f64; FAMILY_COUNT] = [0.113, -0.237, 0.349, -0.421, 0.196];

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
        let shading = StartupShading::new(theme, &canvas.tiling, self.arc_origin_fraction, self.motion_phase);
        let sample_scale = GRAPHICS_SAMPLES_PER_CANVAS_UNIT as f32;
        let quads = canvas
            .tiling
            .tiles
            .iter()
            .filter(|tile| tile_is_revealed(tile, &canvas.tiling, elapsed))
            .map(|tile| RasterQuad {
                vertices: tile.vertices.map(|point| [point.x as f32 * sample_scale, point.y as f32 * sample_scale]),
                color: shading.tile_color(tile, elapsed),
            })
            .collect();
        grid.raster_overlay = Some(Arc::new(RasterOverlay {
            frame_id: grid.epoch,
            width: grid.width.saturating_mul(GRAPHICS_SAMPLES_PER_CANVAS_UNIT),
            height: canvas.sample_rows.saturating_mul(GRAPHICS_SAMPLES_PER_CANVAS_UNIT),
            columns: grid.width,
            rows: content_rows,
            background: theme.color(CatppuccinColor::Base),
            quads: Arc::new(quads),
        }));
        grid
    }
}

fn build_canvas(columns: usize, sample_rows: usize) -> CachedCanvas {
    let edge_length = tile_edge_length(columns, sample_rows);
    let tiling = Tiling::cover(columns as f64, sample_rows as f64, edge_length);
    CachedCanvas { columns, sample_rows, tiling }
}

fn tile_edge_length(columns: usize, sample_rows: usize) -> f64 {
    let shortest_side = columns.min(sample_rows) as f64;
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

#[derive(Debug)]
struct Tiling {
    width: f64,
    height: f64,
    tiles: Vec<Tile>,
}

#[derive(Debug)]
struct Tile {
    id: u64,
    vertices: [Point; 4],
    center: Point,
    edge_length: f64,
}

impl Tiling {
    fn cover(width: f64, height: f64, edge_length: f64) -> Self {
        let bounds = Bounds {
            min_x: -width / (2.0 * edge_length),
            max_x: width / (2.0 * edge_length),
            min_y: -height / (2.0 * edge_length),
            max_y: height / (2.0 * edge_length),
        };
        let directions: [Point; FAMILY_COUNT] = std::array::from_fn(|family| {
            let angle = TAU * family as f64 / FAMILY_COUNT as f64;
            Point::new(angle.cos(), angle.sin())
        });
        let line_limit = (bounds.max_radius() * 0.5).ceil() as i32 + 7;
        let mut tiles = Vec::new();
        for first_family in 0..FAMILY_COUNT {
            for second_family in first_family + 1..FAMILY_COUNT {
                let first = directions[first_family];
                let second = directions[second_family];
                let determinant = first.perp_dot(second);
                for first_line in -line_limit..=line_limit {
                    for second_line in -line_limit..=line_limit {
                        let first_distance = f64::from(first_line) - GRID_OFFSETS[first_family];
                        let second_distance = f64::from(second_line) - GRID_OFFSETS[second_family];
                        let intersection = Point::new(
                            first_distance.mul_add(second.y, -(first.y * second_distance)) / determinant,
                            first.x.mul_add(second_distance, -(first_distance * second.x)) / determinant,
                        );
                        let mesh = std::array::from_fn(|family| match family {
                            family if family == first_family => first_line,
                            family if family == second_family => second_line,
                            family => (intersection.dot(directions[family]) + GRID_OFFSETS[family]).ceil() as i32,
                        });
                        let base = mesh.iter().zip(directions).fold(Point::ZERO, |point, (coefficient, direction)| point + direction * f64::from(*coefficient));
                        let (first_edge, second_edge) = if determinant > 0.0 { (first, second) } else { (second, first) };
                        let unit_vertices = [base, base + first_edge, base + first_edge + second_edge, base + second_edge];
                        if !Bounds::from_points(&unit_vertices).intersects(bounds) {
                            continue;
                        }
                        let vertices =
                            unit_vertices.map(|point| Point::new(point.x.mul_add(edge_length, width / 2.0), point.y.mul_add(edge_length, height / 2.0)));
                        tiles.push(Tile { id: tile_hash(first_family, second_family, mesh), center: (vertices[0] + vertices[2]) * 0.5, vertices, edge_length });
                    }
                }
            }
        }
        tiles.sort_by_key(|tile| tile.id);
        Self { width, height, tiles }
    }
}

struct StartupShading {
    shadow: RgbColor,
    midtone: RgbColor,
    highlight: RgbColor,
    arc_origin: Point,
    motion_phase: f64,
}

impl StartupShading {
    fn new(theme: CatppuccinPalette, tiling: &Tiling, arc_origin: Point, motion_phase: f64) -> Self {
        Self {
            shadow: theme.color(CatppuccinColor::Mantle),
            midtone: theme.color(CatppuccinColor::Surface1),
            highlight: theme.color(CatppuccinColor::Mauve),
            arc_origin: Point::new(tiling.width * arc_origin.x, tiling.height * arc_origin.y),
            motion_phase,
        }
    }

    fn tile_color(&self, tile: &Tile, elapsed: Duration) -> RgbColor {
        let first_diagonal = tile.vertices[2] - tile.vertices[0];
        let second_diagonal = tile.vertices[3] - tile.vertices[1];
        let long_axis = if first_diagonal.length_squared() >= second_diagonal.length_squared() { first_diagonal } else { second_diagonal };
        let orientation = long_axis.y.atan2(long_axis.x);
        let seconds = elapsed.as_secs_f64();
        let easing_phase = seconds.mul_add(LIGHT_EASING_FREQUENCY, self.motion_phase);
        let light_motion = seconds.mul_add(LIGHT_MEAN_ROTATION_RADIANS_PER_SECOND, easing_phase.sin() * LIGHT_EASING_AMPLITUDE);
        let arc_offset = ((tile.center - self.arc_origin).length() * ARC_SPATIAL_FREQUENCY).sin() * ARC_LIGHT_BEND;
        self.color_for_level(0.5 + (2.0 * (orientation - light_motion - arc_offset)).cos() * 0.5)
    }

    fn color_for_level(&self, level: f64) -> RgbColor {
        if level < 0.30 {
            self.shadow
        } else if level < 0.38 {
            mix_rgb(self.shadow, self.midtone, smoothstep(0.30, 0.38, level))
        } else if level < HIGHLIGHT_FADE_START {
            self.midtone
        } else if level < HIGHLIGHT_PEAK_START {
            mix_rgb(self.midtone, self.highlight, smoothstep(HIGHLIGHT_FADE_START, HIGHLIGHT_PEAK_START, level))
        } else {
            self.highlight
        }
    }
}

#[derive(Clone, Copy)]
struct Bounds {
    min_x: f64,
    max_x: f64,
    min_y: f64,
    max_y: f64,
}

impl Bounds {
    fn from_points(points: &[Point; 4]) -> Self {
        points.iter().fold(Self { min_x: f64::INFINITY, max_x: f64::NEG_INFINITY, min_y: f64::INFINITY, max_y: f64::NEG_INFINITY }, |bounds, point| Self {
            min_x: bounds.min_x.min(point.x),
            max_x: bounds.max_x.max(point.x),
            min_y: bounds.min_y.min(point.y),
            max_y: bounds.max_y.max(point.y),
        })
    }

    fn intersects(self, other: Self) -> bool {
        self.max_x >= other.min_x - GEOMETRY_EPSILON
            && self.min_x <= other.max_x + GEOMETRY_EPSILON
            && self.max_y >= other.min_y - GEOMETRY_EPSILON
            && self.min_y <= other.max_y + GEOMETRY_EPSILON
    }

    fn max_radius(self) -> f64 {
        self.min_x.abs().max(self.max_x.abs()).hypot(self.min_y.abs().max(self.max_y.abs()))
    }
}

fn tile_is_revealed(tile: &Tile, tiling: &Tiling, elapsed: Duration) -> bool {
    let Some(elapsed) = elapsed.checked_sub(REVEAL_DELAY) else {
        return false;
    };
    let progress = (elapsed.as_secs_f64() / REVEAL_DURATION.as_secs_f64()).clamp(0.0, 1.0);
    let center = Point::new(tiling.width * 0.5, tiling.height * 0.5);
    let radius = tiling.width.hypot(tiling.height) * 0.5 + tile.edge_length;
    (tile.center - center).length() <= radius * progress
}

fn tile_hash(first_family: usize, second_family: usize, mesh: [i32; FAMILY_COUNT]) -> u64 {
    wren_types::stable_hash([first_family as i64, second_family as i64].into_iter().chain(mesh.map(i64::from)).flat_map(i64::to_le_bytes))
}

fn smoothstep(start: f64, end: f64, value: f64) -> f64 {
    let progress = ((value - start) / (end - start).max(GEOMETRY_EPSILON)).clamp(0.0, 1.0);
    progress * progress * (3.0 - 2.0 * progress)
}

fn mix_rgb(start: RgbColor, end: RgbColor, progress: f64) -> RgbColor {
    let channel = |start: u8, end: u8| f64::from(start).mul_add(1.0 - progress, f64::from(end) * progress).round().clamp(0.0, 255.0) as u8;
    RgbColor::new(channel(start.red, end.red), channel(start.green, end.green), channel(start.blue, end.blue))
}

#[cfg(test)]
mod tests {
    use super::*;
    use wren_view::CellRow;

    fn blank_grid(width: usize, height: usize) -> DesiredGrid {
        DesiredGrid { epoch: 1, width, height, rows: (0..height).map(|_| Arc::new(CellRow::default())).collect(), cursor: (0, 0), raster_overlay: None }
    }

    #[test]
    fn startup_patch_contains_only_exact_penrose_rhombi() {
        let edge = 10.0;
        let tiling = Tiling::cover(320.0, 180.0, edge);
        assert!(tiling.tiles.len() > 100);
        for tile in &tiling.tiles {
            for (start, end) in tile.vertices.iter().copied().zip(tile.vertices.iter().copied().cycle().skip(1)).take(4) {
                assert!(((end - start).length() - edge).abs() < 1.0e-8);
            }
            let first = tile.vertices[1] - tile.vertices[0];
            let second = tile.vertices[3] - tile.vertices[0];
            let angle = (first.dot(second) / edge.powi(2)).acos().to_degrees();
            let acute = angle.min(180.0 - angle);
            assert!([36.0, 72.0].into_iter().any(|expected| (acute - expected).abs() < 1.0e-8), "unexpected acute angle {acute}");
        }
    }

    #[test]
    fn startup_shading_animates_and_reveal_finishes() {
        let tiling = Tiling::cover(80.0, 46.0, 7.0);
        let theme = CatppuccinPalette::for_flavor(CatppuccinFlavor::Mocha);
        let shading = StartupShading::new(theme, &tiling, Point::splat(0.5), 0.0);
        let colors = |elapsed| tiling.tiles.iter().map(|tile| shading.tile_color(tile, elapsed)).collect::<Vec<_>>();
        assert_ne!(colors(Duration::ZERO), colors(Duration::from_millis(750)));
        assert!(tiling.tiles.iter().all(|tile| !tile_is_revealed(tile, &tiling, Duration::ZERO)));
        assert!(tiling.tiles.iter().all(|tile| tile_is_revealed(tile, &tiling, Duration::from_secs(2))));
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
        assert_eq!(screen.canvas.as_ref().map(|canvas| (canvas.columns, canvas.sample_rows)), Some((43, 32)));

        let restored = screen.paint(blank_grid(80, 24), Duration::from_millis(700), CatppuccinPalette::for_flavor(CatppuccinFlavor::Mocha));
        assert_eq!(restored.raster_overlay.expect("restored overlay").columns, 80);
        assert_eq!(screen.canvas.as_ref().map(|canvas| (canvas.columns, canvas.sample_rows)), Some((80, 46)));
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
        let narrow_edge = tile_edge_length(52, 226);
        assert_eq!(narrow_edge, NARROW_WINDOW_TILE_EDGE);

        // A thin Penrose rhombus has altitude edge * sin(36 degrees). Keep at
        // least four raster samples across that altitude in the narrow layout.
        let visible_thin_face = narrow_edge * 36.0_f64.to_radians().sin();
        assert!(visible_thin_face > 4.0);

        assert_eq!(tile_edge_length(240, 158), FULL_DENSITY_TILE_EDGE);

        let theme = CatppuccinPalette::for_flavor(CatppuccinFlavor::Mocha);
        let mut screen = StartupScreen::default();
        let _blank = screen.paint(blank_grid(52, 114), Duration::ZERO, theme);
        let finished = screen.paint(blank_grid(52, 114), Duration::from_secs(3), theme);
        assert!(!finished.raster_overlay.expect("narrow raster overlay").quads.is_empty());
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
