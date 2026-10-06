"""Regenerate the committed Baberu OCR fixtures of `ms-onnx`.

Why this script exists
----------------------
The native Baberu OCR engine (`crates/ms-onnx/src/baberu_ocr/`) ports the
upstream reference `onnx_infer.py` of `genshiai-daichi/baberu-ocr` (Apache-2.0):
Pillow BICUBIC resize of the whole crop to 224x224, ImageNet normalization, and
a greedy decode loop with a repetition penalty and run caps. This script records
what Pillow and the reference loop produce, so the Rust port is parity-tested
against them instead of against hand-written expectations.

Outputs (all under `--out`, default `crates/ms-onnx/fixtures/baberu/`):

- `resize_cases.json` (always): RGB inputs generated from a documented
  xorshift32 PRNG (the Rust test reimplements it), resized with
  `Image.resize(size, Image.BICUBIC)`; each case stores the sha256 of the
  output RGB bytes. No model files are needed.
- `noncontent_ids.json` (with `--vocab`): the token ids (index + 4) of the
  vocab entries the reference does NOT count as "content" (category L*/N*,
  excluding `ーｰ〜~`), for the opt-in real-vocab test.
- `e2e_cases.json` + `sample_*.png` (with `--model-dir` and `--font`): small
  rendered bubble crops and the token ids / text the reference loop decodes on
  the CPU execution provider, for the opt-in real-weights test.

PRNG contract (xorshift32): `state` starts at the case seed (non-zero); each
step does `x ^= x << 13; x ^= x >> 17; x ^= x << 5` on u32 and yields the new
state; a pixel channel byte is `state >> 24`. Bytes are generated row-major,
RGB-interleaved.

Usage
-----
    ./venv/bin/python tools/make_baberu_fixtures.py \
        [--vocab <model>/tokenizer/vocab.json] \
        [--model-dir <model> --font <a CJK-capable .ttf/.ttc>]

`<model>` is a Baberu directory holding `onnx/vision_fp16.onnx`,
`onnx/decoder_prefill_int8.onnx`, `onnx/decoder_step_int8.onnx` and
`tokenizer/vocab.json` (the app downloads it into
`ManhwaStudio_AI_Models/side_models/BaberuOCR/`). Output is deterministic for a
given Pillow / onnxruntime version (both are recorded in the files). This is an
offline developer tool; runtime code never imports it.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import logging
import unicodedata
from pathlib import Path

from PIL import Image, ImageDraw, ImageFont

log = logging.getLogger("make_baberu_fixtures")

REPO_ROOT = Path(__file__).resolve().parents[1]
DEFAULT_OUT = REPO_ROOT / "crates" / "ms-onnx" / "fixtures" / "baberu"

# (name, seed, in_w, in_h, out_w, out_h): identity, up/down scaling, degenerate
# 1xN / Nx1 inputs, one-axis-only passes, non-square non-224 targets, and the pass order.
RESIZE_CASES: list[tuple[str, int, int, int, int, int]] = [
    ("identity_37x23", 1, 37, 23, 37, 23),
    ("up_13x9_to_224", 2, 13, 9, 224, 224),
    ("down_300x120_to_224", 3, 300, 120, 224, 224),
    ("column_1x40_to_224", 4, 1, 40, 224, 224),
    ("row_50x1_to_224", 5, 50, 1, 224, 224),
    ("wide_640x48_to_224", 6, 640, 48, 224, 224),
    ("tall_5x700_to_224", 7, 5, 700, 224, 224),
    ("vertical_only_224x500", 8, 224, 500, 224, 224),
    ("horizontal_only_500x224", 9, 500, 224, 224, 224),
    ("odd_31x17_to_11x29", 10, 31, 17, 11, 29),
    ("to_1x1_from_9x7", 11, 9, 7, 1, 1),
    # Pillow's `Image.resize` runs the vertical pass FIRST when height > width * 100 and
    # the height shrinks (`tall_5x700_to_224` above, too); these pin both sides of it.
    ("aspect_100_exact_3x300_horizontal_first", 12, 3, 300, 224, 224),
    ("aspect_over_100_3x301_vertical_first", 13, 3, 301, 224, 224),
    ("aspect_over_100_growing_2x300_horizontal_first", 14, 2, 300, 224, 400),
]

# Characters the reference never counts as content even though they are L*.
NON_CONTENT_CHARS = "ーｰ〜~"

# Reference decode settings (upstream `onnx_infer.py` defaults).
MAX_NEW_TOKENS = 256
REPETITION_PENALTY = 1.2
MAX_CONTENT_RUN = 12
MAX_SYMBOL_RUN = 16
BOS, EOS = 1, 2


def xorshift32_bytes(seed: int, count: int) -> bytes:
    """Return `count` bytes from the documented xorshift32 stream for `seed`."""
    if seed == 0:
        raise ValueError("xorshift32 seed must be non-zero")
    state = seed & 0xFFFFFFFF
    out = bytearray(count)
    for i in range(count):
        state ^= (state << 13) & 0xFFFFFFFF
        state ^= state >> 17
        state ^= (state << 5) & 0xFFFFFFFF
        out[i] = state >> 24
    return bytes(out)


def build_resize_cases() -> list[dict[str, object]]:
    """Resize every PRNG input with Pillow BICUBIC and hash the output bytes."""
    cases: list[dict[str, object]] = []
    for name, seed, in_w, in_h, out_w, out_h in RESIZE_CASES:
        src = Image.frombytes("RGB", (in_w, in_h), xorshift32_bytes(seed, in_w * in_h * 3))
        dst = src.resize((out_w, out_h), Image.BICUBIC)
        cases.append(
            {
                "name": name,
                "seed": seed,
                "in_w": in_w,
                "in_h": in_h,
                "out_w": out_w,
                "out_h": out_h,
                "sha256": hashlib.sha256(dst.tobytes()).hexdigest(),
            }
        )
    return cases


def is_content(ch: str) -> bool:
    """The reference content rule (`onnx_infer.py` `Vocab.content_ids`)."""
    return len(ch) == 1 and ch not in NON_CONTENT_CHARS and unicodedata.category(ch)[0] in "LN"


def build_noncontent(vocab_path: Path) -> dict[str, object]:
    """List the ids of every non-content vocab entry."""
    charset = json.loads(vocab_path.read_text(encoding="utf-8"))
    ids = [i + 4 for i, ch in enumerate(charset) if not is_content(ch)]
    return {
        "vocab_sha256": hashlib.sha256(vocab_path.read_bytes()).hexdigest(),
        "vocab_len": len(charset),
        "python_unicode_version": unicodedata.unidata_version,
        "noncontent_ids": ids,
    }


def render_bubble(font_path: Path, lines: list[str], vertical: bool, size: int) -> Image.Image:
    """Render black text on a white greyscale canvas (one column per line when vertical)."""
    font = ImageFont.truetype(str(font_path), size)
    if vertical:
        width = len(lines) * (size + 12) + 40
        height = max(len(col) for col in lines) * (size + 4) + 40
        img = Image.new("L", (width, height), 255)
        draw = ImageDraw.Draw(img)
        for ci, col in enumerate(lines):
            x = width - 20 - (ci + 1) * (size + 12)
            for ri, ch in enumerate(col):
                draw.text((x, 20 + ri * (size + 4)), ch, font=font, fill=0)
    else:
        width = max(int(font.getlength(line)) for line in lines) + 40
        height = len(lines) * (size + 10) + 40
        img = Image.new("L", (width, height), 255)
        draw = ImageDraw.Draw(img)
        for i, line in enumerate(lines):
            draw.text((20, 20 + i * (size + 10)), line, font=font, fill=0)
    return img


class ReferenceOcr:
    """Verbatim port of the upstream `onnx_infer.py` loop (CPU EP, ORT_ENABLE_ALL)."""

    def __init__(self, model_dir: Path) -> None:
        import numpy as np  # noqa: PLC0415 - only needed with --model-dir
        import onnxruntime as ort  # noqa: PLC0415

        self.np = np
        self.ort_version = ort.__version__
        opts = ort.SessionOptions()
        opts.graph_optimization_level = ort.GraphOptimizationLevel.ORT_ENABLE_ALL

        def make(path: Path) -> object:
            return ort.InferenceSession(str(path), opts, providers=["CPUExecutionProvider"])

        onnx_dir = model_dir / "onnx"
        self.vision = make(onnx_dir / "vision_fp16.onnx")
        self.prefill = make(onnx_dir / "decoder_prefill_int8.onnx")
        self.step = make(onnx_dir / "decoder_step_int8.onnx")
        charset = json.loads((model_dir / "tokenizer" / "vocab.json").read_text(encoding="utf-8"))
        self.charset = charset
        self.content_ids = {i + 4 for i, ch in enumerate(charset) if is_content(ch)}

    def __call__(self, image: Image.Image) -> list[int]:
        """Return the generated token ids (without BOS/EOS) for one crop."""
        np = self.np
        mean = np.array([0.485, 0.456, 0.406], np.float32)
        std = np.array([0.229, 0.224, 0.225], np.float32)
        img = image.convert("RGB").resize((224, 224), Image.BICUBIC)
        pixel_values = ((np.asarray(img, np.float32) / 255.0 - mean) / std).transpose(2, 0, 1)[None]
        past_names = [f"past_k{i}" for i in range(6)] + [f"past_v{i}" for i in range(6)]
        vis = self.vision.run(["vision_embeds"], {"pixel_values": pixel_values})[0]
        out = self.prefill.run(None, {"vision_embeds": vis, "input_ids": np.array([[BOS]], np.int64)})
        logits, present = out[0][0, -1].astype(np.float64), out[1:]
        seq, toks, pos = [BOS], [], vis.shape[1] + 1
        for _ in range(MAX_NEW_TOKENS):
            for tid in set(seq):
                s = logits[tid]
                logits[tid] = s * REPETITION_PENALTY if s < 0 else s / REPETITION_PENALTY
            last = toks[-1] if toks else 0
            cap = MAX_CONTENT_RUN if last in self.content_ids else MAX_SYMBOL_RUN if last > 3 else 0
            if cap:
                run = 0
                for t in reversed(toks):
                    if t == last:
                        run += 1
                    else:
                        break
                if run >= cap:
                    logits[last] = -np.inf
            nxt = int(np.argmax(logits))
            if nxt == EOS:
                break
            toks.append(nxt)
            seq.append(nxt)
            if len(toks) >= MAX_NEW_TOKENS:
                break
            feed = {"input_ids": np.array([[nxt]], np.int64), "position_ids": np.array([[pos]], np.int64)}
            feed.update(dict(zip(past_names, present)))
            out = self.step.run(None, feed)
            logits, present = out[0][0, -1].astype(np.float64), out[1:]
            pos += 1
        return toks

    def decode(self, ids: list[int]) -> str:
        """Map ids >= 4 to their vocab characters."""
        return "".join(self.charset[i - 4] for i in ids if i >= 4)


# (file stem, lines, vertical, font size): small crops in the model's three languages.
E2E_SAMPLES: list[tuple[str, list[str], bool, int]] = [
    ("sample_ja_vertical", ["私がこの手で", "啓太を守る！！"], True, 24),
    ("sample_ja_horizontal", ["なんだと…？"], False, 28),
    ("sample_en", ["I WON'T LET YOU", "GET AWAY WITH THIS!"], False, 24),
    ("sample_zh", ["我一定会保护你的！"], False, 26),
]


def build_e2e(model_dir: Path, font_path: Path, out_dir: Path) -> dict[str, object]:
    """Render the samples, save them as PNG and record the reference decode."""
    ref = ReferenceOcr(model_dir)
    cases: list[dict[str, object]] = []
    for stem, lines, vertical, size in E2E_SAMPLES:
        img = render_bubble(font_path, lines, vertical, size)
        png_path = out_dir / f"{stem}.png"
        img.save(png_path, optimize=True)
        # Decode the image read back from disk, exactly what the Rust test sees.
        with Image.open(png_path) as reread:
            ids = ref(reread.copy())
        text = ref.decode(ids)
        log.info("e2e %s -> %r (%d tokens)", stem, text, len(ids))
        cases.append({"image": png_path.name, "token_ids": ids, "text": text})
    return {"onnxruntime_version": ref.ort_version, "provider": "CPUExecutionProvider", "cases": cases}


def write_json(path: Path, payload: dict[str, object]) -> None:
    """Write `payload` as stable, human-diffable UTF-8 JSON."""
    path.write_text(json.dumps(payload, ensure_ascii=False, indent=1) + "\n", encoding="utf-8")
    log.info("wrote %s", path)


def main() -> None:
    """Parse arguments and regenerate the requested fixture files."""
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--vocab", type=Path, help="tokenizer/vocab.json of the Baberu model")
    parser.add_argument("--model-dir", type=Path, help="Baberu directory with onnx/ and tokenizer/")
    parser.add_argument("--font", type=Path, help="CJK-capable font used to render the e2e samples")
    parser.add_argument("--out", type=Path, default=DEFAULT_OUT, help="output directory")
    args = parser.parse_args()
    logging.basicConfig(level=logging.INFO, format="%(levelname)s %(message)s")

    if args.model_dir is not None and args.font is None:
        parser.error("--model-dir needs --font to render the e2e samples")

    out_dir: Path = args.out
    out_dir.mkdir(parents=True, exist_ok=True)
    import PIL  # noqa: PLC0415 - version stamp only

    write_json(
        out_dir / "resize_cases.json",
        {"pillow_version": PIL.__version__, "filter": "BICUBIC", "prng": "xorshift32", "cases": build_resize_cases()},
    )
    if args.vocab is not None:
        write_json(out_dir / "noncontent_ids.json", build_noncontent(args.vocab))
    if args.model_dir is not None:
        write_json(out_dir / "e2e_cases.json", build_e2e(args.model_dir, args.font, out_dir))


if __name__ == "__main__":
    main()
