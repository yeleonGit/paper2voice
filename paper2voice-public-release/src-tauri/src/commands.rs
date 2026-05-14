use std::{
    fs,
    panic::{catch_unwind, AssertUnwindSafe},
    path::{Path, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    thread,
    time::Instant,
};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::{AppHandle, Emitter};

use crate::{
    audio::merge_wav_chunks_to_m4b,
    chunk::chunk_text,
    clean::clean_text,
    pdf::extract_pdf_text_by_pages,
    tts::{synthesize_chunk_with_macos_say, TtsEngine},
};

#[derive(Serialize)]
pub struct ConvertResult {
    pub pdf_path: String,
    pub output_path: String,
    pub raw_text_path: String,
    pub cleaned_text_path: String,
    pub chunk_count: usize,
    pub duration_seconds: Option<f32>,
    pub page_count: usize,
    pub chunks: Vec<ChunkTiming>,
    pub chunk_audio_paths: Vec<String>,
}

#[derive(Serialize)]
pub struct ChunkTiming {
    pub chunk_index: usize,
    pub page_number: usize,
    pub start_seconds: f32,
    pub end_seconds: f32,
}

#[derive(Serialize)]
pub struct DependencyStatus {
    pub ffmpeg_found: bool,
    pub ffmpeg_path: Option<String>,
    pub model_found: bool,
    pub voices_found: bool,
}

#[derive(Deserialize)]
pub struct SaveSessionRequest {
    pub session: Value,
}

struct AppPaths {
    project_root: PathBuf,
    output_dir: PathBuf,
    text_dir: PathBuf,
    chunks_dir: PathBuf,
    output_path: PathBuf,
    raw_text_path: PathBuf,
    cleaned_text_path: PathBuf,
    model_path: PathBuf,
    voices_path: PathBuf,
}

struct PageChunk {
    text: String,
    page_number: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConversionMode {
    SuperQuick,
    FastExport,
    Quality,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TtsBackend {
    MacosSay,
    Kokoro,
}

#[derive(Clone, Copy)]
struct ModeSettings {
    mode: ConversionMode,
    tts_backend: TtsBackend,
    chunk_chars: usize,
    worker_count: usize,
    write_debug_text: bool,
    initial_priority_pages: usize,
}

#[derive(Clone)]
struct SynthesizedChunk {
    wav_path: PathBuf,
    duration_seconds: f32,
}

const FALLBACK_SAMPLE_RATE: u32 = 24_000;

#[tauri::command]
pub async fn choose_pdf_file() -> Result<Option<String>, String> {
    let file = rfd::FileDialog::new()
        .add_filter("PDF", &["pdf"])
        .pick_file()
        .map(|path| path.to_string_lossy().to_string());

    Ok(file)
}

#[tauri::command]
pub async fn convert_pdf_to_audiobook(
    app: AppHandle,
    pdf_path: String,
    voice: String,
    speed: f32,
    max_chars: usize,
    conversion_mode: String,
) -> Result<ConvertResult, String> {
    tauri::async_runtime::spawn_blocking(move || {
        catch_unwind(AssertUnwindSafe(|| {
            convert_pdf_to_audiobook_inner(app, pdf_path, voice, speed, max_chars, conversion_mode)
        }))
        .map_err(|payload| {
            anyhow::anyhow!("Internal conversion error: {}", panic_message(payload))
        })?
    })
    .await
    .map_err(|error| error.to_string())?
    .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn open_output_folder(path: String) -> Result<(), String> {
    open_output_folder_inner(Path::new(&path)).map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn check_dependencies() -> Result<DependencyStatus, String> {
    let paths = AppPaths::for_dependency_check().map_err(|error| error.to_string())?;
    let audio_tool_path = resolve_tool("ffmpeg").or_else(|| resolve_tool("afconvert"));

    Ok(DependencyStatus {
        ffmpeg_found: audio_tool_path.is_some(),
        ffmpeg_path: audio_tool_path.map(|path| path.to_string_lossy().to_string()),
        model_found: paths.model_path.is_file(),
        voices_found: paths.voices_path.is_file(),
    })
}

#[tauri::command]
pub async fn file_exists(path: String) -> Result<bool, String> {
    Ok(Path::new(&path).is_file())
}

#[tauri::command]
pub async fn load_app_session() -> Result<Option<Value>, String> {
    let path = session_path().map_err(|error| error.to_string())?;
    if !path.is_file() {
        return Ok(None);
    }

    let raw = fs::read_to_string(&path).map_err(|error| error.to_string())?;
    serde_json::from_str(&raw).map(Some).map_err(|error| {
        format!(
            "Failed to read saved Paper2Voice session at {}: {error}",
            path.display()
        )
    })
}

#[tauri::command]
pub async fn save_app_session(request: SaveSessionRequest) -> Result<(), String> {
    let path = session_path().map_err(|error| error.to_string())?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }

    let body = serde_json::to_string_pretty(&request.session).map_err(|error| error.to_string())?;
    fs::write(&path, body).map_err(|error| error.to_string())
}

fn convert_pdf_to_audiobook_inner(
    app: AppHandle,
    pdf_path: String,
    voice: String,
    speed: f32,
    max_chars: usize,
    conversion_mode: String,
) -> Result<ConvertResult> {
    let started_at = Instant::now();
    let settings = ModeSettings::new(&conversion_mode, max_chars);
    let input_pdf = PathBuf::from(pdf_path);

    if !input_pdf.exists() {
        bail!("PDF not found: {}", input_pdf.display());
    }

    if input_pdf
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| !extension.eq_ignore_ascii_case("pdf"))
        .unwrap_or(true)
    {
        bail!("Selected file is not a PDF: {}", input_pdf.display());
    }

    let paths = AppPaths::new(&input_pdf)?;
    fs::create_dir_all(&paths.output_dir).with_context(|| {
        format!(
            "Failed to create output directory: {}",
            paths.output_dir.display()
        )
    })?;
    fs::create_dir_all(&paths.text_dir).with_context(|| {
        format!(
            "Failed to create text directory: {}",
            paths.text_dir.display()
        )
    })?;
    fs::create_dir_all(&paths.chunks_dir).with_context(|| {
        format!(
            "Failed to create chunks directory: {}",
            paths.chunks_dir.display()
        )
    })?;

    emit(&app, "Checking dependencies...");
    ensure_dependencies(&paths, settings)?;

    emit(
        &app,
        format!("Extracting PDF in {} mode...", settings.label()),
    );
    let pages = extract_pdf_text_by_pages(&input_pdf)?;
    let raw_text = pages.join("\n\n");
    if settings.write_debug_text {
        fs::write(&paths.raw_text_path, raw_text).with_context(|| {
            format!(
                "Failed to write raw text: {}",
                paths.raw_text_path.display()
            )
        })?;
    }

    emit(&app, "Cleaning text...");
    let cleaned_pages = pages
        .iter()
        .map(|page| clean_text(page))
        .collect::<Vec<_>>();
    let cleaned_text = cleaned_pages
        .iter()
        .filter(|page| !page.trim().is_empty())
        .cloned()
        .collect::<Vec<_>>()
        .join("\n\n");
    if settings.write_debug_text {
        fs::write(&paths.cleaned_text_path, cleaned_text).with_context(|| {
            format!(
                "Failed to write cleaned text: {}",
                paths.cleaned_text_path.display()
            )
        })?;
    }

    emit(&app, "Splitting text into chunks...");
    let chunks = cleaned_pages
        .iter()
        .enumerate()
        .flat_map(|(page_index, page_text)| {
            chunk_text(page_text, settings.chunk_chars)
                .into_iter()
                .map(move |text| PageChunk {
                    text,
                    page_number: page_index + 1,
                })
        })
        .filter(|chunk| !chunk.text.trim().is_empty())
        .collect::<Vec<_>>();

    if chunks.is_empty() {
        bail!("No chunks were generated from the PDF text.");
    }

    if settings.write_debug_text {
        for (index, chunk) in chunks.iter().enumerate() {
            let chunk_text_path = paths.chunks_dir.join(format!("chunk_{:04}.txt", index + 1));
            fs::write(&chunk_text_path, &chunk.text).with_context(|| {
                format!("Failed to write chunk text: {}", chunk_text_path.display())
            })?;
        }
    }

    let synthesized = synthesize_chunks_parallel(&app, &paths, &chunks, voice, speed, settings)?;
    let wav_paths = synthesized
        .iter()
        .map(|chunk| chunk.wav_path.clone())
        .collect::<Vec<_>>();
    let mut timings = Vec::with_capacity(chunks.len());
    let mut cursor_seconds = 0.0_f32;

    for (index, chunk) in chunks.iter().enumerate() {
        let duration = synthesized[index].duration_seconds;
        let start_seconds = cursor_seconds;
        let end_seconds = start_seconds + duration;
        cursor_seconds = end_seconds;

        timings.push(ChunkTiming {
            chunk_index: index + 1,
            page_number: chunk.page_number,
            start_seconds,
            end_seconds,
        });
    }

    emit(&app, "Merging audiobook...");
    merge_wav_chunks_to_m4b(&wav_paths, &paths.output_path)?;

    let duration_seconds = if cursor_seconds > 0.0 {
        Some(cursor_seconds)
    } else {
        audio_duration_seconds(&paths.output_path)
    };

    emit(
        &app,
        format!(
            "Done. Saved {} in ~/Documents/Paper2Voice/.",
            paths
                .output_path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("audiobook.m4b")
        ),
    );

    Ok(ConvertResult {
        pdf_path: input_pdf.to_string_lossy().to_string(),
        output_path: paths.output_path.to_string_lossy().to_string(),
        raw_text_path: paths.raw_text_path.to_string_lossy().to_string(),
        cleaned_text_path: paths.cleaned_text_path.to_string_lossy().to_string(),
        chunk_count: chunks.len(),
        duration_seconds: duration_seconds.or_else(|| Some(started_at.elapsed().as_secs_f32())),
        page_count: pages.len(),
        chunks: timings,
        chunk_audio_paths: wav_paths
            .iter()
            .map(|path| path.to_string_lossy().to_string())
            .collect(),
    })
}

fn synthesize_chunks_parallel(
    app: &AppHandle,
    paths: &AppPaths,
    chunks: &[PageChunk],
    voice: String,
    speed: f32,
    settings: ModeSettings,
) -> Result<Vec<SynthesizedChunk>> {
    let worker_count = settings.worker_count.min(chunks.len().max(1));
    let work_order = prioritized_work_order(chunks, settings);
    emit(
        app,
        format!(
            "Generating audio in {} mode with {} worker{}...",
            settings.label(),
            worker_count,
            if worker_count == 1 { "" } else { "s" }
        ),
    );

    if settings.tts_backend == TtsBackend::MacosSay {
        return synthesize_chunks_with_macos_say_parallel(
            app,
            chunks,
            voice,
            speed,
            &paths.chunks_dir,
            settings,
        );
    }

    if worker_count == 1 {
        let mut tts = TtsEngine::new(voice.clone(), speed, &paths.model_path, &paths.voices_path)?;
        let mut synthesized = vec![None; chunks.len()];

        for &index in &work_order {
            let chunk = &chunks[index];
            emit(
                app,
                format!("Generating audio chunk {} / {}...", index + 1, chunks.len()),
            );
            synthesized[index] = Some(synthesize_chunk_resilient(
                app,
                &mut tts,
                &chunk.text,
                index,
                chunks.len(),
                &voice,
                speed,
                &paths.model_path,
                &paths.voices_path,
                &paths.chunks_dir,
            )?);
        }

        return collect_synthesized_results(synthesized);
    }

    let work_order = Arc::new(work_order);
    let next_index = Arc::new(AtomicUsize::new(0));
    let completed = Arc::new(AtomicUsize::new(0));
    let results = Arc::new(Mutex::new(vec![None; chunks.len()]));

    thread::scope(|scope| -> Result<()> {
        let mut handles = Vec::with_capacity(worker_count);

        for _ in 0..worker_count {
            let app = app.clone();
            let voice = voice.clone();
            let model_path = paths.model_path.clone();
            let voices_path = paths.voices_path.clone();
            let chunks_dir = paths.chunks_dir.clone();
            let next_index = Arc::clone(&next_index);
            let completed = Arc::clone(&completed);
            let results = Arc::clone(&results);
            let work_order = Arc::clone(&work_order);

            handles.push(scope.spawn(move || -> Result<()> {
                let mut tts = TtsEngine::new(voice.clone(), speed, &model_path, &voices_path)?;

                loop {
                    let order_index = next_index.fetch_add(1, Ordering::SeqCst);
                    let Some(&index) = work_order.get(order_index) else {
                        break;
                    };

                    let chunk = &chunks[index];
                    let synthesized = synthesize_chunk_resilient(
                        &app,
                        &mut tts,
                        &chunk.text,
                        index,
                        chunks.len(),
                        &voice,
                        speed,
                        &model_path,
                        &voices_path,
                        &chunks_dir,
                    )?;
                    let done = completed.fetch_add(1, Ordering::SeqCst) + 1;

                    {
                        let mut results = results
                            .lock()
                            .map_err(|_| anyhow::anyhow!("TTS worker result lock poisoned."))?;
                        results[index] = Some(synthesized);
                    }

                    emit(
                        &app,
                        format!("Generating audio chunk {} / {}...", done, chunks.len()),
                    );
                }

                Ok(())
            }));
        }

        for handle in handles {
            match handle.join() {
                Ok(result) => result?,
                Err(_) => bail!("A TTS worker panicked."),
            }
        }

        Ok(())
    })?;

    let results = Arc::try_unwrap(results)
        .map_err(|_| anyhow::anyhow!("TTS worker results still have active references."))?
        .into_inner()
        .map_err(|_| anyhow::anyhow!("TTS worker result lock poisoned."))?;

    collect_synthesized_results(results)
}

fn synthesize_chunks_with_macos_say_parallel(
    app: &AppHandle,
    chunks: &[PageChunk],
    voice: String,
    speed: f32,
    chunks_dir: &Path,
    settings: ModeSettings,
) -> Result<Vec<SynthesizedChunk>> {
    let worker_count = settings.worker_count.min(chunks.len().max(1));
    let work_order = Arc::new(prioritized_work_order(chunks, settings));
    let next_index = Arc::new(AtomicUsize::new(0));
    let completed = Arc::new(AtomicUsize::new(0));
    let results = Arc::new(Mutex::new(vec![None; chunks.len()]));

    emit(
        app,
        "Super Quick uses macOS draft speech for fastest local export.",
    );

    thread::scope(|scope| -> Result<()> {
        let mut handles = Vec::with_capacity(worker_count);

        for _ in 0..worker_count {
            let app = app.clone();
            let voice = voice.clone();
            let chunks_dir = chunks_dir.to_path_buf();
            let next_index = Arc::clone(&next_index);
            let completed = Arc::clone(&completed);
            let results = Arc::clone(&results);
            let work_order = Arc::clone(&work_order);

            handles.push(scope.spawn(move || -> Result<()> {
                loop {
                    let order_index = next_index.fetch_add(1, Ordering::SeqCst);
                    let Some(&index) = work_order.get(order_index) else {
                        break;
                    };

                    let chunk = &chunks[index];
                    let synthesized = synthesize_macos_say_chunk_resilient(
                        &app,
                        &chunk.text,
                        index,
                        chunks.len(),
                        &voice,
                        speed,
                        &chunks_dir,
                    )?;
                    let done = completed.fetch_add(1, Ordering::SeqCst) + 1;

                    {
                        let mut results = results
                            .lock()
                            .map_err(|_| anyhow::anyhow!("TTS worker result lock poisoned."))?;
                        results[index] = Some(synthesized);
                    }

                    emit(
                        &app,
                        format!("Generating audio chunk {} / {}...", done, chunks.len()),
                    );
                }

                Ok(())
            }));
        }

        for handle in handles {
            match handle.join() {
                Ok(result) => result?,
                Err(_) => bail!("A macOS speech worker panicked."),
            }
        }

        Ok(())
    })?;

    let results = Arc::try_unwrap(results)
        .map_err(|_| anyhow::anyhow!("TTS worker results still have active references."))?
        .into_inner()
        .map_err(|_| anyhow::anyhow!("TTS worker result lock poisoned."))?;

    collect_synthesized_results(results)
}

fn synthesize_macos_say_chunk_resilient(
    app: &AppHandle,
    text: &str,
    index: usize,
    total: usize,
    voice: &str,
    speed: f32,
    chunks_dir: &Path,
) -> Result<SynthesizedChunk> {
    match synthesize_chunk_with_macos_say(text, index, voice, speed, chunks_dir) {
        Ok(wav_path) => {
            let duration_seconds = audio_duration_seconds(&wav_path).unwrap_or(0.0);
            return Ok(SynthesizedChunk {
                wav_path,
                duration_seconds,
            });
        }
        Err(error) => {
            emit(
                app,
                format!(
                    "Fast speech chunk {} / {} failed once; retrying. {}",
                    index + 1,
                    total,
                    short_error(&error)
                ),
            );
        }
    }

    match synthesize_chunk_with_macos_say(text, index, voice, speed, chunks_dir) {
        Ok(wav_path) => {
            let duration_seconds = audio_duration_seconds(&wav_path).unwrap_or(0.0);
            Ok(SynthesizedChunk {
                wav_path,
                duration_seconds,
            })
        }
        Err(error) => {
            emit(
                app,
                format!(
                    "Fast speech chunk {} / {} failed after retry; inserting silence and continuing. {}",
                    index + 1,
                    total,
                    short_error(&error)
                ),
            );
            write_silent_placeholder(text, index, speed, chunks_dir)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn synthesize_chunk_resilient(
    app: &AppHandle,
    tts: &mut TtsEngine,
    text: &str,
    index: usize,
    total: usize,
    voice: &str,
    speed: f32,
    model_path: &Path,
    voices_path: &Path,
    chunks_dir: &Path,
) -> Result<SynthesizedChunk> {
    match tts.synthesize_chunk(text, index, chunks_dir) {
        Ok(wav_path) => {
            let duration_seconds = audio_duration_seconds(&wav_path).unwrap_or(0.0);
            return Ok(SynthesizedChunk {
                wav_path,
                duration_seconds,
            });
        }
        Err(error) => {
            emit(
                app,
                format!(
                    "Chunk {} / {} failed once; retrying. {}",
                    index + 1,
                    total,
                    short_error(&error)
                ),
            );
        }
    }

    match TtsEngine::new(voice.to_string(), speed, model_path, voices_path).and_then(|mut retry| {
        let wav_path = retry.synthesize_chunk(text, index, chunks_dir)?;
        *tts = retry;
        Ok(wav_path)
    }) {
        Ok(wav_path) => {
            let duration_seconds = audio_duration_seconds(&wav_path).unwrap_or(0.0);
            Ok(SynthesizedChunk {
                wav_path,
                duration_seconds,
            })
        }
        Err(error) => {
            emit(
                app,
                format!(
                    "Chunk {} / {} failed after retry; inserting silence and continuing. {}",
                    index + 1,
                    total,
                    short_error(&error)
                ),
            );
            write_silent_placeholder(text, index, speed, chunks_dir)
        }
    }
}

fn collect_synthesized_results(
    results: Vec<Option<SynthesizedChunk>>,
) -> Result<Vec<SynthesizedChunk>> {
    results
        .into_iter()
        .enumerate()
        .map(|(index, chunk)| {
            chunk.with_context(|| format!("TTS failed to produce chunk_{:04}.wav", index + 1))
        })
        .collect()
}

fn write_silent_placeholder(
    text: &str,
    index: usize,
    speed: f32,
    chunks_dir: &Path,
) -> Result<SynthesizedChunk> {
    fs::create_dir_all(chunks_dir).with_context(|| {
        format!(
            "Failed to create chunk output directory: {}",
            chunks_dir.display()
        )
    })?;

    let wav_path = chunks_dir.join(format!("chunk_{:04}.wav", index + 1));
    let duration_seconds = estimated_placeholder_duration(text, speed);
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: FALLBACK_SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(&wav_path, spec)
        .with_context(|| format!("Failed to create fallback WAV: {}", wav_path.display()))?;
    let sample_count = (duration_seconds * FALLBACK_SAMPLE_RATE as f32).ceil() as usize;

    for _ in 0..sample_count {
        writer.write_sample(0_i16)?;
    }

    writer
        .finalize()
        .with_context(|| format!("Failed to finalize fallback WAV: {}", wav_path.display()))?;

    Ok(SynthesizedChunk {
        wav_path,
        duration_seconds,
    })
}

fn estimated_placeholder_duration(text: &str, speed: f32) -> f32 {
    let speed = speed.max(0.25);
    let word_count = text.split_whitespace().count().max(1) as f32;
    ((word_count / 155.0) * 60.0 / speed).clamp(1.0, 45.0)
}

fn short_error(error: &anyhow::Error) -> String {
    let message = error.to_string();
    let truncated = message.chars().take(180).collect::<String>();
    if truncated.len() < message.len() {
        format!("{truncated}...")
    } else {
        message
    }
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "unknown panic".to_string()
    }
}

impl ModeSettings {
    fn new(mode: &str, requested_max_chars: usize) -> Self {
        let parsed_mode = ConversionMode::from_str(mode);
        let cpu_count = thread::available_parallelism()
            .map(|count| count.get())
            .unwrap_or(2);

        match parsed_mode {
            ConversionMode::SuperQuick => Self {
                mode: parsed_mode,
                tts_backend: TtsBackend::MacosSay,
                chunk_chars: requested_max_chars.clamp(3_500, 5_000),
                worker_count: cpu_count.saturating_sub(1).clamp(4, 8),
                write_debug_text: false,
                initial_priority_pages: 3,
            },
            ConversionMode::FastExport => Self {
                mode: parsed_mode,
                tts_backend: TtsBackend::Kokoro,
                chunk_chars: requested_max_chars.clamp(1_600, 2_200),
                worker_count: cpu_count.saturating_sub(1).clamp(2, 6),
                write_debug_text: false,
                initial_priority_pages: 0,
            },
            ConversionMode::Quality => Self {
                mode: parsed_mode,
                tts_backend: TtsBackend::Kokoro,
                chunk_chars: requested_max_chars.clamp(700, 1_000),
                worker_count: cpu_count.saturating_sub(1).clamp(1, 2),
                write_debug_text: true,
                initial_priority_pages: 0,
            },
        }
    }

    fn label(self) -> &'static str {
        match self.mode {
            ConversionMode::SuperQuick => "Super Quick Draft",
            ConversionMode::FastExport => "Fast Export",
            ConversionMode::Quality => "Quality",
        }
    }
}

impl ConversionMode {
    fn from_str(value: &str) -> Self {
        match value {
            "fast_export" => Self::FastExport,
            "quality" => Self::Quality,
            _ => Self::SuperQuick,
        }
    }
}

fn prioritized_work_order(chunks: &[PageChunk], settings: ModeSettings) -> Vec<usize> {
    if settings.initial_priority_pages == 0 {
        return (0..chunks.len()).collect();
    }

    let mut priority = Vec::new();
    let mut rest = Vec::new();

    for (index, chunk) in chunks.iter().enumerate() {
        if chunk.page_number <= settings.initial_priority_pages {
            priority.push(index);
        } else {
            rest.push(index);
        }
    }

    priority.extend(rest);
    priority
}

impl AppPaths {
    fn new(input_pdf: &Path) -> Result<Self> {
        let project_root = project_root()?;
        let output_dir = output_base_dir()?;
        let text_dir = output_dir.join("text");
        let chunks_dir = output_dir.join("chunks");
        let output_path = output_dir.join(output_file_name(input_pdf));

        Ok(Self {
            model_path: project_root.join("models/kokoro-v1.0.onnx"),
            voices_path: project_root.join("models/voices-v1.0.bin"),
            raw_text_path: text_dir.join("raw.txt"),
            cleaned_text_path: text_dir.join("cleaned.txt"),
            project_root,
            output_dir,
            text_dir,
            chunks_dir,
            output_path,
        })
    }

    fn for_dependency_check() -> Result<Self> {
        Self::new(Path::new("audiobook.pdf"))
    }
}

fn output_base_dir() -> Result<PathBuf> {
    let documents =
        dirs::document_dir().context("Could not locate your Documents directory on this Mac.")?;
    Ok(documents.join("Paper2Voice"))
}

fn session_path() -> Result<PathBuf> {
    Ok(output_base_dir()?.join("session.json"))
}

fn output_file_name(input_pdf: &Path) -> String {
    let stem = input_pdf
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("audiobook");

    let safe = stem
        .chars()
        .map(|ch| match ch {
            '/' | '\\' | ':' | '\0' => '_',
            _ => ch,
        })
        .collect::<String>();
    let safe = safe.trim().trim_matches('.').to_string();

    if safe.is_empty() {
        "audiobook.m4b".to_string()
    } else {
        format!("{safe}.m4b")
    }
}

fn project_root() -> Result<PathBuf> {
    let mut searched = Vec::new();
    let mut roots = Vec::new();

    if let Ok(current_dir) = std::env::current_dir() {
        roots.push(current_dir);
    }

    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            roots.push(parent.to_path_buf());
        }
    }

    for root in roots {
        for ancestor in root.ancestors() {
            searched.push(ancestor.display().to_string());
            if ancestor.join("models").is_dir() && ancestor.join("scripts").is_dir() {
                return Ok(ancestor.to_path_buf());
            }

            let resource_root = ancestor.join("Resources");
            if resource_root.join("models").is_dir() && resource_root.join("scripts").is_dir() {
                return Ok(resource_root);
            }
        }
    }

    bail!(
        "Could not find project root containing models/ and scripts/. Searched from: {}",
        searched.join(", ")
    )
}

fn ensure_dependencies(paths: &AppPaths, settings: ModeSettings) -> Result<()> {
    if resolve_tool("ffmpeg").is_none() && resolve_tool("afconvert").is_none() {
        bail!("No audio exporter found. Paper2Voice needs FFmpeg or macOS afconvert.");
    }

    if settings.tts_backend == TtsBackend::MacosSay {
        if !Path::new("/usr/bin/say").is_file() {
            bail!("macOS speech engine not found at /usr/bin/say.");
        }
        if !Path::new("/usr/bin/afconvert").is_file() {
            bail!("macOS audio converter not found at /usr/bin/afconvert.");
        }
        return Ok(());
    }

    if !paths.model_path.is_file() {
        bail!("Model file not found: {}", paths.model_path.display());
    }

    if !paths.voices_path.is_file() {
        bail!("Voices file not found: {}", paths.voices_path.display());
    }

    if !paths
        .project_root
        .join("scripts/kokoro_bridge.py")
        .is_file()
    {
        bail!(
            "Kokoro bridge script not found: {}",
            paths
                .project_root
                .join("scripts/kokoro_bridge.py")
                .display()
        );
    }

    Ok(())
}

fn audio_duration_seconds(path: &Path) -> Option<f32> {
    if path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.eq_ignore_ascii_case("wav"))
        .unwrap_or(false)
    {
        return wav_duration_seconds(path);
    }

    let ffprobe = resolve_tool("ffprobe")?;
    let output = Command::new(ffprobe)
        .arg("-v")
        .arg("error")
        .arg("-show_entries")
        .arg("format=duration")
        .arg("-of")
        .arg("default=noprint_wrappers=1:nokey=1")
        .arg(path)
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<f32>()
        .ok()
}

fn wav_duration_seconds(path: &Path) -> Option<f32> {
    let reader = hound::WavReader::open(path).ok()?;
    let spec = reader.spec();
    if spec.sample_rate == 0 || spec.channels == 0 {
        return None;
    }

    Some(reader.duration() as f32 / spec.sample_rate as f32 / spec.channels as f32)
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

fn open_output_folder_inner(path: &Path) -> Result<()> {
    let folder = if path.is_file() {
        path.parent().unwrap_or(path)
    } else {
        path
    };

    Command::new("open")
        .arg(folder)
        .status()
        .context("Failed to open output folder in Finder.")?;

    Ok(())
}

fn emit(app: &AppHandle, message: impl Into<String>) {
    let _ = app.emit("conversion-progress", message.into());
}

#[cfg(test)]
mod tests {
    use super::{output_file_name, ModeSettings, TtsBackend};
    use std::path::Path;

    #[test]
    fn uses_pdf_stem_for_output_file_name() {
        assert_eq!(
            output_file_name(Path::new("/tmp/My Book.pdf")),
            "My Book.m4b"
        );
    }

    #[test]
    fn falls_back_for_empty_or_unsafe_output_file_name() {
        assert_eq!(output_file_name(Path::new("/tmp/..pdf")), "audiobook.m4b");
        assert_eq!(
            output_file_name(Path::new("/tmp/chapter:one.pdf")),
            "chapter_one.m4b"
        );
    }

    #[test]
    fn super_quick_uses_fast_local_draft_engine() {
        let settings = ModeSettings::new("super_quick", 900);
        assert_eq!(settings.tts_backend, TtsBackend::MacosSay);
        assert!(settings.chunk_chars >= 3_500);
        assert!(settings.worker_count >= 4);
    }
}
