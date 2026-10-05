"""
File: modules/ai_backend/inpaint/flux2_klein/test_memory.py

Purpose:
Unit tests for `memory.py`: the RAM/VRAM forecast and the pre-load guard built
on it, including the guard's placement advice and the `estimate` method that
reports both to the client.

Main responsibilities:
- verify the forecast treats denoise and decode as two separate peaks;
- verify the pipeline's transient host copy follows the LOADER rather than the
  `low_cpu_mem_usage` flag, so a single-file transformer `from_single_file` will
  materialize in full is forecast in full;
- verify each of the guard's refusals fires on the right shortfall and names a
  placement preset that would fit;
- verify `estimate` answers without loading anything;
- verify a marks reference adds exactly the region's token count to the denoise
  and reaches the guard;
- verify `text_attention_in_mask` adds its attention mask to the denoise device
  term only, and nothing under `whole_region`.

Notes:
`_weight_bytes` is patched on `components` and `memory_snapshot` on `hardware`:
those are the modules that define them, and `memory.py` reaches them through the
module for exactly that reason.
"""

from __future__ import annotations

import unittest
from unittest.mock import patch

from modules.ai_backend.inpaint import flux2_klein as svc
from modules.ai_backend.inpaint.flux2_klein import components, hardware
from modules.ai_backend.runtime.model_manager import LoadedModelManager

from ._test_fixtures import _PlacementFixture, _TempTreeCase


class MemoryGuardTests(_PlacementFixture):
    """A request is refused BEFORE the first byte when a phase cannot fit.

    A host-side shortfall is not an exception: the kernel OOM killer picks a
    victim among everything running, and on this project's reference host it
    closed the user's editor while the 9B transformer and the 8B encoder were
    being loaded side by side.
    """

    #: klein's real component sizes, so the tests assert on the figures a user
    #: actually sees in the message.
    TRANSFORMER_BYTES = 18_157_185_168
    TEXT_ENCODER_BYTES = 16_381_516_808
    VAE_BYTES = 168_120_878
    GIB = 1024**3

    def setUp(self) -> None:
        super().setUp()
        sizes = {
            self.paths["transformer_path"]: self.TRANSFORMER_BYTES,
            self.paths["text_encoder_path"]: self.TEXT_ENCODER_BYTES,
            self.paths["vae_path"]: self.VAE_BYTES,
        }
        # Both sizers are patched: the forecast asks `text_encoder_resident_bytes`
        # for the encoder and `_weight_bytes` for the other two, so patching only
        # one would leave the encoder at the fixture tree's real (tiny) size and
        # silently stop covering it. They return the same figure here — the
        # divergence between disk and resident bytes is covered by
        # `TextEncoderResidentBytesTests` in `test_components.py`.
        for name in ("_weight_bytes", "text_encoder_resident_bytes"):
            weights_patch = patch.object(
                components, name, lambda path, *_a, **_k: sizes.get(path, 0)
            )
            weights_patch.start()
            self.addCleanup(weights_patch.stop)

    def _with_memory(self, *, ram_free: float, vram_free: float) -> None:
        snapshot = {
            "ram_free": int(ram_free * self.GIB),
            "ram_total": int(64 * self.GIB),
            "vram_free": int(vram_free * self.GIB),
            "vram_total": int(32 * self.GIB),
        }
        memory_patch = patch.object(hardware, "memory_snapshot", lambda _device=None: snapshot)
        memory_patch.start()
        self.addCleanup(memory_patch.stop)

    def _guard(self, **overrides: object) -> None:
        normalized = svc.normalize_flux2_klein_params(self.params(**overrides))
        self.service._require_headroom_locked(normalized, 128, 128, "flux2_klein:test")

    def test_a_phase_short_of_host_memory_is_refused_before_anything_is_read(self) -> None:
        # 16 GiB of encoder on a host with 12 GiB free: phase 1 alone does not fit.
        self._with_memory(ram_free=12.0, vram_free=31.9)
        with self.assertRaises(RuntimeError) as caught:
            self._guard(placement="encoder_cpu", low_cpu_mem_usage=True)
        message = str(caught.exception)
        self.assertIn("оперативной памяти на этап «кодирование промпта»", message)
        self.assertIn("свободно 12.0 ГиБ", message)
        self.assertEqual(self.load_kwargs, {})
        self.assertIsNone(self.service._pipe)

    def test_a_phase_short_of_device_memory_is_refused_and_names_the_card(self) -> None:
        self._with_memory(ram_free=64.0, vram_free=8.0)
        with self.assertRaises(RuntimeError) as caught:
            self._guard(placement="encoder_cpu", low_cpu_mem_usage=True)
        message = str(caught.exception)
        self.assertIn("видеопамяти на cuda:0 на этап «денойз»", message)
        self.assertIn("свободно 8.0 ГиБ", message)
        self.assertEqual(self.load_kwargs, {})

    def test_the_refusal_names_the_settings_that_do_fit(self) -> None:
        # Enough host memory for the encoder, not enough to also copy the 9B
        # transformer back for the decode: the advice must name the one lever
        # that removes that copy rather than declaring the machine hopeless.
        self._with_memory(ram_free=18.0, vram_free=31.9)
        with self.assertRaises(RuntimeError) as caught:
            self._guard(placement="encoder_cpu", low_cpu_mem_usage=True)
        message = str(caught.exception)
        self.assertIn("Сейчас помещаются", message)
        self.assertIn("без выгрузки трансформера перед VAE", message)

    def test_nothing_fitting_says_so_instead_of_naming_a_preset(self) -> None:
        self._with_memory(ram_free=1.0, vram_free=1.0)
        with self.assertRaises(RuntimeError) as caught:
            self._guard(placement="encoder_cpu")
        self.assertIn("Ни один из встроенных профилей", str(caught.exception))

    def test_enough_memory_passes_the_guard(self) -> None:
        self._with_memory(ram_free=27.0, vram_free=31.9)
        # Both host-heavy levers off, so this exercises the guard rather than the
        # shipped defaults: a resident encoder AND a parked transformer put ~34
        # GiB in host memory at once, which is its own case below.
        self._guard(
            placement="encoder_cpu",
            low_cpu_mem_usage=True,
            unload_text_encoder_after_encode=True,
            unload_transformer_before_vae=False,
        )

    def test_a_resident_encoder_and_a_parked_transformer_share_the_host(self) -> None:
        # The one combination the reorder does NOT make cheaper: parking the 9B
        # transformer for the decode copies it back into host memory, where the
        # kept encoder already sits. They genuinely coexist, so this is a sum and
        # the guard must say so rather than hide it behind a maximum.
        normalized = svc.normalize_flux2_klein_params(
            self.params(
                placement="encoder_cpu",
                low_cpu_mem_usage=True,
                unload_text_encoder_after_encode=False,
                unload_transformer_before_vae=True,
            )
        )
        phases = svc.forecast_memory(normalized, 128, 128)["phases"]
        self.assertEqual(
            phases["decode"]["ram_bytes"], self.TEXT_ENCODER_BYTES + self.TRANSFORMER_BYTES
        )

    def test_unknown_free_memory_never_refuses(self) -> None:
        # `memory_snapshot` reports 0 when psutil or an accelerator is missing;
        # 0 means unknown, not empty, and must not gate a run.
        self._with_memory(ram_free=0.0, vram_free=0.0)
        self._guard(placement="full_gpu")

    def test_a_cached_prompt_and_a_resident_pipeline_are_not_gated(self) -> None:
        self._with_memory(ram_free=27.0, vram_free=31.9)
        settings: dict[str, object] = {"placement": "encoder_cpu", "low_cpu_mem_usage": True}
        self._encode(**settings)
        self._build("flux2_klein:test", **settings)
        # Nothing new is allocated on a double cache hit, so the guard must not
        # fire even if the machine has since run out.
        with patch.object(
            hardware,
            "memory_snapshot",
            lambda _device=None: {
                "ram_free": 1, "ram_total": 1, "vram_free": 1, "vram_total": 1
            },
        ):
            self._guard(**settings)

    def test_a_new_prompt_is_not_charged_for_memory_the_service_already_holds(self) -> None:
        # Measured on the reference host: after one run the pipeline is on the
        # card and the encoder is in host memory, so the free figures already
        # exclude both — and the guard refused the next prompt for 17.6 GiB of
        # VRAM that the very pipeline it was about to reuse was occupying.
        settings: dict[str, object] = {
            "placement": "encoder_cpu",
            "low_cpu_mem_usage": True,
            "unload_text_encoder_after_encode": False,
        }
        self._with_memory(ram_free=27.0, vram_free=31.9)
        self._encode(**settings)
        self._build("flux2_klein:test", **settings)
        # What the machine looks like now: our own 16 GiB of encoder and 17 GiB
        # of pipeline are gone from the free figures, because we are holding them.
        with patch.object(
            hardware,
            "memory_snapshot",
            lambda _device=None: {
                "ram_free": int(13.0 * self.GIB),
                "ram_total": int(64 * self.GIB),
                "vram_free": int(13.5 * self.GIB),
                "vram_total": int(32 * self.GIB),
            },
        ):
            self._guard(prompt="a different prompt", **settings)

    def test_memory_the_service_does_not_hold_is_still_charged(self) -> None:
        # The discount must not become a blanket exemption: with nothing resident
        # the same figures still refuse the run.
        self._with_memory(ram_free=13.0, vram_free=13.5)
        with self.assertRaises(RuntimeError):
            self._guard(
                placement="encoder_cpu",
                low_cpu_mem_usage=True,
                unload_text_encoder_after_encode=False,
            )

    def test_the_peak_is_the_maximum_of_the_phases_not_their_sum(self) -> None:
        normalized = svc.normalize_flux2_klein_params(
            self.params(placement="full_gpu", low_cpu_mem_usage=True)
        )
        forecast = svc.forecast_memory(normalized, 128, 128)
        phases = forecast["phases"]
        for side in ("vram_bytes", "ram_bytes"):
            with self.subTest(side=side):
                self.assertEqual(
                    forecast[side], max(phase[side] for phase in phases.values())
                )
        self.assertLess(
            forecast["vram_bytes"], sum(phase["vram_bytes"] for phase in phases.values())
        )

    def test_the_encoder_is_forecast_in_host_memory_in_every_placement(self) -> None:
        # It encodes on the host now, in every placement, so its cost is a RAM
        # cost everywhere. The encode phase's VRAM is only the already-placed
        # pipeline sitting idle on the card.
        for placement in svc.VALID_PLACEMENTS:
            with self.subTest(placement=placement):
                normalized = svc.normalize_flux2_klein_params(
                    self.params(placement=placement, low_cpu_mem_usage=True)
                )
                phases = svc.forecast_memory(normalized, 128, 128)["phases"]
                encode = phases["encode"]
                self.assertGreaterEqual(
                    encode["ram_bytes"],
                    self.TEXT_ENCODER_BYTES + svc.ENCODE_ACTIVATION_BYTES,
                )
                # Nothing but the already-placed pipeline sitting idle: the same
                # weights the denoise uses, without the denoise activations.
                activations = 64 * svc.ACTIVATION_BYTES_PER_LATENT_TOKEN
                self.assertEqual(
                    encode["vram_bytes"], phases["denoise"]["vram_bytes"] - activations
                )

    def test_the_two_host_peaks_never_overlap_off_the_offload_placements(self) -> None:
        # The reorder's whole point: the transformer's host copy is gone by the
        # time the 16 GiB encoder arrives, so the two are a maximum and not a sum.
        normalized = svc.normalize_flux2_klein_params(
            self.params(
                placement="encoder_cpu",
                low_cpu_mem_usage=False,
                unload_text_encoder_after_encode=False,
                unload_transformer_before_vae=False,
            )
        )
        forecast = svc.forecast_memory(normalized, 128, 128)
        pipeline_bytes = self.TRANSFORMER_BYTES + self.VAE_BYTES
        self.assertEqual(
            forecast["phases"]["encode"]["ram_bytes"],
            self.TEXT_ENCODER_BYTES + svc.ENCODE_ACTIVATION_BYTES,
        )
        self.assertEqual(
            forecast["phases"]["denoise"]["ram_bytes"],
            max(pipeline_bytes, self.TEXT_ENCODER_BYTES),
        )
        self.assertLess(forecast["ram_bytes"], pipeline_bytes + self.TEXT_ENCODER_BYTES)

    def test_an_offload_placement_keeps_the_pipeline_in_the_host_sum(self) -> None:
        # There the pipeline never leaves host memory, so the same maximum has to
        # degenerate into the sum it really is.
        normalized = svc.normalize_flux2_klein_params(
            self.params(
                placement="sequential_cpu_offload",
                unload_text_encoder_after_encode=False,
            )
        )
        phases = svc.forecast_memory(normalized, 128, 128)["phases"]
        pipeline_bytes = self.TRANSFORMER_BYTES + self.VAE_BYTES
        self.assertEqual(
            phases["encode"]["ram_bytes"],
            self.TEXT_ENCODER_BYTES + svc.ENCODE_ACTIVATION_BYTES + pipeline_bytes,
        )
        self.assertEqual(
            phases["denoise"]["ram_bytes"], pipeline_bytes + self.TEXT_ENCODER_BYTES
        )

    def test_the_denoise_phase_no_longer_carries_the_text_encoder(self) -> None:
        normalized = svc.normalize_flux2_klein_params(
            self.params(
                placement="full_gpu",
                low_cpu_mem_usage=True,
                unload_text_encoder_after_encode=True,
            )
        )
        phases = svc.forecast_memory(normalized, 128, 128)["phases"]
        # Exactly transformer + VAE + the per-token activations of a 128x128
        # region (8x8 latent tokens), with nothing of the encoder left in it.
        activations = 64 * svc.ACTIVATION_BYTES_PER_LATENT_TOKEN
        self.assertEqual(
            phases["denoise"]["vram_bytes"],
            self.TRANSFORMER_BYTES + self.VAE_BYTES + activations,
        )

    def test_a_resident_encoder_is_counted_in_the_later_phases(self) -> None:
        kept = svc.forecast_memory(
            svc.normalize_flux2_klein_params(
                self.params(
                    placement="full_gpu",
                    low_cpu_mem_usage=True,
                    unload_text_encoder_after_encode=False,
                )
            ),
            128,
            128,
        )
        released = svc.forecast_memory(
            svc.normalize_flux2_klein_params(
                self.params(
                    placement="full_gpu",
                    low_cpu_mem_usage=True,
                    unload_text_encoder_after_encode=True,
                )
            ),
            128,
            128,
        )
        # It is counted in HOST memory now: the encoder never goes on the card
        # under the new order, whatever the placement.
        self.assertEqual(
            kept["phases"]["denoise"]["ram_bytes"] - released["phases"]["denoise"]["ram_bytes"],
            self.TEXT_ENCODER_BYTES,
        )
        self.assertEqual(
            kept["phases"]["denoise"]["vram_bytes"],
            released["phases"]["denoise"]["vram_bytes"],
        )

    def test_fp8_halves_the_resident_encoder_but_not_the_encode_peak(self) -> None:
        def forecast(fp8: bool) -> dict[str, object]:
            return svc.forecast_memory(
                svc.normalize_flux2_klein_params(
                    self.params(
                        placement="full_gpu",
                        low_cpu_mem_usage=True,
                        unload_text_encoder_after_encode=False,
                        text_encoder_fp8=fp8,
                    )
                ),
                128,
                128,
            )

        plain, quantized = forecast(False), forecast(True)
        # The load peak is unchanged: the bf16 weights have to exist before they
        # can be quantized.
        self.assertEqual(
            plain["phases"]["encode"]["ram_bytes"], quantized["phases"]["encode"]["ram_bytes"]
        )
        self.assertEqual(
            plain["phases"]["denoise"]["ram_bytes"] - quantized["phases"]["denoise"]["ram_bytes"],
            self.TEXT_ENCODER_BYTES - self.TEXT_ENCODER_BYTES // 2,
        )

    def test_the_guard_and_the_forecast_are_the_same_arithmetic(self) -> None:
        # `estimate` is what the UI shows; the guard must refuse exactly what it
        # reports as not fitting, so both go through `forecast_memory`.
        self._with_memory(ram_free=12.0, vram_free=31.9)
        params = self.params(placement="encoder_cpu", low_cpu_mem_usage=True)
        answer = self.service.estimate(params=params, region_width=128, region_height=128)
        self.assertFalse(answer["fits"])
        normalized = svc.normalize_flux2_klein_params(params)
        forecast = svc.forecast_memory(normalized, 128, 128)
        self.assertEqual(answer["ram_bytes"], forecast["ram_bytes"])
        self.assertEqual(answer["vram_bytes"], forecast["vram_bytes"])
        self.assertEqual(answer["breakdown"], forecast["breakdown"])

    def test_parking_the_transformer_is_counted_as_host_memory(self) -> None:
        # It is a full 9B device->host copy, and that peak is what the OOM
        # killer sees.
        def decode_ram(parked: bool) -> int:
            normalized = svc.normalize_flux2_klein_params(
                self.params(
                    placement="encoder_cpu",
                    low_cpu_mem_usage=True,
                    unload_transformer_before_vae=parked,
                    # Isolated from the resident encoder, which has its own test.
                    unload_text_encoder_after_encode=True,
                )
            )
            return svc.forecast_memory(normalized, 128, 128)["phases"]["decode"]["ram_bytes"]

        self.assertEqual(decode_ram(False), 0)
        self.assertEqual(decode_ram(True), self.TRANSFORMER_BYTES)


class EstimateTests(_TempTreeCase):
    def setUp(self) -> None:
        super().setUp()
        memory_patch = patch.object(
            hardware,
            "memory_snapshot",
            lambda *_args, **_kwargs: {
                "vram_total": 12 << 30,
                "vram_free": 10 << 30,
                "ram_total": 32 << 30,
                "ram_free": 16 << 30,
            },
        )
        memory_patch.start()
        self.addCleanup(memory_patch.stop)
        self.service = svc.Flux2KleinInpaintService(LoadedModelManager())

    def test_the_breakdown_reports_every_phase_peak(self) -> None:
        out = self.service.estimate(
            params=self.params(placement="full_gpu"), region_width=512, region_height=512
        )
        breakdown = out["breakdown"]
        for key in ("peak_encode", "peak_denoise", "peak_decode"):
            with self.subTest(key=key):
                self.assertIn(key, breakdown)
        # The Rust side renders every breakdown entry it does not recognise as an
        # extra row, so the key set is part of the wire contract, not an internal
        # detail (`Flux2Estimate::breakdown` is a `Vec<(String, u64)>`).
        self.assertEqual(
            set(breakdown),
            {
                "transformer",
                "text_encoder",
                "vae",
                "activations",
                "peak_encode",
                "peak_denoise",
                "peak_decode",
            },
        )
        # The encode phase is the one whose cost is dominated by HOST memory now,
        # so its peak is the larger of the two sides rather than only its VRAM.
        phases = svc.forecast_memory(
            svc.normalize_flux2_klein_params(self.params(placement="full_gpu")), 512, 512
        )["phases"]
        self.assertEqual(
            breakdown["peak_encode"],
            max(phases["encode"]["vram_bytes"], phases["encode"]["ram_bytes"]),
        )
        # A run is a SEQUENCE of phases, so the answer is their maximum.
        self.assertEqual(
            out["vram_bytes"],
            max(phase["vram_bytes"] for phase in svc.forecast_memory(
                svc.normalize_flux2_klein_params(self.params(placement="full_gpu")), 512, 512
            )["phases"].values()),
        )

    def test_parking_the_transformer_lowers_the_decode_peak(self) -> None:
        with_park = self.service.estimate(
            params=self.params(placement="full_gpu", unload_transformer_before_vae=True),
            region_width=512,
            region_height=512,
        )
        without = self.service.estimate(
            params=self.params(placement="full_gpu", unload_transformer_before_vae=False),
            region_width=512,
            region_height=512,
        )
        self.assertLess(
            with_park["breakdown"]["peak_decode"], without["breakdown"]["peak_decode"]
        )

    def test_the_weight_terms_come_from_disk(self) -> None:
        out = self.service.estimate(params=self.params(), region_width=256, region_height=256)
        self.assertEqual(out["breakdown"]["transformer"], 4096)
        self.assertEqual(out["breakdown"]["text_encoder"], 2048)
        self.assertEqual(out["breakdown"]["vae"], 1024)

    def test_a_marks_reference_costs_the_region_tokens_again_in_the_denoise(self) -> None:
        # The reference is required to be the region's size, so it lengthens the
        # transformer's sequence by exactly the region's own token count — and
        # touches nothing else: no weight, no encode, no decode.
        normalized = svc.normalize_flux2_klein_params(self.params(placement="full_gpu"))
        plain = svc.forecast_memory(normalized, 256, 128)
        marked = svc.forecast_memory(normalized, 256, 128, with_reference=True)
        tokens = (256 // svc.REGION_SIZE_MULTIPLE) * (128 // svc.REGION_SIZE_MULTIPLE)
        extra = tokens * svc.ACTIVATION_BYTES_PER_LATENT_TOKEN
        self.assertEqual(
            marked["phases"]["denoise"]["vram_bytes"],
            plain["phases"]["denoise"]["vram_bytes"] + extra,
        )
        self.assertEqual(
            marked["breakdown"]["activations"], plain["breakdown"]["activations"] + extra
        )
        for phase in ("encode", "encode_standalone", "decode"):
            with self.subTest(phase=phase):
                self.assertEqual(marked["phases"][phase], plain["phases"][phase])
        self.assertEqual(
            marked["phases"]["denoise"]["ram_bytes"], plain["phases"]["denoise"]["ram_bytes"]
        )

    def test_text_attention_in_mask_adds_its_mask_to_the_denoise_device_only(self) -> None:
        # One additive (S, S) mask over the REAL joint sequence: the text padded
        # to `max_sequence_length`, the noisy tokens, the region's clean copy and
        # the reference — computed by the same formula the run allocates by.
        plain = svc.normalize_flux2_klein_params(self.params(placement="full_gpu"))
        masked = svc.normalize_flux2_klein_params(
            self.params(placement="full_gpu", text_attention_in_mask=True)
        )
        tokens = (256 // svc.REGION_SIZE_MULTIPLE) * (128 // svc.REGION_SIZE_MULTIPLE)
        for with_reference in (False, True):
            with self.subTest(with_reference=with_reference):
                base = svc.forecast_memory(plain, 256, 128, with_reference=with_reference)
                out = svc.forecast_memory(masked, 256, 128, with_reference=with_reference)
                reference = tokens if with_reference else 0
                expected = svc.text_attention_mask_bytes(512, tokens, reference, "bfloat16")
                sequence = 512 + 2 * tokens + reference
                self.assertEqual(expected, sequence * (-(-sequence // 8) * 8) * 2)
                self.assertEqual(
                    out["phases"]["denoise"]["vram_bytes"],
                    base["phases"]["denoise"]["vram_bytes"] + expected,
                )
                self.assertEqual(out["breakdown"]["attention_mask"], expected)
                self.assertNotIn("attention_mask", base["breakdown"])
                self.assertEqual(
                    out["phases"]["denoise"]["ram_bytes"], base["phases"]["denoise"]["ram_bytes"]
                )
                for phase in ("encode", "encode_standalone", "decode"):
                    self.assertEqual(out["phases"][phase], base["phases"][phase])

    def test_text_attention_in_mask_costs_nothing_under_whole_region(self) -> None:
        # Normalization forces the flag off there, so the forecast must not
        # reserve memory for a mask the run will never build.
        plain = svc.normalize_flux2_klein_params(self.params(whole_region=True))
        asked = svc.normalize_flux2_klein_params(
            self.params(whole_region=True, text_attention_in_mask=True)
        )
        self.assertEqual(
            svc.forecast_memory(asked, 256, 128), svc.forecast_memory(plain, 256, 128)
        )

    def test_without_an_encoder_the_encode_phases_cost_nothing(self) -> None:
        params = self.params()
        params.pop("text_encoder_path")
        forecast = svc.forecast_memory(svc.normalize_flux2_klein_params(params), 512, 512)
        for phase in ("encode", "encode_standalone"):
            with self.subTest(phase=phase):
                self.assertEqual(forecast["phases"][phase]["vram_bytes"], 0)
                self.assertEqual(forecast["phases"][phase]["ram_bytes"], 0)
        self.assertEqual(forecast["breakdown"]["text_encoder"], 0)
        self.assertEqual(forecast["breakdown"]["peak_encode"], 0)
        self.assertEqual(forecast["resident"]["text_encoder_host"], 0)
        # The denoise and the decode are untouched: they never needed the encoder.
        with_encoder = svc.forecast_memory(
            svc.normalize_flux2_klein_params(self.params()), 512, 512
        )
        for phase in ("denoise", "decode"):
            with self.subTest(phase=phase):
                self.assertEqual(
                    forecast["phases"][phase]["vram_bytes"],
                    with_encoder["phases"][phase]["vram_bytes"],
                )

    def test_the_estimate_is_lower_without_an_encoder(self) -> None:
        # One calculation feeds both the UI and the guard, so a machine that
        # cannot run an encode must be forecast — and gated — as one that will not.
        without = dict(self.params())
        without.pop("text_encoder_path")
        cheap = self.service.estimate(params=without, region_width=512, region_height=512)
        full = self.service.estimate(params=self.params(), region_width=512, region_height=512)
        self.assertLess(cheap["ram_bytes"], full["ram_bytes"])
        self.assertEqual(set(cheap["breakdown"]), set(full["breakdown"]))

    def test_an_invalid_region_is_refused_before_any_arithmetic(self) -> None:
        with self.assertRaises(ValueError):
            self.service.estimate(params=self.params(), region_width=100, region_height=256)

    def test_fits_is_false_when_the_forecast_exceeds_the_free_vram(self) -> None:
        with patch.object(
            hardware,
            "memory_snapshot",
            lambda *_args, **_kwargs: {
                "vram_total": 1 << 20,
                "vram_free": 1 << 10,
                "ram_total": 1 << 20,
                "ram_free": 1 << 20,
            },
        ):
            out = self.service.estimate(
                params=self.params(), region_width=512, region_height=512
            )
        self.assertFalse(out["fits"])

class VaeTilingForecastTests(_TempTreeCase):
    """The decode is forecast by what the VAE will DO, not by what was asked.

    `vae_tiling` only flips `use_tiling`; `AutoencoderKLFlux2._decode` enters the
    tiled path only when a latent side exceeds `tile_latent_min_size`, i.e. above
    1024 output px on the shipped klein VAE. Since the flag DEFAULTS to `True`,
    crediting it unconditionally under-forecast the decode of every region up to
    that side by a factor of four — in the one calculation that also gates the
    load, and whose failure mode is the kernel OOM killer.
    """

    #: No side reaches the fixture VAE's 1024 px threshold, which is true of every
    #: SQUARE region this service accepts (`MAX_REGION_PIXELS` is 1 MP).
    SMALL = (512, 512)
    #: An elongated region that does reach it: inside the 1 MP budget and the 8:1
    #: aspect limit, a multiple of 16, and 1152 > 1024.
    LARGE = (1152, 896)

    def _decode_vram(self, size: tuple[int, int], **overrides: object) -> int:
        normalized = svc.normalize_flux2_klein_params(self.params(**overrides))
        forecast = svc.forecast_memory(normalized, size[0], size[1])
        return forecast["phases"]["decode"]["vram_bytes"]

    def test_the_threshold_is_read_from_the_vae_config(self) -> None:
        self.assertEqual(svc.vae_tile_threshold_pixels(self.paths["vae_path"]), 1024)

    def test_an_unreadable_vae_config_forecasts_the_expensive_decode(self) -> None:
        # Rounding a memory need DOWN is the one error that ends in an OOM kill,
        # so an unknown threshold means "tiling will not help".
        (self.root / "vae" / "config.json").write_text("{}", encoding="utf-8")
        self.assertEqual(
            svc.vae_tile_threshold_pixels(self.paths["vae_path"]),
            svc.VAE_TILE_THRESHOLD_FALLBACK_PIXELS,
        )
        self.assertEqual(
            self._decode_vram(self.LARGE, vae_tiling=True),
            self._decode_vram(self.LARGE, vae_tiling=False),
        )

    def test_a_region_below_the_threshold_is_forecast_untiled(self) -> None:
        width, height = self.SMALL
        with_tiling = self._decode_vram(self.SMALL, vae_tiling=True, vae_slicing=False)
        without = self._decode_vram(self.SMALL, vae_tiling=False, vae_slicing=False)
        self.assertEqual(with_tiling, without)
        # And it is genuinely the untiled constant, not merely a matching pair:
        # halving the area must move the forecast by the untiled per-pixel cost.
        half = self._decode_vram((width, height // 2), vae_tiling=True, vae_slicing=False)
        self.assertEqual(
            with_tiling - half,
            width * (height - height // 2) * svc.VAE_DECODE_BYTES_PER_PIXEL,
        )

    def test_a_region_past_the_threshold_is_forecast_tiled(self) -> None:
        width, height = self.LARGE
        with_tiling = self._decode_vram(self.LARGE, vae_tiling=True, vae_slicing=False)
        without = self._decode_vram(self.LARGE, vae_tiling=False, vae_slicing=False)
        self.assertEqual(
            without - with_tiling,
            width
            * height
            * (svc.VAE_DECODE_BYTES_PER_PIXEL - svc.VAE_DECODE_TILED_BYTES_PER_PIXEL),
        )

    def test_slicing_alone_never_changes_the_forecast(self) -> None:
        # `decode` slices only when `z.shape[0] > 1`; this service decodes one
        # image, so slicing is inert here in every case (KG-008).
        for size in (self.SMALL, self.LARGE):
            with self.subTest(size=size):
                self.assertEqual(
                    self._decode_vram(size, vae_tiling=False, vae_slicing=True),
                    self._decode_vram(size, vae_tiling=False, vae_slicing=False),
                )


class SingleFileHostTransientTests(_TempTreeCase):
    """The pipeline's transient host copy follows the LOADER, not the flag.

    `low_cpu_mem_usage` is a REQUEST. accelerate honours it (a diffusers folder,
    and the VAE in every case); diffusers' `from_single_file` does not — it reads
    the whole checkpoint into host memory first. Forecasting zero there
    under-charged ~17 GiB on the one path most likely to end in an OOM kill, so
    the term is now credited only when `streaming.load_transformer_streaming`
    will really take the file.
    """

    single_file_transformer = True

    TRANSFORMER_BYTES = 18_157_185_168
    TEXT_ENCODER_BYTES = 16_381_516_808
    VAE_BYTES = 168_120_878

    def setUp(self) -> None:
        super().setUp()
        sizes = {
            self.paths["transformer_path"]: self.TRANSFORMER_BYTES,
            self.paths["text_encoder_path"]: self.TEXT_ENCODER_BYTES,
            self.paths["vae_path"]: self.VAE_BYTES,
        }
        # Both sizers, together: `forecast_memory` asks `text_encoder_resident_bytes`
        # for the encoder and `_weight_bytes` for the other two, so patching one
        # alone would drop the encoder out of the forecast under test.
        for name in ("_weight_bytes", "text_encoder_resident_bytes"):
            size_patch = patch.object(
                components, name, lambda path, _sizes=sizes: _sizes.get(str(path), 0)
            )
            size_patch.start()
            self.addCleanup(size_patch.stop)

    def _denoise_ram(self, **overrides: object) -> int:
        """The `denoise` host figure with the resident encoder taken out of the way.

        `run_ram` is the MAXIMUM of the transient pipeline copy and the resident
        encoder, so dropping the encoder after the encode is what leaves the
        transient term alone in the answer.
        """
        settings: dict[str, object] = {
            "placement": "encoder_cpu",
            "unload_text_encoder_after_encode": True,
        }
        settings.update(overrides)
        normalized = svc.normalize_flux2_klein_params(self.params(**settings))
        return svc.forecast_memory(normalized, 128, 128)["phases"]["denoise"]["ram_bytes"]

    def test_a_streamable_checkpoint_costs_no_transient_host_memory(self) -> None:
        # The tree's transformer is one plain `.safetensors` file, which is
        # exactly what `streaming_load_eligible` accepts.
        self.assertEqual(self._denoise_ram(low_cpu_mem_usage=True), 0)

    def test_a_checkpoint_streaming_cannot_take_is_charged_in_full(self) -> None:
        # One part of a sharded checkout: refused by name, so `from_single_file`
        # runs and the whole file lands in host memory.
        shard = self.root / "flux2-klein-00001-of-00002.safetensors"
        shard.write_bytes((self.root / "flux2-klein.safetensors").read_bytes())
        with patch.object(
            components,
            "_weight_bytes",
            lambda path, _size=self.TRANSFORMER_BYTES: _size if "flux2-klein" in str(path) else 0,
        ):
            charged = self._denoise_ram(low_cpu_mem_usage=True, transformer_path=str(shard))
        self.assertEqual(charged, self.TRANSFORMER_BYTES)

    def test_a_diffusers_folder_still_costs_nothing(self) -> None:
        # It goes through `from_pretrained` with a whole-model `device_map`, i.e.
        # accelerate's shard-by-shard path, which really does honour the flag.
        folder = self.root / "transformer"
        with patch.object(components, "_weight_bytes", lambda _path: 0):
            self.assertEqual(
                self._denoise_ram(low_cpu_mem_usage=True, transformer_path=str(folder)), 0
            )

    def test_without_low_cpu_mem_usage_the_whole_pipeline_is_charged(self) -> None:
        # Nothing changed on this path: no flag, no direct-to-VRAM loader, so the
        # transformer AND the VAE pass through host memory.
        self.assertEqual(
            self._denoise_ram(low_cpu_mem_usage=False),
            self.TRANSFORMER_BYTES + self.VAE_BYTES,
        )

    def test_the_corrected_term_reaches_the_pre_load_guard(self) -> None:
        # Not only the number on screen: `_require_memory_headroom` reads the very
        # same forecast, so a host that cannot hold the checkpoint is refused
        # BEFORE the first byte. Under the old flag-only term this figure was
        # zero and the phase was not even checked.
        shard = self.root / "flux2-klein-00001-of-00002.safetensors"
        shard.write_bytes((self.root / "flux2-klein.safetensors").read_bytes())
        normalized = svc.normalize_flux2_klein_params(
            self.params(
                placement="encoder_cpu",
                low_cpu_mem_usage=True,
                unload_text_encoder_after_encode=True,
                transformer_path=str(shard),
            )
        )
        with patch.object(
            components,
            "_weight_bytes",
            lambda path, _size=self.TRANSFORMER_BYTES: _size if "flux2-klein" in str(path) else 0,
        ):
            needed = svc.forecast_memory(normalized, 128, 128)["phases"]["denoise"]["ram_bytes"]
            self.assertEqual(needed, self.TRANSFORMER_BYTES)
            with patch.object(
                hardware,
                "memory_snapshot",
                lambda *_args, **_kwargs: {
                    "vram_total": 64 << 30,
                    "vram_free": 64 << 30,
                    "ram_total": 64 << 30,
                    "ram_free": needed,  # short by exactly the host reserve
                },
            ):
                with self.assertRaises(RuntimeError) as caught:
                    svc._require_memory_headroom(
                        normalized, 128, 128, "cuda:0", phases=("denoise",)
                    )
        self.assertIn("оперативной памяти", str(caught.exception))


if __name__ == "__main__":
    unittest.main()
