use std::path::Path;

use anyhow::{bail, Result};

pub fn ensure_file_exists(path: &Path, label: &str) -> Result<()> {
    if !path.exists() {
        bail!("{label} not found: {}", path.display());
    }

    if !path.is_file() {
        bail!("{label} is not a file: {}", path.display());
    }

    Ok(())
}

pub fn ensure_positive_speed(speed: f32) -> Result<()> {
    if !speed.is_finite() || speed <= 0.0 {
        bail!("Speed must be a positive number.");
    }

    Ok(())
}

pub fn ensure_max_chars(max_chars: usize) -> Result<()> {
    if max_chars == 0 {
        bail!("--max-chars must be greater than zero.");
    }

    Ok(())
}
