#!/usr/bin/env python3
"""Persistent Kokoro ONNX bridge for Paper2Voice.

Rust owns the app, PDF pipeline, progress events, and FFmpeg merge. This helper
keeps Kokoro loaded once and accepts newline-delimited JSON synthesis requests.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
import traceback
from pathlib import Path


MAX_SEGMENT_CHARS = 520


def send(message: dict) -> None:
    print(json.dumps(message), flush=True)


def load_kokoro(model_path: str, voices_path: str):
    try:
        import soundfile as sf
        from kokoro_onnx import Kokoro
    except Exception as exc:
        send(
            {
                "ok": False,
                "error": (
                    "Missing Kokoro Python dependencies. Run: "
                    "uv pip install --python .venv/bin/python -r scripts/requirements-kokoro.txt. "
                    f"Import error: {exc}"
                ),
            }
        )
        return None, None

    try:
        kokoro = Kokoro(model_path, voices_path)
    except Exception as exc:
        send({"ok": False, "error": f"Failed to load Kokoro model files: {exc}"})
        return None, None

    send({"ok": True, "error": None})
    return kokoro, sf


def synthesize(kokoro, sf, request: dict) -> dict:
    output_path = Path(request["output_path"])
    output_path.parent.mkdir(parents=True, exist_ok=True)

    import numpy as np

    sample_parts = []
    sample_rate = None

    for segment in split_text(request["text"], MAX_SEGMENT_CHARS):
        samples, segment_rate = kokoro.create(
            segment,
            voice=request["voice"],
            speed=float(request["speed"]),
            lang="en-us",
        )

        if sample_rate is None:
            sample_rate = segment_rate
        elif sample_rate != segment_rate:
            raise RuntimeError("Kokoro returned inconsistent sample rates")

        sample_parts.append(samples)

    if not sample_parts:
        sample_rate = 24000
        audio = np.zeros(1, dtype=np.float32)
    elif len(sample_parts) == 1:
        audio = sample_parts[0]
    else:
        pause = np.zeros(int(sample_rate * 0.08), dtype=np.float32)
        joined = []
        for index, samples in enumerate(sample_parts):
            if index > 0:
                joined.append(pause)
            joined.append(samples)
        audio = np.concatenate(joined)

    sf.write(output_path, audio, sample_rate)
    return {"ok": True, "error": None}


def split_text(text: str, max_chars: int) -> list[str]:
    text = re.sub(r"\s+", " ", text).strip()
    if not text:
        return []

    segments: list[str] = []
    current = ""

    for sentence in re.split(r"(?<=[.!?])\s+", text):
        sentence = sentence.strip()
        if not sentence:
            continue

        if len(sentence) > max_chars:
            if current:
                segments.append(current)
                current = ""
            segments.extend(split_long_sentence(sentence, max_chars))
            continue

        candidate = f"{current} {sentence}".strip()
        if len(candidate) <= max_chars:
            current = candidate
        else:
            if current:
                segments.append(current)
            current = sentence

    if current:
        segments.append(current)

    return segments


def split_long_sentence(text: str, max_chars: int) -> list[str]:
    chunks: list[str] = []
    current = ""

    for word in text.split():
        candidate = f"{current} {word}".strip()
        if len(candidate) <= max_chars:
            current = candidate
        else:
            if current:
                chunks.append(current)
            current = word

    if current:
        chunks.append(current)

    return chunks


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--model", required=True)
    parser.add_argument("--voices", required=True)
    args = parser.parse_args()

    kokoro, sf = load_kokoro(args.model, args.voices)
    if kokoro is None:
        return 1

    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue

        try:
            request = json.loads(line)
            if request.get("output_path") == "__shutdown__":
                return 0

            send(synthesize(kokoro, sf, request))
        except Exception as exc:
            traceback.print_exc(file=sys.stderr)
            send({"ok": False, "error": str(exc)})

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
