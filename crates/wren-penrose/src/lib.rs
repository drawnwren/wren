//! Exact Penrose P3 rhombi and backend-neutral animated shading.
//!
//! Geometry is generated as the dual of a regular de Bruijn pentagrid. The
//! public boundary deliberately stops at canvas geometry and RGB samples so
//! terminal, Metal, Core Graphics, and other renderers can share one tiling.

use std::f64::consts::{PI, TAU};
use std::time::Duration;

const FAMILY_COUNT: usize = 5;
pub const FACET_ACCENT_COUNT: usize = FAMILY_COUNT;
const GEOMETRY_EPSILON: f64 = 1.0e-9;
const DEFAULT_EDGE_LENGTH: f64 = 12.0;
const LIGHT_MEAN_ROTATION_RADIANS_PER_SECOND: f64 = 0.21;
const LIGHT_EASING_AMPLITUDE: f64 = 0.14;
const LIGHT_EASING_FREQUENCY: f64 = 0.40;
const ARC_SPATIAL_FREQUENCY: f64 = 0.032;
const ARC_LIGHT_BEND: f64 = 0.24;
const HIGHLIGHT_FADE_START: f64 = 0.90;
const HIGHLIGHT_PEAK_START: f64 = 0.993;
const SHADOW_ACCENT_WEIGHT: f64 = 0.10;
const MIDTONE_ACCENT_WEIGHT: f64 = 0.48;

// A regular, non-singular pentagrid needs offsets whose sum is zero and for
// which no three family lines meet. Keeping these fixed makes the scene stable
// across window sizes while still selecting a genuine Penrose tiling.
const GRID_OFFSETS: [f64; FAMILY_COUNT] = [0.113, -0.237, 0.349, -0.421, 0.196];

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

impl Point {
    #[must_use]
    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }

    fn dot(self, other: Self) -> f64 {
        self.x.mul_add(other.x, self.y * other.y)
    }

    fn cross(self, other: Self) -> f64 {
        self.x.mul_add(other.y, -(self.y * other.x))
    }

    fn length(self) -> f64 {
        self.dot(self).sqrt()
    }
}

impl std::ops::Add for Point {
    type Output = Self;

    fn add(self, rhs: Self) -> Self::Output {
        Self::new(self.x + rhs.x, self.y + rhs.y)
    }
}

impl std::ops::Sub for Point {
    type Output = Self;

    fn sub(self, rhs: Self) -> Self::Output {
        Self::new(self.x - rhs.x, self.y - rhs.y)
    }
}

impl std::ops::Mul<f64> for Point {
    type Output = Self;

    fn mul(self, rhs: f64) -> Self::Output {
        Self::new(self.x * rhs, self.y * rhs)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Canvas {
    pub width: f64,
    pub height: f64,
}

impl Canvas {
    #[must_use]
    pub fn new(width: f64, height: f64) -> Self {
        Self { width: finite_positive_or(width, 1.0), height: finite_positive_or(height, 1.0) }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TileKind {
    /// A 72°/108° Penrose rhombus.
    Thick,
    /// A 36°/144° Penrose rhombus.
    Thin,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TileId(u64);

impl TileId {
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Tile {
    id: TileId,
    kind: TileKind,
    facet_axis: u8,
    vertices: [Point; 4],
    center: Point,
    edge_length: f64,
}

impl Tile {
    #[must_use]
    pub const fn id(&self) -> TileId {
        self.id
    }

    #[must_use]
    pub const fn kind(&self) -> TileKind {
        self.kind
    }

    /// One of the five unoriented axes used for stable facet coloring.
    #[must_use]
    pub const fn facet_axis(&self) -> usize {
        self.facet_axis as usize
    }

    /// Counter-clockwise rhombus vertices, without canvas clipping.
    #[must_use]
    pub const fn vertices(&self) -> &[Point; 4] {
        &self.vertices
    }

    #[must_use]
    pub const fn center(&self) -> Point {
        self.center
    }

    #[must_use]
    pub const fn edge_length(&self) -> f64 {
        self.edge_length
    }

    #[must_use]
    pub fn contains(&self, point: Point) -> bool {
        point_in_convex_quad(point, &self.vertices)
    }

    fn bounds(&self) -> Bounds {
        Bounds::from_points(&self.vertices)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Tiling {
    canvas: Canvas,
    edge_length: f64,
    tiles: Vec<Tile>,
}

impl Tiling {
    /// Builds the exact pentagrid-dual patch needed by `canvas`.
    ///
    /// Tiles crossing the canvas boundary are intentionally retained, which
    /// lets vector and GPU renderers clip them at the real drawable edge.
    #[must_use]
    pub fn cover(canvas: Canvas, edge_length: f64) -> Self {
        let edge_length = finite_positive_or(edge_length, DEFAULT_EDGE_LENGTH);
        let unit_bounds = Bounds {
            min_x: -canvas.width / (2.0 * edge_length),
            max_x: canvas.width / (2.0 * edge_length),
            min_y: -canvas.height / (2.0 * edge_length),
            max_y: canvas.height / (2.0 * edge_length),
        };
        let directions = pentagrid_directions();
        let radius = unit_bounds.max_radius();
        let line_limit = (radius * 0.5).ceil() as i32 + 7;
        let mut tiles = Vec::new();

        for first_family in 0..FAMILY_COUNT {
            for second_family in (first_family + 1)..FAMILY_COUNT {
                let first = directions[first_family];
                let second = directions[second_family];
                let determinant = first.cross(second);
                if determinant.abs() <= GEOMETRY_EPSILON {
                    continue;
                }
                for first_line in -line_limit..=line_limit {
                    for second_line in -line_limit..=line_limit {
                        let first_distance = f64::from(first_line) - GRID_OFFSETS[first_family];
                        let second_distance = f64::from(second_line) - GRID_OFFSETS[second_family];
                        let intersection = Point::new(
                            first_distance.mul_add(second.y, -(first.y * second_distance)) / determinant,
                            first.x.mul_add(second_distance, -(first_distance * second.x)) / determinant,
                        );
                        let mut mesh = [0_i32; FAMILY_COUNT];
                        for family in 0..FAMILY_COUNT {
                            mesh[family] = if family == first_family {
                                first_line
                            } else if family == second_family {
                                second_line
                            } else {
                                (intersection.dot(directions[family]) + GRID_OFFSETS[family]).ceil() as i32
                            };
                        }
                        let base = mesh
                            .iter()
                            .zip(directions)
                            .fold(Point::new(0.0, 0.0), |point, (coefficient, direction)| point + direction * f64::from(*coefficient));
                        let (first_edge, second_edge) = if determinant > 0.0 { (first, second) } else { (second, first) };
                        let unit_vertices = [base, base + first_edge, base + first_edge + second_edge, base + second_edge];
                        if !Bounds::from_points(&unit_vertices).intersects(unit_bounds) {
                            continue;
                        }
                        let vertices = unit_vertices
                            .map(|point| Point::new(point.x.mul_add(edge_length, canvas.width / 2.0), point.y.mul_add(edge_length, canvas.height / 2.0)));
                        let center = (vertices[0] + vertices[2]) * 0.5;
                        let kind = if first.dot(second).abs() < 0.5 { TileKind::Thick } else { TileKind::Thin };
                        let facet_axis = facet_axis_for_vertices(&vertices) as u8;
                        tiles.push(Tile { id: TileId(tile_hash(first_family, second_family, mesh)), kind, facet_axis, vertices, center, edge_length });
                    }
                }
            }
        }

        tiles.sort_by_key(|tile| tile.id);
        Self { canvas, edge_length, tiles }
    }

    /// Builds the exact pentagrid patch containing only complete rhombi.
    ///
    /// Unlike [`Self::cover`], tiles crossing the drawable boundary are
    /// discarded. Raster clients can paint uncovered boundary samples with
    /// their background color instead of displaying clipped tile fragments.
    #[must_use]
    pub fn inscribed(canvas: Canvas, edge_length: f64) -> Self {
        let mut tiling = Self::cover(canvas, edge_length);
        tiling.tiles.retain(|tile| {
            tile.vertices.iter().all(|point| {
                point.x >= -GEOMETRY_EPSILON
                    && point.x <= canvas.width + GEOMETRY_EPSILON
                    && point.y >= -GEOMETRY_EPSILON
                    && point.y <= canvas.height + GEOMETRY_EPSILON
            })
        });
        tiling
    }

    #[must_use]
    pub const fn canvas(&self) -> Canvas {
        self.canvas
    }

    #[must_use]
    pub const fn edge_length(&self) -> f64 {
        self.edge_length
    }

    #[must_use]
    pub fn tiles(&self) -> &[Tile] {
        &self.tiles
    }
}

pub use wren_types::RgbColor as Rgb8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    pub background: Rgb8,
    pub edge: Rgb8,
    pub shadow: Rgb8,
    pub midtone: Rgb8,
    /// Stable colors assigned to the five unoriented Penrose facet axes.
    pub accents: [Rgb8; FACET_ACCENT_COUNT],
}

impl Default for Palette {
    fn default() -> Self {
        Self {
            background: Rgb8::new(0x1e, 0x1e, 0x2e),
            edge: Rgb8::new(0x11, 0x11, 0x1b),
            shadow: Rgb8::new(0x18, 0x18, 0x25),
            midtone: Rgb8::new(0x45, 0x47, 0x5a),
            accents: [
                Rgb8::new(0xcb, 0xa6, 0xf7),
                Rgb8::new(0x89, 0xb4, 0xfa),
                Rgb8::new(0x94, 0xe2, 0xd5),
                Rgb8::new(0xa6, 0xe3, 0xa1),
                Rgb8::new(0xfa, 0xb3, 0x87),
            ],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Shading {
    pub palette: Palette,
    /// Animation multiplier for the eased directional light.
    pub speed: f64,
    /// Dark outline width in canvas units.
    pub edge_width: f64,
    /// Center of the curved lighting wave, in canvas units.
    pub arc_origin: Point,
    /// Starting phase of the acceleration/deceleration cycle, in radians.
    pub motion_phase: f64,
}

impl Default for Shading {
    fn default() -> Self {
        Self { palette: Palette::default(), speed: 1.0, edge_width: 0.55, arc_origin: Point::new(0.0, 0.0), motion_phase: 0.0 }
    }
}

/// Reveals complete tiles from the canvas center toward its corners.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RadialReveal {
    /// Time for which the canvas remains entirely blank.
    pub delay: Duration,
    /// Travel time from the center to the farthest corner.
    pub duration: Duration,
}

impl Default for RadialReveal {
    fn default() -> Self {
        Self { delay: Duration::from_millis(120), duration: Duration::from_millis(1_650) }
    }
}

impl RadialReveal {
    #[must_use]
    pub fn tile_is_visible(self, tile: &Tile, canvas: Canvas, elapsed: Duration) -> bool {
        let Some(elapsed) = elapsed.checked_sub(self.delay) else {
            return false;
        };
        if self.duration.is_zero() {
            return true;
        }
        let progress = (elapsed.as_secs_f64() / self.duration.as_secs_f64()).clamp(0.0, 1.0);
        let canvas_center = Point::new(canvas.width * 0.5, canvas.height * 0.5);
        // A covering patch deliberately retains tiles whose centers sit just
        // outside the viewport. Include one edge length in the wave radius so
        // every cropped boundary tile is visible when the reveal completes.
        let farthest_tile_center = canvas.width.hypot(canvas.height) * 0.5 + tile.edge_length;
        (tile.center - canvas_center).length() <= farthest_tile_center * progress
    }
}

impl Shading {
    /// Returns the flat animated fill for a complete tile.
    #[must_use]
    pub fn tile_color(&self, tile: &Tile, elapsed: Duration) -> Rgb8 {
        let seconds = elapsed.as_secs_f64() * self.speed.max(0.0);
        let level = self.tile_level(tile, seconds).clamp(0.0, 1.0);
        self.color_for_level(tile, level)
    }

    /// Returns the stable theme accent selected by this tile's facet axis.
    #[must_use]
    pub fn tile_accent(&self, tile: &Tile) -> Rgb8 {
        self.palette.accents[tile_accent_index(tile)]
    }

    /// Samples one point using the rasterizer's flat fill and hard edge.
    #[must_use]
    pub fn color_at(&self, tile: &Tile, point: Point, elapsed: Duration) -> Rgb8 {
        self.apply_edge(self.tile_color(tile, elapsed), edge_distance(tile, point))
    }

    fn tile_level(&self, tile: &Tile, seconds: f64) -> f64 {
        let long_axis = tile_long_axis(tile);
        let orientation = long_axis.y.atan2(long_axis.x);
        let light_angle = self.light_motion(seconds) + self.arc_offset(tile.center);
        let facing = (2.0 * (orientation - light_angle)).cos();
        0.5 + facing * 0.5
    }

    fn light_motion(&self, seconds: f64) -> f64 {
        let easing_phase = seconds.mul_add(LIGHT_EASING_FREQUENCY, self.motion_phase);
        seconds.mul_add(LIGHT_MEAN_ROTATION_RADIANS_PER_SECOND, easing_phase.sin() * LIGHT_EASING_AMPLITUDE)
    }

    fn arc_offset(&self, center: Point) -> f64 {
        (((center - self.arc_origin).length()) * ARC_SPATIAL_FREQUENCY).sin() * ARC_LIGHT_BEND
    }

    fn color_for_level(&self, tile: &Tile, level: f64) -> Rgb8 {
        let accent = self.tile_accent(tile);
        let shadow = mix_rgb(self.palette.shadow, accent, SHADOW_ACCENT_WEIGHT);
        let midtone = mix_rgb(self.palette.midtone, accent, MIDTONE_ACCENT_WEIGHT);
        if level < 0.30 {
            shadow
        } else if level < 0.38 {
            mix_rgb(shadow, midtone, smoothstep(0.30, 0.38, level))
        } else if level < HIGHLIGHT_FADE_START {
            midtone
        } else if level < HIGHLIGHT_PEAK_START {
            mix_rgb(midtone, accent, smoothstep(HIGHLIGHT_FADE_START, HIGHLIGHT_PEAK_START, level))
        } else {
            accent
        }
    }

    fn apply_edge(&self, fill: Rgb8, edge_distance: f64) -> Rgb8 {
        if edge_distance <= self.edge_width.max(GEOMETRY_EPSILON) { self.palette.edge } else { fill }
    }
}

fn tile_long_axis(tile: &Tile) -> Point {
    long_axis_for_vertices(&tile.vertices)
}

fn long_axis_for_vertices(vertices: &[Point; 4]) -> Point {
    let first_diagonal = vertices[2] - vertices[0];
    let second_diagonal = vertices[3] - vertices[1];
    if first_diagonal.dot(first_diagonal) >= second_diagonal.dot(second_diagonal) { first_diagonal } else { second_diagonal }
}

fn facet_axis_for_vertices(vertices: &[Point; 4]) -> usize {
    let long_axis = long_axis_for_vertices(vertices);
    let unoriented_angle = long_axis.y.atan2(long_axis.x).rem_euclid(PI);
    ((unoriented_angle / (PI / FACET_ACCENT_COUNT as f64)).round() as usize) % FACET_ACCENT_COUNT
}

fn tile_accent_index(tile: &Tile) -> usize {
    tile.facet_axis()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RasterSample {
    tile_index: u32,
    edge_distance_bits: u32,
}

impl RasterSample {
    const UNCOVERED: Self = Self { tile_index: u32::MAX, edge_distance_bits: 0 };

    fn covered(tile_index: u32, tile: &Tile, point: Point) -> Self {
        Self { tile_index, edge_distance_bits: (edge_distance(tile, point) as f32).to_bits() }
    }

    fn edge_distance(self) -> f64 {
        f64::from(f32::from_bits(self.edge_distance_bits))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rasterizer {
    width: usize,
    height: usize,
    samples: Vec<RasterSample>,
    uncovered_samples: usize,
}

impl Rasterizer {
    /// Precomputes exact tile membership for a regularly sampled raster.
    #[must_use]
    pub fn new(tiling: &Tiling, width: usize, height: usize) -> Self {
        Self::new_with_edge_recovery(tiling, width, height, true)
    }

    /// Precomputes tile membership while preserving intentional background gaps.
    ///
    /// Use this with [`Tiling::inscribed`]. It avoids treating the empty border
    /// left by discarded boundary tiles as floating-point coverage errors.
    #[must_use]
    pub fn new_allowing_gaps(tiling: &Tiling, width: usize, height: usize) -> Self {
        Self::new_with_edge_recovery(tiling, width, height, false)
    }

    fn new_with_edge_recovery(tiling: &Tiling, width: usize, height: usize, recover_shared_edges: bool) -> Self {
        let width = width.max(1);
        let height = height.max(1);
        let mut samples = vec![RasterSample::UNCOVERED; width.saturating_mul(height)];
        let canvas = tiling.canvas;
        for (tile_index, tile) in tiling.tiles.iter().enumerate() {
            let bounds = tile.bounds();
            let (start_column, end_column) = sample_span(bounds.min_x, bounds.max_x, canvas.width, width);
            let (start_row, end_row) = sample_span(bounds.min_y, bounds.max_y, canvas.height, height);
            for row in start_row..end_row {
                for column in start_column..end_column {
                    let point = sample_point(canvas, width, height, column, row);
                    if tile.contains(point)
                        && let Ok(index) = u32::try_from(tile_index)
                    {
                        samples[row * width + column] = RasterSample::covered(index, tile, point);
                    }
                }
            }
        }

        // A sample exactly on a shared edge can miss both half-plane tests due
        // to floating-point roundoff. Resolve only those rare points with the
        // same convex test and a larger tolerance before reporting a gap.
        if recover_shared_edges {
            for (sample_index, sample) in samples.iter_mut().enumerate() {
                if *sample != RasterSample::UNCOVERED {
                    continue;
                }
                let row = sample_index / width;
                let column = sample_index % width;
                let point = sample_point(canvas, width, height, column, row);
                if let Some((index, tile)) = tiling.tiles.iter().enumerate().find(|(_, tile)| point_in_convex_quad_with_epsilon(point, &tile.vertices, 1.0e-7))
                    && let Ok(index) = u32::try_from(index)
                {
                    *sample = RasterSample::covered(index, tile, point);
                }
            }
        }
        let uncovered_samples = samples.iter().filter(|sample| **sample == RasterSample::UNCOVERED).count();
        Self { width, height, samples, uncovered_samples }
    }

    #[must_use]
    pub const fn width(&self) -> usize {
        self.width
    }

    #[must_use]
    pub const fn height(&self) -> usize {
        self.height
    }

    #[must_use]
    pub const fn uncovered_samples(&self) -> usize {
        self.uncovered_samples
    }

    #[must_use]
    pub fn render(&self, tiling: &Tiling, shading: &Shading, elapsed: Duration) -> Vec<Rgb8> {
        let mut pixels = vec![shading.palette.background; self.samples.len()];
        self.render_into(tiling, shading, elapsed, &mut pixels);
        pixels
    }

    pub fn render_into(&self, tiling: &Tiling, shading: &Shading, elapsed: Duration, pixels: &mut [Rgb8]) {
        self.render_visible_into(tiling, shading, elapsed, pixels, None);
    }

    /// Renders complete tiles only after a center-out wave reaches each tile.
    pub fn render_revealed_into(&self, tiling: &Tiling, shading: &Shading, reveal: RadialReveal, elapsed: Duration, pixels: &mut [Rgb8]) {
        self.render_visible_into(tiling, shading, elapsed, pixels, Some(reveal));
    }

    fn render_visible_into(&self, tiling: &Tiling, shading: &Shading, elapsed: Duration, pixels: &mut [Rgb8], reveal: Option<RadialReveal>) {
        let seconds = elapsed.as_secs_f64() * shading.speed.max(0.0);
        let tile_colors = tiling
            .tiles
            .iter()
            .map(|tile| {
                reveal.is_none_or(|reveal| reveal.tile_is_visible(tile, tiling.canvas, elapsed)).then(|| {
                    let level = shading.tile_level(tile, seconds).clamp(0.0, 1.0);
                    shading.color_for_level(tile, level)
                })
            })
            .collect::<Vec<_>>();
        for (pixel, sample) in pixels.iter_mut().zip(&self.samples) {
            let Ok(tile_index) = usize::try_from(sample.tile_index) else {
                *pixel = shading.palette.background;
                continue;
            };
            let Some(Some(fill)) = tile_colors.get(tile_index) else {
                *pixel = shading.palette.background;
                continue;
            };
            *pixel = shading.apply_edge(*fill, sample.edge_distance());
        }
        if pixels.len() > self.samples.len() {
            pixels[self.samples.len()..].fill(shading.palette.background);
        }
    }
}

#[derive(Debug, Clone, Copy)]
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

fn pentagrid_directions() -> [Point; FAMILY_COUNT] {
    std::array::from_fn(|family| {
        let angle = TAU * family as f64 / FAMILY_COUNT as f64;
        Point::new(angle.cos(), angle.sin())
    })
}

fn finite_positive_or(value: f64, fallback: f64) -> f64 {
    if value.is_finite() && value > 0.0 { value } else { fallback }
}

fn tile_hash(first_family: usize, second_family: usize, mesh: [i32; FAMILY_COUNT]) -> u64 {
    wren_types::stable_hash([first_family as i64, second_family as i64].into_iter().chain(mesh.map(i64::from)).flat_map(i64::to_le_bytes))
}

fn point_in_convex_quad(point: Point, vertices: &[Point; 4]) -> bool {
    point_in_convex_quad_with_epsilon(point, vertices, GEOMETRY_EPSILON)
}

fn point_in_convex_quad_with_epsilon(point: Point, vertices: &[Point; 4], epsilon: f64) -> bool {
    vertices.iter().copied().zip(vertices.iter().copied().cycle().skip(1)).take(4).all(|(start, end)| (end - start).cross(point - start) >= -epsilon)
}

fn distance_to_segment(point: Point, start: Point, end: Point) -> f64 {
    let edge = end - start;
    let denominator = edge.dot(edge);
    if denominator <= GEOMETRY_EPSILON {
        return (point - start).length();
    }
    let progress = ((point - start).dot(edge) / denominator).clamp(0.0, 1.0);
    (point - (start + edge * progress)).length()
}

fn edge_distance(tile: &Tile, point: Point) -> f64 {
    tile.vertices
        .iter()
        .copied()
        .zip(tile.vertices.iter().copied().cycle().skip(1))
        .take(4)
        .map(|(start, end)| distance_to_segment(point, start, end))
        .fold(f64::INFINITY, f64::min)
}

fn sample_span(minimum: f64, maximum: f64, canvas: f64, samples: usize) -> (usize, usize) {
    let scale = samples as f64 / canvas;
    let start = (minimum.mul_add(scale, -0.5)).floor().max(0.0) as usize;
    let end = (maximum.mul_add(scale, -0.5)).ceil().max(0.0) as usize + 1;
    (start.min(samples), end.min(samples))
}

fn sample_point(canvas: Canvas, width: usize, height: usize, column: usize, row: usize) -> Point {
    Point::new((column as f64 + 0.5) * canvas.width / width as f64, (row as f64 + 0.5) * canvas.height / height as f64)
}

fn smoothstep(start: f64, end: f64, value: f64) -> f64 {
    let progress = ((value - start) / (end - start).max(GEOMETRY_EPSILON)).clamp(0.0, 1.0);
    progress * progress * (3.0 - 2.0 * progress)
}

fn mix_rgb(start: Rgb8, end: Rgb8, progress: f64) -> Rgb8 {
    let progress = progress.clamp(0.0, 1.0);
    let channel = |start: u8, end: u8| {
        let value = f64::from(start).mul_add(1.0 - progress, f64::from(end) * progress);
        value.round().clamp(0.0, 255.0) as u8
    };
    Rgb8::new(channel(start.red, end.red), channel(start.green, end.green), channel(start.blue, end.blue))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn vertex_key(point: Point) -> (i64, i64) {
        const QUANTIZATION: f64 = 1.0e8;
        ((point.x * QUANTIZATION).round() as i64, (point.y * QUANTIZATION).round() as i64)
    }

    fn edge_key(first: Point, second: Point) -> ((i64, i64), (i64, i64)) {
        let first = vertex_key(first);
        let second = vertex_key(second);
        if first <= second { (first, second) } else { (second, first) }
    }

    #[test]
    fn patch_contains_only_exact_penrose_rhombi() {
        let edge = 10.0;
        let tiling = Tiling::cover(Canvas::new(320.0, 180.0), edge);
        assert!(tiling.tiles().len() > 100);
        for tile in tiling.tiles() {
            let vertices = tile.vertices();
            for (start, end) in vertices.iter().copied().zip(vertices.iter().copied().cycle().skip(1)).take(4) {
                assert!(((end - start).length() - edge).abs() < 1.0e-8);
            }
            let first = vertices[1] - vertices[0];
            let second = vertices[3] - vertices[0];
            let angle = (first.dot(second) / edge.powi(2)).acos().to_degrees();
            let acute = angle.min(180.0 - angle);
            let expected = match tile.kind() {
                TileKind::Thick => 72.0,
                TileKind::Thin => 36.0,
            };
            assert!((acute - expected).abs() < 1.0e-8, "{acute} != {expected}");
        }
    }

    #[test]
    fn interior_edges_have_exactly_two_incident_penrose_tiles() {
        let canvas = Canvas::new(320.0, 180.0);
        let tiling = Tiling::cover(canvas, 8.0);
        let mut incidence = BTreeMap::new();
        for tile in tiling.tiles() {
            for (first, second) in tile.vertices().iter().copied().zip(tile.vertices().iter().copied().cycle().skip(1)).take(4) {
                *incidence.entry(edge_key(first, second)).or_insert(0_usize) += 1;
            }
        }

        for (((first_x, first_y), (second_x, second_y)), count) in incidence {
            assert!(count <= 2, "an edge was shared by {count} tiles");
            let midpoint_x = (first_x + second_x) as f64 / 2.0e8;
            let midpoint_y = (first_y + second_y) as f64 / 2.0e8;
            if midpoint_x > GEOMETRY_EPSILON
                && midpoint_x < canvas.width - GEOMETRY_EPSILON
                && midpoint_y > GEOMETRY_EPSILON
                && midpoint_y < canvas.height - GEOMETRY_EPSILON
            {
                assert_eq!(count, 2, "interior Penrose edge at ({midpoint_x}, {midpoint_y}) was not paired");
            }
        }
    }

    #[test]
    fn generated_patch_covers_every_canvas_sample() {
        for (width, height, edge) in [(80, 46, 7.0), (127, 91, 9.0), (320, 180, 12.0)] {
            let tiling = Tiling::cover(Canvas::new(width as f64, height as f64), edge);
            let rasterizer = Rasterizer::new(&tiling, width, height);
            assert_eq!(rasterizer.uncovered_samples(), 0, "uncovered samples in {width}x{height} canvas");
        }
    }

    #[test]
    fn inscribed_patch_contains_no_boundary_clipped_tiles() {
        let canvas = Canvas::new(127.0, 91.0);
        let tiling = Tiling::inscribed(canvas, 5.0);
        assert!(!tiling.tiles().is_empty());
        assert!(
            tiling
                .tiles()
                .iter()
                .all(|tile| { tile.vertices().iter().all(|point| point.x >= 0.0 && point.x <= canvas.width && point.y >= 0.0 && point.y <= canvas.height) })
        );
        let rasterizer = Rasterizer::new_allowing_gaps(&tiling, 127, 91);
        assert!(rasterizer.uncovered_samples() > 0);
    }

    #[test]
    fn thick_to_thin_frequency_approaches_the_golden_ratio() {
        let tiling = Tiling::cover(Canvas::new(900.0, 900.0), 8.0);
        let thick = tiling.tiles().iter().filter(|tile| tile.kind() == TileKind::Thick).count();
        let thin = tiling.tiles().len() - thick;
        let ratio = thick as f64 / thin as f64;
        let golden_ratio = (1.0 + 5.0_f64.sqrt()) / 2.0;
        assert!((ratio - golden_ratio).abs() < 0.08, "ratio was {ratio}");
    }

    #[test]
    fn shading_is_animated_but_geometry_is_stable() {
        let tiling = Tiling::cover(Canvas::new(80.0, 46.0), 7.0);
        let rasterizer = Rasterizer::new(&tiling, 80, 46);
        let first = rasterizer.render(&tiling, &Shading::default(), Duration::ZERO);
        let later = rasterizer.render(&tiling, &Shading::default(), Duration::from_millis(750));
        assert_ne!(first, later);
        assert_eq!(first.len(), later.len());
        assert_eq!(rasterizer.uncovered_samples(), 0);
    }

    #[test]
    fn directional_shading_uses_all_five_accent_families_with_dominant_facet_tones() {
        let tiling = Tiling::cover(Canvas::new(160.0, 90.0), 7.0);
        let shading = Shading::default();
        let colors = tiling.tiles().iter().map(|tile| shading.tile_color(tile, Duration::from_millis(4_250))).collect::<Vec<_>>();
        let color_count = colors.len();
        for accent in shading.palette.accents {
            assert!(tiling.tiles().iter().any(|tile| shading.tile_accent(tile) == accent), "missing facet accent {accent:?}");
        }
        let dominant = tiling
            .tiles()
            .iter()
            .zip(colors)
            .filter(|(tile, color)| {
                let accent = shading.tile_accent(tile);
                let shadow = mix_rgb(shading.palette.shadow, accent, SHADOW_ACCENT_WEIGHT);
                let midtone = mix_rgb(shading.palette.midtone, accent, MIDTONE_ACCENT_WEIGHT);
                [shadow, midtone, accent].contains(color)
            })
            .count();
        assert!(dominant * 4 >= color_count * 3, "crossfades covered too many facets");
    }

    #[test]
    fn brightest_facets_remain_brief_while_arc_speed_eases() {
        let tiling = Tiling::cover(Canvas::new(160.0, 90.0), 7.0);
        let shading = Shading::default();
        let sample_millis = 40_u64;

        for tile in tiling.tiles() {
            let mut current_run = 0_usize;
            let mut longest_run = 0_usize;
            for sample in 0..1_500_u64 {
                let elapsed = Duration::from_millis(sample * sample_millis);
                if shading.tile_level(tile, elapsed.as_secs_f64() * shading.speed.max(0.0)) >= HIGHLIGHT_PEAK_START {
                    current_run += 1;
                    longest_run = longest_run.max(current_run);
                } else {
                    current_run = 0;
                }
            }
            let longest_millis = longest_run as u64 * sample_millis;
            assert!(longest_millis <= 1_400, "tile {:?} stayed at peak brightness for {longest_millis} ms", tile.id());
        }
    }

    #[test]
    fn lighting_wave_is_curved_and_changes_speed() {
        let shading = Shading { arc_origin: Point::new(30.0, 20.0), motion_phase: 0.7, ..Shading::default() };
        let first_arc_point = Point::new(50.0, 20.0);
        let second_arc_point = Point::new(30.0, 40.0);
        assert!((shading.arc_offset(first_arc_point) - shading.arc_offset(second_arc_point)).abs() < 1.0e-12);

        let first_step = shading.light_motion(1.0) - shading.light_motion(0.0);
        let later_step = shading.light_motion(5.0) - shading.light_motion(4.0);
        assert!((first_step - later_step).abs() > 0.02, "lighting motion stayed effectively constant");
        assert!(first_step > 0.0 && later_step > 0.0, "lighting arc must keep moving forward");
    }

    #[test]
    fn radial_reveal_starts_blank_and_finishes_with_cropped_boundary_tiles() {
        let tiling = Tiling::cover(Canvas::new(80.0, 46.0), 4.0);
        let rasterizer = Rasterizer::new(&tiling, 80, 46);
        let shading = Shading::default();
        let reveal = RadialReveal { delay: Duration::from_millis(100), duration: Duration::from_millis(900) };
        let mut blank = vec![Rgb8::new(255, 255, 255); 80 * 46];
        rasterizer.render_revealed_into(&tiling, &shading, reveal, Duration::ZERO, &mut blank);
        assert!(blank.iter().all(|pixel| *pixel == shading.palette.background));

        let finished_at = Duration::from_secs(2);
        let expected = rasterizer.render(&tiling, &shading, finished_at);
        let mut revealed = vec![shading.palette.background; 80 * 46];
        rasterizer.render_revealed_into(&tiling, &shading, reveal, finished_at, &mut revealed);
        assert_eq!(revealed, expected);
    }
}
