"""
File: modules/ai_backend/inpaint/flux3_klein/test_dit.py

Purpose:
Unit tests for `flux3_klein/dit.py` on a tiny randomly initialised model: the
forward's shape, dtype and determinism; the phantom token of an empty mode is
never attended; samples of one pack never see each other; the
`timesteps_ref` and image-token refusals; checkpoint key names; and the math
helpers (rope shapes, norm-preserving rotation, span-isolated attention).

Key helpers (also used by `test_loading.py`):
- `tiny_dit_config()`, `make_pack()`

Notes:
Real torch on the CPU; skipped where torch is absent. The equivalence with the
reference implementation was checked once with a scratch probe (bitwise equal
under the MATH SDPA backend) and is not a test: the reference is not in the
repository.
"""

from __future__ import annotations

import importlib.util
import unittest
from types import MappingProxyType

from .config import DitConfig

_HAS_TORCH = importlib.util.find_spec("torch") is not None

if _HAS_TORCH:
    import torch

    from . import dit as dit_mod
    from .packing import MODES, Packed


def tiny_dit_config() -> DitConfig:
    """The tiny DiT of the unit tests: hidden 64, 2 heads (head width 32 = 4 rope axes of 8)."""
    return DitConfig(
        hidden=64,
        heads=2,
        depth=1,
        depth_single=2,
        mlp_ratio=2.0,
        axes_dim=(8, 8, 8, 8),
        theta=10000.0,
        in_channels=MappingProxyType({"image": 16, "image_cond": 16, "text": 48}),
        image_rope_scale=0.5,
        flow_map=True,
    )


def make_pack(
    config: DitConfig,
    samples: list[dict[str, int]],
    *,
    dtype: torch.dtype,
    seed: int = 0,
    t: float = 0.6,
) -> Packed:
    """A random mode-major pack: `samples[s][mode]` is that sample's token count
    (0 = the mode is absent). Image tokens get timestep `t`, the others 0, as
    the reference sampler does. Data and timesteps are `dtype` (the reference
    packs bf16; a float32 model outside autocast needs float32 throughout)."""
    generator = torch.Generator().manual_seed(seed)
    data: dict[str, list[torch.Tensor]] = {m: [] for m in MODES}
    ids: dict[str, list[torch.Tensor]] = {m: [] for m in MODES}
    indices: dict[str, list[tuple[int, int]]] = {m: [] for m in MODES}
    for sample in samples:
        for m in MODES:
            count = sample.get(m, 0)
            start = sum(part.shape[0] for part in data[m])
            data[m].append(torch.randn(count, config.in_channels[m], generator=generator).to(dtype))
            ids[m].append(torch.randint(0, 8, (count, 4), generator=generator).float())
            indices[m].append((start, start + count))
    cat_data = {m: torch.cat(data[m]) for m in MODES}
    timesteps = {m: torch.zeros(cat_data[m].shape[0], dtype=dtype) for m in MODES}
    timesteps["image"].fill_(t)
    return Packed(data=cat_data, ids={m: torch.cat(ids[m]) for m in MODES}, timesteps=timesteps, mode_indices=indices)


def _model(dtype: torch.dtype, seed: int = 0) -> dit_mod.Flux3Dit:
    """A tiny `Flux3Dit` with weights large enough that every path matters."""
    torch.manual_seed(seed)
    model = dit_mod.Flux3Dit(tiny_dit_config())
    with torch.no_grad():
        for parameter in model.parameters():
            parameter.normal_(std=0.2)
    return model.to(dtype).eval()


@unittest.skipUnless(_HAS_TORCH, "torch is not installed")
class ForwardTests(unittest.TestCase):
    def test_shape_dtype_and_determinism(self) -> None:
        model = _model(torch.bfloat16)
        pack = make_pack(tiny_dit_config(), [{"image": 6, "image_cond": 4, "text": 5}], dtype=torch.bfloat16)
        with torch.inference_mode():
            first = model(pack, torch.tensor([0.3]))
            second = model(pack, torch.tensor([0.3]))
        self.assertEqual(tuple(first.shape), (6, 16))
        self.assertEqual(first.dtype, torch.bfloat16)
        self.assertTrue(torch.isfinite(first.float()).all())
        self.assertTrue(torch.equal(first, second))

    def test_the_target_time_changes_the_prediction(self) -> None:
        model = _model(torch.float32)
        pack = make_pack(tiny_dit_config(), [{"image": 6, "text": 5}], dtype=torch.float32)
        with torch.inference_mode():
            a = model(pack, torch.tensor([0.3]))
            b = model(pack, torch.tensor([0.1]))
        self.assertFalse(torch.allclose(a, b))

    def test_phantom_token_of_an_empty_mode_is_never_attended(self) -> None:
        # With no image_cond tokens the mode runs on one zero phantom token. Changing
        # every weight that only the phantom passes through must not move the output.
        model = _model(torch.float32)
        pack = make_pack(tiny_dit_config(), [{"image": 6, "image_cond": 0, "text": 5}], dtype=torch.float32)
        with torch.inference_mode():
            before = model(pack, torch.tensor([0.3]))
            for module in (
                model.emb_in["image_cond"],
                model.content_mode_blocks["image_cond"],
                model.early_stream_modulations["image_cond"],
                model.single_stream_modulations["image_cond"],
            ):
                for parameter in module.parameters():
                    parameter.normal_(std=1.0)
            after = model(pack, torch.tensor([0.3]))
        self.assertTrue(torch.equal(before, after))

    def test_text_tokens_are_attended(self) -> None:
        model = _model(torch.float32)
        pack = make_pack(tiny_dit_config(), [{"image": 6, "text": 5}], dtype=torch.float32)
        with torch.inference_mode():
            before = model(pack, torch.tensor([0.3]))
            pack.data["text"] = pack.data["text"] + 1.0
            after = model(pack, torch.tensor([0.3]))
        self.assertFalse(torch.allclose(before, after))

    def test_samples_of_one_pack_are_isolated(self) -> None:
        model = _model(torch.float32)
        first = {"image": 6, "image_cond": 4, "text": 5}
        second = {"image": 3, "image_cond": 0, "text": 7}
        both = make_pack(tiny_dit_config(), [first, second], dtype=torch.float32, seed=1)
        alone = Packed(
            data={m: both.data[m][: first[m]] for m in MODES},
            ids={m: both.ids[m][: first[m]] for m in MODES},
            timesteps={m: both.timesteps[m][: first[m]] for m in MODES},
            mode_indices={m: [(0, first[m])] for m in MODES},
        )
        with torch.inference_mode():
            together = model(both, torch.tensor([0.3, 0.3]))
            single = model(alone, torch.tensor([0.3]))
            # The second sample's content and target time must not reach the first.
            both.data["text"][first["text"] :] += 5.0
            changed = model(both, torch.tensor([0.3, 0.9]))
        self.assertEqual(tuple(together.shape), (9, 16))
        torch.testing.assert_close(together[:6], single, rtol=1e-4, atol=1e-4)
        torch.testing.assert_close(changed[:6], together[:6], rtol=1e-4, atol=1e-4)
        self.assertFalse(torch.allclose(changed[6:], together[6:]))

    def test_timesteps_ref_is_required_and_sized_per_sample(self) -> None:
        model = _model(torch.float32)
        pack = make_pack(tiny_dit_config(), [{"image": 4, "text": 3}], dtype=torch.float32)
        with self.assertRaises(ValueError):
            model(pack, None)  # type: ignore[arg-type]
        with self.assertRaises(ValueError):
            model(pack, torch.tensor([0.3, 0.2]))

    def test_a_pack_without_image_tokens_is_refused(self) -> None:
        model = _model(torch.float32)
        pack = make_pack(tiny_dit_config(), [{"image": 0, "text": 3}], dtype=torch.float32)
        with self.assertRaises(ValueError):
            model(pack, torch.tensor([0.3]))

    def test_parameter_names_are_the_checkpoint_keys(self) -> None:
        keys = set(_model(torch.float32).state_dict())
        for key in (
            "emb_in.image.weight",
            "emb_in.image_cond.weight",
            "txt_in.weight",
            "time_in.in_layer.weight",
            "ref_time_in.out_layer.weight",
            "early_stream_modulations.txt.lin.weight",
            "single_stream_modulations.image_cond.lin.weight",
            "content_mode_blocks.image.0.norm.query_norm.scale",
            "txt_mode_blocks.0.mlp_in.weight",
            "single_blocks.1.attn_out.weight",
            "final_layer.image.adaLN_modulation.1.weight",
            "final_layer.image.linear.weight",
        ):
            self.assertIn(key, keys)
        self.assertFalse(any(key.endswith(".bias") for key in keys))


@unittest.skipUnless(_HAS_TORCH, "torch is not installed")
class HelperTests(unittest.TestCase):
    def test_rope_embed_shape(self) -> None:
        ids = torch.rand(1, 5, 4)
        freqs = dit_mod.rope_embed(ids, (8, 8, 8, 8), 10000.0)
        self.assertEqual(tuple(freqs.shape), (1, 1, 5, 16, 2, 2))
        self.assertEqual(freqs.dtype, torch.float32)

    def test_rmsnorm_rope_rotation_preserves_the_normalized_length(self) -> None:
        x = torch.randn(1, 2, 5, 32)
        freqs = dit_mod.rope_embed(torch.rand(1, 5, 4) * 10, (8, 8, 8, 8), 10000.0)
        out = dit_mod.rmsnorm_rope(x, torch.ones(32), freqs)
        # RMS-normalized vectors have norm sqrt(D); a rotation keeps it.
        torch.testing.assert_close(out.norm(dim=-1), torch.full((1, 2, 5), 32**0.5), rtol=1e-4, atol=1e-4)

    def test_timestep_embedding_follows_the_timestep_dtype(self) -> None:
        emb = dit_mod.timestep_embedding(torch.tensor([0.5, 0.0], dtype=torch.bfloat16))
        self.assertEqual((tuple(emb.shape), emb.dtype), ((2, 256), torch.bfloat16))
        # t = 0: cos half is 1, sin half is 0.
        self.assertTrue(torch.equal(emb[1, :128], torch.ones(128, dtype=torch.bfloat16)))

    def test_packed_attention_isolates_spans_and_leaves_gaps_zero(self) -> None:
        generator = torch.Generator().manual_seed(2)
        q, k, v = (torch.randn(1, 2, 7, 8, generator=generator) for _ in range(3))
        out = dit_mod.packed_attention(q, k, v, [(0, 3), (3, 6)])
        self.assertTrue(torch.equal(out[6], torch.zeros(16)))
        k2, v2 = k.clone(), v.clone()
        k2[:, :, 3:6] += 1.0
        v2[:, :, 3:6] += 1.0
        moved = dit_mod.packed_attention(q, k2, v2, [(0, 3), (3, 6)])
        self.assertTrue(torch.equal(out[:3], moved[:3]))
        self.assertFalse(torch.equal(out[3:6], moved[3:6]))


if __name__ == "__main__":
    unittest.main()
