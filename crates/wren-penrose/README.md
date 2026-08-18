# wren-penrose

Backend-neutral Penrose P3 geometry and animated shading for Wren's empty
startup screen. The crate has no editor, terminal, windowing, or GPU
dependencies: native clients can draw the returned rhombi directly, while
raster clients can use `Rasterizer`.

`Tiling::cover` constructs a canvas-covering patch with de Bruijn's regular
pentagrid dual. `Tiling::inscribed` uses the same exact patch but removes every
boundary-crossing rhombus, which is useful when clipped tile fragments are not
desired. Every visible tile is one of the two exact Penrose rhombi (36°/144°
thin or 72°/108° thick), rather than an approximated or periodically repeated
motif. `RadialReveal` provides a backend-neutral center-out opening animation.

```rust
use std::time::Duration;
use wren_penrose::{Canvas, Rasterizer, Shading, Tiling};

let tiling = Tiling::cover(Canvas::new(1920.0, 1080.0), 36.0);
let rasterizer = Rasterizer::new(&tiling, 1920, 1080);
let pixels = rasterizer.render(&tiling, &Shading::default(), Duration::from_secs(2));
assert_eq!(pixels.len(), 1920 * 1080);
```

For a macOS screen saver, retain a `Tiling`, upload `Tile::vertices()` once,
and evaluate `Shading::color_at` (or an equivalent shader) on each display
tick. Rebuild only when the drawable size changes.
