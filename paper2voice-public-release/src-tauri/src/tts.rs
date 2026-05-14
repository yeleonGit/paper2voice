use std::{
    env, fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

const MIN_MODEL_BYTES: u64 = 1_000_000;
const MIN_VOICES_BYTES: u64 = 1_000_000;

pub struct TtsEngine {
    voice: String,
    speed: f32,
    bridge: KokoroBridge,
}

impl TtsEngine {
    pub fn new(voice: String, speed: f32, model_path: &Path, voices_path: &Path) -> Result<Self> {
        validate_model_file(model_path, "Model", MIN_MODEL_BYTES)?;
        validate_model_file(voices_path, "Voices", MIN_VOICES_BYTES)?;

        Ok(Self {
            voice,
            speed,
            bridge: KokoroBridge::start(model_path, voices_path)?,
        })
    }

    pub fn synthesize_chunk(
        &mut self,
        text: &str,
        chunk_index: usize,
        output_dir: &Path,
    ) -> Result<PathBuf> {
        fs::create_dir_all(output_dir).with_context(|| {
            format!(
                "Failed to create chunk output directory: {}",
                output_dir.display()
            )
        })?;

        let output_path = output_dir.join(format!("chunk_{:04}.wav", chunk_index + 1));
        self.bridge
            .synthesize(text, &self.voice, self.speed, &output_path)
            .with_context(|| format!("TTS failed on chunk_{:04}.txt", chunk_index + 1))?;
        Ok(output_path)
    }
}

pub fn synthesize_chunk_with_macos_say(
    text: &str,
    chunk_index: usize,
    voice: &str,
    speed: f32,
    output_dir: &Path,
) -> Result<PathBuf> {
    fs::create_dir_all(output_dir).with_context(|| {
        format!(
            "Failed to create chunk output directory: {}",
            output_dir.display()
        )
    })?;

    let wav_path = output_dir.join(format!("chunk_{:04}.wav", chunk_index + 1));
    let aiff_path = output_dir.join(format!("chunk_{:04}.aiff", chunk_index + 1));
    let rate = macos_say_rate(speed);

    let mut child = Command::new("/usr/bin/say")
        .arg("-v")
        .arg(macos_say_voice(voice))
        .arg("-r")
        .arg(rate.to_string())
        .arg("-o")
        .arg(&aiff_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("Failed to start macOS speech engine.")?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(text.as_bytes())
            .context("Failed to send text to macOS speech engine.")?;
    }

    let output = child
        .wait_with_output()
        .context("Failed to wait for macOS speech engine.")?;
    if !output.status.success() {
        bail!(
            "macOS speech engine failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let output = Command::new("/usr/bin/afconvert")
        .arg("-f")
        .arg("WAVE")
        .arg("-d")
        .arg("LEI16@24000")
        .arg(&aiff_path)
        .arg(&wav_path)
        .output()
        .context("Failed to convert macOS speech output to WAV.")?;
    if !output.status.success() {
        bail!(
            "afconvert failed while creating WAV chunk: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let _ = fs::remove_file(aiff_path);
    Ok(wav_path)
}

fn macos_say_voice(voice: &str) -> &str {
    if voice.starts_with("af_") || voice.starts_with("am_") {
        "Samantha"
    } else {
        voice
    }
}

fn macos_say_rate(speed: f32) -> u16 {
    (320.0 * speed.clamp(0.5, 2.0)).round().clamp(180.0, 420.0) as u16
}

fn validate_model_file(path: &Path, label: &str, min_bytes: u64) -> Result<()> {
    let metadata = fs::metadata(path)
        .with_context(|| format!("{label} file not found: {}", path.display()))?;

    if !metadata.is_file() {
        bail!("{label} path is not a file: {}", path.display());
    }

    if metadata.len() < min_bytes {
        bail!(
            "{label} file looks incomplete: {} ({} bytes)",
            path.display(),
            metadata.len()
        );
    }

    Ok(())
}

struct KokoroBridge {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl KokoroBridge {
    fn start(model_path: &Path, voices_path: &Path) -> Result<Self> {
        let python = python_executable();
        let script = bridge_script_path()?;
        let site_packages = bundled_site_packages_path();

        let mut command = Command::new(&python);
        command
            .arg(&script)
            .arg("--model")
            .arg(model_path)
            .arg("--voices")
            .arg(voices_path)
            .env("PYTHONUNBUFFERED", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());

        if let Some(site_packages) = site_packages {
            command.env("PYTHONPATH", site_packages);
        }

        let mut child = command.spawn().with_context(|| {
            format!(
                "Failed to start Kokoro bridge with Python executable: {}",
                python.display()
            )
        })?;

        let stdin = child
            .stdin
            .take()
            .context("Failed to open stdin for Kokoro bridge.")?;
        let stdout = child
            .stdout
            .take()
            .context("Failed to open stdout for Kokoro bridge.")?;

        let mut bridge = Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
        };

        let ready = bridge.read_response()?;
        if !ready.ok {
            bail!(
                "{}",
                ready
                    .error
                    .unwrap_or_else(|| "Kokoro bridge failed to start.".to_string())
            );
        }

        Ok(bridge)
    }

    fn synthesize(
        &mut self,
        text: &str,
        voice: &str,
        speed: f32,
        output_path: &Path,
    ) -> Result<()> {
        let output_path_string = output_path.to_string_lossy();
        let request = SynthesizeRequest {
            text,
            voice,
            speed,
            output_path: output_path_string.as_ref(),
        };

        serde_json::to_writer(&mut self.stdin, &request)
            .context("Failed to send synthesis request to Kokoro bridge.")?;
        self.stdin
            .write_all(b"\n")
            .context("Failed to send synthesis newline to Kokoro bridge.")?;
        self.stdin
            .flush()
            .context("Failed to flush Kokoro bridge request.")?;

        let response = self.read_response()?;
        if !response.ok {
            bail!(
                "{}",
                response
                    .error
                    .unwrap_or_else(|| "Kokoro bridge synthesis failed.".to_string())
            );
        }

        Ok(())
    }

    fn read_response(&mut self) -> Result<BridgeResponse> {
        let mut line = String::new();
        let bytes = self
            .stdout
            .read_line(&mut line)
            .context("Failed to read response from Kokoro bridge.")?;

        if bytes == 0 {
            bail!("Kokoro bridge exited unexpectedly.");
        }

        serde_json::from_str(line.trim()).context("Kokoro bridge returned invalid JSON.")
    }
}

impl Drop for KokoroBridge {
    fn drop(&mut self) {
        let _ = serde_json::to_writer(
            &mut self.stdin,
            &serde_json::json!({ "output_path": "__shutdown__" }),
        );
        let _ = self.stdin.write_all(b"\n");
        let _ = self.stdin.flush();
        let _ = self.child.wait();
    }
}

#[derive(Serialize)]
struct SynthesizeRequest<'a> {
    text: &'a str,
    voice: &'a str,
    speed: f32,
    output_path: &'a str,
}

#[derive(Deserialize)]
struct BridgeResponse {
    ok: bool,
    error: Option<String>,
}

fn python_executable() -> PathBuf {
    if let Ok(path) = env::var("PAPER2VOICE_PYTHON") {
        return PathBuf::from(path);
    }

    find_from_runtime_roots("runtime/python/bin/python3.12")
        .or_else(|| find_from_runtime_roots("runtime/python/bin/python3"))
        .or_else(|| find_from_runtime_roots("runtime/python/bin/python"))
        .or_else(|| find_from_runtime_roots(".venv/bin/python"))
        .unwrap_or_else(|| PathBuf::from("python3"))
}

fn bridge_script_path() -> Result<PathBuf> {
    find_from_runtime_roots("scripts/kokoro_bridge.py")
        .context("Could not find scripts/kokoro_bridge.py from the app runtime location.")
}

fn bundled_site_packages_path() -> Option<PathBuf> {
    find_from_runtime_roots("runtime/site-packages")
}

fn find_from_runtime_roots(relative: &str) -> Option<PathBuf> {
    let mut roots = Vec::new();

    if let Ok(current_dir) = env::current_dir() {
        roots.push(current_dir);
    }

    if let Ok(exe) = env::current_exe() {
        if let Some(parent) = exe.parent() {
            roots.push(parent.to_path_buf());
        }
    }

    for root in roots {
        for ancestor in root.ancestors() {
            let candidate = ancestor.join(relative);
            if candidate.exists() {
                return Some(candidate);
            }

            let resource_candidate = ancestor.join("Resources").join(relative);
            if resource_candidate.exists() {
                return Some(resource_candidate);
            }
        }
    }

    None
}
