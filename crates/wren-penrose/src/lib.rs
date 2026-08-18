//! Exact Penrose P3 rhombi and backend-neutral animated shading.
//!
//! Geometry is generated as the dual of a regular de Bruijn pentagrid. The
//! public boundary deliberately stops at canvas geometry and RGB samples so
//! terminal, Metal, Core Graphics, and other renderers can share one tiling.

use std::f64::consts::TAU;
use std::time::Duration;

const FAMILY_COUNT: usize = 5;
const GEOMETRY_EPSILON: f64 = 1.0e-9;
const DEFAULT_EDGE_LENGTH: f64 = 12.0;

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
        Self {
            width: finite_positive_or(width, 1.0),
            height: finite_positive_or(height, 1.0),
        }
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
                            first_distance.mul_add(second.y, -(first.y * second_distance))
                                / determinant,
                            first
                                .x
                                .mul_add(second_distance, -(first_distance * second.x))
                                / determinant,
                        );
                        let mut mesh = [0_i32; FAMILY_COUNT];
                        for family in 0..FAMILY_COUNT {
                            mesh[family] = if family == first_family {
                                first_line
                            } else if family == second_family {
                                second_line
                            } else {
                                (intersection.dot(directions[family]) + GRID_OFFSETS[family]).ceil()
                                    as i32
                            };
                        }
                        let base = mesh.iter().zip(directions).fold(
                            Point::new(0.0, 0.0),
                            |point, (coefficient, direction)| {
                                point + direction * f64::from(*coefficient)
                            },
                        );
                        let (first_edge, second_edge) = if determinant > 0.0 {
                            (first, second)
                        } else {
                            (second, first)
                        };
                        let unit_vertices = [
                            base,
                            base + first_edge,
                            base + first_edge + second_edge,
                            base + second_edge,
                        ];
                        if !Bounds::from_points(&unit_vertices).intersects(unit_bounds) {
                            continue;
                        }
                        let vertices = unit_vertices.map(|point| {
                            Point::new(
                                point.x.mul_add(edge_length, canvas.width / 2.0),
                                point.y.mul_add(edge_length, canvas.height / 2.0),
                            )
                        });
                        let center = (vertices[0] + vertices[2]) * 0.5;
                        let kind = if first.dot(second).abs() < 0.5 {
                            TileKind::Thick
                        } else {
                            TileKind::Thin
                        };
                        tiles.push(Tile {
                            id: TileId(tile_hash(first_family, second_family, mesh)),
                            kind,
                            vertices,
                            center,
                            edge_length,
                        });
                    }
                }
            }
        }

        tiles.sort_by_key(|tile| tile.id);
        Self {
            canvas,
            edge_length,
            tiles,
        }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb8 {
    pub red: u8,
    pub green: u8,
    pub blue: u8,
}

impl Rgb8 {
    #[must_use]
    pub const fn new(red: u8, green: u8, blue: u8) -> Self {
        Self { red, green, blue }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    pub background: Rgb8,
    pub edge: Rgb8,
    pub shadow: Rgb8,
    pub midtone: Rgb8,
    pub highlight: Rgb8,
}

impl Default for Palette {
    fn default() -> Self {
        Self {
            background: Rgb8::new(0x1e, 0x1e, 0x2e),
            edge: Rgb8::new(0x11, 0x11, 0x1b),
            shadow: Rgb8::new(0x18, 0x18, 0x25),
            midtone: Rgb8::new(0x45, 0x47, 0x5a),
            highlight: Rgb8::new(0xcb, 0xa6, 0xf7),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Shading {
    pub palette: Palette,
    /// Animation multiplier; `1.0` completes the slowest wave in about 11 s.
    pub speed: f64,
    /// Dark outline width in canvas units.
    pub edge_width: f64,
}

impl Default for Shading {
    fn default() -> Self {
        Self {
            palette: Palette::default(),
            speed: 1.0,
            edge_width: 0.55,
        }
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
        Self {
            delay: Duration::from_millis(120),
            duration: Duration::from_millis(1_650),
        }
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
    /// Samples the animated lighting for one tile.
    ///
    /// Each rhombus receives one flat color and a hard outline, keeping small
    /// tiles crisp while the lighting wave moves between tiles.
    #[must_use]
    pub fn color_at(&self, tile: &Tile, point: Point, elapsed: Duration) -> Rgb8 {
        let seconds = elapsed.as_secs_f64() * self.speed.max(0.0);
        let level = self.tile_level(tile, seconds).clamp(0.0, 1.0);
        let fill = self.color_for_level(level);
        self.apply_edge(fill, edge_distance(tile, point))
    }

    fn tile_level(&self, tile: &Tile, seconds: f64) -> f64 {
        let seed = hash_unit(tile.id.get());
        let spatial = tile.center.x.mul_add(0.071, tile.center.y * 0.047);
        let counter = tile.center.x.mul_add(-0.029, tile.center.y * 0.083);
        let primary = (spatial - seconds * 0.57 + seed * TAU).sin();
        let secondary = (counter + seconds * 0.31 + seed * 2.4).sin();
        let wave = primary.mul_add(0.72, secondary * 0.28);
        let glint = smoothstep(0.48, 0.97, wave).powi(3);
        let kind_bias = match tile.kind {
            TileKind::Thick => 0.035,
            TileKind::Thin => -0.025,
        };
        0.14 + seed * 0.20 + kind_bias + glint * 0.72
    }

    fn color_for_level(&self, level: f64) -> Rgb8 {
        if level < 0.62 {
            mix_rgb(self.palette.shadow, self.palette.midtone, level / 0.62)
        } else {
            mix_rgb(
                self.palette.midtone,
                self.palette.highlight,
                (level - 0.62) / 0.38,
            )
        }
    }

    fn apply_edge(&self, fill: Rgb8, edge_distance: f64) -> Rgb8 {
        if edge_distance <= self.edge_width.max(GEOMETRY_EPSILON) {
            self.palette.edge
        } else {
            fill
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RasterSample {
    tile_index: u32,
    edge_distance_bits: u32,
}

impl RasterSample {
    const UNCOVERED: Self = Self {
        tile_index: u32::MAX,
        edge_distance_bits: 0,
    };

    fn covered(tile_index: u32, tile: &Tile, point: Point) -> Self {
        Self {
            tile_index,
            edge_distance_bits: (edge_distance(tile, point) as f32).to_bits(),
        }
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

    fn new_with_edge_recovery(
        tiling: &Tiling,
        width: usize,
        height: usize,
        recover_shared_edges: bool,
    ) -> Self {
        let width = width.max(1);
        let height = height.max(1);
        let mut tile_at_sample = vec![u32::MAX; width.saturating_mul(height)];
        let canvas = tiling.canvas;
        for (tile_index, tile) in tiling.tiles.iter().enumerate() {
            let bounds = tile.bounds();
            let (start_column, end_column) =
                sample_span(bounds.min_x, bounds.max_x, canvas.width, width);
            let (start_row, end_row) =
                sample_span(bounds.min_y, bounds.max_y, canvas.height, height);
            for row in start_row..end_row {
                for column in start_column..end_column {
                    let point = sample_point(canvas, width, height, column, row);
                    if tile.contains(point)
                        && let Ok(index) = u32::try_from(tile_index)
                    {
                        tile_at_sample[row * width + column] = index;
                    }
                }
            }
        }

        // A sample exactly on a shared edge can miss both half-plane tests due
        // to floating-point roundoff. Resolve only those rare points with the
        // same convex test and a larger tolerance before reporting a gap.
        if recover_shared_edges {
            for (sample_index, tile_index) in tile_at_sample.iter_mut().enumerate() {
                if *tile_index != u32::MAX {
                    continue;
                }
                let row = sample_index / width;
                let column = sample_index % width;
                let point = sample_point(canvas, width, height, column, row);
                if let Some((index, _)) = tiling.tiles.iter().enumerate().find(|(_, tile)| {
                    point_in_convex_quad_with_epsilon(point, &tile.vertices, 1.0e-7)
                }) && let Ok(index) = u32::try_from(index)
                {
                    *tile_index = index;
                }
            }
        }
        let uncovered_samples = tile_at_sample
            .iter()
            .filter(|index| **index == u32::MAX)
            .count();
        let samples = tile_at_sample
            .into_iter()
            .enumerate()
            .map(|(sample_index, tile_index)| {
                let Ok(tile_index_usize) = usize::try_from(tile_index) else {
                    return RasterSample::UNCOVERED;
                };
                let Some(tile) = tiling.tiles.get(tile_index_usize) else {
                    return RasterSample::UNCOVERED;
                };
                let point = sample_point(
                    canvas,
                    width,
                    height,
                    sample_index % width,
                    sample_index / width,
                );
                RasterSample::covered(tile_index, tile, point)
            })
            .collect();
        Self {
            width,
            height,
            samples,
            uncovered_samples,
        }
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

    pub fn render_into(
        &self,
        tiling: &Tiling,
        shading: &Shading,
        elapsed: Duration,
        pixels: &mut [Rgb8],
    ) {
        self.render_visible_into(tiling, shading, elapsed, pixels, None);
    }

    /// Renders complete tiles only after a center-out wave reaches each tile.
    pub fn render_revealed_into(
        &self,
        tiling: &Tiling,
        shading: &Shading,
        reveal: RadialReveal,
        elapsed: Duration,
        pixels: &mut [Rgb8],
    ) {
        self.render_visible_into(tiling, shading, elapsed, pixels, Some(reveal));
    }

    fn render_visible_into(
        &self,
        tiling: &Tiling,
        shading: &Shading,
        elapsed: Duration,
        pixels: &mut [Rgb8],
        reveal: Option<RadialReveal>,
    ) {
        let seconds = elapsed.as_secs_f64() * shading.speed.max(0.0);
        let tile_levels = tiling
            .tiles
            .iter()
            .map(|tile| {
                reveal
                    .is_none_or(|reveal| reveal.tile_is_visible(tile, tiling.canvas, elapsed))
                    .then(|| shading.tile_level(tile, seconds))
            })
            .collect::<Vec<_>>();
        for (pixel, sample) in pixels.iter_mut().zip(&self.samples) {
            let Ok(tile_index) = usize::try_from(sample.tile_index) else {
                *pixel = shading.palette.background;
                continue;
            };
            let Some(Some(tile_level)) = tile_levels.get(tile_index) else {
                *pixel = shading.palette.background;
                continue;
            };
            let level = tile_level.clamp(0.0, 1.0);
            let fill = shading.color_for_level(level);
            *pixel = shading.apply_edge(fill, sample.edge_distance());
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
        points.iter().fold(
            Self {
                min_x: f64::INFINITY,
                max_x: f64::NEG_INFINITY,
                min_y: f64::INFINITY,
                max_y: f64::NEG_INFINITY,
            },
            |bounds, point| Self {
                min_x: bounds.min_x.min(point.x),
                max_x: bounds.max_x.max(point.x),
                min_y: bounds.min_y.min(point.y),
                max_y: bounds.max_y.max(point.y),
            },
        )
    }

    fn intersects(self, other: Self) -> bool {
        self.max_x >= other.min_x - GEOMETRY_EPSILON
            && self.min_x <= other.max_x + GEOMETRY_EPSILON
            && self.max_y >= other.min_y - GEOMETRY_EPSILON
            && self.min_y <= other.max_y + GEOMETRY_EPSILON
    }

    fn max_radius(self) -> f64 {
        self.min_x
            .abs()
            .max(self.max_x.abs())
            .hypot(self.min_y.abs().max(self.max_y.abs()))
    }
}

fn pentagrid_directions() -> [Point; FAMILY_COUNT] {
    std::array::from_fn(|family| {
        let angle = TAU * family as f64 / FAMILY_COUNT as f64;
        Point::new(angle.cos(), angle.sin())
    })
}

fn finite_positive_or(value: f64, fallback: f64) -> f64 {
    if value.is_finite() && value > 0.0 {
        value
    } else {
        fallback
    }
}

fn tile_hash(first_family: usize, second_family: usize, mesh: [i32; FAMILY_COUNT]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for value in [first_family as i64, second_family as i64]
        .into_iter()
        .chain(mesh.map(i64::from))
    {
        for byte in value.to_le_bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    hash
}

fn hash_unit(value: u64) -> f64 {
    let mixed = value ^ (value >> 30);
    let mixed = mixed.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    let mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    let mixed = mixed ^ (mixed >> 31);
    (mixed >> 11) as f64 / (1_u64 << 53) as f64
}

fn point_in_convex_quad(point: Point, vertices: &[Point; 4]) -> bool {
    point_in_convex_quad_with_epsilon(point, vertices, GEOMETRY_EPSILON)
}

fn point_in_convex_quad_with_epsilon(point: Point, vertices: &[Point; 4], epsilon: f64) -> bool {
    vertices
        .iter()
        .copied()
        .zip(vertices.iter().copied().cycle().skip(1))
        .take(4)
        .all(|(start, end)| (end - start).cross(point - start) >= -epsilon)
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
    Point::new(
        (column as f64 + 0.5) * canvas.width / width as f64,
        (row as f64 + 0.5) * canvas.height / height as f64,
    )
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
    Rgb8::new(
        channel(start.red, end.red),
        channel(start.green, end.green),
        channel(start.blue, end.blue),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patch_contains_only_exact_penrose_rhombi() {
        let edge = 10.0;
        let tiling = Tiling::cover(Canvas::new(320.0, 180.0), edge);
        assert!(tiling.tiles().len() > 100);
        for tile in tiling.tiles() {
            let vertices = tile.vertices();
            for (start, end) in vertices
                .iter()
                .copied()
                .zip(vertices.iter().copied().cycle().skip(1))
                .take(4)
            {
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
    fn generated_patch_covers_every_canvas_sample() {
        for (width, height, edge) in [(80, 46, 7.0), (127, 91, 9.0), (320, 180, 12.0)] {
            let tiling = Tiling::cover(Canvas::new(width as f64, height as f64), edge);
            let rasterizer = Rasterizer::new(&tiling, width, height);
            assert_eq!(
                rasterizer.uncovered_samples(),
                0,
                "uncovered samples in {width}x{height} canvas"
            );
        }
    }

    #[test]
    fn inscribed_patch_contains_no_boundary_clipped_tiles() {
        let canvas = Canvas::new(127.0, 91.0);
        let tiling = Tiling::inscribed(canvas, 5.0);
        assert!(!tiling.tiles().is_empty());
        assert!(tiling.tiles().iter().all(|tile| {
            tile.vertices().iter().all(|point| {
                point.x >= 0.0
                    && point.x <= canvas.width
                    && point.y >= 0.0
                    && point.y <= canvas.height
            })
        }));
        let rasterizer = Rasterizer::new_allowing_gaps(&tiling, 127, 91);
        assert!(rasterizer.uncovered_samples() > 0);
    }

    #[test]
    fn thick_to_thin_frequency_approaches_the_golden_ratio() {
        let tiling = Tiling::cover(Canvas::new(900.0, 900.0), 8.0);
        let thick = tiling
            .tiles()
            .iter()
            .filter(|tile| tile.kind() == TileKind::Thick)
            .count();
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
    fn radial_reveal_starts_blank_and_finishes_with_cropped_boundary_tiles() {
        let tiling = Tiling::cover(Canvas::new(80.0, 46.0), 4.0);
        let rasterizer = Rasterizer::new(&tiling, 80, 46);
        let shading = Shading::default();
        let reveal = RadialReveal {
            delay: Duration::from_millis(100),
            duration: Duration::from_millis(900),
        };
        let mut blank = vec![Rgb8::new(255, 255, 255); 80 * 46];
        rasterizer.render_revealed_into(&tiling, &shading, reveal, Duration::ZERO, &mut blank);
        assert!(
            blank
                .iter()
                .all(|pixel| *pixel == shading.palette.background)
        );

        let finished_at = Duration::from_secs(2);
        let expected = rasterizer.render(&tiling, &shading, finished_at);
        let mut revealed = vec![shading.palette.background; 80 * 46];
        rasterizer.render_revealed_into(&tiling, &shading, reveal, finished_at, &mut revealed);
        assert_eq!(revealed, expected);
    }
}
