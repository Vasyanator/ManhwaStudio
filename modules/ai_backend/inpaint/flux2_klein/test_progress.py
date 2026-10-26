"""
File: modules/ai_backend/inpaint/flux2_klein/test_progress.py

Purpose:
Unit tests of `progress.LoadFrames`: the request's progress callback that turns
the large-model manager's disk-read frames (`Session.set_load_progress`) into
the wire's byte level on the CURRENT step frame — a `load` step, or a
`generate` step for a re-read inside the denoise. No torch, no manager: the
read frames are plain stand-ins with the fields the sink reads.
"""

from __future__ import annotations

import unittest
from dataclasses import dataclass
from typing import Any

from .progress import FILE_COMPONENT_TEXT_ENCODER, FILE_COMPONENT_TRANSFORMER, LoadFrames


@dataclass(frozen=True)
class _Read:
    component: str
    done_bytes: int
    total_bytes: int


class LoadFramesTests(unittest.TestCase):
    def setUp(self) -> None:
        self.sent: list[tuple[str, int, int, str, dict[str, Any]]] = []
        files = {"transformer": (FILE_COMPONENT_TRANSFORMER, "flux.gguf"), "text_encoder": (FILE_COMPONENT_TEXT_ENCODER, "text_encoder")}
        self.frames = LoadFrames(self._record, files)

    def _record(self, phase: str, step: int, total: int, label: str, **file_level: Any) -> None:
        self.sent.append((phase, step, total, label, file_level))

    def test_step_frames_pass_through_unchanged(self) -> None:
        self.frames("load", 3, 9, "Загрузка VAE")
        self.assertEqual(self.sent, [("load", 3, 9, "Загрузка VAE", {})])

    def test_a_read_rides_on_the_last_step_frame(self) -> None:
        self.frames("load", 7, 9, "Загрузка текстового энкодера")
        self.frames.sink(_Read("text_encoder", 5, 10))
        self.assertEqual(self.sent[-1], ("load", 7, 9, "Загрузка текстового энкодера", {"file_step": 5, "file_total": 10, "file_label": "text_encoder", "file_component": "text_encoder"}))

    def test_a_reread_during_the_denoise_keeps_the_generate_step(self) -> None:
        self.frames("load", 9, 9, "Промпт взят из кэша")
        self.frames("generate", 2, 8, "Генерация")
        self.frames.sink(_Read("transformer", 10, 10))
        phase, step, total, label, fields = self.sent[-1]
        self.assertEqual((phase, step, total, label), ("generate", 2, 8, "Генерация"))
        self.assertEqual((fields["file_component"], fields["file_label"]), ("transformer", "flux.gguf"))
        # A byte frame is not a step frame: the next read still rides on the generate step.
        self.frames.sink(_Read("transformer", 10, 10))
        self.assertEqual(self.sent[-1][:2], ("generate", 2))

    def test_nothing_is_sent_for_an_unnamed_component_an_empty_read_or_before_any_step(self) -> None:
        self.frames.sink(_Read("transformer", 1, 2))
        self.frames("load", 3, 9, "Загрузка VAE")
        self.frames.sink(_Read("vae", 1, 2))
        self.frames.sink(_Read("transformer", 0, 0))
        self.assertEqual(self.sent, [("load", 3, 9, "Загрузка VAE", {})])

    def test_the_sink_never_raises_but_a_step_frame_does(self) -> None:
        def gone(*_args: Any, **_kwargs: Any) -> None:
            raise RuntimeError("peer gone")

        frames = LoadFrames(gone, {"transformer": (FILE_COMPONENT_TRANSFORMER, "t")})
        with self.assertRaises(RuntimeError):
            frames("generate", 1, 4, "Генерация")
        frames.sink(_Read("transformer", 1, 2))


if __name__ == "__main__":
    unittest.main()
