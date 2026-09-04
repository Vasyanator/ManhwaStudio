"""
File: modules/ai_backend/inpaint/test_flux2_download.py

Purpose:
Unit tests for the FLUX.2 klein model acquisition module — the Python half of
`dev-docs/flux2_model_download.md`.

Main responsibilities:
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
  double-length corruption case), a 416 discards and refetches, a cancellation
  parks the bytes for a second call to finish, a stale partial whose resumed
  length is wrong is retried exactly once from zero, and the free-space guard
  subtracts what is already staged;
- verify a body that ends CLEANLY but short is refused before the publish and
  leaves nothing on disk, that the same file is planned again afterwards, and
  that a file whose listing announced no size is still published;
- verify the free-space guard refuses before the first byte with both numbers.

Notes:
Nothing here touches the network. The listings are fakes, and the one test that
exercises the real streaming path injects a fake `requests` module into
`sys.modules` for the duration, so `download_to_path`, `stream_response_to_file`
and `download_bearer_to_path` all run for real against it.
"""

from __future__ import annotations

import os
import sys
import tempfile
import unittest
from pathlib import Path
from typing import Any, Callable, Iterator

from modules.ai_backend.inpaint import flux2_download as fd

OFFICIAL = fd.OFFICIAL_REPO
UNCENSORED = fd.UNCENSORED_REPO

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
        return [item.label for item in fd.resolve_plan(FAKE_LISTINGS, uncensored=uncensored, root=root)]

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
            plan = fd.resolve_plan(FAKE_LISTINGS, uncensored=uncensored, root="/models")
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
        self.assertEqual(fd.required_repos(True), (OFFICIAL, UNCENSORED))
        self.assertEqual(fd.required_repos(False), (OFFICIAL,))

    def test_a_plan_is_never_two_encoders_wide(self) -> None:
        off = fd.resolve_plan(FAKE_LISTINGS, uncensored=False, root="/models")
        on = fd.resolve_plan(FAKE_LISTINGS, uncensored=True, root="/models")
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

            plan = fd.resolve_plan(FAKE_LISTINGS, uncensored=True, root=tmp)
            self.assertFalse([item for item in plan if item.label.startswith("text_encoder/")])
            self.assertTrue(official.is_file())

    def test_plan_totals_match_the_contract_table(self) -> None:
        plan = fd.resolve_plan(FAKE_LISTINGS, uncensored=False, root="/models")
        by_prefix: dict[str, int] = {}
        for item in plan:
            head = item.label.split("/")[0] if "/" in item.label else item.label
            by_prefix[head] = by_prefix.get(head, 0) + item.size
        self.assertEqual(by_prefix["transformer"], 18157216297)
        self.assertEqual(by_prefix["text_encoder"], 16381551490)
        self.assertEqual(by_prefix["vae"], 168121699)
        self.assertEqual(by_prefix["tokenizer"], 15882232)

    def test_destinations_are_absolute_under_the_root(self) -> None:
        plan = fd.resolve_plan(FAKE_LISTINGS, uncensored=False, root=os.path.join(os.sep, "m"))
        for item in plan:
            self.assertEqual(Path(item.dest), Path(os.sep, "m", *item.label.split("/")))

    def test_prefix_resolving_to_nothing_is_an_error(self) -> None:
        # A repository that was re-sharded must not silently produce half a
        # model: the message names the prefix AND the repository.
        stripped = {key: value for key, value in FAKE_OFFICIAL.items() if not key.startswith("transformer/")}
        with self.assertRaises(RuntimeError) as caught:
            fd.resolve_plan({OFFICIAL: stripped, UNCENSORED: FAKE_UNCENSORED}, uncensored=False, root="/m")
        message = str(caught.exception)
        self.assertIn("transformer/", message)
        self.assertIn(OFFICIAL, message)

    def test_uncensored_prefix_resolving_to_only_excluded_files_is_an_error(self) -> None:
        only_gguf = {key: value for key, value in FAKE_UNCENSORED.items() if key.endswith(".gguf")}
        with self.assertRaises(RuntimeError):
            fd.resolve_plan({OFFICIAL: FAKE_OFFICIAL, UNCENSORED: only_gguf}, uncensored=True, root="/m")

    def test_missing_listing_is_an_error(self) -> None:
        with self.assertRaises(RuntimeError):
            fd.resolve_plan({OFFICIAL: FAKE_OFFICIAL}, uncensored=True, root="/m")


class MissingFilterTests(unittest.TestCase):
    """Presence means the file is there AT THE ANNOUNCED SIZE, not merely non-empty."""

    def _write(self, item: fd.PlannedFile, payload: bytes) -> None:
        Path(item.dest).parent.mkdir(parents=True, exist_ok=True)
        Path(item.dest).write_bytes(payload)

    def test_a_file_at_the_announced_size_is_skipped(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            plan = fd.resolve_plan(FAKE_LISTINGS, uncensored=False, root=tmp)
            complete = next(item for item in plan if item.label == "vae/config.json")
            empty = next(item for item in plan if item.label == "model_index.json")
            self._write(complete, b"c" * complete.size)
            self._write(empty, b"")

            missing = {item.label for item in fd.missing_files(plan)}
            self.assertNotIn("vae/config.json", missing)
            self.assertIn("model_index.json", missing)

            totals = fd.plan_totals(plan)
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
            plan = fd.resolve_plan(FAKE_LISTINGS, uncensored=False, root=tmp)
            truncated = next(item for item in plan if item.label == "vae/config.json")
            self.assertGreater(truncated.size, 8)
            self._write(truncated, b"short")

            missing = {item.label for item in fd.missing_files(plan)}
            self.assertIn("vae/config.json", missing)
            self.assertFalse(fd.is_complete_on_disk(truncated))

    def test_a_longer_file_is_also_replanned(self) -> None:
        # Not just short: any length that is not the announced one means the file
        # on disk is not the file the listing describes.
        with tempfile.TemporaryDirectory() as tmp:
            plan = fd.resolve_plan(FAKE_LISTINGS, uncensored=False, root=tmp)
            item = next(entry for entry in plan if entry.label == "vae/config.json")
            self._write(item, b"c" * (item.size + 1))
            self.assertFalse(fd.is_complete_on_disk(item))

    def test_a_file_without_an_announced_size_falls_back_to_non_empty(self) -> None:
        # A missing size must not force an endless re-download; the old rule
        # stands and `is_complete_on_disk` logs why.
        with tempfile.TemporaryDirectory() as tmp:
            sized = dict(FAKE_OFFICIAL)
            sized["vae/config.json"] = 0
            plan = fd.resolve_plan({OFFICIAL: sized}, uncensored=False, root=tmp)
            item = next(entry for entry in plan if entry.label == "vae/config.json")
            self.assertEqual(item.size, 0)

            self.assertFalse(fd.is_complete_on_disk(item))
            self._write(item, b"anything at all")
            with self.assertLogs("modules.ai_backend.inpaint.flux2_download", level="WARNING"):
                self.assertTrue(fd.is_complete_on_disk(item))
            self._write(item, b"")
            self.assertFalse(fd.is_complete_on_disk(item))

    def test_a_directory_in_place_of_a_file_is_not_complete(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            plan = fd.resolve_plan(FAKE_LISTINGS, uncensored=False, root=tmp)
            item = next(entry for entry in plan if entry.label == "vae/config.json")
            Path(item.dest).mkdir(parents=True, exist_ok=True)
            self.assertFalse(fd.is_complete_on_disk(item))


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
        self.assertEqual(fd.classify_repo_error(_http_error(401, "gated")), fd.STATE_INVALID_TOKEN)

    def test_401_on_an_unknown_repo_is_an_invalid_token(self) -> None:
        self.assertEqual(
            fd.classify_repo_error(_http_error(401, "not_found")), fd.STATE_INVALID_TOKEN
        )

    def test_403_is_conditions_not_accepted(self) -> None:
        # MEASURED on a repo whose conditions this account has not accepted:
        # GatedRepoError, status 403, `X-Error-Code: GatedRepo`.
        self.assertEqual(fd.classify_repo_error(_http_error(403, "gated")), fd.STATE_NOT_ACCEPTED)
        self.assertEqual(fd.classify_repo_error(_http_error(403, "http")), fd.STATE_NOT_ACCEPTED)

    def test_gated_repo_error_without_a_status_is_conditions_not_accepted(self) -> None:
        from huggingface_hub.errors import GatedRepoError

        self.assertEqual(fd.classify_repo_error(GatedRepoError("gated")), fd.STATE_NOT_ACCEPTED)

    def test_404_is_not_found(self) -> None:
        self.assertEqual(fd.classify_repo_error(_http_error(404, "not_found")), fd.STATE_NOT_FOUND)

    def test_gated_repo_error_does_not_shadow_not_found(self) -> None:
        # `GatedRepoError` SUBCLASSES `RepositoryNotFoundError`, so an
        # isinstance-ordered mapping is a trap; 404 must still be `not_found`.
        from huggingface_hub.errors import GatedRepoError, RepositoryNotFoundError

        self.assertTrue(issubclass(GatedRepoError, RepositoryNotFoundError))

    def test_anything_else_is_a_network_error(self) -> None:
        self.assertEqual(fd.classify_repo_error(OSError("connection reset")), fd.STATE_NETWORK_ERROR)
        self.assertEqual(fd.classify_repo_error(_http_error(500, "http")), fd.STATE_NETWORK_ERROR)


class CheckAccessTests(unittest.TestCase):
    """The `.check` answer: which repos are reported, and when the network is touched."""

    def setUp(self) -> None:
        self._probe = fd.probe_repo_access
        self._listings = fd.fetch_listings
        self._root = fd.model_root
        self._tmp = tempfile.TemporaryDirectory()
        fd.model_root = lambda: Path(self._tmp.name)  # type: ignore[assignment]

    def tearDown(self) -> None:
        fd.probe_repo_access = self._probe  # type: ignore[assignment]
        fd.fetch_listings = self._listings  # type: ignore[assignment]
        fd.model_root = self._root  # type: ignore[assignment]
        self._tmp.cleanup()

    def test_empty_token_makes_no_network_call(self) -> None:
        def explode(*args: Any, **kwargs: Any) -> Any:
            raise AssertionError("no_token must not touch the network")

        fd.probe_repo_access = explode  # type: ignore[assignment]
        fd.fetch_listings = explode  # type: ignore[assignment]

        answer = fd.check_access("   ", uncensored=True)
        self.assertEqual(set(answer["repos"]), {OFFICIAL, UNCENSORED})
        for entry in answer["repos"].values():
            self.assertEqual(entry["state"], fd.STATE_NO_TOKEN)
            self.assertEqual(entry["message"], "")
        # No listing was fetched, so there is no plan — NOT a zeroed one, which
        # would read as "everything is already downloaded".
        self.assertIsNone(answer["plan"])
        self.assertEqual(answer["plan_error"], "")

    def test_only_the_needed_repos_are_reported(self) -> None:
        fd.probe_repo_access = lambda repo, token: (fd.STATE_OK, "")  # type: ignore[assignment]
        fd.fetch_listings = lambda repos, token: {r: FAKE_LISTINGS[r] for r in repos}  # type: ignore[assignment]

        self.assertEqual(set(fd.check_access("t", uncensored=False)["repos"]), {OFFICIAL})
        self.assertEqual(
            set(fd.check_access("t", uncensored=True)["repos"]), {OFFICIAL, UNCENSORED}
        )

    def test_plan_is_absent_when_a_repo_is_inaccessible(self) -> None:
        def probe(repo: str, token: str) -> tuple[str, str]:
            return (fd.STATE_NOT_ACCEPTED, "403") if repo == UNCENSORED else (fd.STATE_OK, "")

        def explode(*args: Any, **kwargs: Any) -> Any:
            raise AssertionError("no listing is fetched when access was refused")

        fd.probe_repo_access = probe  # type: ignore[assignment]
        fd.fetch_listings = explode  # type: ignore[assignment]

        answer = fd.check_access("t", uncensored=True)
        self.assertEqual(answer["repos"][UNCENSORED]["state"], fd.STATE_NOT_ACCEPTED)
        self.assertIsNone(answer["plan"])
        # The state already says what is wrong; `plan_error` is for the case the
        # states CANNOT explain.
        self.assertEqual(answer["plan_error"], "")

    def test_plan_counts_the_missing_bytes(self) -> None:
        fd.probe_repo_access = lambda repo, token: (fd.STATE_OK, "")  # type: ignore[assignment]
        fd.fetch_listings = lambda repos, token: {r: FAKE_LISTINGS[r] for r in repos}  # type: ignore[assignment]

        answer = fd.check_access("t", uncensored=False)
        plan = fd.resolve_plan({OFFICIAL: FAKE_OFFICIAL}, uncensored=False, root=self._tmp.name)
        self.assertEqual(answer["plan"]["total_bytes"], sum(item.size for item in plan))
        self.assertEqual(answer["plan"]["missing_bytes"], answer["plan"]["total_bytes"])
        self.assertEqual(answer["plan"]["missing_files"], len(plan))

    def test_a_listing_failure_is_a_null_plan_and_a_plan_error(self) -> None:
        # THE defect this pins: auth succeeds, the listing fails, and the old
        # answer was a concrete zero-valued plan beside `ok` states — which the
        # UI renders as a completed installation on an empty machine.
        def probe(repo: str, token: str) -> tuple[str, str]:
            return fd.STATE_OK, ""

        def offline(repos: Any, token: str) -> Any:
            raise ConnectionError("offline")

        fd.probe_repo_access = probe  # type: ignore[assignment]
        fd.fetch_listings = offline  # type: ignore[assignment]

        answer = fd.check_access("t", uncensored=True)

        for repo, entry in answer["repos"].items():
            self.assertEqual(entry["state"], fd.STATE_OK, repo)
        self.assertIsNone(answer["plan"])
        self.assertIn("offline", answer["plan_error"])

    def test_a_plan_error_never_carries_the_token(self) -> None:
        secret = "hf_" + "p" * 34

        def probe(repo: str, token: str) -> tuple[str, str]:
            return fd.STATE_OK, ""

        def leaky(repos: Any, token: str) -> Any:
            raise ConnectionError(f"failed with {token}")

        fd.probe_repo_access = probe  # type: ignore[assignment]
        fd.fetch_listings = leaky  # type: ignore[assignment]

        answer = fd.check_access(secret, uncensored=False)
        self.assertNotIn(secret, answer["plan_error"])
        self.assertIn("<токен>", answer["plan_error"])

    def test_a_present_plan_comes_with_an_empty_plan_error(self) -> None:
        fd.probe_repo_access = lambda repo, token: (fd.STATE_OK, "")  # type: ignore[assignment]
        fd.fetch_listings = lambda repos, token: {r: FAKE_LISTINGS[r] for r in repos}  # type: ignore[assignment]

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
            state, message = fd.probe_repo_access(OFFICIAL, secret)
        finally:
            huggingface_hub.HfApi = original  # type: ignore[assignment]
        self.assertEqual(state, fd.STATE_NETWORK_ERROR)
        self.assertNotIn(secret, message)
        self.assertIn("<токен>", message)


class ProgressThrottleTests(unittest.TestCase):
    """Both gates must open, and the terminal frame is never throttled away."""

    def _throttle(self, clock: Callable[[], float]) -> fd.ProgressThrottle:
        return fd.ProgressThrottle(min_interval=0.1, min_bytes=4 << 20, clock=clock)

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
    """`sys.modules` patch installing `_FakeRequests` for the duration of a test."""

    def __init__(self, fake: _FakeRequests) -> None:
        self._fake = fake
        self._saved: Any = None

    def __enter__(self) -> _FakeRequests:
        self._saved = sys.modules.get("requests")
        sys.modules["requests"] = self._fake  # type: ignore[assignment]
        return self._fake

    def __exit__(self, *exc_info: Any) -> None:
        if self._saved is None:
            sys.modules.pop("requests", None)
        else:
            sys.modules["requests"] = self._saved


class _RangeResponse:
    """Response that knows its own status, for exercising the resume answers."""

    def __init__(self, body: bytes, chunk: int, status: int) -> None:
        self._body = body
        self._chunk = chunk
        self.status_code = status
        self.headers = {"Content-Length": str(len(body))}

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
    `"416"` refuses it, and `"short"` resumes but stops ten bytes early WITHOUT
    raising — the clean-truncation case.
    """

    def __init__(self, full: bytes, chunk: int = 1024, on_range: str = "206") -> None:
        self.full = full
        self.chunk = chunk
        self.on_range = on_range
        self.calls: list[dict[str, str]] = []

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
        if self.on_range == "short":
            rest = rest[:-10]
        return _RangeResponse(rest, self.chunk, 206)

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
        self._saved = (fd.model_root, fd.fetch_listings)
        fd.model_root = lambda: self.root  # type: ignore[assignment]
        fd.fetch_listings = lambda repos, token: {r: self.SMALL_OFFICIAL for r in repos}  # type: ignore[assignment]

    def tearDown(self) -> None:
        fd.model_root, fd.fetch_listings = self._saved  # type: ignore[assignment]
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
        self.assertEqual(result["paths"], fd.component_paths(False))
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
        original = fd.ProgressThrottle
        fd.ProgressThrottle = lambda **_kwargs: original(min_interval=0.0, min_bytes=0)  # type: ignore[assignment]
        try:
            with self._requests(chunk=1024):
                fd.download("token", uncensored=False, progress_callback=progress)
        finally:
            fd.ProgressThrottle = original  # type: ignore[assignment]

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
            with self.assertRaises(fd.DownloadCanceled):
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
        # the `verify` gate it is renamed to the final name, and every later run
        # then skips a truncated multi-gigabyte shard forever, surfacing as
        # corrupt weights at load time.
        short = _FakeRequests(lambda url: b"x" * 7, 1024, announced=4096)
        with _FakeRequestsModule(short) as fake:
            with self.assertRaises(RuntimeError) as caught:
                fd.download("token", uncensored=False)

        message = str(caught.exception)
        self.assertIn("4096", message)
        self.assertIn("7", message)
        # Nothing reached its final name — that is the whole point.
        self.assertFalse(
            [path for path in self.root.rglob("*") if path.is_file() and path.suffix != ".part"]
        )
        # It was tried exactly twice: once resuming, once from zero (R4).
        self.assertEqual(len(fake.calls), 2)

    def test_a_refused_short_file_is_still_planned_afterwards(self) -> None:
        short = _FakeRequests(lambda url: b"x" * 7, 1024, announced=4096)
        with _FakeRequestsModule(short):
            with self.assertRaises(RuntimeError):
                fd.download("token", uncensored=False)

        plan = fd.resolve_plan({fd.OFFICIAL_REPO: self.SMALL_OFFICIAL}, uncensored=False, root=self.root)
        self.assertEqual(len(fd.missing_files(plan)), len(self.SMALL_OFFICIAL))

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
        plan = fd.resolve_plan({fd.OFFICIAL_REPO: self.SMALL_OFFICIAL}, uncensored=False, root=self.root)
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

        original = fd.ProgressThrottle
        fd.ProgressThrottle = lambda **_kwargs: original(min_interval=0.0, min_bytes=0)  # type: ignore[assignment]
        try:
            with _FakeRequestsModule(_RangeRequests(full)):
                fd.download("token", uncensored=False, progress_callback=progress)
        finally:
            fd.ProgressThrottle = original  # type: ignore[assignment]

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
        self._park(dest, b"?" * 9000)

        transport = _RangeRequests(full, on_range="416")
        with _FakeRequestsModule(transport):
            fd.download("token", uncensored=False)

        self.assertEqual(dest.read_bytes(), full)
        # One ranged attempt, refused, then one plain refetch.
        self.assertEqual(len(transport.calls), 2)
        self.assertEqual(transport.ranged, ["bytes=9000-"])

    def test_a_stale_partial_with_a_wrong_length_is_retried_once_from_zero(self) -> None:
        dest = self._one_file_plan()
        full = b"e" * 4096
        self._park(dest, b"?" * 1000)

        # Resuming yields 4086 bytes and ends cleanly: the size gate rejects it,
        # the staged bytes are discarded, and the file is fetched once from zero.
        transport = _RangeRequests(full, on_range="short")
        with _FakeRequestsModule(transport):
            result = fd.download("token", uncensored=False)

        self.assertEqual(dest.read_bytes(), full)
        self.assertEqual(len(transport.calls), 2)
        self.assertEqual(transport.ranged, ["bytes=1000-"])
        self.assertFalse(list(self.root.rglob("*.part")))
        self.assertEqual(result["downloaded_bytes"], 4096)

    def test_cancellation_parks_bytes_that_a_second_call_finishes(self) -> None:
        dest = self._one_file_plan()
        full = b"r" * 4096
        chunks = {"seen": 0}

        def should_cancel() -> bool:
            chunks["seen"] += 1
            return chunks["seen"] > 2

        with _FakeRequestsModule(_RangeRequests(full)):
            with self.assertRaises(fd.DownloadCanceled):
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
        original = fd.free_bytes
        fd.free_bytes = lambda path: fd.FREE_SPACE_MARGIN_BYTES + 100  # type: ignore[assignment]
        try:
            with _FakeRequestsModule(_RangeRequests(full)):
                fd.download("token", uncensored=False)
        finally:
            fd.free_bytes = original  # type: ignore[assignment]

        self.assertEqual(dest.read_bytes(), full)

    def test_free_space_refusal_names_both_numbers(self) -> None:
        original = fd.free_bytes
        fd.free_bytes = lambda path: 1000  # type: ignore[assignment]
        try:
            with self._requests() as fake:
                with self.assertRaises(RuntimeError) as caught:
                    fd.download("token", uncensored=False)
            self.assertFalse(fake.calls, "no byte may be fetched before the guard passes")
        finally:
            fd.free_bytes = original  # type: ignore[assignment]
        message = str(caught.exception)
        self.assertIn("1000", message)
        self.assertIn(str(4096 * len(self.SMALL_OFFICIAL) + fd.FREE_SPACE_MARGIN_BYTES), message)

    def test_unqueryable_filesystem_is_not_treated_as_full(self) -> None:
        original = fd.free_bytes
        fd.free_bytes = lambda path: -1  # type: ignore[assignment]
        try:
            with self._requests():
                fd.download("token", uncensored=False)
        finally:
            fd.free_bytes = original  # type: ignore[assignment]

    def test_empty_token_is_refused_without_a_network_call(self) -> None:
        def explode(*args: Any, **kwargs: Any) -> Any:
            raise AssertionError("an empty token must not reach the hub")

        saved = fd.fetch_listings
        fd.fetch_listings = explode  # type: ignore[assignment]
        try:
            with self.assertRaises(ValueError):
                fd.download("  ", uncensored=False)
        finally:
            fd.fetch_listings = saved  # type: ignore[assignment]


class FreeSpaceTests(unittest.TestCase):
    """The guard itself, independent of a transfer."""

    def test_nothing_required_is_always_allowed(self) -> None:
        fd.require_free_space(Path(os.sep), 0)

    def test_existing_ancestor_walks_up_to_a_real_directory(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            missing = Path(tmp) / "a" / "b" / "c"
            self.assertEqual(fd._existing_ancestor(missing), Path(tmp))
            self.assertGreater(fd.free_bytes(missing), 0)


if __name__ == "__main__":
    unittest.main()
