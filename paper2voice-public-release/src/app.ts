import { convertFileSrc, invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

type DependencyStatus = {
  ffmpeg_found: boolean;
  ffmpeg_path: string | null;
  model_found: boolean;
  voices_found: boolean;
};

type ChunkTiming = {
  chunk_index: number;
  page_number: number;
  start_seconds: number;
  end_seconds: number;
};

type ConvertResult = {
  pdf_path: string;
  output_path: string;
  raw_text_path: string;
  cleaned_text_path: string;
  chunk_count: number;
  duration_seconds: number | null;
  page_count: number;
  chunks: ChunkTiming[];
  chunk_audio_paths: string[];
};

type ConversionMode = "super_quick" | "fast_export" | "quality";
type ThemeMode = "dark" | "light";

type SavedBook = ConvertResult & {
  id: string;
  title: string;
  playback_speed: number;
  last_position_seconds: number;
  current_page: number;
  status_text: string;
  created_at: string;
  saved_at: string;
};

type LibraryState = {
  version: 1;
  active_book_id: string | null;
  books: SavedBook[];
};

const STORAGE_KEY = "paper2voice:library";
const LEGACY_STORAGE_KEY = "paper2voice:last-session";
const THEME_KEY = "paper2voice:theme";

const dropZone = byId<HTMLDivElement>("drop-zone");
const selectedPdfLabel = byId<HTMLElement>("selected-pdf");
const dependencyStatus = byId<HTMLDivElement>("dependency-status");
const voiceSelect = byId<HTMLSelectElement>("voice-select");
const modeInputs = Array.from(
  document.querySelectorAll<HTMLInputElement>('input[name="conversion-mode"]'),
);
const themeInputs = Array.from(
  document.querySelectorAll<HTMLInputElement>('input[name="theme-mode"]'),
);
const speedInput = byId<HTMLInputElement>("speed-input");
const speedOutput = byId<HTMLOutputElement>("speed-output");
const convertButton = byId<HTMLButtonElement>("convert-button");
const progressVisual = byId<HTMLDivElement>("progress-visual");
const progressStatus = byId<HTMLElement>("progress-status");
const progressDetail = byId<HTMLParagraphElement>("progress-detail");
const progressLog = byId<HTMLPreElement>("progress-log");
const outputPath = byId<HTMLParagraphElement>("output-path");
const openOutputButton = byId<HTMLButtonElement>("open-output-button");
const libraryList = byId<HTMLDivElement>("library-list");
const libraryEmpty = byId<HTMLParagraphElement>("library-empty");
const audioPlayer = byId<HTMLAudioElement>("audio-player");
const playbackSpeedInput = byId<HTMLInputElement>("playback-speed-input");
const playbackSpeedOutput = byId<HTMLOutputElement>("playback-speed-output");
const prevPageButton = byId<HTMLButtonElement>("prev-page-button");
const nextPageButton = byId<HTMLButtonElement>("next-page-button");
const pageSlider = byId<HTMLInputElement>("page-slider");
const pageInput = byId<HTMLInputElement>("page-input");
const pdfViewer = byId<HTMLIFrameElement>("pdf-viewer");
const pageRail = byId<HTMLElement>("page-rail");
const pageIndicator = byId<HTMLDivElement>("page-indicator");
const readerStatus = byId<HTMLParagraphElement>("reader-status");

let libraryState: LibraryState = {
  version: 1,
  active_book_id: null,
  books: [],
};
let conversionPdfPath: string | null = null;
let lastOutputPath: string | null = null;
let timings: ChunkTiming[] = [];
let activePage = 0;
let currentBook: SavedBook | null = null;
let pendingSeekSeconds: number | null = null;
let lastPlaybackSaveAt = 0;
let usingChunkAudio = false;
let activeChunkIndex = 0;
let suppressAudioError = false;

applyTheme(savedThemeMode());
void boot();

async function boot() {
  wireUi();
  await listen<string>("conversion-progress", (event) => appendLog(event.payload));
  await checkDependencies();
  await restoreLibrary();
}

function wireUi() {
  speedInput.addEventListener("input", () => {
    speedOutput.value = `${Number(speedInput.value).toFixed(2)}x`;
  });

  playbackSpeedInput.addEventListener("input", () => updatePlaybackSpeed());

  for (const input of themeInputs) {
    input.addEventListener("change", () => {
      if (input.checked) applyTheme(themeModeFromValue(input.value), true);
    });
  }

  dropZone.addEventListener("click", choosePdfWithNativeDialog);
  dropZone.addEventListener("keydown", (event) => {
    if (event.key === "Enter" || event.key === " ") {
      event.preventDefault();
      void choosePdfWithNativeDialog();
    }
  });

  dropZone.addEventListener("dragover", (event) => {
    event.preventDefault();
    dropZone.classList.add("drag-over");
  });

  dropZone.addEventListener("dragleave", () => {
    dropZone.classList.remove("drag-over");
  });

  dropZone.addEventListener("drop", (event) => {
    event.preventDefault();
    dropZone.classList.remove("drag-over");

    const file = event.dataTransfer?.files?.[0] as (File & { path?: string }) | undefined;
    if (!file) {
      appendLog("No file was dropped.");
      return;
    }

    if (!file.name.toLowerCase().endsWith(".pdf")) {
      appendLog("Choose a PDF file.");
      return;
    }

    if (!file.path) {
      appendLog("Dropped PDF path is unavailable. Click the drop zone to choose it.");
      return;
    }

    setSelectedPdf(file.path);
  });

  convertButton.addEventListener("click", startConversion);
  openOutputButton.addEventListener("click", async () => {
    if (!lastOutputPath) return;
    try {
      await invoke("open_output_folder", { path: lastOutputPath });
    } catch (error) {
      appendLog(`Open output failed: ${String(error)}`);
    }
  });

  audioPlayer.addEventListener("timeupdate", syncPageToPlayback);
  audioPlayer.addEventListener("timeupdate", persistPlaybackPosition);
  audioPlayer.addEventListener("error", handleAudioError);
  audioPlayer.addEventListener("ended", handleAudioEnded);
  audioPlayer.addEventListener("pause", () => persistPlaybackState());
  audioPlayer.addEventListener("loadedmetadata", () => {
    playbackSpeedInput.disabled = false;
    updatePlaybackSpeed(false);
    if (pendingSeekSeconds !== null && Number.isFinite(audioPlayer.duration) && !usingChunkAudio) {
      audioPlayer.currentTime = Math.min(pendingSeekSeconds, audioPlayer.duration);
      pendingSeekSeconds = null;
    }
  });

  prevPageButton.addEventListener("click", () => goToPage(activePage - 1));
  nextPageButton.addEventListener("click", () => goToPage(activePage + 1));
  pageSlider.addEventListener("input", () => goToPage(Number(pageSlider.value)));
  pageInput.addEventListener("change", () => goToPage(Number(pageInput.value)));
}

async function checkDependencies() {
  try {
    const status = await invoke<DependencyStatus>("check_dependencies");
    const missing = [
      !status.ffmpeg_found ? "audio exporter" : null,
      !status.model_found ? "model" : null,
      !status.voices_found ? "voices" : null,
    ].filter(Boolean);

    if (missing.length === 0) {
      dependencyStatus.textContent = "Ready";
      dependencyStatus.title = status.ffmpeg_path
        ? `Audio exporter: ${status.ffmpeg_path}`
        : "Dependencies ready";
      dependencyStatus.classList.remove("danger");
    } else {
      dependencyStatus.textContent = `Missing: ${missing.join(", ")}`;
      dependencyStatus.title = dependencyStatus.textContent;
      dependencyStatus.classList.add("danger");
    }
  } catch (error) {
    dependencyStatus.textContent = "Check failed";
    dependencyStatus.title = String(error);
    dependencyStatus.classList.add("danger");
  }
}

async function choosePdfWithNativeDialog() {
  try {
    const path = await invoke<string | null>("choose_pdf_file");
    if (path) setSelectedPdf(path);
  } catch (error) {
    appendLog(`File picker failed: ${String(error)}`);
  }
}

function setSelectedPdf(path: string) {
  conversionPdfPath = path;
  selectedPdfLabel.textContent = fileName(path);
  convertButton.disabled = false;
  outputPath.textContent = "Waiting for conversion.";
  setProgress("Ready", "PDF selected. Convert when ready.", "idle");

  if (!currentBook) {
    pageRail.innerHTML = "";
    pageIndicator.textContent = "Page 1";
    configurePageControls(1);
    playbackSpeedInput.disabled = true;
    readerStatus.textContent = "PDF selected. You can read it while conversion runs.";
    setPdfViewer(path, 1);
  }

  renderLibrary();
}

async function startConversion() {
  if (!conversionPdfPath) {
    appendLog("Choose a PDF first.");
    return;
  }

  const pdfPath = conversionPdfPath;
  const shouldLoadAfterConversion = currentBook === null;
  convertButton.disabled = true;
  progressLog.textContent = "";
  outputPath.textContent = "Working...";
  setProgress("Converting", "Preparing audiobook pipeline...", "busy");

  try {
    const result = await invoke<ConvertResult>("convert_pdf_to_audiobook", {
      pdfPath,
      voice: voiceSelect.value,
      speed: Number(speedInput.value),
      maxChars: 900,
      conversionMode: selectedConversionMode(),
    });

    const book = saveBookFromResult(result, shouldLoadAfterConversion);
    lastOutputPath = result.output_path;
    outputPath.textContent = result.output_path;
    openOutputButton.disabled = false;

    if (shouldLoadAfterConversion) {
      loadBook(book, 0);
    }

    appendLog("Done.");
  } catch (error) {
    outputPath.textContent = "Conversion failed.";
    appendLog(`Error: ${String(error)}`);
  } finally {
    convertButton.disabled = conversionPdfPath === null;
  }
}

async function restoreLibrary() {
  libraryState = await readLibraryState();
  libraryState.books = await existingBooks(libraryState.books);

  if (!libraryState.books.some((book) => book.id === libraryState.active_book_id)) {
    libraryState.active_book_id = libraryState.books[0]?.id ?? null;
  }

  renderLibrary();
  await persistLibraryState();

  const activeBook = libraryState.books.find((book) => book.id === libraryState.active_book_id);
  if (!activeBook) return;

  loadBook(activeBook, activeBook.last_position_seconds);
  appendLog(`Restored ${activeBook.title}.`);
}

async function existingBooks(books: SavedBook[]) {
  const checks = await Promise.all(
    books.map(async (book) => {
      const [pdfExists, outputExists] = await Promise.all([
        invoke<boolean>("file_exists", { path: book.pdf_path }).catch(() => false),
        invoke<boolean>("file_exists", { path: book.output_path }).catch(() => false),
      ]);

      return pdfExists && outputExists ? book : null;
    }),
  );

  const existing = checks.filter((book): book is SavedBook => book !== null);
  if (existing.length !== books.length) {
    appendLog("Some library items were hidden because their PDF or audiobook moved.");
  }

  return existing;
}

function saveBookFromResult(result: ConvertResult, activate: boolean) {
  const now = new Date().toISOString();
  const existing = libraryState.books.find((book) => book.id === result.output_path);
  const book: SavedBook = {
    ...result,
    id: result.output_path,
    title: fileName(result.pdf_path).replace(/\.pdf$/i, ""),
    playback_speed: Number(playbackSpeedInput.value),
    last_position_seconds: 0,
    current_page: 1,
    status_text: readyStatusText(result),
    created_at: existing?.created_at ?? now,
    saved_at: now,
  };

  libraryState.books = [
    book,
    ...libraryState.books.filter((item) => item.id !== book.id),
  ];

  if (activate) {
    libraryState.active_book_id = book.id;
    currentBook = book;
  }

  persistLibraryState();
  renderLibrary();
  return book;
}

function loadBook(book: SavedBook, seekSeconds = book.last_position_seconds) {
  currentBook = book;
  libraryState.active_book_id = book.id;
  conversionPdfPath = book.pdf_path;
  lastOutputPath = book.output_path;
  timings = book.chunks;
  usingChunkAudio = false;
  activeChunkIndex = 0;
  suppressAudioError = false;
  selectedPdfLabel.textContent = fileName(book.pdf_path);
  outputPath.textContent = book.output_path;
  convertButton.disabled = false;
  openOutputButton.disabled = false;
  playbackSpeedInput.disabled = false;
  playbackSpeedInput.value = String(clamp(book.playback_speed, 1, 4));
  pendingSeekSeconds = Math.max(0, seekSeconds);
  audioPlayer.onloadedmetadata = null;

  if (book.chunk_audio_paths.length > 0 && book.chunk_count > 200) {
    readerStatus.textContent = "Large audiobook loaded. Playing generated chunks for smoother seeking.";
    startChunkAudioFallback(pendingSeekSeconds);
  } else {
    audioPlayer.src = convertFileSrc(book.output_path);
    audioPlayer.load();
    updatePlaybackSpeed(false);
  }

  const page = pageForTime(seekSeconds) ?? book.current_page ?? 1;
  setPdfViewer(book.pdf_path, page);
  configurePageControls(book.page_count);
  renderPageRail(page);
  setActivePage(page, false);
  readerStatus.textContent = book.status_text || readyStatusText(book);
  renderLibrary();
}

function renderPageRail(centerPage = activePage || 1) {
  pageRail.innerHTML = "";
  if (!currentBook) return;

  const pageCount = currentBook.page_count;
  const windowSize = 11;
  const half = Math.floor(windowSize / 2);
  let start = Math.max(1, centerPage - half);
  const end = Math.min(pageCount, start + windowSize - 1);
  start = Math.max(1, end - windowSize + 1);

  if (start > 1) {
    pageRail.append(pageRailJumpButton(1));
    if (start > 2) pageRail.append(pageRailGap());
  }

  for (let page = start; page <= end; page += 1) {
    const button = document.createElement("button");
    button.type = "button";
    button.textContent = String(page);
    button.classList.toggle("active", page === activePage);
    button.addEventListener("click", () => {
      goToPage(page);
    });
    pageRail.append(button);
  }

  if (end < pageCount) {
    if (end < pageCount - 1) pageRail.append(pageRailGap());
    pageRail.append(pageRailJumpButton(pageCount));
  }
}

function pageRailJumpButton(page: number) {
  const button = document.createElement("button");
  button.type = "button";
  button.textContent = String(page);
  button.classList.toggle("active", page === activePage);
  button.addEventListener("click", () => goToPage(page));
  return button;
}

function pageRailGap() {
  const gap = document.createElement("span");
  gap.className = "page-gap";
  gap.textContent = "...";
  return gap;
}

function renderLibrary() {
  libraryList.innerHTML = "";
  libraryEmpty.hidden = libraryState.books.length > 0;

  for (const book of libraryState.books) {
    const button = document.createElement("button");
    button.type = "button";
    button.className = "library-item";
    button.classList.toggle("active", book.id === libraryState.active_book_id);
    button.addEventListener("click", () => loadBook(book));

    const title = document.createElement("strong");
    title.textContent = book.title;

    const meta = document.createElement("span");
    meta.textContent = `Page ${book.current_page || 1} / ${book.page_count} - ${formatDuration(
      book.last_position_seconds,
    )}`;

    button.append(title, meta);
    libraryList.append(button);
  }
}

function syncPageToPlayback() {
  if (timings.length === 0 || !currentBook) return;

  const current = currentPlaybackSeconds();
  const chunk = chunkForTime(current);

  if (chunk && chunk.page_number !== activePage) {
    setPdfViewer(currentBook.pdf_path, chunk.page_number);
    setActivePage(chunk.page_number);
  }
}

function updatePlaybackSpeed(shouldPersist = true) {
  const speed = Number(playbackSpeedInput.value);
  audioPlayer.playbackRate = speed;
  audioPlayer.defaultPlaybackRate = speed;
  playbackSpeedOutput.value = `${speed.toFixed(2)}x`;
  if (shouldPersist) {
    persistPlaybackState();
  }
}

function persistPlaybackPosition() {
  if (!currentBook) return;

  const now = Date.now();
  if (now - lastPlaybackSaveAt < 2500) return;

  lastPlaybackSaveAt = now;
  persistPlaybackState();
}

function persistPlaybackState(positionOverride?: number) {
  if (!currentBook) return;
  const playbackSeconds = currentPlaybackSeconds();

  const updated: SavedBook = {
    ...currentBook,
    playback_speed: Number(playbackSpeedInput.value),
    last_position_seconds: typeof positionOverride === "number" && Number.isFinite(positionOverride)
      ? positionOverride
      : Number.isFinite(playbackSeconds)
      ? playbackSeconds
      : currentBook.last_position_seconds,
    current_page: activePage || currentBook.current_page || 1,
    saved_at: new Date().toISOString(),
  };

  currentBook = updated;
  libraryState.books = libraryState.books.map((book) =>
    book.id === updated.id ? updated : book,
  );
  libraryState.active_book_id = updated.id;
  renderLibrary();
  void persistLibraryState();
}

async function persistLibraryState() {
  localStorage.setItem(STORAGE_KEY, JSON.stringify(libraryState));
  localStorage.setItem(LEGACY_STORAGE_KEY, JSON.stringify(currentBook));

  await invoke("save_app_session", {
    request: {
      session: libraryState,
    },
  }).catch(() => {
    // Local storage still keeps the library if the filesystem write is unavailable.
  });
}

async function readLibraryState(): Promise<LibraryState> {
  const backendSession = await invoke<unknown | null>("load_app_session").catch(() => null);
  return (
    normalizeLibraryState(backendSession) ??
    normalizeLibraryState(localStorage.getItem(STORAGE_KEY)) ??
    normalizeLegacyBook(localStorage.getItem(LEGACY_STORAGE_KEY)) ??
    { version: 1, active_book_id: null, books: [] }
  );
}

function normalizeLibraryState(value: unknown): LibraryState | null {
  try {
    const parsed = parseUnknown(value);
    if (!parsed || typeof parsed !== "object") return null;

    const maybeState = parsed as Partial<LibraryState>;
    if (Array.isArray(maybeState.books)) {
      const books = maybeState.books
        .map((book) => normalizeBook(book))
        .filter((book): book is SavedBook => book !== null);

      return {
        version: 1,
        active_book_id:
          typeof maybeState.active_book_id === "string" ? maybeState.active_book_id : null,
        books,
      };
    }

    const legacyBook = normalizeBook(parsed);
    return legacyBook
      ? { version: 1, active_book_id: legacyBook.id, books: [legacyBook] }
      : null;
  } catch {
    return null;
  }
}

function normalizeLegacyBook(value: unknown): LibraryState | null {
  const book = normalizeBook(parseUnknown(value));
  return book ? { version: 1, active_book_id: book.id, books: [book] } : null;
}

function normalizeBook(value: unknown): SavedBook | null {
  if (!value || typeof value !== "object") return null;
  const book = value as Partial<SavedBook>;

  if (
    typeof book.pdf_path !== "string" ||
    typeof book.output_path !== "string" ||
    !Array.isArray(book.chunks) ||
    typeof book.page_count !== "number"
  ) {
    return null;
  }

  const title =
    typeof book.title === "string" && book.title.trim()
      ? book.title
      : fileName(book.pdf_path).replace(/\.pdf$/i, "");
  const chunkAudioPaths = Array.isArray(book.chunk_audio_paths)
    ? book.chunk_audio_paths.filter((path): path is string => typeof path === "string")
    : inferredChunkAudioPaths(book.output_path, book.chunks.length);

  return {
    raw_text_path: "",
    cleaned_text_path: "",
    chunk_count: book.chunks.length,
    duration_seconds: null,
    playback_speed: 1,
    last_position_seconds: 0,
    current_page: 1,
    status_text: "",
    created_at: "",
    saved_at: "",
    ...book,
    id: typeof book.id === "string" ? book.id : book.output_path,
    title,
    chunk_audio_paths: chunkAudioPaths,
  } as SavedBook;
}

function inferredChunkAudioPaths(outputPath: string, chunkCount: number) {
  if (chunkCount <= 0) return [];
  const separator = outputPath.includes("\\") ? "\\" : "/";
  const baseDir = outputPath.split(/[\\/]/).slice(0, -1).join(separator);
  if (!baseDir) return [];

  return Array.from({ length: chunkCount }, (_, index) => {
    return `${baseDir}${separator}chunks${separator}chunk_${String(index + 1).padStart(4, "0")}.wav`;
  });
}

function parseUnknown(value: unknown) {
  if (typeof value === "string") {
    if (!value) return null;
    return JSON.parse(value) as unknown;
  }
  return value;
}

function chunkForTime(seconds: number) {
  return (
    timings.find(
      (item) => seconds >= item.start_seconds && seconds < item.end_seconds,
    ) ?? timings[timings.length - 1]
  );
}

function pageForTime(seconds: number) {
  return chunkForTime(seconds)?.page_number;
}

function goToPage(page: number) {
  if (!currentBook) return;

  const clampedPage = clamp(Math.round(page), 1, Math.max(1, currentBook.page_count));
  const seekSeconds = secondsForPage(clampedPage);

  setPdfViewer(currentBook.pdf_path, clampedPage);
  setActivePage(clampedPage);

  if (seekSeconds !== null) {
    seekAudio(seekSeconds);
    readerStatus.textContent = `Synced to page ${clampedPage}.`;
  } else {
    readerStatus.textContent = `Page ${clampedPage} has no synced audio yet.`;
  }

  persistPlaybackState(seekSeconds ?? undefined);
}

function secondsForPage(page: number) {
  if (timings.length === 0) return null;

  const exact = timings.find((chunk) => chunk.page_number === page);
  if (exact) return exact.start_seconds;

  const next = timings.find((chunk) => chunk.page_number > page);
  if (next) return next.start_seconds;

  for (let index = timings.length - 1; index >= 0; index -= 1) {
    const previous = timings[index];
    if (previous.page_number < page) return previous.start_seconds;
  }

  return timings[0]?.start_seconds ?? null;
}

function seekAudio(seconds: number) {
  const safeSeconds = Math.max(0, seconds);

  if (usingChunkAudio) {
    loadChunkAudioAt(safeSeconds);
    return;
  }

  if (Number.isFinite(audioPlayer.duration)) {
    audioPlayer.currentTime = Math.min(safeSeconds, audioPlayer.duration);
    return;
  }

  pendingSeekSeconds = safeSeconds;
}

function setPdfViewer(path: string, page: number) {
  const url = `${convertFileSrc(path)}#page=${page}`;
  pdfViewer.src = url;
}

function setActivePage(page: number, shouldPersist = true) {
  activePage = page;
  pageIndicator.textContent = `Page ${page}`;
  pageSlider.value = String(page);
  pageInput.value = String(page);
  prevPageButton.disabled = !currentBook || page <= 1;
  nextPageButton.disabled = !currentBook || page >= currentBook.page_count;
  renderPageRail(page);

  for (const button of Array.from(pageRail.querySelectorAll("button"))) {
    const isActive = button.textContent === String(page);
    button.classList.toggle("active", isActive);
    if (isActive) {
      button.scrollIntoView({ block: "nearest", inline: "nearest" });
    }
  }

  if (currentBook && shouldPersist) {
    currentBook.current_page = page;
  }
}

function configurePageControls(pageCount: number) {
  const max = Math.max(1, pageCount);
  pageSlider.max = String(max);
  pageInput.max = String(max);
  pageSlider.disabled = max <= 1;
  pageInput.disabled = max <= 1;
  prevPageButton.disabled = max <= 1;
  nextPageButton.disabled = max <= 1;
}

function handleAudioError() {
  if (suppressAudioError || usingChunkAudio || !currentBook?.chunk_audio_paths.length) return;
  readerStatus.textContent = "Final audiobook did not load. Playing generated chunks instead.";
  startChunkAudioFallback(pendingSeekSeconds ?? currentPlaybackSeconds());
}

function handleAudioEnded() {
  if (usingChunkAudio && currentBook) {
    if (activeChunkIndex + 1 < currentBook.chunk_audio_paths.length) {
      loadChunkByIndex(activeChunkIndex + 1, 0, true);
      return;
    }
  }

  persistPlaybackState();
}

function startChunkAudioFallback(startSeconds: number) {
  usingChunkAudio = true;
  pendingSeekSeconds = null;
  loadChunkAudioAt(startSeconds);
}

function loadChunkAudioAt(seconds: number) {
  const chunk = chunkForTime(seconds);
  if (!chunk) return;

  const localSeconds = clamp(seconds - chunk.start_seconds, 0, Math.max(0, chunk.end_seconds - chunk.start_seconds));
  loadChunkByIndex(chunk.chunk_index - 1, localSeconds, !audioPlayer.paused);
}

function loadChunkByIndex(index: number, localSeconds = 0, shouldPlay = false) {
  if (!currentBook) return;
  const path = currentBook.chunk_audio_paths[index];
  if (!path) return;

  activeChunkIndex = index;
  usingChunkAudio = true;
  suppressAudioError = true;
  audioPlayer.src = convertFileSrc(path);
  audioPlayer.load();
  audioPlayer.onloadedmetadata = () => {
    audioPlayer.currentTime = Math.min(localSeconds, Number.isFinite(audioPlayer.duration) ? audioPlayer.duration : localSeconds);
    updatePlaybackSpeed(false);
    suppressAudioError = false;
    audioPlayer.onloadedmetadata = null;
    if (shouldPlay) {
      void audioPlayer.play().catch(() => {
        readerStatus.textContent = "Chunk audio is ready. Press play to continue.";
      });
    }
  };
}

function currentPlaybackSeconds() {
  if (usingChunkAudio) {
    const chunk = timings[activeChunkIndex];
    return (chunk?.start_seconds ?? 0) + (Number.isFinite(audioPlayer.currentTime) ? audioPlayer.currentTime : 0);
  }

  return Number.isFinite(audioPlayer.currentTime) ? audioPlayer.currentTime : 0;
}

function appendLog(message: string) {
  progressLog.textContent += `${message}\n`;
  progressLog.scrollTop = progressLog.scrollHeight;
  setProgress(progressLabelFor(message), message, progressKindFor(message));
}

function setProgress(
  status: string,
  detail: string,
  kind: "idle" | "busy" | "done" | "error",
) {
  progressStatus.textContent = status;
  progressDetail.textContent = detail;
  progressVisual.classList.remove("idle", "busy", "done", "error");
  progressVisual.classList.add(kind);
}

function progressLabelFor(message: string) {
  if (message.startsWith("Error:")) return "Failed";
  if (message === "Done." || message.startsWith("Restored")) return "Ready";
  if (message.includes("Generating audio")) return "Synthesizing";
  if (message.includes("Merging")) return "Merging";
  if (message.includes("Extracting")) return "Extracting";
  if (message.includes("Cleaning")) return "Cleaning";
  if (message.includes("Splitting")) return "Chunking";
  if (message.includes("Checking")) return "Checking";
  return "Status";
}

function progressKindFor(message: string): "idle" | "busy" | "done" | "error" {
  if (message.startsWith("Error:")) return "error";
  if (message === "Done." || message.startsWith("Restored")) return "done";
  if (
    message.includes("Generating") ||
    message.includes("Extracting") ||
    message.includes("Cleaning") ||
    message.includes("Splitting") ||
    message.includes("Merging") ||
    message.includes("Checking")
  ) {
    return "busy";
  }
  return "idle";
}

function fileName(path: string) {
  return path.split(/[\\/]/).pop() ?? path;
}

function formatDuration(seconds: number | null) {
  if (!seconds || !Number.isFinite(seconds)) return "0:00";

  const minutes = Math.floor(seconds / 60);
  const remaining = Math.round(seconds % 60);
  return `${minutes}:${remaining.toString().padStart(2, "0")}`;
}

function readyStatusText(result: ConvertResult) {
  return `Ready: ${result.chunk_count} chunks, ${formatDuration(result.duration_seconds)}`;
}

function selectedConversionMode(): ConversionMode {
  const selected = modeInputs.find((input) => input.checked)?.value;
  if (selected === "fast_export" || selected === "quality") return selected;
  return "super_quick";
}

function savedThemeMode(): ThemeMode {
  return themeModeFromValue(localStorage.getItem(THEME_KEY) ?? "dark");
}

function themeModeFromValue(value: string): ThemeMode {
  return value === "light" ? "light" : "dark";
}

function applyTheme(theme: ThemeMode, shouldPersist = false) {
  document.documentElement.dataset.theme = theme;

  for (const input of themeInputs) {
    input.checked = input.value === theme;
  }

  if (shouldPersist) {
    localStorage.setItem(THEME_KEY, theme);
  }
}

function clamp(value: number, min: number, max: number) {
  return Math.min(max, Math.max(min, value));
}

function byId<T extends HTMLElement>(id: string): T {
  const element = document.getElementById(id);
  if (!element) throw new Error(`Missing element #${id}`);
  return element as T;
}
