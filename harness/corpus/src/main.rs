use std::env;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

const MIB: usize = 1024 * 1024;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn generate_normal(path: &Path) -> Result<()> {
    let mut output = String::from("// Deterministic, real-looking Rust benchmark corpus.\n\n");
    for module in 0..250 {
        output.push_str(&format!(
            "pub fn transform_{module}(input: &[u64]) -> u64 {{\n"
        ));
        output.push_str("    input.iter().copied()\n");
        output.push_str(&format!(
            "        .map(|value| value.rotate_left({}))\n",
            module % 63 + 1
        ));
        output.push_str("        .filter(|value| value & 1 == 0)\n");
        output.push_str(&format!(
            "        .fold({module}, |sum, value| sum.wrapping_add(value))\n"
        ));
        output.push_str("}\n\n");
        output.push_str(&format!("const CHECK_{module}: u64 = 0x{module:08x};\n"));
    }
    fs::write(path, output).with_context(|| format!("write {}", path.display()))
}

fn generate_unicode(path: &Path) -> Result<()> {
    let samples = [
        "東京では桜が咲く。漢字とかな、全角 punctuation。",
        "👩🏽‍💻 edits Rust 🦀 while family 👨‍👩‍👧‍👦 watches.",
        "naïve café: e\u{301}, a\u{308}, Z\u{0351}; മലയാളം; ภาษาไทย",
        "中文測試／한국어 문장／العَرَبِيَّة／עברית",
    ];
    let mut output = String::new();
    for index in 0..4096 {
        output.push_str(samples[index % samples.len()]);
        output.push('\n');
    }
    fs::write(path, output).with_context(|| format!("write {}", path.display()))
}

fn write_repeated(
    path: &Path,
    target: usize,
    prefix: &[u8],
    pattern: &[u8],
    suffix: &[u8],
) -> Result<()> {
    let file = File::create(path).with_context(|| format!("create {}", path.display()))?;
    let mut writer = BufWriter::new(file);
    writer.write_all(prefix)?;
    let body_len = target
        .checked_sub(prefix.len() + suffix.len())
        .context("target is smaller than wrapper")?;
    let full = body_len / pattern.len();
    let remainder = body_len % pattern.len();
    for _ in 0..full {
        writer.write_all(pattern)?;
    }
    writer.write_all(&pattern[..remainder])?;
    writer.write_all(suffix)?;
    writer.flush()?;
    Ok(())
}

fn generate_all() -> Result<()> {
    let corpus = root();
    let documents = corpus.join("documents");
    let generated = corpus.join("generated");
    fs::create_dir_all(&documents)?;
    fs::create_dir_all(&generated)?;
    generate_normal(&documents.join("normal.rs"))?;
    generate_unicode(&documents.join("unicode.txt"))?;
    write_repeated(
        &generated.join("large-100mb.js"),
        100 * MIB,
        b"// generated; do not commit\n",
        b"export function compute(v) { return (v * 1664525 + 1013904223) >>> 0; }\n",
        b"\n",
    )?;
    write_repeated(
        &generated.join("oneline-8mb.json"),
        8 * MIB,
        b"[\"",
        b"abcdef0123456789",
        b"\"]",
    )?;
    println!("generated deterministic corpus at {}", corpus.display());
    Ok(())
}

fn main() -> Result<()> {
    match env::args().nth(1).as_deref() {
        Some("generate") => generate_all(),
        _ => bail!("usage: wren-corpus generate"),
    }
}
