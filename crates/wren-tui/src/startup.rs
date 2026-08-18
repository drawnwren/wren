use std::sync::Arc;
use std::time::Duration;

use wren_penrose::{Canvas, Palette, RadialReveal, Rasterizer, Rgb8, Shading, Tiling};
use wren_view::{
    CatppuccinPalette, Cell, CellColor, CellGrapheme, CellRow, CellStyle, DesiredGrid, RgbColor,
};

pub(super) const ANIMATION_FRAME_MILLIS: u64 = 83;
pub(super) const ANIMATION_FRAME_PERIOD: Duration = Duration::from_millis(ANIMATION_FRAME_MILLIS);
const FULL_DENSITY_SHORT_SIDE: f64 = 80.0;
const NARROW_WINDOW_TRANSITION: f64 = 28.0;
const FULL_DENSITY_TILE_EDGE: f64 = 6.0;
const NARROW_WINDOW_TILE_EDGE: f64 = 8.0;

#[derive(Debug)]
struct CachedCanvas {
    columns: usize,
    sample_rows: usize,
    tiling: Tiling,
    rasterizer: Rasterizer,
    pixels: Vec<Rgb8>,
}

#[derive(Debug, Default)]
pub(super) struct StartupScreen {
    canvas: Option<CachedCanvas>,
    alternate_canvas: Option<CachedCanvas>,
    reveal_origin: Option<Duration>,
}

impl StartupScreen {
    pub(super) fn paint(
        &mut self,
        mut grid: DesiredGrid,
        elapsed: Duration,
        theme: CatppuccinPalette,
    ) -> DesiredGrid {
        let content_rows = grid.height.saturating_sub(1);
        if grid.width == 0 || content_rows == 0 {
            return grid;
        }
        let sample_rows = content_rows.saturating_mul(2);
        let rebuild = self
            .canvas
            .as_ref()
            .is_none_or(|cached| cached.columns != grid.width || cached.sample_rows != sample_rows);
        if rebuild {
            let alternate_matches = self.alternate_canvas.as_ref().is_some_and(|cached| {
                cached.columns == grid.width && cached.sample_rows == sample_rows
            });
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
        let shading = tiling_shading(theme);
        canvas.rasterizer.render_revealed_into(
            &canvas.tiling,
            &shading,
            RadialReveal::default(),
            elapsed,
            &mut canvas.pixels,
        );
        for row in 0..content_rows {
            let upper = row.saturating_mul(2).saturating_mul(grid.width);
            let lower = upper.saturating_add(grid.width);
            let cells = (0..grid.width)
                .map(|column| {
                    half_block(canvas.pixels[upper + column], canvas.pixels[lower + column])
                })
                .collect();
            grid.rows[row] = Arc::new(CellRow { cells });
        }
        grid
    }
}

fn build_canvas(columns: usize, sample_rows: usize) -> CachedCanvas {
    let edge_length = tile_edge_length(columns, sample_rows);
    let canvas = Canvas::new(columns as f64, sample_rows as f64);
    let tiling = Tiling::cover(canvas, edge_length);
    let rasterizer = Rasterizer::new(&tiling, columns, sample_rows);
    let pixels = vec![Rgb8::new(0, 0, 0); columns.saturating_mul(sample_rows)];
    CachedCanvas {
        columns,
        sample_rows,
        tiling,
        rasterizer,
        pixels,
    }
}

fn tile_edge_length(columns: usize, sample_rows: usize) -> f64 {
    let shortest_side = columns.min(sample_rows) as f64;
    let narrowness =
        ((FULL_DENSITY_SHORT_SIDE - shortest_side) / NARROW_WINDOW_TRANSITION).clamp(0.0, 1.0);
    FULL_DENSITY_TILE_EDGE + (NARROW_WINDOW_TILE_EDGE - FULL_DENSITY_TILE_EDGE) * narrowness
}

fn tiling_shading(theme: CatppuccinPalette) -> Shading {
    Shading {
        palette: tiling_palette(theme),
        // Terminal half-blocks already make adjacent flat facets discrete.
        // An extra sampled outline consumes most of a thin rhombus in a small
        // window and turns the tessellation into disconnected bars.
        edge_width: 0.0,
        ..Shading::default()
    }
}

const fn tiling_palette(theme: CatppuccinPalette) -> Palette {
    Palette {
        background: penrose_color(theme.base),
        edge: penrose_color(theme.crust),
        shadow: penrose_color(theme.mantle),
        midtone: penrose_color(theme.surface1),
        highlight: penrose_color(theme.mauve),
    }
}

const fn penrose_color(color: RgbColor) -> Rgb8 {
    Rgb8::new(color.red, color.green, color.blue)
}

fn half_block(upper: Rgb8, lower: Rgb8) -> Cell {
    Cell {
        grapheme: CellGrapheme::from("▀"),
        width: 1,
        style: CellStyle {
            foreground: Some(CellColor::Rgb(view_color(upper))),
            background: Some(CellColor::Rgb(view_color(lower))),
            ..CellStyle::default()
        },
    }
}

const fn view_color(color: Rgb8) -> RgbColor {
    RgbColor::new(color.red, color.green, color.blue)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blank_grid(width: usize, height: usize) -> DesiredGrid {
        DesiredGrid {
            epoch: 1,
            width,
            height,
            rows: (0..height).map(|_| Arc::new(CellRow::default())).collect(),
            cursor: (0, 0),
        }
    }

    #[test]
    fn startup_screen_rebuilds_for_the_exact_canvas_size() {
        let mut screen = StartupScreen::default();
        let first = screen.paint(blank_grid(80, 24), Duration::ZERO, CatppuccinPalette::MOCHA);
        assert_eq!(first.rows[0].cells.len(), 80);
        assert!(
            first.rows[0]
                .cells
                .iter()
                .all(|cell| cell.grapheme.as_ref() == "▀")
        );
        assert!(
            first.rows[23].cells.is_empty(),
            "status row stays available"
        );

        let resized = screen.paint(
            blank_grid(43, 17),
            Duration::from_millis(500),
            CatppuccinPalette::MOCHA,
        );
        assert_eq!(resized.rows[0].cells.len(), 43);
        let canvas = screen.canvas.as_ref().expect("cached canvas");
        assert_eq!((canvas.columns, canvas.sample_rows), (43, 32));
        assert_eq!(canvas.rasterizer.uncovered_samples(), 0);
        assert!(canvas.tiling.tiles().iter().any(|tile| {
            tile.vertices().iter().any(|point| {
                point.x < 0.0
                    || point.x > canvas.columns as f64
                    || point.y < 0.0
                    || point.y > canvas.sample_rows as f64
            })
        }));

        let restored = screen.paint(
            blank_grid(80, 24),
            Duration::from_millis(700),
            CatppuccinPalette::MOCHA,
        );
        assert_eq!(restored.rows[0].cells.len(), 80);
        assert_eq!(
            screen
                .canvas
                .as_ref()
                .map(|canvas| (canvas.columns, canvas.sample_rows)),
            Some((80, 46))
        );
    }

    #[test]
    fn startup_starts_blank_then_reveals_theme_colored_tiles() {
        let mut screen = StartupScreen::default();
        let theme = CatppuccinPalette::MOCHA;
        let first = screen.paint(blank_grid(40, 12), Duration::from_secs(7), theme);
        let later = screen.paint(blank_grid(40, 12), Duration::from_millis(7_900), theme);
        let background = Some(CellColor::Rgb(theme.base));
        assert!(first.rows[..11].iter().all(|row| row.cells.iter().all(
            |cell| cell.style.foreground == background && cell.style.background == background
        )));
        assert_ne!(first.rows[3], later.rows[3]);
        assert_eq!(first.rows[3].cells.len(), later.rows[3].cells.len());
        assert!(later.rows[..11].iter().any(|row| row.cells.iter().any(
            |cell| cell.style.foreground != background || cell.style.background != background
        )));
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
        assert_eq!(tiling_shading(theme).edge_width, 0.0);
        let mut screen = StartupScreen::default();
        let _blank = screen.paint(blank_grid(52, 114), Duration::ZERO, theme);
        let finished = screen.paint(blank_grid(52, 114), Duration::from_secs(3), theme);
        let background = Some(CellColor::Rgb(theme.base));
        assert!(
            finished.rows[..113]
                .iter()
                .all(|row| row
                    .cells
                    .iter()
                    .all(|cell| cell.style.foreground != background
                        && cell.style.background != background))
        );
    }
}
