use std::env;
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::time::Duration;

use wren_penrose::{Canvas, Palette, Rasterizer, Rgb8, Shading, Tiling};

fn main() -> io::Result<()> {
    let mut arguments = env::args().skip(1);
    let path = arguments.next().unwrap_or_else(|| "penrose-preview.ppm".to_owned());
    let width = parse_dimension(arguments.next(), 960);
    let height = parse_dimension(arguments.next(), 540);
    let shortest_side = width.min(height) as f64;
    let tiling = Tiling::cover(Canvas::new(width as f64, height as f64), shortest_side / 24.0);
    let rasterizer = Rasterizer::new(&tiling, width, height);
    let shading = Shading {
        palette: Palette {
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
        },
        ..Shading::default()
    };
    let pixels = rasterizer.render(&tiling, &shading, Duration::from_millis(4_250));

    let mut output = BufWriter::new(File::create(path)?);
    writeln!(output, "P6\n{width} {height}\n255")?;
    for pixel in pixels {
        output.write_all(&[pixel.red, pixel.green, pixel.blue])?;
    }
    output.flush()
}

fn parse_dimension(value: Option<String>, fallback: usize) -> usize {
    value.and_then(|value| value.parse().ok()).filter(|value| *value > 0).unwrap_or(fallback)
}
