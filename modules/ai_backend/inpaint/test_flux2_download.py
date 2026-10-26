"""
File: modules/ai_backend/inpaint/test_flux2_download.py

Purpose:
Unit tests for the FLUX.2 klein model acquisition module — the Python half of
`dev-docs/flux2_model_download.md`.

Main responsibilities:
- pin the 9B `.check` / `.start` answers, the `DownloadTarget` they hand the
  shared machinery and the 9B token rules value for value (characterization
  written before the 4B download was retired);
- verify variant resolution: an absent or empty `variant` field means `9b`, it
  resolves case-insensitively, an unknown name and a non-string are typed
  refusals rather than a silent fallback, and a variant without an uncensored
  encoder refuses the toggle with no downgrade to the official encoder;
- verify the retired `4b` is refused by `resolve_variant`, `.check` and
  `.start` (with or without `uncensored`, any case) with the message that the
  application downloads it, before any network call;
- verify a token-less 9B check short-circuits to `no_token` without a network
  call and `.check` echoes the variant it answered for;
- verify the token routing: the request's token reaches every gated 9B
  repository unchanged (valid token -> `ok`, stale token -> `invalid_token`),
  never leaks into a reported message, and the transfer sends it as a bearer
  header;
- verify the plan builder resolves PREFIXES against a listing and, in doing so,
  excludes the duplicate 18 GB root transformer, the four GGUF quants, the sample
  images, the README and `.gitattributes`, and takes the tokenizer from the
  OFFICIAL repository in both toggle states;
- verify the two encoder rules are EXCLUSIVE: the toggle REPLACES the official
  encoder with the uncensored one instead of adding it, so a plan is ~34.7 GB
  either way, everything else is byte-identical between the two states, and an
  encoder already on disk from an earlier run is neither re-fetched nor removed;
- verify a prefix that resolves to nothing is an error naming the prefix and the
  repository, so a re-sharded repo cannot silently produce half a model;
- verify missing-only filtering: presence means the ANNOUNCED SIZE, so a file at
  the wrong length (short or long) is planned again — which is what repairs a
  truncated file an earlier run published — while a file whose listing announced
  no size falls back to the non-empty rule;
- verify the `state` taxonomy against the exception types `huggingface_hub`
  0.35.1 really raises, including that a 401 on a gated repo is `invalid_token`
  even though it arrives as a `GatedRepoError`;
- verify the progress throttle emits on both gates together and that the last
  frame of a run always reports the completed byte count;
- verify cancellation stops the transfer at a chunk boundary and leaves the files
  already published intact;
- verify resume: a parked partial is claimed and continued with `Range`, its
  progress frames start at the resumed offset instead of jumping backwards, a
  server that IGNORES the range rewrites the file instead of appending (the
  double-length corruption case), a 416 discards and refetches, leftovers longer
  than the announced size are dropped and refetched, a cancellation parks the
  bytes for a second call to finish, a body that ends cleanly but short is
  RESUMED (not discarded) after one backoff wait, and the free-space guard
  subtracts what is already staged;
- verify a body that keeps ending short is never published: the retry budget
  runs out with both lengths in the message, the bytes stay parked, and the same
  file is planned again afterwards; a file whose listing announced no size is
  still published;
- verify a retry is reported as an ordinary progress frame whose `label`
  announces it, and that a cancel during the backoff wait is `DownloadCanceled`;
- verify the free-space guard refuses before the first byte with both numbers.

Notes:
The generic machinery these tests drive lives in `hf_gated_download.py` (plan,
access states, transfer, throttle, free space), so its patch points are set on
THAT module (`hgd.probe_repo_access`, `hgd.fetch_listings`, `hgd.free_bytes`,
...); the FLUX.2 seams (`fd.model_root`) stay on `flux2_download`.
Nothing here touches the network. The listings are fakes, and the tests that
exercise the real streaming path replace the shared primitive's two seams —
`model_download._default_get` and `model_download._default_sleep` — for the
duration, so `download_to_path` and `download_bearer_to_path` run for real
against a fake transport and a clock that never waits.
"""

from __future__ import annotations

import os
import tempfile
import unittest
from pathlib import Path
from typing import Any, Callable, Iterator

from modules.ai_backend.engines import model_download as md
from modules.ai_backend.inpaint import flux2_download as fd
from modules.ai_backend.inpaint import hf_gated_download as hgd

OFFICIAL = fd.OFFICIAL_REPO_9B
UNCENSORED = fd.UNCENSORED_REPO_9B
V9B = fd.VARIANT_9B

#: A trimmed but structurally faithful copy of the two real listings (measured
#: against the hub): the shard names, the duplicate root transformer, the four
#: GGUF quants and the decoy files are all present, so an exclusion that stops
#: working shows up here.
FAKE_OFFICIAL: dict[str, int] = {
    ".gitattributes": 1580,
    "LICENSE.md": 18158,
    "README.md": 10404,
    "editing.jpg": 2506178,
    "flux-2-klein-9b.safetensors": 18157185168,
    "model_index.json": 446,
    "scheduler/scheduler_config.json": 486,
    "text_encoder/config.json": 1538,
    "text_encoder/generation_config.json": 214,
    "text_encoder/model-00001-of-00004.safetensors": 4902257696,
    "text_encoder/model-00002-of-00004.safetensors": 4915960368,
    "text_encoder/model-00003-of-00004.safetensors": 4983068496,
    "text_encoder/model-00004-of-00004.safetensors": 1580230264,
    "text_encoder/model.safetensors.index.json": 32914,
    "tokenizer/added_tokens.json": 707,
    "tokenizer/chat_template.jinja": 4168,
    "tokenizer/merges.txt": 1671853,
    "tokenizer/special_tokens_map.json": 613,
    "tokenizer/tokenizer.json": 11422654,
    "tokenizer/tokenizer_config.json": 5404,
    "tokenizer/vocab.json": 2776833,
    "transformer/config.json": 542,
    "transformer/diffusion_pytorch_model-00001-of-00002.safetensors": 9801069272,
    "transformer/diffusion_pytorch_model-00002-of-00002.safetensors": 8356121608,
    "transformer/diffusion_pytorch_model.safetensors.index.json": 24875,
    "vae/config.json": 821,
    "vae/diffusion_pytorch_model.safetensors": 168120878,
}

FAKE_UNCENSORED: dict[str, int] = {
    ".gitattributes": 394,
    "README.md": 16523,
    "chat_template.jinja": 4256,
    "config.json": 1664,
    "flux2-klein-9b-uncensored-f16.gguf": 16388043744,
    "flux2-klein-9b-uncensored-q4_k_m.gguf": 5027783648,
    "flux2-klein-9b-uncensored-q6_k.gguf": 6725899232,
    "flux2-klein-9b-uncensored-q8_0.gguf": 8709518304,
    "generation_config.json": 226,
    "model.safetensors": 16381516808,
    "tokenizer.json": 11422650,
    "tokenizer_config.json": 393,
}

FAKE_LISTINGS = {OFFICIAL: FAKE_OFFICIAL, UNCENSORED: FAKE_UNCENSORED}


class PlanBuilderTests(unittest.TestCase):
    """`resolve_plan` against a fake listing — the contract's §1 table."""

    def _labels(self, *, uncensored: bool, root: str = "/models") -> list[str]:
        return [item.label for item in fd.resolve_plan(FAKE_LISTINGS, variant=V9B, uncensored=uncensored, root=root)]

    def test_duplicate_root_transformer_is_excluded(self) -> None:
        # The 18.157 GB single file is the SAME transformer as `transformer/`;
        # taking it would lose the layered offload the engine depends on.
        labels = self._labels(uncensored=False)
        self.assertNotIn("flux-2-klein-9b.safetensors", labels)
        self.assertIn("transformer/diffusion_pytorch_model-00001-of-00002.safetensors", labels)
        self.assertIn("transformer/diffusion_pytorch_model-00002-of-00002.safetensors", labels)
        self.assertIn("transformer/config.json", labels)

    def test_gguf_quants_and_decoys_are_excluded(self) -> None:
        labels = self._labels(uncensored=True)
        self.assertFalse([label for label in labels if label.endswith(".gguf")])
        self.assertFalse([label for label in labels if label.endswith(".jpg")])
        self.assertFalse([label for label in labels if label.endswith("README.md")])
        self.assertFalse([label for label in labels if label.endswith(".gitattributes")])
        # LICENSE.md is an explicit manifest entry and must survive the README rule.
        self.assertIn("LICENSE.md", labels)

    def test_tokenizer_always_comes_from_the_official_repo(self) -> None:
        for uncensored in (False, True):
            plan = fd.resolve_plan(FAKE_LISTINGS, variant=V9B, uncensored=uncensored, root="/models")
            tokenizer = [item for item in plan if item.label.startswith("tokenizer/")]
            self.assertEqual(len(tokenizer), 7, uncensored)
            for item in tokenizer:
                self.assertEqual(item.repo, OFFICIAL, uncensored)
            # The uncensored repo's own tokenizer files land in the encoder
            # directory, never in `tokenizer/`.
            self.assertEqual(
                {item.source for item in tokenizer},
                {path for path in FAKE_OFFICIAL if path.startswith("tokenizer/")},
            )

    def test_the_toggle_selects_one_encoder_and_never_adds_a_second(self) -> None:
        # THE point of the toggle: the two encoder rules are EXCLUSIVE. A plan
        # that carried both would be ~51 GB instead of ~34.7 GB and would make the
        # user pay for 16 GB of weights the pipeline will not load.
        off = self._labels(uncensored=False)
        on = self._labels(uncensored=True)

        official_encoder = [label for label in off if label.startswith("text_encoder/")]
        self.assertIn("text_encoder/model-00001-of-00004.safetensors", official_encoder)
        self.assertFalse([label for label in off if label.startswith("text_encoder_uncensored/")])

        self.assertFalse(
            [label for label in on if label.startswith("text_encoder/")],
            "the official encoder must be REMOVED from the plan, not joined by the uncensored one",
        )
        self.assertIn("text_encoder_uncensored/model.safetensors", on)

    def test_everything_but_the_encoder_is_identical_in_both_states(self) -> None:
        # The transformer, tokenizer, VAE and configs come from the official repo
        # either way — which is why both repositories are still access-checked
        # when the toggle is on.
        off = {label for label in self._labels(uncensored=False) if not label.startswith("text_encoder")}
        on = {label for label in self._labels(uncensored=True) if not label.startswith("text_encoder")}
        self.assertEqual(off, on)
        self.assertEqual(fd.required_repos(V9B, True), (OFFICIAL, UNCENSORED))
        self.assertEqual(fd.required_repos(V9B, False), (OFFICIAL,))

    def test_a_plan_is_never_two_encoders_wide(self) -> None:
        off = fd.resolve_plan(FAKE_LISTINGS, variant=V9B, uncensored=False, root="/models")
        on = fd.resolve_plan(FAKE_LISTINGS, variant=V9B, uncensored=True, root="/models")
        # Both plans are ~34.7 GB: one encoder each, never ~51 GB. The numbers
        # are the ones the LIVE repositories resolve to (the fake listings above
        # carry the real sizes), so a drift in either repo shows up here.
        self.assertEqual(sum(item.size for item in off), 34722790808)
        self.assertEqual(sum(item.size for item in on), 34734185315)
        self.assertEqual(len(off), 23)
        self.assertEqual(len(on), 22)

    def test_an_already_downloaded_other_encoder_is_left_alone(self) -> None:
        # Two runs with different settings legitimately leave both encoder
        # directories on disk. The unselected one is never re-fetched, and
        # nothing here deletes it: 16 GB the user paid for does not vanish
        # because a checkbox moved.
        with tempfile.TemporaryDirectory() as tmp:
            official = Path(tmp) / "text_encoder" / "model-00001-of-00004.safetensors"
            official.parent.mkdir(parents=True, exist_ok=True)
            official.write_bytes(b"already downloaded")

            plan = fd.resolve_plan(FAKE_LISTINGS, variant=V9B, uncensored=True, root=tmp)
            self.assertFalse([item for item in plan if item.label.startswith("text_encoder/")])
            self.assertTrue(official.is_file())

    def test_plan_totals_match_the_contract_table(self) -> None:
        plan = fd.resolve_plan(FAKE_LISTINGS, variant=V9B, uncensored=False, root="/models")
        by_prefix: dict[str, int] = {}
        for item in plan:
            head = item.label.split("/")[0] if "/" in item.label else item.label
            by_prefix[head] = by_prefix.get(head, 0) + item.size
        self.assertEqual(by_prefix["transformer"], 18157216297)
        self.assertEqual(by_prefix["text_encoder"], 16381551490)
        self.assertEqual(by_prefix["vae"], 168121699)
        self.assertEqual(by_prefix["tokenizer"], 15882232)

    def test_destinations_are_absolute_under_the_root(self) -> None:
        plan = fd.resolve_plan(FAKE_LISTINGS, variant=V9B, uncensored=False, root=os.path.join(os.sep, "m"))
        for item in plan:
            self.assertEqual(Path(item.dest), Path(os.sep, "m", *item.label.split("/")))

    def test_prefix_resolving_to_nothing_is_an_error(self) -> None:
        # A repository that was re-sharded must not silently produce half a
        # model: the message names the prefix AND the repository.
        stripped = {key: value for key, value in FAKE_OFFICIAL.items() if not key.startswith("transformer/")}
        with self.assertRaises(RuntimeError) as caught:
            fd.resolve_plan({OFFICIAL: stripped, UNCENSORED: FAKE_UNCENSORED}, variant=V9B, uncensored=False, root="/m")
        message = str(caught.exception)
        self.assertIn("transformer/", message)
        self.assertIn(OFFICIAL, message)

    def test_uncensored_prefix_resolving_to_only_excluded_files_is_an_error(self) -> None:
        only_gguf = {key: value for key, value in FAKE_UNCENSORED.items() if key.endswith(".gguf")}
        with self.assertRaises(RuntimeError):
            fd.resolve_plan({OFFICIAL: FAKE_OFFICIAL, UNCENSORED: only_gguf}, variant=V9B, uncensored=True, root="/m")

    def test_missing_listing_is_an_error(self) -> None:
        with self.assertRaises(RuntimeError):
            fd.resolve_plan({OFFICIAL: FAKE_OFFICIAL}, variant=V9B, uncensored=True, root="/m")


class VariantResolutionTests(unittest.TestCase):
    """`resolve_variant` / `require_uncensored_supported`."""

    def test_an_absent_variant_field_means_9b(self) -> None:
        # The wire default, pinned: a client built before the field existed sends
        # nothing and must keep downloading exactly what it used to.
        self.assertIs(fd.resolve_variant(None), fd.VARIANT_9B)
        self.assertIs(fd.DEFAULT_VARIANT, fd.VARIANT_9B)

    def test_an_empty_variant_field_also_means_9b(self) -> None:
        # A client that serialises its default as "" carries no information; the
        # contract's default applies. An UNKNOWN name is a different case below.
        self.assertIs(fd.resolve_variant(""), fd.VARIANT_9B)
        self.assertIs(fd.resolve_variant("   "), fd.VARIANT_9B)

    def test_the_variant_resolves_case_insensitively(self) -> None:
        for value in ("9b", "9B", " 9b "):
            self.assertIs(fd.resolve_variant(value), V9B, value)
        self.assertEqual(set(fd.VARIANTS), {"9b"})

    def test_an_unknown_variant_is_a_typed_refusal(self) -> None:
        # Never a silent fallback to 9B: that would fetch 34.7 GB the user did
        # not ask for into a directory they did not name.
        with self.assertRaises(ValueError) as caught:
            fd.resolve_variant("9")
        message = str(caught.exception)
        self.assertIn("9", message)
        self.assertIn("«9b»", message)
        self.assertNotIn("«4b»", message)

    def test_a_non_string_variant_is_a_typed_refusal(self) -> None:
        for value in (9, 4.0, True, ["4b"]):
            with self.assertRaises(ValueError):
                fd.resolve_variant(value)

    def test_the_uncensored_encoder_is_refused_for_a_variant_without_one(self) -> None:
        # No table entry lacks an uncensored encoder today; the guard is pinned
        # on a synthetic one so it cannot rot into a silent downgrade.
        bare = fd.ModelVariant(key="xb", repo="example/xb", dir_name="XB", uncensored_repo=None)
        with self.assertRaises(ValueError) as caught:
            fd.require_uncensored_supported(bare, True)
        message = str(caught.exception)
        self.assertIn("XB", message)
        self.assertIn("не поддерживается", message)
        # And no downgrade: the refusal points at the toggle and at 9B.
        self.assertIn("«9b»", message)
        fd.require_uncensored_supported(bare, False)
        self.assertFalse(bare.supports_uncensored)

    def test_the_uncensored_encoder_is_allowed_for_9b(self) -> None:
        fd.require_uncensored_supported(V9B, True)
        self.assertTrue(V9B.supports_uncensored)

    def test_the_9b_destination_directory(self) -> None:
        self.assertEqual(V9B.dir_name, "FLUX.2-klein-9B")
        self.assertEqual(fd.model_root(V9B).name, "FLUX.2-klein-9B")


class NineBAnswerCharacterizationTests(unittest.TestCase):
    """The 9B `.check` / `.start` answers and token rules, pinned value for value.

    Written against the module as it stood before the 4B download was retired,
    so that retiring it provably changed nothing a 9B request can observe: the
    answer dictionaries, the `DownloadTarget` handed to the shared machinery,
    the per-repository token and the no-token refusal.
    """

    TOKEN = "hf_" + "c" * 34

    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self._saved = (fd.model_root, hgd.probe_repo_access, hgd.fetch_listings, hgd.download)
        fd.model_root = lambda variant: Path(self._tmp.name) / variant.dir_name  # type: ignore[assignment]
        hgd.probe_repo_access = lambda repo, token: (hgd.STATE_OK, "")  # type: ignore[assignment]
        hgd.fetch_listings = lambda repos, token: {r: FAKE_LISTINGS[r] for r in repos}  # type: ignore[assignment]

    def tearDown(self) -> None:
        fd.model_root, hgd.probe_repo_access, hgd.fetch_listings, hgd.download = self._saved  # type: ignore[assignment]
        self._tmp.cleanup()

    def _root(self) -> Path:
        return Path(self._tmp.name) / "FLUX.2-klein-9B"

    def test_the_check_answer_with_the_official_encoder(self) -> None:
        expected = {
            "variant": "9b",
            "repos": {OFFICIAL: {"state": "ok", "message": ""}},
            "plan": {"total_bytes": 34722790808, "missing_bytes": 34722790808, "missing_files": 23},
            "plan_error": "",
        }
        for variant in (None, "", "9b", " 9B "):
            with self.subTest(variant=variant):
                self.assertEqual(fd.check_access(self.TOKEN, uncensored=False, variant=variant), expected)

    def test_the_check_answer_with_the_uncensored_encoder(self) -> None:
        expected = {
            "variant": "9b",
            "repos": {
                OFFICIAL: {"state": "ok", "message": ""},
                UNCENSORED: {"state": "ok", "message": ""},
            },
            "plan": {"total_bytes": 34734185315, "missing_bytes": 34734185315, "missing_files": 22},
            "plan_error": "",
        }
        self.assertEqual(fd.check_access(self.TOKEN, uncensored=True, variant="9b"), expected)

    def test_a_tokenless_check_is_no_token_without_the_network(self) -> None:
        def explode(*args: Any, **kwargs: Any) -> Any:
            raise AssertionError("a gated repo must not be probed without a token")

        hgd.probe_repo_access = explode  # type: ignore[assignment]
        hgd.fetch_listings = explode  # type: ignore[assignment]
        answer = fd.check_access("", uncensored=True)
        self.assertEqual(answer["variant"], "9b")
        self.assertEqual(
            {repo: state["state"] for repo, state in answer["repos"].items()},
            {OFFICIAL: hgd.STATE_NO_TOKEN, UNCENSORED: hgd.STATE_NO_TOKEN},
        )
        self.assertIsNone(answer["plan"])

    def test_the_start_answer_and_the_target_it_hands_the_machinery(self) -> None:
        seen: list[tuple[str, hgd.DownloadTarget]] = []

        def fake_download(token: str, target: hgd.DownloadTarget, **kwargs: Any) -> dict[str, int]:
            seen.append((token, target))
            return {"downloaded_bytes": 7, "skipped_files": 3}

        hgd.download = fake_download  # type: ignore[assignment]
        root = self._root()
        for uncensored, encoder in ((False, "text_encoder"), (True, "text_encoder_uncensored")):
            with self.subTest(uncensored=uncensored):
                answer = fd.download(self.TOKEN, uncensored=uncensored, variant="9b")
                self.assertEqual(
                    answer,
                    {
                        "paths": {
                            "transformer": str(root / "transformer"),
                            "text_encoder": str(root / encoder),
                            "vae": str(root / "vae"),
                        },
                        "downloaded_bytes": 7,
                        "skipped_files": 3,
                    },
                )
                token, target = seen[-1]
                self.assertEqual(token, self.TOKEN)
                self.assertEqual(target.label, "FLUX.2 klein")
                self.assertEqual(target.root, root)
                self.assertEqual(target.no_token_message, fd.NO_TOKEN_MESSAGE)
                repos = (hgd.RepoSpec(OFFICIAL),)
                if uncensored:
                    repos += (hgd.RepoSpec(UNCENSORED),)
                self.assertEqual(target.repos, repos)
                encoder_rule = (
                    hgd.ManifestEntry(UNCENSORED, "", "text_encoder_uncensored")
                    if uncensored
                    else hgd.ManifestEntry(OFFICIAL, "text_encoder/", "text_encoder")
                )
                self.assertEqual(
                    target.manifest,
                    (
                        hgd.ManifestEntry(OFFICIAL, "transformer/", "transformer"),
                        hgd.ManifestEntry(OFFICIAL, "tokenizer/", "tokenizer"),
                        hgd.ManifestEntry(OFFICIAL, "vae/", "vae"),
                        hgd.ManifestEntry(OFFICIAL, "scheduler/scheduler_config.json", "scheduler/scheduler_config.json"),
                        hgd.ManifestEntry(OFFICIAL, "model_index.json", "model_index.json"),
                        hgd.ManifestEntry(OFFICIAL, "LICENSE.md", "LICENSE.md"),
                        encoder_rule,
                    ),
                )

    def test_a_tokenless_start_is_refused_with_the_no_token_message(self) -> None:
        def explode(*args: Any, **kwargs: Any) -> Any:
            raise AssertionError("a gated download must not reach the network without a token")

        hgd.fetch_listings = explode  # type: ignore[assignment]
        hgd.download = self._saved[3]  # type: ignore[assignment]
        with self.assertRaises(ValueError) as caught:
            fd.download("", uncensored=False)
        self.assertEqual(
            str(caught.exception),
            "Не указан токен Hugging Face. Оба репозитория FLUX.2 klein закрыты "
            "условиями, поэтому загрузка без токена невозможна.",
        )


class RetiredVariantTests(unittest.TestCase):
    """`variant="4b"` is refused: the application's model catalog downloads 4B."""

    MESSAGE = (
        "FLUX.2 klein 4B скачивается самим приложением (каталог моделей), "
        "а не бэкендом. Обновите ManhwaStudio."
    )

    def setUp(self) -> None:
        self._saved = (hgd.probe_repo_access, hgd.fetch_listings, hgd.download)

        def explode(*args: Any, **kwargs: Any) -> Any:
            raise AssertionError("a retired variant must not touch the network")

        hgd.probe_repo_access = explode  # type: ignore[assignment]
        hgd.fetch_listings = explode  # type: ignore[assignment]
        hgd.download = explode  # type: ignore[assignment]

    def tearDown(self) -> None:
        hgd.probe_repo_access, hgd.fetch_listings, hgd.download = self._saved  # type: ignore[assignment]

    def test_resolve_variant_refuses_4b_in_any_case(self) -> None:
        for value in ("4b", "4B", " 4b "):
            with self.subTest(value=value):
                with self.assertRaises(ValueError) as caught:
                    fd.resolve_variant(value)
                self.assertEqual(str(caught.exception), self.MESSAGE)

    def test_check_refuses_4b_with_the_message(self) -> None:
        for token, uncensored in (("", False), ("t", False), ("t", True)):
            with self.subTest(token=token, uncensored=uncensored):
                with self.assertRaises(ValueError) as caught:
                    fd.check_access(token, uncensored=uncensored, variant="4b")
                self.assertEqual(str(caught.exception), self.MESSAGE)

    def test_start_refuses_4b_with_the_message(self) -> None:
        for token, uncensored in (("", False), ("t", False), ("t", True)):
            with self.subTest(token=token, uncensored=uncensored):
                with self.assertRaises(ValueError) as caught:
                    fd.download(token, uncensored=uncensored, variant="4b")
                self.assertEqual(str(caught.exception), self.MESSAGE)

    def test_4b_is_retired_not_offered(self) -> None:
        self.assertNotIn("4b", fd.VARIANTS)
        self.assertEqual(set(fd.RETIRED_VARIANTS), {"4b"})


class TokenlessAccessTests(unittest.TestCase):
    """An empty token on the gated 9B request short-circuits to `no_token`."""

    def setUp(self) -> None:
        self._probe = hgd.probe_repo_access
        self._listings = hgd.fetch_listings
        self._root = fd.model_root
        self._tmp = tempfile.TemporaryDirectory()
        fd.model_root = lambda variant: Path(self._tmp.name)  # type: ignore[assignment]

    def tearDown(self) -> None:
        hgd.probe_repo_access = self._probe  # type: ignore[assignment]
        hgd.fetch_listings = self._listings  # type: ignore[assignment]
        fd.model_root = self._root  # type: ignore[assignment]
        self._tmp.cleanup()

    def test_an_empty_token_is_still_no_token_for_9b(self) -> None:
        def explode(*args: Any, **kwargs: Any) -> Any:
            raise AssertionError("a gated repo must not be probed without a token")

        hgd.probe_repo_access = explode  # type: ignore[assignment]
        hgd.fetch_listings = explode  # type: ignore[assignment]

        answer = fd.check_access("", uncensored=False)
        self.assertEqual(answer["variant"], "9b")
        self.assertEqual(answer["repos"][OFFICIAL]["state"], hgd.STATE_NO_TOKEN)

    def test_the_answer_echoes_the_variant_it_was_computed_for(self) -> None:
        hgd.probe_repo_access = lambda repo, token: (hgd.STATE_OK, "")  # type: ignore[assignment]
        hgd.fetch_listings = lambda repos, token: {r: FAKE_LISTINGS[r] for r in repos}  # type: ignore[assignment]

        self.assertEqual(fd.check_access("t", uncensored=False)["variant"], "9b")
        self.assertEqual(fd.check_access("t", uncensored=False, variant="9B")["variant"], "9b")

    def test_check_refuses_an_unknown_variant(self) -> None:
        def explode(*args: Any, **kwargs: Any) -> Any:
            raise AssertionError("a malformed request must not touch the network")

        hgd.probe_repo_access = explode  # type: ignore[assignment]
        hgd.fetch_listings = explode  # type: ignore[assignment]

        with self.assertRaises(ValueError):
            fd.check_access("t", uncensored=False, variant="2b")

    def test_download_needs_a_token_for_9b_and_refuses_an_unknown_variant(self) -> None:
        def explode(*args: Any, **kwargs: Any) -> Any:
            raise AssertionError("a refused request must not touch the network")

        hgd.fetch_listings = explode  # type: ignore[assignment]

        # Refused for the token itself, before any listing is attempted.
        with self.assertRaises(ValueError):
            fd.download("", uncensored=False)
        with self.assertRaises(ValueError):
            fd.download("t", uncensored=False, variant="nope")


class _Sibling:
    """One entry of a fake `model_info().siblings` list."""

    def __init__(self, rfilename: str, size: int) -> None:
        self.rfilename = rfilename
        self.size = size


class _Info:
    """Fake `model_info()` result carrying only the siblings the module reads."""

    def __init__(self, listing: dict[str, int]) -> None:
        self.siblings = [_Sibling(name, size) for name, size in sorted(listing.items())]


def _token_recording_api(
    seen: list[tuple[str, str, str | None]],
    *,
    valid_token: str | None,
    listings: dict[str, dict[str, int]],
) -> Any:
    """Build a fake `HfApi` class that records the token every call carried.

    It answers the way the real hub does for a GATED repository: 401 to no
    token and to a wrong one. `valid_token` is the only token accepted; `None`
    means no token is valid anywhere.
    """

    def answer(call: str, repo: str, token: str | None) -> None:
        seen.append((call, repo, token))
        if token is None or token != valid_token:
            raise _http_error(401, "gated")

    class _Api:
        def __init__(self, token: str | None = None) -> None:
            self._token = token

        def auth_check(self, repo: str) -> None:
            answer("auth", repo, self._token)

        def model_info(self, repo: str, files_metadata: bool = False) -> _Info:
            answer("info", repo, self._token)
            return _Info(listings[repo])

    return _Api


class _FakeHfApi:
    """`huggingface_hub.HfApi` patch installing a fake class for the duration."""

    def __init__(self, api: Any) -> None:
        self._api = api
        self._saved: Any = None

    def __enter__(self) -> Any:
        import huggingface_hub

        self._saved = huggingface_hub.HfApi
        huggingface_hub.HfApi = self._api  # type: ignore[assignment]
        return self._api

    def __exit__(self, *exc_info: Any) -> None:
        import huggingface_hub

        huggingface_hub.HfApi = self._saved  # type: ignore[assignment]


class ListingDigestTests(unittest.TestCase):
    """LFS digests travel from the hub listing into the plan and the transfer."""

    SHA = "ef" * 32

    def test_repo_listing_keeps_the_lfs_digest_and_the_plan_carries_it(self) -> None:
        import types as _types

        listing = dict(FAKE_OFFICIAL)
        planned = fd.resolve_plan({OFFICIAL: listing}, variant=V9B, uncensored=False, root="/m")
        shard = next(item.source for item in planned if item.source.endswith(".safetensors"))

        class _Api:
            def __init__(self, token: str | None = None) -> None:
                del token

            def model_info(self, repo: str, files_metadata: bool = False) -> Any:
                siblings = []
                for name, size in sorted(listing.items()):
                    sib = _Sibling(name, size)
                    sib.lfs = _types.SimpleNamespace(sha256=self_sha) if name == shard else None
                    siblings.append(sib)
                return _types.SimpleNamespace(siblings=siblings)

        self_sha = self.SHA
        with _FakeHfApi(_Api):
            got = hgd.repo_listing(OFFICIAL, "")

        self.assertEqual(got[shard], hgd.ListedFile(listing[shard], self.SHA))
        self.assertTrue(all(value.sha256 is None for path, value in got.items() if path != shard))
        plan = fd.resolve_plan({OFFICIAL: got}, variant=V9B, uncensored=False, root="/m")
        by_source = {item.source: item for item in plan}
        self.assertEqual(by_source[shard].sha256, self.SHA)

    def test_the_transfer_gets_the_planned_digest(self) -> None:
        item = hgd.PlannedFile(OFFICIAL, "a.safetensors", "/m/a.safetensors", "a.safetensors", 10, self.SHA)
        seen: dict[str, Any] = {}

        def fake_transfer(url: str, dest: str, **kwargs: Any) -> bool:
            seen.update(kwargs)
            return True

        saved = (hgd.download_bearer_to_path, hgd._file_url)
        hgd.download_bearer_to_path = fake_transfer  # type: ignore[assignment]
        hgd._file_url = lambda _item: "https://hf/a"  # type: ignore[assignment]
        try:
            hgd._fetch_one(item, token="", on_chunk=lambda *_: None, on_retry=lambda _e: None, should_cancel=None, label=fd.LABEL)
        finally:
            hgd.download_bearer_to_path, hgd._file_url = saved  # type: ignore[assignment]

        self.assertEqual(seen["expected_sha256"], self.SHA)
        self.assertEqual(seen["expected_size"], 10)


class TokenRoutingTests(unittest.TestCase):
    """The request's token reaches every (gated) repository, unchanged and never leaked.

    `probe_repo_access` and `repo_listing` run FOR REAL here against a fake
    `HfApi`; only the transport is a stand-in.
    """

    #: A token shaped like a real one, never a valid one for the fake hub.
    STALE = "hf_" + "e" * 34

    def setUp(self) -> None:
        # An empty destination keeps the plan totals independent of whatever the
        # developer running the suite happens to have downloaded.
        self._tmp = tempfile.TemporaryDirectory()
        self._root = fd.model_root
        fd.model_root = lambda variant: Path(self._tmp.name) / variant.dir_name  # type: ignore[assignment]

    def tearDown(self) -> None:
        fd.model_root = self._root  # type: ignore[assignment]
        self._tmp.cleanup()

    def test_a_valid_token_still_reaches_the_gated_9b_repository(self) -> None:
        good = "hf_" + "g" * 34
        seen: list[tuple[str, str, str | None]] = []
        api = _token_recording_api(seen, valid_token=good, listings=FAKE_LISTINGS)

        with _FakeHfApi(api):
            answer = fd.check_access(good, uncensored=True)

        for repo in (OFFICIAL, UNCENSORED):
            self.assertEqual(answer["repos"][repo]["state"], hgd.STATE_OK)
        self.assertIsNotNone(answer["plan"])
        self.assertEqual(answer["plan_error"], "")
        # Every call to a gated repository carried the token, unchanged.
        self.assertEqual({token for _call, _repo, token in seen}, {good})
        self.assertEqual({repo for _call, repo, _tok in seen}, {OFFICIAL, UNCENSORED})

    def test_an_invalid_token_is_still_invalid_token_for_9b(self) -> None:
        seen: list[tuple[str, str, str | None]] = []
        api = _token_recording_api(seen, valid_token=None, listings=FAKE_LISTINGS)

        with _FakeHfApi(api):
            answer = fd.check_access(self.STALE, uncensored=False)

        self.assertEqual(answer["repos"][OFFICIAL]["state"], hgd.STATE_INVALID_TOKEN)
        self.assertIsNone(answer["plan"])
        self.assertEqual(answer["plan_error"], "")
        # The gated repository was probed WITH the token, as before.
        self.assertEqual(seen, [("auth", OFFICIAL, self.STALE)])

    def test_the_reported_message_never_carries_the_token(self) -> None:
        seen: list[tuple[str, str, str | None]] = []
        api = _token_recording_api(seen, valid_token=None, listings=FAKE_LISTINGS)

        with _FakeHfApi(api):
            answer = fd.check_access(self.STALE, uncensored=False)

        self.assertNotIn(self.STALE, answer["repos"][OFFICIAL]["message"])


class DownloadTokenTests(unittest.TestCase):
    """The transfer itself sends the request's token as a bearer header.

    A check that passes followed by a download that 401s would be worse than
    either alone, so the token is verified on the transfer path too, where
    `download_bearer_to_path` runs for real against a fake `requests`.
    """

    SMALL = {
        "LICENSE.md": 4096,
        "model_index.json": 4096,
        "scheduler/scheduler_config.json": 4096,
        "text_encoder/config.json": 4096,
        "tokenizer/tokenizer.json": 4096,
        "transformer/config.json": 4096,
        "vae/config.json": 4096,
    }

    STALE = "hf_" + "e" * 34

    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self._saved = (fd.model_root, hgd.fetch_listings)
        fd.model_root = lambda variant: Path(self._tmp.name) / variant.dir_name  # type: ignore[assignment]
        hgd.fetch_listings = lambda repos, token: {r: self.SMALL for r in repos}  # type: ignore[assignment]

    def tearDown(self) -> None:
        fd.model_root, hgd.fetch_listings = self._saved  # type: ignore[assignment]
        self._tmp.cleanup()

    def _headers(self, run: Callable[[], Any]) -> list[dict[str, str]]:
        """Run a whole download with a stale token through `run`; return the headers sent."""
        fake = _FakeRequests(lambda url: b"x" * 4096, 1024)
        with _FakeRequestsModule(fake):
            run()
        return [headers for _url, headers in fake.calls]

    def test_a_gated_download_still_sends_the_token(self) -> None:
        headers = self._headers(lambda: fd.download(self.STALE, uncensored=False, variant="9b"))

        self.assertEqual(len(headers), len(self.SMALL))
        for sent in headers:
            self.assertEqual(sent.get("Authorization"), f"Bearer {self.STALE}")


class MissingFilterTests(unittest.TestCase):
    """Presence means the file is there AT THE ANNOUNCED SIZE, not merely non-empty."""

    def _write(self, item: hgd.PlannedFile, payload: bytes) -> None:
        Path(item.dest).parent.mkdir(parents=True, exist_ok=True)
        Path(item.dest).write_bytes(payload)

    def test_a_file_at_the_announced_size_is_skipped(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            plan = fd.resolve_plan(FAKE_LISTINGS, variant=V9B, uncensored=False, root=tmp)
            complete = next(item for item in plan if item.label == "vae/config.json")
            empty = next(item for item in plan if item.label == "model_index.json")
            self._write(complete, b"c" * complete.size)
            self._write(empty, b"")

            missing = {item.label for item in hgd.missing_files(plan)}
            self.assertNotIn("vae/config.json", missing)
            self.assertIn("model_index.json", missing)

            totals = hgd.plan_totals(plan)
            self.assertEqual(totals["total_bytes"], sum(item.size for item in plan))
            self.assertEqual(totals["missing_files"], len(plan) - 1)
            self.assertEqual(
                totals["missing_bytes"], sum(item.size for item in plan) - complete.size
            )

    def test_a_file_at_the_wrong_length_is_replanned(self) -> None:
        # The repair half of the truncation fix: a short file some earlier run
        # published under its final name must be fetched again, not skipped
        # forever because it happens to be non-empty.
        with tempfile.TemporaryDirectory() as tmp:
            plan = fd.resolve_plan(FAKE_LISTINGS, variant=V9B, uncensored=False, root=tmp)
            truncated = next(item for item in plan if item.label == "vae/config.json")
            self.assertGreater(truncated.size, 8)
            self._write(truncated, b"short")

            missing = {item.label for item in hgd.missing_files(plan)}
            self.assertIn("vae/config.json", missing)
            self.assertFalse(hgd.is_complete_on_disk(truncated))

    def test_a_longer_file_is_also_replanned(self) -> None:
        # Not just short: any length that is not the announced one means the file
        # on disk is not the file the listing describes.
        with tempfile.TemporaryDirectory() as tmp:
            plan = fd.resolve_plan(FAKE_LISTINGS, variant=V9B, uncensored=False, root=tmp)
            item = next(entry for entry in plan if entry.label == "vae/config.json")
            self._write(item, b"c" * (item.size + 1))
            self.assertFalse(hgd.is_complete_on_disk(item))

    def test_a_file_without_an_announced_size_falls_back_to_non_empty(self) -> None:
        # A missing size must not force an endless re-download; the old rule
        # stands and `is_complete_on_disk` logs why.
        with tempfile.TemporaryDirectory() as tmp:
            sized = dict(FAKE_OFFICIAL)
            sized["vae/config.json"] = 0
            plan = fd.resolve_plan({OFFICIAL: sized}, variant=V9B, uncensored=False, root=tmp)
            item = next(entry for entry in plan if entry.label == "vae/config.json")
            self.assertEqual(item.size, 0)

            self.assertFalse(hgd.is_complete_on_disk(item))
            self._write(item, b"anything at all")
            with self.assertLogs("modules.ai_backend.inpaint.hf_gated_download", level="WARNING"):
                self.assertTrue(hgd.is_complete_on_disk(item))
            self._write(item, b"")
            self.assertFalse(hgd.is_complete_on_disk(item))

    def test_a_directory_in_place_of_a_file_is_not_complete(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            plan = fd.resolve_plan(FAKE_LISTINGS, variant=V9B, uncensored=False, root=tmp)
            item = next(entry for entry in plan if entry.label == "vae/config.json")
            Path(item.dest).mkdir(parents=True, exist_ok=True)
            self.assertFalse(hgd.is_complete_on_disk(item))


def _http_error(status: int, kind: str) -> Exception:
    """Build the real `huggingface_hub` error `kind` carrying HTTP `status`."""
    import requests
    from huggingface_hub.errors import (
        GatedRepoError,
        HfHubHTTPError,
        RepositoryNotFoundError,
    )

    response = requests.Response()
    response.status_code = status
    classes = {
        "gated": GatedRepoError,
        "not_found": RepositoryNotFoundError,
        "http": HfHubHTTPError,
    }
    return classes[kind](f"{status} Client Error.", response=response)


class StateMappingTests(unittest.TestCase):
    """Every row of §3, driven by the exception types 0.35.1 really raises."""

    def test_gated_repo_error_with_401_is_an_invalid_token(self) -> None:
        # MEASURED: an absent or wrong token on a gated repo raises
        # GatedRepoError with status 401. The user must be told the token is
        # wrong, not that they failed to accept conditions — which is why the
        # status is consulted before the exception type.
        self.assertEqual(hgd.classify_repo_error(_http_error(401, "gated")), hgd.STATE_INVALID_TOKEN)

    def test_401_on_an_unknown_repo_is_an_invalid_token(self) -> None:
        self.assertEqual(
            hgd.classify_repo_error(_http_error(401, "not_found")), hgd.STATE_INVALID_TOKEN
        )

    def test_403_is_conditions_not_accepted(self) -> None:
        # MEASURED on a repo whose conditions this account has not accepted:
        # GatedRepoError, status 403, `X-Error-Code: GatedRepo`.
        self.assertEqual(hgd.classify_repo_error(_http_error(403, "gated")), hgd.STATE_NOT_ACCEPTED)
        self.assertEqual(hgd.classify_repo_error(_http_error(403, "http")), hgd.STATE_NOT_ACCEPTED)

    def test_gated_repo_error_without_a_status_is_conditions_not_accepted(self) -> None:
        from huggingface_hub.errors import GatedRepoError

        self.assertEqual(hgd.classify_repo_error(GatedRepoError("gated")), hgd.STATE_NOT_ACCEPTED)

    def test_404_is_not_found(self) -> None:
        self.assertEqual(hgd.classify_repo_error(_http_error(404, "not_found")), hgd.STATE_NOT_FOUND)

    def test_gated_repo_error_does_not_shadow_not_found(self) -> None:
        # `GatedRepoError` SUBCLASSES `RepositoryNotFoundError`, so an
        # isinstance-ordered mapping is a trap; 404 must still be `not_found`.
        from huggingface_hub.errors import GatedRepoError, RepositoryNotFoundError

        self.assertTrue(issubclass(GatedRepoError, RepositoryNotFoundError))

    def test_anything_else_is_a_network_error(self) -> None:
        self.assertEqual(hgd.classify_repo_error(OSError("connection reset")), hgd.STATE_NETWORK_ERROR)
        self.assertEqual(hgd.classify_repo_error(_http_error(500, "http")), hgd.STATE_NETWORK_ERROR)


class CheckAccessTests(unittest.TestCase):
    """The `.check` answer: which repos are reported, and when the network is touched."""

    def setUp(self) -> None:
        self._probe = hgd.probe_repo_access
        self._listings = hgd.fetch_listings
        self._root = fd.model_root
        self._tmp = tempfile.TemporaryDirectory()
        fd.model_root = lambda variant: Path(self._tmp.name)  # type: ignore[assignment]

    def tearDown(self) -> None:
        hgd.probe_repo_access = self._probe  # type: ignore[assignment]
        hgd.fetch_listings = self._listings  # type: ignore[assignment]
        fd.model_root = self._root  # type: ignore[assignment]
        self._tmp.cleanup()

    def test_empty_token_makes_no_network_call(self) -> None:
        def explode(*args: Any, **kwargs: Any) -> Any:
            raise AssertionError("no_token must not touch the network")

        hgd.probe_repo_access = explode  # type: ignore[assignment]
        hgd.fetch_listings = explode  # type: ignore[assignment]

        answer = fd.check_access("   ", uncensored=True)
        self.assertEqual(set(answer["repos"]), {OFFICIAL, UNCENSORED})
        for entry in answer["repos"].values():
            self.assertEqual(entry["state"], hgd.STATE_NO_TOKEN)
            self.assertEqual(entry["message"], "")
        # No listing was fetched, so there is no plan — NOT a zeroed one, which
        # would read as "everything is already downloaded".
        self.assertIsNone(answer["plan"])
        self.assertEqual(answer["plan_error"], "")

    def test_only_the_needed_repos_are_reported(self) -> None:
        hgd.probe_repo_access = lambda repo, token: (hgd.STATE_OK, "")  # type: ignore[assignment]
        hgd.fetch_listings = lambda repos, token: {r: FAKE_LISTINGS[r] for r in repos}  # type: ignore[assignment]

        self.assertEqual(set(fd.check_access("t", uncensored=False)["repos"]), {OFFICIAL})
        self.assertEqual(
            set(fd.check_access("t", uncensored=True)["repos"]), {OFFICIAL, UNCENSORED}
        )

    def test_plan_is_absent_when_a_repo_is_inaccessible(self) -> None:
        def probe(repo: str, token: str) -> tuple[str, str]:
            return (hgd.STATE_NOT_ACCEPTED, "403") if repo == UNCENSORED else (hgd.STATE_OK, "")

        def explode(*args: Any, **kwargs: Any) -> Any:
            raise AssertionError("no listing is fetched when access was refused")

        hgd.probe_repo_access = probe  # type: ignore[assignment]
        hgd.fetch_listings = explode  # type: ignore[assignment]

        answer = fd.check_access("t", uncensored=True)
        self.assertEqual(answer["repos"][UNCENSORED]["state"], hgd.STATE_NOT_ACCEPTED)
        self.assertIsNone(answer["plan"])
        # The state already says what is wrong; `plan_error` is for the case the
        # states CANNOT explain.
        self.assertEqual(answer["plan_error"], "")

    def test_plan_counts_the_missing_bytes(self) -> None:
        hgd.probe_repo_access = lambda repo, token: (hgd.STATE_OK, "")  # type: ignore[assignment]
        hgd.fetch_listings = lambda repos, token: {r: FAKE_LISTINGS[r] for r in repos}  # type: ignore[assignment]

        answer = fd.check_access("t", uncensored=False)
        plan = fd.resolve_plan({OFFICIAL: FAKE_OFFICIAL}, variant=V9B, uncensored=False, root=self._tmp.name)
        self.assertEqual(answer["plan"]["total_bytes"], sum(item.size for item in plan))
        self.assertEqual(answer["plan"]["missing_bytes"], answer["plan"]["total_bytes"])
        self.assertEqual(answer["plan"]["missing_files"], len(plan))

    def test_a_listing_failure_is_a_null_plan_and_a_plan_error(self) -> None:
        # THE defect this pins: auth succeeds, the listing fails, and the old
        # answer was a concrete zero-valued plan beside `ok` states — which the
        # UI renders as a completed installation on an empty machine.
        def probe(repo: str, token: str) -> tuple[str, str]:
            return hgd.STATE_OK, ""

        def offline(repos: Any, token: str) -> Any:
            raise ConnectionError("offline")

        hgd.probe_repo_access = probe  # type: ignore[assignment]
        hgd.fetch_listings = offline  # type: ignore[assignment]

        answer = fd.check_access("t", uncensored=True)

        for repo, entry in answer["repos"].items():
            self.assertEqual(entry["state"], hgd.STATE_OK, repo)
        self.assertIsNone(answer["plan"])
        self.assertIn("offline", answer["plan_error"])

    def test_a_plan_error_never_carries_the_token(self) -> None:
        secret = "hf_" + "p" * 34

        def probe(repo: str, token: str) -> tuple[str, str]:
            return hgd.STATE_OK, ""

        def leaky(repos: Any, token: str) -> Any:
            raise ConnectionError(f"failed with {token}")

        hgd.probe_repo_access = probe  # type: ignore[assignment]
        hgd.fetch_listings = leaky  # type: ignore[assignment]

        answer = fd.check_access(secret, uncensored=False)
        self.assertNotIn(secret, answer["plan_error"])
        self.assertIn("<токен>", answer["plan_error"])

    def test_a_present_plan_comes_with_an_empty_plan_error(self) -> None:
        hgd.probe_repo_access = lambda repo, token: (hgd.STATE_OK, "")  # type: ignore[assignment]
        hgd.fetch_listings = lambda repos, token: {r: FAKE_LISTINGS[r] for r in repos}  # type: ignore[assignment]

        answer = fd.check_access("t", uncensored=False)
        self.assertIsNotNone(answer["plan"])
        self.assertEqual(answer["plan_error"], "")

    def test_the_token_never_leaks_into_a_message(self) -> None:
        secret = "hf_" + "s" * 34

        class _Api:
            def __init__(self, token: str | None = None) -> None:
                self._token = token

            def auth_check(self, repo: str) -> None:
                raise RuntimeError(f"leaked {self._token} in the message")

        import huggingface_hub

        original = huggingface_hub.HfApi
        huggingface_hub.HfApi = _Api  # type: ignore[assignment]
        try:
            state, message = hgd.probe_repo_access(OFFICIAL, secret)
        finally:
            huggingface_hub.HfApi = original  # type: ignore[assignment]
        self.assertEqual(state, hgd.STATE_NETWORK_ERROR)
        self.assertNotIn(secret, message)
        self.assertIn("<токен>", message)


class ProgressThrottleTests(unittest.TestCase):
    """Both gates must open, and the terminal frame is never throttled away."""

    def _throttle(self, clock: Callable[[], float]) -> hgd.ProgressThrottle:
        return hgd.ProgressThrottle(min_interval=0.1, min_bytes=4 << 20, clock=clock)

    def test_both_gates_must_open(self) -> None:
        now = [0.0]
        throttle = self._throttle(lambda: now[0])

        # Enough bytes, not enough time.
        now[0] = 0.05
        self.assertFalse(throttle.should_emit(100 << 20))
        # Enough time, not enough bytes.
        now[0] = 1.0
        self.assertFalse(throttle.should_emit(1 << 20))
        # Both.
        now[0] = 2.0
        self.assertTrue(throttle.should_emit(100 << 20))

    def test_scripted_stream_emits_once_per_four_megabytes(self) -> None:
        # 1 MiB chunks arriving 50 ms apart: the time gate opens every second
        # chunk, the byte gate every fourth, so only every fourth chunk emits.
        now = [0.0]
        throttle = self._throttle(lambda: now[0])
        chunk = 1 << 20
        emitted = 0
        for index in range(1, 41):
            now[0] += 0.05
            if throttle.should_emit(index * chunk):
                emitted += 1
        self.assertEqual(emitted, 10)

    def test_force_bypasses_both_gates(self) -> None:
        now = [0.0]
        throttle = self._throttle(lambda: now[0])
        self.assertFalse(throttle.should_emit(1))
        self.assertTrue(throttle.should_emit(1, force=True))


class _FakeResponse:
    """Minimal `requests` response: a `Content-Length` and a chunk iterator.

    `announced` lets a test advertise a length the body does not deliver and then
    END NORMALLY — the clean-truncation case no transport error covers.
    """

    def __init__(self, payload: bytes, chunk: int, announced: int | None = None) -> None:
        self._payload = payload
        self._chunk = chunk
        length = len(payload) if announced is None else announced
        self.headers = {"Content-Length": str(length)}
        self.closed = False

    def __enter__(self) -> "_FakeResponse":
        return self

    def __exit__(self, *exc_info: Any) -> None:
        self.closed = True

    def raise_for_status(self) -> None:
        return None

    def iter_content(self, chunk_size: int = 1) -> Iterator[bytes]:
        for start in range(0, len(self._payload), self._chunk):
            yield self._payload[start : start + self._chunk]


class _FakeRequests:
    """Stand-in for the `requests` module, recording the headers it was given."""

    def __init__(
        self,
        payload_for: Callable[[str], bytes],
        chunk: int,
        announced: int | None = None,
    ) -> None:
        self._payload_for = payload_for
        self._chunk = chunk
        self._announced = announced
        self.calls: list[tuple[str, dict[str, str]]] = []

    def get(self, url: str, **kwargs: Any) -> _FakeResponse:
        self.calls.append((url, dict(kwargs.get("headers") or {})))
        return _FakeResponse(self._payload_for(url), self._chunk, self._announced)


class _FakeRequestsModule:
    """Installs a fake transport into the shared primitive's seams for a block.

    `model_download._default_get` becomes the fake's `get`, and
    `model_download._default_sleep` records the backoff waits in `sleeps`
    instead of sleeping, so a retry costs no wall-clock time.
    """

    def __init__(self, fake: Any) -> None:
        self._fake = fake
        self._saved: Any = None
        self.sleeps: list[float] = []

    def __enter__(self) -> Any:
        self._saved = (md._default_get, md._default_sleep)
        md._default_get = self._fake.get  # type: ignore[assignment]
        md._default_sleep = self.sleeps.append  # type: ignore[assignment]
        self._fake.sleeps = self.sleeps
        return self._fake

    def __exit__(self, *exc_info: Any) -> None:
        md._default_get, md._default_sleep = self._saved  # type: ignore[assignment]


class _RangeResponse:
    """Response that knows its own status, for exercising the resume answers.

    A `206` carries `Content-Range: bytes <start>-<end>/<total>`, which the
    primitive checks before it appends anything.
    """

    def __init__(
        self,
        body: bytes,
        chunk: int,
        status: int,
        *,
        start: int = 0,
        total: int | None = None,
    ) -> None:
        self._body = body
        self._chunk = chunk
        self.status_code = status
        self.headers = {"Content-Length": str(len(body))}
        if status == 206:
            whole = total if total is not None else start + len(body)
            self.headers["Content-Range"] = f"bytes {start}-{start + len(body) - 1}/{whole}"

    def __enter__(self) -> "_RangeResponse":
        return self

    def __exit__(self, *exc_info: Any) -> None:
        return None

    def raise_for_status(self) -> None:
        if self.status_code >= 400:
            raise RuntimeError(f"HTTP {self.status_code}")

    def iter_content(self, chunk_size: int = 1) -> Iterator[bytes]:
        for start in range(0, len(self._body), self._chunk):
            yield self._body[start : start + self._chunk]


class _RangeRequests:
    """`requests` stand-in that honours (or deliberately mishandles) `Range`.

    `on_range` picks which legal answer the server gives to a range request:
    `"206"` resumes properly, `"200"` ignores the range and sends the whole body,
    `"416"` refuses it, and `"short_once"` resumes but its FIRST ranged answer
    stops ten bytes early WITHOUT raising — the clean-truncation case — while
    later ones complete.
    """

    def __init__(self, full: bytes, chunk: int = 1024, on_range: str = "206") -> None:
        self.full = full
        self.chunk = chunk
        self.on_range = on_range
        self.calls: list[dict[str, str]] = []
        self.sleeps: list[float] = []
        self._short_sent = False

    def get(self, url: str, **kwargs: Any) -> _RangeResponse:
        headers = dict(kwargs.get("headers") or {})
        self.calls.append(headers)
        span = headers.get("Range")
        if span is None:
            return _RangeResponse(self.full, self.chunk, 200)
        start = int(span.split("=", 1)[1].split("-", 1)[0])
        if self.on_range == "416":
            return _RangeResponse(b"", self.chunk, 416)
        if self.on_range == "200":
            return _RangeResponse(self.full, self.chunk, 200)
        rest = self.full[start:]
        if self.on_range == "short_once" and not self._short_sent:
            self._short_sent = True
            # Announces the whole remainder, delivers less, ends normally.
            response = _RangeResponse(rest[:-10], self.chunk, 206, start=start, total=len(self.full))
            response.headers["Content-Length"] = str(len(rest))
            return response
        return _RangeResponse(rest, self.chunk, 206, start=start)

    @property
    def ranged(self) -> list[str]:
        """The `Range` values actually sent, in order."""
        return [headers["Range"] for headers in self.calls if "Range" in headers]


class DownloadLoopTests(unittest.TestCase):
    """The transfer itself: skipping, progress, cancellation, the free-space guard."""

    #: A tiny listing so a whole "download" is a handful of KiB. It keeps the
    #: real prefix structure, so the plan builder still runs for real.
    SMALL_OFFICIAL = {
        "LICENSE.md": 4096,
        "model_index.json": 4096,
        "scheduler/scheduler_config.json": 4096,
        "text_encoder/config.json": 4096,
        "tokenizer/tokenizer.json": 4096,
        "transformer/config.json": 4096,
        "vae/config.json": 4096,
    }

    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.root = Path(self._tmp.name) / "FLUX.2-klein-9B"
        self._saved = (fd.model_root, hgd.fetch_listings)
        fd.model_root = lambda variant: self.root  # type: ignore[assignment]
        hgd.fetch_listings = lambda repos, token: {r: self.SMALL_OFFICIAL for r in repos}  # type: ignore[assignment]

    def tearDown(self) -> None:
        fd.model_root, hgd.fetch_listings = self._saved  # type: ignore[assignment]
        self._tmp.cleanup()

    def _requests(self, chunk: int = 1024) -> _FakeRequestsModule:
        """Install a fake `requests` for the block; yields the recorder."""
        return _FakeRequestsModule(_FakeRequests(lambda url: b"x" * 4096, chunk))

    def test_downloads_every_missing_file_and_reports_the_paths(self) -> None:
        frames: list[dict[str, Any]] = []

        def progress(phase: str, step: int, total: int, label: str, **extra: Any) -> None:
            frames.append({"phase": phase, "step": step, "total": total, "label": label, **extra})

        with self._requests() as fake:
            result = fd.download("token", uncensored=False, progress_callback=progress)

        self.assertEqual(len(fake.calls), len(self.SMALL_OFFICIAL))
        for _url, headers in fake.calls:
            self.assertEqual(headers.get("Authorization"), "Bearer token")
        self.assertEqual(result["downloaded_bytes"], 4096 * len(self.SMALL_OFFICIAL))
        self.assertEqual(result["skipped_files"], 0)
        self.assertEqual(result["paths"], fd.component_paths(V9B, False))
        for label in self.SMALL_OFFICIAL:
            self.assertTrue((self.root / label).is_file(), label)
        # No staging file survives a successful run.
        self.assertFalse(list(self.root.rglob("*.part")))

        # The last frame always reports the completed byte count.
        self.assertTrue(frames)
        self.assertEqual(frames[-1]["step"], frames[-1]["total"])
        self.assertEqual(frames[-1]["total"], 4096 * len(self.SMALL_OFFICIAL))
        self.assertEqual({frame["phase"] for frame in frames}, {"download"})

    def test_present_files_are_skipped(self) -> None:
        done = self.root / "vae" / "config.json"
        done.parent.mkdir(parents=True, exist_ok=True)
        done.write_bytes(b"y" * 4096)

        with self._requests() as fake:
            result = fd.download("token", uncensored=False)

        self.assertEqual(len(fake.calls), len(self.SMALL_OFFICIAL) - 1)
        self.assertEqual(result["skipped_files"], 1)
        self.assertEqual(done.read_bytes(), b"y" * 4096)

    def test_two_level_progress_carries_the_file_in_flight(self) -> None:
        frames: list[dict[str, Any]] = []

        def progress(phase: str, step: int, total: int, label: str, **extra: Any) -> None:
            frames.append({"step": step, "total": total, "label": label, **extra})

        # A throttle that lets everything through, so the per-chunk frame shape
        # is observable at all.
        original = hgd.ProgressThrottle
        hgd.ProgressThrottle = lambda **_kwargs: original(min_interval=0.0, min_bytes=0)  # type: ignore[assignment]
        try:
            with self._requests(chunk=1024):
                fd.download("token", uncensored=False, progress_callback=progress)
        finally:
            hgd.ProgressThrottle = original  # type: ignore[assignment]

        detailed = [frame for frame in frames if "file_label" in frame]
        self.assertTrue(detailed)
        for frame in detailed:
            self.assertLessEqual(frame["file_step"], frame["file_total"])
            self.assertLessEqual(frame["step"], frame["total"])
            self.assertTrue(frame["label"].endswith(frame["file_label"]))
        # `step` is the OVERALL level and never restarts at a file boundary.
        steps = [frame["step"] for frame in detailed]
        self.assertEqual(steps, sorted(steps))

    def test_cancellation_stops_the_transfer_and_keeps_published_files(self) -> None:
        seen: list[str] = []

        def payload(url: str) -> bytes:
            seen.append(url)
            return b"x" * 4096

        # Cancel once the third file has started: the first two are published.
        cancel = {"after": 2}

        def should_cancel() -> bool:
            return len(seen) > cancel["after"]

        fake = _FakeRequests(payload, 1024)
        with _FakeRequestsModule(fake):
            with self.assertRaises(hgd.DownloadCanceled):
                fd.download("token", uncensored=False, should_cancel=should_cancel)

        published = sorted(
            path.name for path in self.root.rglob("*") if path.is_file() and path.suffix != ".part"
        )
        self.assertEqual(len(published), 2)
        # The in-flight file is NOT lost: its bytes are parked under the stable
        # `.part` name so the next run resumes instead of restarting.
        parked = list(self.root.rglob("*.part"))
        self.assertEqual(len(parked), 1)
        self.assertGreater(parked[0].stat().st_size, 0)
        # No pid-scoped staging file survives: exactly one owner at a time.
        self.assertFalse([path for path in parked if f".{os.getpid()}.part" in path.name])
        # And the loop really stopped: no further file was requested.
        self.assertEqual(len(fake.calls), 3)

    def test_a_clean_short_body_is_never_published(self) -> None:
        # THE defect this pins. A body that advertises the full length, delivers
        # 7 bytes and ends NORMALLY raises nothing anywhere in the stack: without
        # the size gate it is renamed to the final name, and every later run
        # then skips a truncated multi-gigabyte shard forever, surfacing as
        # corrupt weights at load time. A server that keeps doing it exhausts
        # the retry budget instead.
        short = _FakeRequests(lambda url: b"x" * 7, 1024, announced=4096)
        with _FakeRequestsModule(short) as fake:
            with self.assertRaises(md.RetriesExhaustedError) as caught:
                fd.download("token", uncensored=False)

        message = str(caught.exception)
        self.assertIn("4096", message)
        self.assertIn("7", message)
        # Nothing reached its final name — that is the whole point.
        self.assertFalse(
            [path for path in self.root.rglob("*") if path.is_file() and path.suffix != ".part"]
        )
        # The first attempt made progress, then every retry ended at the same
        # 7 bytes: one call plus the whole no-progress budget.
        self.assertEqual(len(fake.calls), 1 + md.RetryPolicy().max_retries)
        policy = md.RetryPolicy()
        expected_wait = sum(policy.delay_for(n, None) for n in range(1, policy.max_retries + 1))
        self.assertAlmostEqual(sum(fake.sleeps), expected_wait)
        # A short body is not proof of wrong bytes: they stay parked.
        self.assertEqual(len(list(self.root.rglob("*.part"))), 1)

    def test_a_refused_short_file_is_still_planned_afterwards(self) -> None:
        short = _FakeRequests(lambda url: b"x" * 7, 1024, announced=4096)
        with _FakeRequestsModule(short):
            with self.assertRaises(RuntimeError):
                fd.download("token", uncensored=False)

        plan = fd.resolve_plan({fd.OFFICIAL_REPO_9B: self.SMALL_OFFICIAL}, variant=V9B, uncensored=False, root=self.root)
        self.assertEqual(len(hgd.missing_files(plan)), len(self.SMALL_OFFICIAL))

    def test_a_file_without_an_announced_size_is_published_unverified(self) -> None:
        # There is nothing to compare against, so the transfer must still
        # complete rather than fail closed on every run.
        self.SMALL_OFFICIAL = dict(self.SMALL_OFFICIAL)
        self.SMALL_OFFICIAL["LICENSE.md"] = 0
        with self._requests():
            result = fd.download("token", uncensored=False)

        self.assertEqual(result["skipped_files"], 0)
        self.assertTrue((self.root / "LICENSE.md").is_file())

    # -- resume ------------------------------------------------------------
    def _one_file_plan(self) -> Path:
        """Leave exactly ONE file of the plan missing, and return its destination.

        Every manifest prefix must still resolve — a prefix that matches nothing
        is an error by design — so the other files are written at their announced
        size instead of being removed from the listing.
        """
        plan = fd.resolve_plan({fd.OFFICIAL_REPO_9B: self.SMALL_OFFICIAL}, variant=V9B, uncensored=False, root=self.root)
        target = next(item for item in plan if item.label == "model_index.json")
        for item in plan:
            if item.label == target.label:
                continue
            Path(item.dest).parent.mkdir(parents=True, exist_ok=True)
            Path(item.dest).write_bytes(b"d" * item.size)
        return Path(target.dest)

    def _park(self, dest: Path, payload: bytes) -> Path:
        """Write `payload` as the parked partial of `dest`, as a failed run would."""
        parked = dest.with_name(dest.name + ".part")
        parked.parent.mkdir(parents=True, exist_ok=True)
        parked.write_bytes(payload)
        return parked

    def test_a_parked_partial_is_resumed_and_published(self) -> None:
        dest = self._one_file_plan()
        full = bytes(range(256)) * 16
        self.assertEqual(len(full), 4096)
        parked = self._park(dest, full[:1000])

        transport = _RangeRequests(full)
        with _FakeRequestsModule(transport):
            result = fd.download("token", uncensored=False)

        self.assertEqual(transport.ranged, ["bytes=1000-"])
        self.assertEqual(dest.read_bytes(), full)
        self.assertFalse(parked.exists())
        self.assertEqual(result["downloaded_bytes"], 4096)

    def test_progress_of_a_resumed_file_starts_at_the_resumed_offset(self) -> None:
        # The Rust side derives speed from consecutive frames, so a first frame
        # reporting 0 after 1000 bytes were already on disk would render as a
        # backwards jump and then an impossible transfer rate.
        dest = self._one_file_plan()
        full = b"z" * 4096
        self._park(dest, full[:1000])
        frames: list[dict[str, Any]] = []

        def progress(phase: str, step: int, total: int, label: str, **extra: Any) -> None:
            frames.append({"step": step, "total": total, **extra})

        original = hgd.ProgressThrottle
        hgd.ProgressThrottle = lambda **_kwargs: original(min_interval=0.0, min_bytes=0)  # type: ignore[assignment]
        try:
            with _FakeRequestsModule(_RangeRequests(full)):
                fd.download("token", uncensored=False, progress_callback=progress)
        finally:
            hgd.ProgressThrottle = original  # type: ignore[assignment]

        detailed = [frame for frame in frames if "file_step" in frame]
        self.assertTrue(detailed)
        self.assertGreaterEqual(detailed[0]["file_step"], 1000)
        self.assertGreaterEqual(detailed[0]["step"], 1000)
        steps = [frame["step"] for frame in detailed]
        self.assertEqual(steps, sorted(steps))
        self.assertLessEqual(detailed[-1]["file_step"], detailed[-1]["file_total"])

    def test_a_server_that_ignores_the_range_rewrites_instead_of_appending(self) -> None:
        # THE corruption case: appending a whole body to 1000 existing bytes
        # produces a 5096-byte file that every transport reports as a success.
        dest = self._one_file_plan()
        full = b"q" * 4096
        self._park(dest, b"?" * 1000)

        with _FakeRequestsModule(_RangeRequests(full, on_range="200")):
            fd.download("token", uncensored=False)

        self.assertEqual(dest.stat().st_size, 4096)
        self.assertEqual(dest.read_bytes(), full)

    def test_a_416_answer_discards_the_partial_and_refetches(self) -> None:
        dest = self._one_file_plan()
        full = b"w" * 4096
        self._park(dest, b"?" * 1000)

        transport = _RangeRequests(full, on_range="416")
        with _FakeRequestsModule(transport):
            fd.download("token", uncensored=False)

        self.assertEqual(dest.read_bytes(), full)
        # One ranged attempt, refused, then one plain refetch — no backoff,
        # because nothing failed on the network.
        self.assertEqual(len(transport.calls), 2)
        self.assertEqual(transport.ranged, ["bytes=1000-"])
        self.assertEqual(transport.sleeps, [])

    def test_a_published_file_at_the_wrong_size_is_replaced(self) -> None:
        # The planner already re-plans it; the transfer must not then skip it as
        # "present" merely because it is non-empty.
        dest = self._one_file_plan()
        dest.parent.mkdir(parents=True, exist_ok=True)
        dest.write_bytes(b"?" * 100)
        full = b"w" * 4096

        transport = _RangeRequests(full)
        with _FakeRequestsModule(transport):
            result = fd.download("token", uncensored=False)

        self.assertEqual(dest.read_bytes(), full)
        self.assertEqual(len(transport.calls), 1)
        self.assertEqual(result["skipped_files"], len(self.SMALL_OFFICIAL) - 1)

    def test_leftovers_longer_than_the_announced_size_are_dropped_and_refetched(self) -> None:
        dest = self._one_file_plan()
        full = b"w" * 4096
        self._park(dest, b"?" * 9000)

        transport = _RangeRequests(full)
        with _FakeRequestsModule(transport):
            fd.download("token", uncensored=False)

        self.assertEqual(dest.read_bytes(), full)
        # Proven-wrong bytes are never offered to the server as a resume point.
        self.assertEqual(transport.ranged, [])
        self.assertEqual(len(transport.calls), 1)

    def test_a_clean_short_body_is_resumed_instead_of_discarded(self) -> None:
        dest = self._one_file_plan()
        full = bytes(range(256)) * 16
        self._park(dest, full[:1000])

        # The first resume ends cleanly ten bytes early. Those 3086 new bytes are
        # right, so the next attempt continues from 4086 instead of from zero.
        transport = _RangeRequests(full, on_range="short_once")
        with _FakeRequestsModule(transport):
            result = fd.download("token", uncensored=False)

        self.assertEqual(dest.read_bytes(), full)
        self.assertEqual(transport.ranged, ["bytes=1000-", "bytes=4086-"])
        self.assertAlmostEqual(sum(transport.sleeps), md.RetryPolicy().base_delay_s)
        self.assertFalse(list(self.root.rglob("*.part")))
        self.assertEqual(result["downloaded_bytes"], 4096)

    def test_a_retry_is_an_ordinary_frame_with_a_retry_label(self) -> None:
        dest = self._one_file_plan()
        full = b"e" * 4096
        self._park(dest, full[:1000])
        frames: list[dict[str, Any]] = []

        def progress(phase: str, step: int, total: int, label: str, **extra: Any) -> None:
            frames.append({"phase": phase, "step": step, "total": total, "label": label, **extra})

        with _FakeRequestsModule(_RangeRequests(full, on_range="short_once")):
            fd.download("token", uncensored=False, progress_callback=progress)

        retry = [frame for frame in frames if frame["label"].startswith("Сеть недоступна")]
        self.assertEqual(len(retry), 1)
        frame = retry[0]
        self.assertEqual(frame["phase"], "download")
        # The staged bytes the retry resumes, at both levels; same frame shape.
        self.assertEqual(frame["file_step"], 4086)
        self.assertEqual(frame["file_total"], 4096)
        self.assertEqual(frame["file_label"], "model_index.json")
        self.assertIn("1/5", frame["label"])
        self.assertTrue(frame["label"].endswith("model_index.json"))

    def test_a_cancel_during_the_backoff_wait_is_download_canceled(self) -> None:
        dest = self._one_file_plan()
        full = b"e" * 4096
        self._park(dest, full[:1000])
        transport = _RangeRequests(full, on_range="short_once")

        def should_cancel() -> bool:
            # Up only once the first backoff slice has been slept.
            return bool(transport.sleeps)

        with _FakeRequestsModule(transport):
            with self.assertRaises(hgd.DownloadCanceled):
                fd.download("token", uncensored=False, should_cancel=should_cancel)

        self.assertEqual(len(transport.sleeps), 1)
        self.assertEqual(len(transport.calls), 1)
        self.assertFalse(dest.exists())
        # The 4086 good bytes are parked for the next run.
        self.assertEqual(md.staged_bytes(dest), 4086)

    def test_cancellation_parks_bytes_that_a_second_call_finishes(self) -> None:
        dest = self._one_file_plan()
        full = b"r" * 4096
        first = _RangeRequests(full)
        polls = {"since_get": 0}

        def should_cancel() -> bool:
            # Up a few chunk-boundary polls into the transfer, so some bytes but
            # not all of them are staged when it stops.
            if not first.calls:
                return False
            polls["since_get"] += 1
            return polls["since_get"] > 3

        with _FakeRequestsModule(first):
            with self.assertRaises(hgd.DownloadCanceled):
                fd.download("token", uncensored=False, should_cancel=should_cancel)

        parked = dest.with_name(dest.name + ".part")
        self.assertTrue(parked.is_file())
        staged = parked.stat().st_size
        self.assertGreater(staged, 0)
        self.assertLess(staged, 4096)
        self.assertFalse(dest.exists())

        transport = _RangeRequests(full)
        with _FakeRequestsModule(transport):
            fd.download("token", uncensored=False)

        self.assertEqual(transport.ranged, [f"bytes={staged}-"])
        self.assertEqual(dest.read_bytes(), full)
        self.assertFalse(parked.exists())

    def test_the_free_space_guard_subtracts_the_staged_bytes(self) -> None:
        dest = self._one_file_plan()
        full = b"t" * 4096
        self._park(dest, full[:4000])

        # 96 bytes are still to fetch. Without subtracting the staged 4000 the
        # guard would demand 4096 and refuse a download that plainly fits.
        original = hgd.free_bytes
        hgd.free_bytes = lambda path: hgd.FREE_SPACE_MARGIN_BYTES + 100  # type: ignore[assignment]
        try:
            with _FakeRequestsModule(_RangeRequests(full)):
                fd.download("token", uncensored=False)
        finally:
            hgd.free_bytes = original  # type: ignore[assignment]

        self.assertEqual(dest.read_bytes(), full)

    def test_free_space_refusal_names_both_numbers(self) -> None:
        original = hgd.free_bytes
        hgd.free_bytes = lambda path: 1000  # type: ignore[assignment]
        try:
            with self._requests() as fake:
                with self.assertRaises(RuntimeError) as caught:
                    fd.download("token", uncensored=False)
            self.assertFalse(fake.calls, "no byte may be fetched before the guard passes")
        finally:
            hgd.free_bytes = original  # type: ignore[assignment]
        message = str(caught.exception)
        self.assertIn("1000", message)
        self.assertIn(str(4096 * len(self.SMALL_OFFICIAL) + hgd.FREE_SPACE_MARGIN_BYTES), message)

    def test_unqueryable_filesystem_is_not_treated_as_full(self) -> None:
        original = hgd.free_bytes
        hgd.free_bytes = lambda path: -1  # type: ignore[assignment]
        try:
            with self._requests():
                fd.download("token", uncensored=False)
        finally:
            hgd.free_bytes = original  # type: ignore[assignment]

    def test_empty_token_is_refused_without_a_network_call(self) -> None:
        def explode(*args: Any, **kwargs: Any) -> Any:
            raise AssertionError("an empty token must not reach the hub")

        saved = hgd.fetch_listings
        hgd.fetch_listings = explode  # type: ignore[assignment]
        try:
            with self.assertRaises(ValueError):
                fd.download("  ", uncensored=False)
        finally:
            hgd.fetch_listings = saved  # type: ignore[assignment]


class FreeSpaceTests(unittest.TestCase):
    """The guard itself, independent of a transfer."""

    def test_nothing_required_is_always_allowed(self) -> None:
        hgd.require_free_space(Path(os.sep), 0, label=fd.LABEL)

    def test_existing_ancestor_walks_up_to_a_real_directory(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            missing = Path(tmp) / "a" / "b" / "c"
            self.assertEqual(hgd._existing_ancestor(missing), Path(tmp))
            self.assertGreater(hgd.free_bytes(missing), 0)


if __name__ == "__main__":
    unittest.main()
