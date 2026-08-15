use std::env;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

#[allow(dead_code)]
mod diff;
#[allow(dead_code)]
mod model;
mod oracle;
mod runner;

use crate::runner::{check_determinism, check_wren_against_goldens, record_goldens};

fn main() -> Result<()> {
    let arguments: Vec<String> = env::args().skip(1).collect();
    if arguments
        .iter()
        .any(|argument| argument == "--check-determinism")
    {
        check_determinism()?;
        println!("golden trace regeneration is deterministic");
        return Ok(());
    }
    if arguments.iter().any(|argument| argument == "--check-wren") {
        check_wren_against_goldens()?;
        println!("wren core state matches the pinned Neovim golden traces");
        return Ok(());
    }
    let destination = if let Some(index) = arguments.iter().position(|argument| argument == "--out")
    {
        PathBuf::from(
            arguments
                .get(index + 1)
                .context("--out needs a directory")?,
        )
    } else {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("goldens")
    };
    let written = record_goldens(&destination)?;
    println!("wrote oracle goldens to {}", written.display());
    Ok(())
}
