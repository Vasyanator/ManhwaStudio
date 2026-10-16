"""
File: modules/ai_backend/runtime/test_encoder_truncation.py

Purpose:
Unit tests for `runtime/encoder_truncation.py`: the `max(indices) + 1`
arithmetic and its refusals, and — with real torch and transformers — the
measured fact the `+ 1` rests on: `hidden_states[num_hidden_layers]` is the
NORMED last state, so a model truncated to exactly `max(indices)` layers hands
back a different tensor at that index, while one truncated to the rule's count
reproduces the deep model's states bit for bit.

Notes:
`RealQwen3TruncationTests` builds tiny randomly initialised `Qwen3Model`s on
the CPU (no download, no file, no GPU) and skips itself when torch or
transformers is missing.
"""

from __future__ import annotations

import importlib.util
import unittest

from . import encoder_truncation


class KeepLayersArithmeticTests(unittest.TestCase):
    def test_keeps_one_layer_past_the_deepest_requested_state(self) -> None:
        self.assertEqual(encoder_truncation.keep_layers_for_hidden_states((9, 18, 27)), 28)
        self.assertEqual(encoder_truncation.keep_layers_for_hidden_states([4, 32, 8]), 33)
        self.assertEqual(encoder_truncation.keep_layers_for_hidden_states((0,)), 1)

    def test_an_empty_request_is_refused(self) -> None:
        with self.assertRaises(ValueError):
            encoder_truncation.keep_layers_for_hidden_states(())

    def test_a_negative_index_is_refused(self) -> None:
        with self.assertRaises(ValueError) as caught:
            encoder_truncation.keep_layers_for_hidden_states((3, -1))
        self.assertIn("-1", str(caught.exception))


_HAVE_STACK = (
    importlib.util.find_spec("torch") is not None
    and importlib.util.find_spec("transformers") is not None
)


@unittest.skipUnless(_HAVE_STACK, "torch and transformers are required")
class RealQwen3TruncationTests(unittest.TestCase):
    INDICES = (1, 3)

    def _hidden_states(self, deep_state: dict[str, object], layers: int) -> tuple[object, ...]:
        """Hidden states of a `layers`-deep Qwen3 carrying the deep model's first weights."""
        import torch
        from transformers import Qwen3Config, Qwen3Model

        config = Qwen3Config(
            vocab_size=64,
            hidden_size=32,
            intermediate_size=64,
            num_hidden_layers=layers,
            num_attention_heads=4,
            num_key_value_heads=2,
            head_dim=8,
            max_position_embeddings=64,
        )
        model = Qwen3Model(config).eval()
        own = model.state_dict()
        model.load_state_dict({key: deep_state[key] for key in own})
        input_ids = torch.arange(1, 9).unsqueeze(0)
        with torch.no_grad():
            return model(input_ids=input_ids, output_hidden_states=True).hidden_states

    def test_the_rule_reproduces_the_deep_states_and_one_layer_less_does_not(self) -> None:
        import torch
        from transformers import Qwen3Config, Qwen3Model

        torch.manual_seed(0)
        deep_layers = 6
        deep = Qwen3Model(
            Qwen3Config(
                vocab_size=64,
                hidden_size=32,
                intermediate_size=64,
                num_hidden_layers=deep_layers,
                num_attention_heads=4,
                num_key_value_heads=2,
                head_dim=8,
                max_position_embeddings=64,
            )
        ).eval()
        deep_state = deep.state_dict()
        reference = self._hidden_states(deep_state, deep_layers)

        keep = encoder_truncation.keep_layers_for_hidden_states(self.INDICES)
        self.assertEqual(keep, 4)
        kept = self._hidden_states(deep_state, keep)
        for index in self.INDICES:
            with self.subTest(index=index):
                self.assertTrue(torch.equal(kept[index], reference[index]))

        # One layer fewer: the last requested index is now the NORMED final state.
        shaved = self._hidden_states(deep_state, max(self.INDICES))
        self.assertFalse(torch.allclose(shaved[max(self.INDICES)], reference[max(self.INDICES)]))


if __name__ == "__main__":
    unittest.main()
