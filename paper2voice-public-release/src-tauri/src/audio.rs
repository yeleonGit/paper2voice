use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use hound::{SampleFormat, WavReader, WavSpec, WavWriter};

pub fn merge_wav_chunks_to_m4b(wav_paths: &[PathBuf], output_path: &Path) -> Result<()> {
    if wav_paths.is_empty() {
        bail!("No WAV chunks were generated.");
    }

    if let Some(parent) = output_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create output directory: {}", parent.display()))?;
    }

    if ffmpeg_path().is_err() && afconvert_path().is_ok() {
        return merge_with_afconvert(wav_paths, output_path);
    }

    ensure_ffmpeg_available()?;

    let concat_path = wav_paths
        .first()
        .and_then(|path| path.parent())
        .unwrap_or_else(|| Path::new("."))
        .join("concat.txt");
    write_concat_file(wav_paths, &concat_path)?;

    let ffmpeg = ffmpeg_path()?;
    let status = Command::new(ffmpeg)
        .arg("-y")
        .arg("-f")
        .arg("concat")
        .arg("-safe")
        .arg("0")
        .arg("-i")
        .arg(&concat_path)
        .arg("-c:a")
        .arg("aac")
        .arg("-b:a")
        .arg("96k")
        .arg(output_path)
        .status()
        .context("Failed to run FFmpeg.")?;

    if !status.success() {
        bail!(
            "FFmpeg failed while creating audiobook: {}",
            output_path.display()
        );
    }

    Ok(())
}

fn ensure_ffmpeg_available() -> Result<()> {
    let ffmpeg = ffmpeg_path()?;
    match Command::new(ffmpeg).arg("-version").output() {
        Ok(output) if output.status.success() => Ok(()),
        Ok(_) => bail!("FFmpeg not found. Install it with: brew install ffmpeg"),
        Err(error) => Err(error).context("Failed to check FFmpeg availability."),
    }
}

fn ffmpeg_path() -> Result<PathBuf> {
    resolve_tool("ffmpeg").context("FFmpeg not found. Install it with: brew install ffmpeg")
}

fn afconvert_path() -> Result<PathBuf> {
    resolve_tool("afconvert").context("macOS afconvert not found.")
}

fn resolve_tool(name: &str) -> Option<PathBuf> {
    let mut candidates = Vec::new();
    candidates.push(PathBuf::from(name));
    if let Some(candidate) = bundled_tool_candidate(name) {
        candidates.push(candidate);
    }
    candidates.push(PathBuf::from(format!("/opt/homebrew/bin/{name}")));
    candidates.push(PathBuf::from(format!("/usr/local/bin/{name}")));
    candidates.push(PathBuf::from(format!("/usr/bin/{name}")));

    candidates
        .into_iter()
        .find(|candidate| tool_responds(name, candidate))
}

fn tool_responds(name: &str, candidate: &Path) -> bool {
    let probe_arg = if name == "afconvert" {
        "-h"
    } else {
        "-version"
    };
    Command::new(candidate)
        .arg(probe_arg)
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn bundled_tool_candidate(name: &str) -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let parent = exe.parent()?;

    for ancestor in parent.ancestors() {
        let candidate = ancestor.join("Resources").join("bin").join(name);
        if candidate.exists() {
            return Some(candidate);
        }
    }

    None
}

fn write_concat_file(wav_paths: &[PathBuf], concat_path: &Path) -> Result<()> {
    let mut content = String::new();

    for wav_path in wav_paths {
        let wav_path = fs::canonicalize(wav_path).unwrap_or_else(|_| wav_path.to_path_buf());
        content.push_str("file '");
        content.push_str(&wav_path.to_string_lossy().replace('\'', "'\\''"));
        content.push_str("'\n");
    }

    fs::write(concat_path, content).with_context(|| {
        format!(
            "Failed to write FFmpeg concat file: {}",
            concat_path.display()
        )
    })
}

fn merge_with_afconvert(wav_paths: &[PathBuf], output_path: &Path) -> Result<()> {
    let merged_wav_path = wav_paths
        .first()
        .and_then(|path| path.parent())
        .unwrap_or_else(|| Path::new("."))
        .join("merged_for_export.wav");

    merge_wav_chunks_to_wav(wav_paths, &merged_wav_path)?;

    let afconvert = afconvert_path()?;
    let status = Command::new(afconvert)
        .arg("-f")
        .arg("m4af")
        .arg("-d")
        .arg("aac")
        .arg("-b")
        .arg("96000")
        .arg(&merged_wav_path)
        .arg(output_path)
        .status()
        .context("Failed to run macOS afconvert.")?;

    if !status.success() {
        bail!(
            "afconvert failed while creating audiobook: {}",
            output_path.display()
        );
    }

    Ok(())
}

fn merge_wav_chunks_to_wav(wav_paths: &[PathBuf], output_path: &Path) -> Result<()> {
    let first_path = wav_paths
        .first()
        .context("No WAV chunks were provided for merge.")?;
    let first_reader = WavReader::open(first_path)
        .with_context(|| format!("Failed to open WAV chunk: {}", first_path.display()))?;
    let spec = first_reader.spec();
    drop(first_reader);

    match spec.sample_format {
        SampleFormat::Float => merge_samples::<f32>(wav_paths, output_path, spec),
        SampleFormat::Int if spec.bits_per_sample <= 16 => {
            merge_samples::<i16>(wav_paths, output_path, spec)
        }
        SampleFormat::Int => merge_samples::<i32>(wav_paths, output_path, spec),
    }
}

fn merge_samples<T>(wav_paths: &[PathBuf], output_path: &Path, spec: WavSpec) -> Result<()>
where
    T: hound::Sample,
{
    let mut writer = WavWriter::create(output_path, spec)
        .with_context(|| format!("Failed to create merged WAV: {}", output_path.display()))?;

    for wav_path in wav_paths {
        let mut reader = WavReader::open(wav_path)
            .with_context(|| format!("Failed to open WAV chunk: {}", wav_path.display()))?;

        if reader.spec() != spec {
            bail!(
                "WAV chunk format mismatch while merging: {}",
                wav_path.display()
            );
        }

        for sample in reader.samples::<T>() {
            writer.write_sample(sample.with_context(|| {
                format!("Failed to read WAV sample from {}", wav_path.display())
            })?)?;
        }
    }

    writer.finalize().context("Failed to finalize merged WAV.")
}
