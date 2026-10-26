"""
File: modules/ai_backend/inpaint/test_hf_gated_download.py

Purpose:
Unit tests of the model-agnostic backend Hugging Face download machinery
(`hf_gated_download.py`), driven through a SYNTHETIC gated `DownloadTarget`:
two repositories (a main one and an extra encoder repository), a prefix
manifest with an exclusive encoder rule and a temporary destination root. The
listings keep the shape of real ones (shard names, a duplicate root single
file, GGUF quants, sample images, README, `.gitattributes`), so an exclusion
that stops working shows up here.

Main responsibilities:
- verify the plan builder resolves PREFIXES against a listing: excluded
  suffixes and names are filtered whatever rule pulled them in, an explicit
  `LICENSE.md` entry survives, a file no rule names is not planned, a
  whole-repository rule is flattened into its destination directory,
  destinations are absolute under the root, and the totals per destination
  are exact;
- verify a prefix that resolves to nothing (or only to excluded files) and a
  missing listing are errors naming the prefix and the repository;
- verify missing-only filtering: presence means the ANNOUNCED SIZE, so a file
  at the wrong length (short or long) is planned again, while a file whose
  listing announced no size falls back to the non-empty rule;
- verify the `.check` answer: the echo, exactly the target's repositories,
  `no_token` without a network call, a NULLABLE plan (never zeros) with
  `plan_error` only when the listing failed after access succeeded, and the
  token scrubbed from every message;
- verify the `state` taxonomy against the exception types `huggingface_hub`
  0.35.1 really raises (status before type);
- verify token routing: the request's token reaches every gated repository
  unchanged at the probe, the listing and the transfer (bearer header);
- verify LFS digests travel from the listing into the plan and the transfer;
- verify the progress throttle and two-level progress, cancellation,
  resume (`Range`, ignored range, 416, overlong leftovers, clean short body,
  cancel-then-resume), the retry frame, the free-space guard and the
  no-token refusal with the target's own message.

Notes:
These tests were written as characterization tests against the module while
`flux2_download.py` still drove it, so retiring that consumer provably changed
nothing the machinery does. Patch points are set on `hf_gated_download`
(`hgd.probe_repo_access`, `hgd.fetch_listings`, `hgd.free_bytes`, ...).
Nothing touches the network: listings are fakes, and the streaming tests
replace the shared primitive's two seams for the duration
(`_hf_download_test_fakes._FakeRequestsModule`). Files go to temporary
directories that each test removes.
"""

from __future__ import annotations

import os
import tempfile
import unittest
from pathlib import Path
from typing import Any, Callable

from modules.ai_backend.engines import model_download as md
from modules.ai_backend.inpaint import hf_gated_download as hgd
from modules.ai_backend.inpaint._hf_download_test_fakes import (
    _FakeHfApi,
    _FakeRequests,
    _FakeRequestsModule,
    _http_error,
    _RangeRequests,
    _Sibling,
    _token_recording_api,
)

#: The synthetic gated main repository (transformer, tokenizer, VAE, configs,
#: its own encoder).
MAIN = "example-org/gated-model"

#: The synthetic gated extra encoder repository, flattened into its own
#: directory by a whole-repository rule.
EXTRA = "example-org/gated-extra-encoder"

#: The model name the machinery puts into logs and messages.
LABEL = "Test model"

#: The target's own refusal for an empty token.
NO_TOKEN_MESSAGE = "Не указан токен Hugging Face (тестовая модель)."

#: A structurally faithful listing with real sizes: shards, the duplicate root
#: single file no rule names, sample images and the decoy files.
LISTING_MAIN: dict[str, int] = {
    ".gitattributes": 1580,
    "LICENSE.md": 18158,
    "README.md": 10404,
    "editing.jpg": 2506178,
    "model-single-file.safetensors": 18157185168,
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

#: The extra encoder repository: weights plus four GGUF quants and decoys.
LISTING_EXTRA: dict[str, int] = {
    ".gitattributes": 394,
    "README.md": 16523,
    "chat_template.jinja": 4256,
    "config.json": 1664,
    "encoder-f16.gguf": 16388043744,
    "encoder-q4_k_m.gguf": 5027783648,
    "encoder-q6_k.gguf": 6725899232,
    "encoder-q8_0.gguf": 8709518304,
    "generation_config.json": 226,
    "model.safetensors": 16381516808,
    "tokenizer.json": 11422650,
    "tokenizer_config.json": 393,
}

LISTINGS = {MAIN: LISTING_MAIN, EXTRA: LISTING_EXTRA}

#: Everything but the encoder, always from the main repository.
SHARED_RULES = (
    hgd.ManifestEntry(MAIN, "transformer/", "transformer"),
    hgd.ManifestEntry(MAIN, "tokenizer/", "tokenizer"),
    hgd.ManifestEntry(MAIN, "vae/", "vae"),
    hgd.ManifestEntry(MAIN, "scheduler/scheduler_config.json", "scheduler/scheduler_config.json"),
    hgd.ManifestEntry(MAIN, "model_index.json", "model_index.json"),
    hgd.ManifestEntry(MAIN, "LICENSE.md", "LICENSE.md"),
)
MAIN_ENCODER_RULE = hgd.ManifestEntry(MAIN, "text_encoder/", "text_encoder")
EXTRA_ENCODER_RULE = hgd.ManifestEntry(EXTRA, "", "text_encoder_extra")


def make_target(root: Path | str, *, extra: bool = False) -> hgd.DownloadTarget:
    """The synthetic target: the main repo alone, or both with the extra encoder rule INSTEAD of the main one."""
    repos = (hgd.RepoSpec(MAIN), hgd.RepoSpec(EXTRA)) if extra else (hgd.RepoSpec(MAIN),)
    encoder = EXTRA_ENCODER_RULE if extra else MAIN_ENCODER_RULE
    return hgd.DownloadTarget(
        label=LABEL,
        repos=repos,
        manifest=SHARED_RULES + (encoder,),
        root=Path(root),
        no_token_message=NO_TOKEN_MESSAGE,
    )


def plan_for(listings: Any, root: Path | str, *, extra: bool = False) -> list[hgd.PlannedFile]:
    """`hgd.resolve_plan` of the synthetic target's manifest under `root`."""
    return hgd.resolve_plan(listings, make_target(root, extra=extra).manifest, root)


def _explode(*args: Any, **kwargs: Any) -> Any:
    raise AssertionError("this path must not touch the network")


class PlanBuilderTests(unittest.TestCase):
    """`resolve_plan` against fake listings."""

    def _labels(self, *, extra: bool, root: str = "/models") -> list[str]:
        return [item.label for item in plan_for(LISTINGS, root, extra=extra)]

    def test_a_file_no_rule_names_is_not_planned(self) -> None:
        labels = self._labels(extra=False)
        self.assertNotIn("model-single-file.safetensors", labels)
        self.assertIn("transformer/diffusion_pytorch_model-00001-of-00002.safetensors", labels)
        self.assertIn("transformer/diffusion_pytorch_model-00002-of-00002.safetensors", labels)
        self.assertIn("transformer/config.json", labels)

    def test_gguf_quants_and_decoys_are_excluded(self) -> None:
        labels = self._labels(extra=True)
        self.assertFalse([label for label in labels if label.endswith(".gguf")])
        self.assertFalse([label for label in labels if label.endswith(".jpg")])
        self.assertFalse([label for label in labels if label.endswith("README.md")])
        self.assertFalse([label for label in labels if label.endswith(".gitattributes")])
        # LICENSE.md is an explicit manifest entry and must survive the README rule.
        self.assertIn("LICENSE.md", labels)

    def test_a_whole_repository_rule_is_flattened_into_its_directory(self) -> None:
        plan = plan_for(LISTINGS, "/models", extra=True)
        extra = {item.label: item for item in plan if item.repo == EXTRA}
        self.assertEqual(
            set(extra),
            {
                "text_encoder_extra/chat_template.jinja",
                "text_encoder_extra/config.json",
                "text_encoder_extra/generation_config.json",
                "text_encoder_extra/model.safetensors",
                "text_encoder_extra/tokenizer.json",
                "text_encoder_extra/tokenizer_config.json",
            },
        )
        self.assertEqual(extra["text_encoder_extra/model.safetensors"].source, "model.safetensors")

    def test_each_entry_takes_only_its_own_repository(self) -> None:
        for extra in (False, True):
            plan = plan_for(LISTINGS, "/models", extra=extra)
            tokenizer = [item for item in plan if item.label.startswith("tokenizer/")]
            self.assertEqual(len(tokenizer), 7, extra)
            for item in tokenizer:
                self.assertEqual(item.repo, MAIN, extra)
            # The extra repository's own tokenizer files land in its encoder
            # directory, never in `tokenizer/`.
            self.assertEqual(
                {item.source for item in tokenizer},
                {path for path in LISTING_MAIN if path.startswith("tokenizer/")},
            )

    def test_the_plan_holds_exactly_the_manifest_in_force(self) -> None:
        # The machinery plans exactly the rules it is given: the main encoder
        # rule and the extra one are never joined unless the manifest says so.
        off = self._labels(extra=False)
        on = self._labels(extra=True)
        self.assertIn("text_encoder/model-00001-of-00004.safetensors", off)
        self.assertFalse([label for label in off if label.startswith("text_encoder_extra/")])
        self.assertFalse([label for label in on if label.startswith("text_encoder/")])
        self.assertIn("text_encoder_extra/model.safetensors", on)
        shared_off = {label for label in off if not label.startswith("text_encoder")}
        shared_on = {label for label in on if not label.startswith("text_encoder")}
        self.assertEqual(shared_off, shared_on)

    def test_plan_sizes_are_the_listing_sizes(self) -> None:
        off = plan_for(LISTINGS, "/models", extra=False)
        on = plan_for(LISTINGS, "/models", extra=True)
        self.assertEqual(sum(item.size for item in off), 34722790808)
        self.assertEqual(sum(item.size for item in on), 34734185315)
        self.assertEqual(len(off), 23)
        self.assertEqual(len(on), 22)

    def test_a_file_already_on_disk_outside_the_manifest_is_left_alone(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            other = Path(tmp) / "text_encoder" / "model-00001-of-00004.safetensors"
            other.parent.mkdir(parents=True, exist_ok=True)
            other.write_bytes(b"already downloaded")

            plan = plan_for(LISTINGS, tmp, extra=True)
            self.assertFalse([item for item in plan if item.label.startswith("text_encoder/")])
            self.assertTrue(other.is_file())

    def test_plan_totals_per_destination(self) -> None:
        plan = plan_for(LISTINGS, "/models", extra=False)
        by_prefix: dict[str, int] = {}
        for item in plan:
            head = item.label.split("/")[0] if "/" in item.label else item.label
            by_prefix[head] = by_prefix.get(head, 0) + item.size
        self.assertEqual(by_prefix["transformer"], 18157216297)
        self.assertEqual(by_prefix["text_encoder"], 16381551490)
        self.assertEqual(by_prefix["vae"], 168121699)
        self.assertEqual(by_prefix["tokenizer"], 15882232)

    def test_destinations_are_absolute_under_the_root(self) -> None:
        root = os.path.join(os.sep, "m")
        for item in plan_for(LISTINGS, root, extra=False):
            self.assertEqual(Path(item.dest), Path(os.sep, "m", *item.label.split("/")))

    def test_prefix_resolving_to_nothing_is_an_error(self) -> None:
        # A repository that was re-sharded must not silently produce half a
        # model: the message names the prefix AND the repository.
        stripped = {key: value for key, value in LISTING_MAIN.items() if not key.startswith("transformer/")}
        with self.assertRaises(RuntimeError) as caught:
            plan_for({MAIN: stripped, EXTRA: LISTING_EXTRA}, "/m", extra=False)
        message = str(caught.exception)
        self.assertIn("transformer/", message)
        self.assertIn(MAIN, message)

    def test_whole_repo_prefix_resolving_to_only_excluded_files_is_an_error(self) -> None:
        only_gguf = {key: value for key, value in LISTING_EXTRA.items() if key.endswith(".gguf")}
        with self.assertRaises(RuntimeError):
            plan_for({MAIN: LISTING_MAIN, EXTRA: only_gguf}, "/m", extra=True)

    def test_missing_listing_is_an_error(self) -> None:
        with self.assertRaises(RuntimeError):
            plan_for({MAIN: LISTING_MAIN}, "/m", extra=True)


class CheckAnswerTests(unittest.TestCase):
    """The `.check` answer of the synthetic target, pinned value for value."""

    TOKEN = "hf_" + "c" * 34

    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.root = Path(self._tmp.name) / "Model"
        self._saved = (hgd.probe_repo_access, hgd.fetch_listings, hgd.download)
        hgd.probe_repo_access = lambda repo, token: (hgd.STATE_OK, "")  # type: ignore[assignment]
        hgd.fetch_listings = lambda repos, token: {r: LISTINGS[r] for r in repos}  # type: ignore[assignment]

    def tearDown(self) -> None:
        hgd.probe_repo_access, hgd.fetch_listings, hgd.download = self._saved  # type: ignore[assignment]
        self._tmp.cleanup()

    def test_the_check_answer_with_the_main_encoder(self) -> None:
        expected = {
            "selection": "a",
            "repos": {MAIN: {"state": "ok", "message": ""}},
            "plan": {"total_bytes": 34722790808, "missing_bytes": 34722790808, "missing_files": 23},
            "plan_error": "",
        }
        for token in (self.TOKEN, f"  {self.TOKEN} "):
            with self.subTest(token=token):
                answer = hgd.check_access(token, make_target(self.root), echo={"selection": "a"})
                self.assertEqual(answer, expected)

    def test_the_check_answer_with_the_extra_encoder(self) -> None:
        expected = {
            "selection": "b",
            "repos": {
                MAIN: {"state": "ok", "message": ""},
                EXTRA: {"state": "ok", "message": ""},
            },
            "plan": {"total_bytes": 34734185315, "missing_bytes": 34734185315, "missing_files": 22},
            "plan_error": "",
        }
        answer = hgd.check_access(self.TOKEN, make_target(self.root, extra=True), echo={"selection": "b"})
        self.assertEqual(answer, expected)

    def test_a_tokenless_check_is_no_token_without_the_network(self) -> None:
        hgd.probe_repo_access = _explode  # type: ignore[assignment]
        hgd.fetch_listings = _explode  # type: ignore[assignment]
        answer = hgd.check_access("", make_target(self.root, extra=True), echo={"selection": "b"})
        self.assertEqual(answer["selection"], "b")
        self.assertEqual(
            {repo: state["state"] for repo, state in answer["repos"].items()},
            {MAIN: hgd.STATE_NO_TOKEN, EXTRA: hgd.STATE_NO_TOKEN},
        )
        self.assertIsNone(answer["plan"])

    def test_a_tokenless_download_is_refused_with_the_targets_message(self) -> None:
        hgd.fetch_listings = _explode  # type: ignore[assignment]
        with self.assertRaises(ValueError) as caught:
            hgd.download("", make_target(self.root))
        self.assertEqual(str(caught.exception), NO_TOKEN_MESSAGE)


class ListingDigestTests(unittest.TestCase):
    """LFS digests travel from the hub listing into the plan and the transfer."""

    SHA = "ef" * 32

    def test_repo_listing_keeps_the_lfs_digest_and_the_plan_carries_it(self) -> None:
        import types as _types

        listing = dict(LISTING_MAIN)
        planned = plan_for({MAIN: listing}, "/m", extra=False)
        shard = next(item.source for item in planned if item.source.endswith(".safetensors"))
        self_sha = self.SHA

        class _Api:
            def __init__(self, token: str | None = None) -> None:
                del token

            def model_info(self, repo: str, files_metadata: bool = False) -> Any:
                siblings = []
                for name, size in sorted(listing.items()):
                    sib = _Sibling(name, size)
                    sib.lfs = _types.SimpleNamespace(sha256=self_sha) if name == shard else None  # type: ignore[attr-defined]
                    siblings.append(sib)
                return _types.SimpleNamespace(siblings=siblings)

        with _FakeHfApi(_Api):
            got = hgd.repo_listing(MAIN, "")

        self.assertEqual(got[shard], hgd.ListedFile(listing[shard], self.SHA))
        self.assertTrue(all(value.sha256 is None for path, value in got.items() if path != shard))
        by_source = {item.source: item for item in plan_for({MAIN: got}, "/m", extra=False)}
        self.assertEqual(by_source[shard].sha256, self.SHA)

    def test_the_transfer_gets_the_planned_digest(self) -> None:
        item = hgd.PlannedFile(MAIN, "a.safetensors", "/m/a.safetensors", "a.safetensors", 10, self.SHA)
        seen: dict[str, Any] = {}

        def fake_transfer(url: str, dest: str, **kwargs: Any) -> bool:
            seen.update(kwargs)
            return True

        saved = (hgd.download_bearer_to_path, hgd._file_url)
        hgd.download_bearer_to_path = fake_transfer  # type: ignore[assignment]
        hgd._file_url = lambda _item: "https://hf/a"  # type: ignore[assignment]
        try:
            hgd._fetch_one(item, token="", on_chunk=lambda *_: None, on_retry=lambda _e: None, should_cancel=None, label=LABEL)
        finally:
            hgd.download_bearer_to_path, hgd._file_url = saved  # type: ignore[assignment]

        self.assertEqual(seen["expected_sha256"], self.SHA)
        self.assertEqual(seen["expected_size"], 10)


class TokenRoutingTests(unittest.TestCase):
    """The request's token reaches every gated repository, unchanged and never leaked.

    `probe_repo_access` and `repo_listing` run FOR REAL here against a fake
    `HfApi`; only the transport is a stand-in.
    """

    #: A token shaped like a real one, never a valid one for the fake hub.
    STALE = "hf_" + "e" * 34

    def setUp(self) -> None:
        # An empty destination keeps the plan totals independent of whatever the
        # developer running the suite happens to have downloaded.
        self._tmp = tempfile.TemporaryDirectory()
        self.root = Path(self._tmp.name) / "Model"

    def tearDown(self) -> None:
        self._tmp.cleanup()

    def test_a_valid_token_reaches_every_gated_repository(self) -> None:
        good = "hf_" + "g" * 34
        seen: list[tuple[str, str, str | None]] = []
        api = _token_recording_api(seen, valid_token=good, listings=LISTINGS)

        with _FakeHfApi(api):
            answer = hgd.check_access(good, make_target(self.root, extra=True), echo={})

        for repo in (MAIN, EXTRA):
            self.assertEqual(answer["repos"][repo]["state"], hgd.STATE_OK)
        self.assertIsNotNone(answer["plan"])
        self.assertEqual(answer["plan_error"], "")
        # Every call to a gated repository carried the token, unchanged.
        self.assertEqual({token for _call, _repo, token in seen}, {good})
        self.assertEqual({repo for _call, repo, _tok in seen}, {MAIN, EXTRA})

    def test_an_invalid_token_is_invalid_token(self) -> None:
        seen: list[tuple[str, str, str | None]] = []
        api = _token_recording_api(seen, valid_token=None, listings=LISTINGS)

        with _FakeHfApi(api):
            answer = hgd.check_access(self.STALE, make_target(self.root), echo={})

        self.assertEqual(answer["repos"][MAIN]["state"], hgd.STATE_INVALID_TOKEN)
        self.assertIsNone(answer["plan"])
        self.assertEqual(answer["plan_error"], "")
        # The gated repository was probed WITH the token, and nothing else ran.
        self.assertEqual(seen, [("auth", MAIN, self.STALE)])

    def test_the_reported_message_never_carries_the_token(self) -> None:
        seen: list[tuple[str, str, str | None]] = []
        api = _token_recording_api(seen, valid_token=None, listings=LISTINGS)

        with _FakeHfApi(api):
            answer = hgd.check_access(self.STALE, make_target(self.root), echo={})

        self.assertNotIn(self.STALE, answer["repos"][MAIN]["message"])


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
        self.root = Path(self._tmp.name) / "Model"
        self._saved = hgd.fetch_listings
        hgd.fetch_listings = lambda repos, token: {r: self.SMALL for r in repos}  # type: ignore[assignment]

    def tearDown(self) -> None:
        hgd.fetch_listings = self._saved  # type: ignore[assignment]
        self._tmp.cleanup()

    def _headers(self, run: Callable[[], Any]) -> list[dict[str, str]]:
        """Run a whole download through `run`; return the headers sent."""
        fake = _FakeRequests(lambda url: b"x" * 4096, 1024)
        with _FakeRequestsModule(fake):
            run()
        return [headers for _url, headers in fake.calls]

    def test_a_gated_download_sends_the_token(self) -> None:
        headers = self._headers(lambda: hgd.download(self.STALE, make_target(self.root)))

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
            plan = plan_for(LISTINGS, tmp)
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
            self.assertEqual(totals["missing_bytes"], sum(item.size for item in plan) - complete.size)

    def test_a_file_at_the_wrong_length_is_replanned(self) -> None:
        # The repair half of the truncation fix: a short file some earlier run
        # published under its final name must be fetched again, not skipped
        # forever because it happens to be non-empty.
        with tempfile.TemporaryDirectory() as tmp:
            plan = plan_for(LISTINGS, tmp)
            truncated = next(item for item in plan if item.label == "vae/config.json")
            self.assertGreater(truncated.size, 8)
            self._write(truncated, b"short")

            missing = {item.label for item in hgd.missing_files(plan)}
            self.assertIn("vae/config.json", missing)
            self.assertFalse(hgd.is_complete_on_disk(truncated))

    def test_a_longer_file_is_also_replanned(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            plan = plan_for(LISTINGS, tmp)
            item = next(entry for entry in plan if entry.label == "vae/config.json")
            self._write(item, b"c" * (item.size + 1))
            self.assertFalse(hgd.is_complete_on_disk(item))

    def test_a_file_without_an_announced_size_falls_back_to_non_empty(self) -> None:
        # A missing size must not force an endless re-download; the old rule
        # stands and `is_complete_on_disk` logs why.
        with tempfile.TemporaryDirectory() as tmp:
            sized = dict(LISTING_MAIN)
            sized["vae/config.json"] = 0
            plan = plan_for({MAIN: sized}, tmp)
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
            plan = plan_for(LISTINGS, tmp)
            item = next(entry for entry in plan if entry.label == "vae/config.json")
            Path(item.dest).mkdir(parents=True, exist_ok=True)
            self.assertFalse(hgd.is_complete_on_disk(item))


class StateMappingTests(unittest.TestCase):
    """Every `state`, driven by the exception types huggingface_hub 0.35.1 really raises."""

    def test_gated_repo_error_with_401_is_an_invalid_token(self) -> None:
        # MEASURED: an absent or wrong token on a gated repo raises
        # GatedRepoError with status 401. The user must be told the token is
        # wrong, not that they failed to accept conditions — which is why the
        # status is consulted before the exception type.
        self.assertEqual(hgd.classify_repo_error(_http_error(401, "gated")), hgd.STATE_INVALID_TOKEN)

    def test_401_on_an_unknown_repo_is_an_invalid_token(self) -> None:
        self.assertEqual(hgd.classify_repo_error(_http_error(401, "not_found")), hgd.STATE_INVALID_TOKEN)

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
        self._saved = (hgd.probe_repo_access, hgd.fetch_listings)
        self._tmp = tempfile.TemporaryDirectory()
        self.root = Path(self._tmp.name)

    def tearDown(self) -> None:
        hgd.probe_repo_access, hgd.fetch_listings = self._saved  # type: ignore[assignment]
        self._tmp.cleanup()

    def _check(self, token: str, *, extra: bool) -> dict[str, Any]:
        return hgd.check_access(token, make_target(self.root, extra=extra), echo={"selection": "x"})

    def test_empty_token_makes_no_network_call(self) -> None:
        hgd.probe_repo_access = _explode  # type: ignore[assignment]
        hgd.fetch_listings = _explode  # type: ignore[assignment]

        answer = self._check("   ", extra=True)
        self.assertEqual(set(answer["repos"]), {MAIN, EXTRA})
        for entry in answer["repos"].values():
            self.assertEqual(entry["state"], hgd.STATE_NO_TOKEN)
            self.assertEqual(entry["message"], "")
        # No listing was fetched, so there is no plan — NOT a zeroed one, which
        # would read as "everything is already downloaded".
        self.assertIsNone(answer["plan"])
        self.assertEqual(answer["plan_error"], "")

    def test_the_answer_echoes_the_selection(self) -> None:
        hgd.probe_repo_access = lambda repo, token: (hgd.STATE_OK, "")  # type: ignore[assignment]
        hgd.fetch_listings = lambda repos, token: {r: LISTINGS[r] for r in repos}  # type: ignore[assignment]
        self.assertEqual(self._check("t", extra=False)["selection"], "x")
        self.assertEqual(self._check("", extra=False)["selection"], "x")

    def test_only_the_targets_repos_are_reported(self) -> None:
        hgd.probe_repo_access = lambda repo, token: (hgd.STATE_OK, "")  # type: ignore[assignment]
        hgd.fetch_listings = lambda repos, token: {r: LISTINGS[r] for r in repos}  # type: ignore[assignment]

        self.assertEqual(set(self._check("t", extra=False)["repos"]), {MAIN})
        self.assertEqual(set(self._check("t", extra=True)["repos"]), {MAIN, EXTRA})

    def test_plan_is_absent_when_a_repo_is_inaccessible(self) -> None:
        def probe(repo: str, token: str) -> tuple[str, str]:
            return (hgd.STATE_NOT_ACCEPTED, "403") if repo == EXTRA else (hgd.STATE_OK, "")

        hgd.probe_repo_access = probe  # type: ignore[assignment]
        hgd.fetch_listings = _explode  # type: ignore[assignment]

        answer = self._check("t", extra=True)
        self.assertEqual(answer["repos"][EXTRA]["state"], hgd.STATE_NOT_ACCEPTED)
        self.assertIsNone(answer["plan"])
        # The state already says what is wrong; `plan_error` is for the case the
        # states CANNOT explain.
        self.assertEqual(answer["plan_error"], "")

    def test_plan_counts_the_missing_bytes(self) -> None:
        hgd.probe_repo_access = lambda repo, token: (hgd.STATE_OK, "")  # type: ignore[assignment]
        hgd.fetch_listings = lambda repos, token: {r: LISTINGS[r] for r in repos}  # type: ignore[assignment]

        answer = self._check("t", extra=False)
        plan = plan_for({MAIN: LISTING_MAIN}, self.root)
        self.assertEqual(answer["plan"]["total_bytes"], sum(item.size for item in plan))
        self.assertEqual(answer["plan"]["missing_bytes"], answer["plan"]["total_bytes"])
        self.assertEqual(answer["plan"]["missing_files"], len(plan))

    def test_a_listing_failure_is_a_null_plan_and_a_plan_error(self) -> None:
        # THE defect this pins: auth succeeds, the listing fails, and a concrete
        # zero-valued plan beside `ok` states would render as a completed
        # installation on an empty machine.
        def offline(repos: Any, token: str) -> Any:
            raise ConnectionError("offline")

        hgd.probe_repo_access = lambda repo, token: (hgd.STATE_OK, "")  # type: ignore[assignment]
        hgd.fetch_listings = offline  # type: ignore[assignment]

        answer = self._check("t", extra=True)
        for repo, entry in answer["repos"].items():
            self.assertEqual(entry["state"], hgd.STATE_OK, repo)
        self.assertIsNone(answer["plan"])
        self.assertIn("offline", answer["plan_error"])

    def test_a_plan_error_never_carries_the_token(self) -> None:
        secret = "hf_" + "p" * 34

        def leaky(repos: Any, token: str) -> Any:
            raise ConnectionError(f"failed with {token}")

        hgd.probe_repo_access = lambda repo, token: (hgd.STATE_OK, "")  # type: ignore[assignment]
        hgd.fetch_listings = leaky  # type: ignore[assignment]

        answer = self._check(secret, extra=False)
        self.assertNotIn(secret, answer["plan_error"])
        self.assertIn("<токен>", answer["plan_error"])

    def test_a_present_plan_comes_with_an_empty_plan_error(self) -> None:
        hgd.probe_repo_access = lambda repo, token: (hgd.STATE_OK, "")  # type: ignore[assignment]
        hgd.fetch_listings = lambda repos, token: {r: LISTINGS[r] for r in repos}  # type: ignore[assignment]

        answer = self._check("t", extra=False)
        self.assertIsNotNone(answer["plan"])
        self.assertEqual(answer["plan_error"], "")

    def test_the_token_never_leaks_into_a_probe_message(self) -> None:
        secret = "hf_" + "s" * 34

        class _Api:
            def __init__(self, token: str | None = None) -> None:
                self._token = token

            def auth_check(self, repo: str) -> None:
                raise RuntimeError(f"leaked {self._token} in the message")

        with _FakeHfApi(_Api):
            state, message = hgd.probe_repo_access(MAIN, secret)
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


class DownloadLoopTests(unittest.TestCase):
    """The transfer itself: skipping, progress, cancellation, resume, the free-space guard."""

    #: A tiny listing so a whole "download" is a handful of KiB. It keeps the
    #: prefix structure, so the plan builder still runs for real.
    SMALL_MAIN = {
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
        self.root = Path(self._tmp.name) / "Model"
        self.target = make_target(self.root)
        self._saved = hgd.fetch_listings
        hgd.fetch_listings = lambda repos, token: {r: self.SMALL_MAIN for r in repos}  # type: ignore[assignment]

    def tearDown(self) -> None:
        hgd.fetch_listings = self._saved  # type: ignore[assignment]
        self._tmp.cleanup()

    def _download(self, token: str = "token", **kwargs: Any) -> dict[str, int]:
        return hgd.download(token, self.target, **kwargs)

    def _requests(self, chunk: int = 1024) -> _FakeRequestsModule:
        """Install a fake `requests` for the block; yields the recorder."""
        return _FakeRequestsModule(_FakeRequests(lambda url: b"x" * 4096, chunk))

    def test_downloads_every_missing_file(self) -> None:
        frames: list[dict[str, Any]] = []

        def progress(phase: str, step: int, total: int, label: str, **extra: Any) -> None:
            frames.append({"phase": phase, "step": step, "total": total, "label": label, **extra})

        with self._requests() as fake:
            result = self._download(progress_callback=progress)

        self.assertEqual(len(fake.calls), len(self.SMALL_MAIN))
        for _url, headers in fake.calls:
            self.assertEqual(headers.get("Authorization"), "Bearer token")
        self.assertEqual(result, {"downloaded_bytes": 4096 * len(self.SMALL_MAIN), "skipped_files": 0})
        for label in self.SMALL_MAIN:
            self.assertTrue((self.root / label).is_file(), label)
        # No staging file survives a successful run.
        self.assertFalse(list(self.root.rglob("*.part")))

        # The last frame always reports the completed byte count.
        self.assertTrue(frames)
        self.assertEqual(frames[-1]["step"], frames[-1]["total"])
        self.assertEqual(frames[-1]["total"], 4096 * len(self.SMALL_MAIN))
        self.assertEqual({frame["phase"] for frame in frames}, {"download"})

    def test_present_files_are_skipped(self) -> None:
        done = self.root / "vae" / "config.json"
        done.parent.mkdir(parents=True, exist_ok=True)
        done.write_bytes(b"y" * 4096)

        with self._requests() as fake:
            result = self._download()

        self.assertEqual(len(fake.calls), len(self.SMALL_MAIN) - 1)
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
                self._download(progress_callback=progress)
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
        def should_cancel() -> bool:
            return len(seen) > 2

        fake = _FakeRequests(payload, 1024)
        with _FakeRequestsModule(fake):
            with self.assertRaises(hgd.DownloadCanceled):
                self._download(should_cancel=should_cancel)

        published = sorted(path.name for path in self.root.rglob("*") if path.is_file() and path.suffix != ".part")
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
        # A body that advertises the full length, delivers 7 bytes and ends
        # NORMALLY raises nothing anywhere in the stack: without the size gate
        # it would be renamed to the final name and skipped forever. A server
        # that keeps doing it exhausts the retry budget instead.
        short = _FakeRequests(lambda url: b"x" * 7, 1024, announced=4096)
        with _FakeRequestsModule(short) as fake:
            with self.assertRaises(md.RetriesExhaustedError) as caught:
                self._download()

        message = str(caught.exception)
        self.assertIn("4096", message)
        self.assertIn("7", message)
        # Nothing reached its final name — that is the whole point.
        self.assertFalse([path for path in self.root.rglob("*") if path.is_file() and path.suffix != ".part"])
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
                self._download()

        plan = plan_for({MAIN: self.SMALL_MAIN}, self.root)
        self.assertEqual(len(hgd.missing_files(plan)), len(self.SMALL_MAIN))

    def test_a_file_without_an_announced_size_is_published_unverified(self) -> None:
        # There is nothing to compare against, so the transfer must still
        # complete rather than fail closed on every run.
        self.SMALL_MAIN = dict(self.SMALL_MAIN)
        self.SMALL_MAIN["LICENSE.md"] = 0
        with self._requests():
            result = self._download()

        self.assertEqual(result["skipped_files"], 0)
        self.assertTrue((self.root / "LICENSE.md").is_file())

    # -- resume ------------------------------------------------------------
    def _one_file_plan(self) -> Path:
        """Leave exactly ONE file of the plan missing, and return its destination.

        Every manifest prefix must still resolve — a prefix that matches nothing
        is an error by design — so the other files are written at their announced
        size instead of being removed from the listing.
        """
        plan = plan_for({MAIN: self.SMALL_MAIN}, self.root)
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
            result = self._download()

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
                self._download(progress_callback=progress)
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
            self._download()

        self.assertEqual(dest.stat().st_size, 4096)
        self.assertEqual(dest.read_bytes(), full)

    def test_a_416_answer_discards_the_partial_and_refetches(self) -> None:
        dest = self._one_file_plan()
        full = b"w" * 4096
        self._park(dest, b"?" * 1000)

        transport = _RangeRequests(full, on_range="416")
        with _FakeRequestsModule(transport):
            self._download()

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
            result = self._download()

        self.assertEqual(dest.read_bytes(), full)
        self.assertEqual(len(transport.calls), 1)
        self.assertEqual(result["skipped_files"], len(self.SMALL_MAIN) - 1)

    def test_leftovers_longer_than_the_announced_size_are_dropped_and_refetched(self) -> None:
        dest = self._one_file_plan()
        full = b"w" * 4096
        self._park(dest, b"?" * 9000)

        transport = _RangeRequests(full)
        with _FakeRequestsModule(transport):
            self._download()

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
            result = self._download()

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
            self._download(progress_callback=progress)

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
                self._download(should_cancel=should_cancel)

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
                self._download(should_cancel=should_cancel)

        parked = dest.with_name(dest.name + ".part")
        self.assertTrue(parked.is_file())
        staged = parked.stat().st_size
        self.assertGreater(staged, 0)
        self.assertLess(staged, 4096)
        self.assertFalse(dest.exists())

        transport = _RangeRequests(full)
        with _FakeRequestsModule(transport):
            self._download()

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
                self._download()
        finally:
            hgd.free_bytes = original  # type: ignore[assignment]

        self.assertEqual(dest.read_bytes(), full)

    def test_free_space_refusal_names_both_numbers(self) -> None:
        original = hgd.free_bytes
        hgd.free_bytes = lambda path: 1000  # type: ignore[assignment]
        try:
            with self._requests() as fake:
                with self.assertRaises(RuntimeError) as caught:
                    self._download()
            self.assertFalse(fake.calls, "no byte may be fetched before the guard passes")
        finally:
            hgd.free_bytes = original  # type: ignore[assignment]
        message = str(caught.exception)
        self.assertIn("1000", message)
        self.assertIn(str(4096 * len(self.SMALL_MAIN) + hgd.FREE_SPACE_MARGIN_BYTES), message)
        self.assertIn(LABEL, message)

    def test_unqueryable_filesystem_is_not_treated_as_full(self) -> None:
        original = hgd.free_bytes
        hgd.free_bytes = lambda path: -1  # type: ignore[assignment]
        try:
            with self._requests():
                self._download()
        finally:
            hgd.free_bytes = original  # type: ignore[assignment]

    def test_empty_token_is_refused_without_a_network_call(self) -> None:
        hgd.fetch_listings = _explode  # type: ignore[assignment]
        with self.assertRaises(ValueError) as caught:
            self._download("  ")
        self.assertEqual(str(caught.exception), NO_TOKEN_MESSAGE)


class FreeSpaceTests(unittest.TestCase):
    """The guard itself, independent of a transfer."""

    def test_nothing_required_is_always_allowed(self) -> None:
        hgd.require_free_space(Path(os.sep), 0, label=LABEL)

    def test_existing_ancestor_walks_up_to_a_real_directory(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            missing = Path(tmp) / "a" / "b" / "c"
            self.assertEqual(hgd._existing_ancestor(missing), Path(tmp))
            self.assertGreater(hgd.free_bytes(missing), 0)


if __name__ == "__main__":
    unittest.main()
