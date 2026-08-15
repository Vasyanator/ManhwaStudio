"""
File: modules/ai_backend/watermark/test_code_fetch.py

Purpose:
Unit tests for the runtime fetch of the upstream watermark-removal network code.

Main responsibilities:
- verify the pinned manifest is self-consistent (full commit SHAs, hex digests,
  no `__init__.py`, dependency order, every registered name inside the model's
  owned namespace) and that unknown model ids are rejected everywhere;
- verify a hash mismatch is fatal, leaves nothing behind and never renames the
  `.part` file into place — this is executable code arriving over the network;
- verify a matching download lands atomically and is reported as ready;
- verify `ensure_model_code` re-downloads a corrupted file and is otherwise a
  no-op;
- verify the loader executes only bytes it has just hashed (a file tampered with
  after a passing `is_code_ready()` is still refused), never reuses a
  pre-existing `src` / `unet_parts` / `tensorboardX`, and restores `sys.modules`
  to its previous contents afterwards;
- verify the per-model size/square contracts and the on-disk layout.

Notes:
- A fake `requests` module is injected into `sys.modules` with `patch.dict` +
  `addCleanup`, so no test needs network access, torch, or weights.
- The download root is redirected to a temporary directory by patching
  `_watermark_dir`, so no test writes into the user's model tree.
- The real manifest pins the real upstream digests, which an offline test cannot
  reproduce. The download tests therefore substitute a manifest whose sizes are
  unchanged and whose digests describe synthetic payloads; the verification code
  under test runs unmodified.
- The load tests substitute a synthetic closure with the same SHAPE as the real
  one (a package tree for SLBR/SplitNet, top-level modules for WDNet) so the
  import machinery runs unmodified without torch.
"""

from __future__ import annotations

import hashlib
import sys
import tempfile
import types
import unittest
from pathlib import Path
from unittest.mock import patch

from modules.ai_backend.watermark import code_fetch as cf


class _FakeResponse:
    """Minimal stand-in for `requests.Response` used as a context manager."""

    def __init__(self, payload: bytes, status: int = 200) -> None:
        self._payload = payload
        self._status = status
        self.headers: dict[str, str] = {"Content-Length": str(len(payload))}

    def __enter__(self) -> "_FakeResponse":
        return self

    def __exit__(self, *_exc_info: object) -> bool:
        return False

    def raise_for_status(self) -> None:
        if self._status >= 400:
            raise RuntimeError(f"HTTP {self._status}")

    def iter_content(self, chunk_size: int = 1 << 16):
        for start in range(0, len(self._payload), chunk_size):
            yield self._payload[start : start + chunk_size]


def _fake_requests(payload_for: dict[str, bytes]) -> types.ModuleType:
    """A `requests` module serving `payload_for[url]`.

    A URL absent from the mapping raises, which is how a test asserts that
    nothing unexpected was fetched.
    """

    def get(url: str, **_kwargs: object) -> _FakeResponse:
        if url not in payload_for:
            raise AssertionError(f"unexpected download: {url}")
        return _FakeResponse(payload_for[url])

    module = types.ModuleType("requests")
    module.get = get
    return module


def _deterministic_payload(path: str, size: int) -> bytes:
    """`size` bytes derived from `path`, stable across runs."""
    seed = hashlib.sha256(path.encode("utf-8")).digest()
    out = bytearray()
    while len(out) < size:
        seed = hashlib.sha256(seed).digest()
        out.extend(seed)
    return bytes(out[:size])


class ManifestTests(unittest.TestCase):
    """The pinned manifest is itself a contract; a typo in it is a security bug."""

    def test_model_ids_match_the_repo_table(self) -> None:
        self.assertEqual(cf.MODEL_IDS, ("slbr", "wdnet", "splitnet"))
        self.assertEqual(set(cf.MODEL_IDS), set(cf._REPOS))

    def test_commits_are_full_shas_not_branch_names(self) -> None:
        for model_id in cf.MODEL_IDS:
            with self.subTest(model=model_id):
                self.assertRegex(cf.pinned_commit(model_id), r"^[0-9a-f]{40}$")

    def test_every_entry_carries_a_hex_digest_and_a_positive_size(self) -> None:
        for model_id in cf.MODEL_IDS:
            for path, size, digest in cf.manifest(model_id):
                with self.subTest(model=model_id, path=path):
                    self.assertGreater(size, 0)
                    self.assertRegex(digest, r"^[0-9a-f]{64}$")

    def test_no_upstream_init_files_are_fetched(self) -> None:
        # The upstream package inits pull tensorboardX / pytorch_ssim /
        # scipy.misc.imread, none of which exist in this environment.
        for model_id in cf.MODEL_IDS:
            for path, _size, _digest in cf.manifest(model_id):
                self.assertNotIn("__init__.py", path, f"{model_id}: {path}")

    def test_unknown_model_is_rejected_everywhere(self) -> None:
        for call in (
            cf.code_root,
            cf.model_dir,
            cf.manifest,
            cf.pinned_commit,
            cf.input_size_multiple,
            cf.requires_square_input,
            cf.is_code_ready,
            cf.ensure_model_code,
            cf.build_network,
        ):
            with self.subTest(call=call.__name__):
                with self.assertRaises(ValueError):
                    call("bogus")

    def test_size_and_shape_contracts(self) -> None:
        self.assertEqual(cf.input_size_multiple("slbr"), 16)
        self.assertEqual(cf.input_size_multiple("splitnet"), 16)
        self.assertEqual(cf.input_size_multiple("wdnet"), 1)
        self.assertTrue(cf.requires_square_input("slbr"))
        self.assertFalse(cf.requires_square_input("wdnet"))
        self.assertFalse(cf.requires_square_input("splitnet"))


class _TempRootTestCase(unittest.TestCase):
    """Base class redirecting the download root into a temporary directory."""

    def setUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)
        root_patch = patch.object(cf, "_watermark_dir", lambda: self.root)
        root_patch.start()
        self.addCleanup(root_patch.stop)

    @staticmethod
    def _url_for(model_id: str, path: str) -> str:
        spec = cf._REPOS[model_id]
        return cf._RAW_URL.format(
            owner=spec.owner, repo=spec.repo, commit=spec.commit, path=path
        )


class LayoutTests(_TempRootTestCase):
    def test_code_root_reproduces_the_upstream_path(self) -> None:
        root = cf.code_root("slbr")
        self.assertEqual(root, self.root / "slbr" / "src")
        # SLBR's files are `src/networks/*.py` upstream, so the checkout root
        # name and the upstream prefix stack up. That is intentional.
        self.assertEqual(
            root / Path("src/networks/resunet.py"),
            self.root / "slbr" / "src" / "src" / "networks" / "resunet.py",
        )

    def test_model_dir_is_the_parent_of_the_code_root(self) -> None:
        self.assertEqual(cf.model_dir("wdnet"), cf.code_root("wdnet").parent)


class DownloadTests(_TempRootTestCase):
    """`wdnet` is used throughout: two files, the smallest closure."""

    MODEL = "wdnet"

    def setUp(self) -> None:
        super().setUp()
        real = cf._REPOS[self.MODEL]
        self.payloads: dict[str, bytes] = {}
        files = []
        for entry in real.files:
            payload = _deterministic_payload(entry.path, entry.size)
            files.append(
                cf._RemoteFile(
                    entry.path, entry.size, hashlib.sha256(payload).hexdigest(), entry.module
                )
            )
            self.payloads[self._url_for(self.MODEL, entry.path)] = payload
        spec = cf._RepoSpec(real.owner, real.repo, real.commit, tuple(files))

        repos_patch = patch.dict(cf._REPOS, {self.MODEL: spec})
        repos_patch.start()
        self.addCleanup(repos_patch.stop)

    def _with_requests(self, payloads: dict[str, bytes] | None = None):
        return patch.dict(
            sys.modules,
            {"requests": _fake_requests(self.payloads if payloads is None else payloads)},
        )

    def test_matching_download_lands_atomically_and_reports_ready(self) -> None:
        with self._with_requests():
            self.assertFalse(cf.is_code_ready(self.MODEL))
            cf.ensure_model_code(self.MODEL)

        self.assertTrue(cf.is_code_ready(self.MODEL))
        root = cf.code_root(self.MODEL)
        self.assertTrue((root / "WDNet.py").is_file())
        self.assertTrue((root / "unet_parts.py").is_file())
        self.assertEqual(list(root.glob("*.part")), [])

    def test_progress_is_reported_in_bytes_under_the_download_phase(self) -> None:
        frames: list[tuple[str, int, int, str]] = []
        with self._with_requests():
            cf.ensure_model_code(self.MODEL, lambda *frame: frames.append(frame))

        self.assertTrue(frames)
        self.assertEqual({frame[0] for frame in frames}, {"download"})
        total = sum(size for _path, size, _digest in cf.manifest(self.MODEL))
        self.assertEqual(frames[-1][1], total)
        self.assertEqual(frames[-1][2], total)

    def test_hash_mismatch_is_fatal_and_leaves_nothing_behind(self) -> None:
        payloads = dict(self.payloads)
        url = self._url_for(self.MODEL, "WDNet.py")
        # Same length, different bytes: only the digest can catch this.
        payloads[url] = b"\x00" * len(payloads[url])

        with self._with_requests(payloads):
            with self.assertRaises(RuntimeError) as caught:
                cf.ensure_model_code(self.MODEL)

        self.assertIn("целостности", str(caught.exception))
        root = cf.code_root(self.MODEL)
        self.assertFalse((root / "WDNet.py").exists())
        self.assertFalse((root / "WDNet.py.part").exists())
        self.assertFalse(cf.is_code_ready(self.MODEL))

    def test_truncated_download_is_rejected(self) -> None:
        payloads = dict(self.payloads)
        url = self._url_for(self.MODEL, "unet_parts.py")
        payloads[url] = payloads[url][:100]

        with self._with_requests(payloads):
            with self.assertRaises(RuntimeError):
                cf.ensure_model_code(self.MODEL)

        self.assertFalse((cf.code_root(self.MODEL) / "unet_parts.py").exists())

    def test_second_call_downloads_nothing(self) -> None:
        with self._with_requests():
            cf.ensure_model_code(self.MODEL)

        # An empty payload map turns any request into an assertion failure.
        with self._with_requests({}):
            cf.ensure_model_code(self.MODEL)

        self.assertTrue(cf.is_code_ready(self.MODEL))

    def test_corrupted_file_on_disk_is_re_downloaded(self) -> None:
        with self._with_requests():
            cf.ensure_model_code(self.MODEL)
            target = cf.code_root(self.MODEL) / "WDNet.py"
            target.write_bytes(b"\x01" * target.stat().st_size)
            self.assertFalse(cf.is_code_ready(self.MODEL))
            cf.ensure_model_code(self.MODEL)

        self.assertTrue(cf.is_code_ready(self.MODEL))

    def test_build_network_refuses_an_incomplete_checkout(self) -> None:
        with self.assertRaises(FileNotFoundError):
            cf.build_network(self.MODEL)


class PackageStubTests(unittest.TestCase):
    """`_make_pkg` is what keeps this repo's own Rust `src/` out of the package."""

    def test_pinned_path_has_exactly_one_entry(self) -> None:
        with patch.dict(sys.modules, {}, clear=False):
            created = cf._make_pkg("ms_watermark_stub_pkg", Path("/nowhere/one"))
            self.addCleanup(sys.modules.pop, "ms_watermark_stub_pkg", None)
            self.assertEqual(list(created.__path__), ["/nowhere/one"])

    def test_child_is_bound_onto_its_parent(self) -> None:
        cf._make_pkg("ms_watermark_stub_parent", Path("/nowhere/a"))
        self.addCleanup(sys.modules.pop, "ms_watermark_stub_parent", None)
        child = cf._make_pkg("ms_watermark_stub_parent.leaf", Path("/nowhere/a/leaf"))
        self.addCleanup(sys.modules.pop, "ms_watermark_stub_parent.leaf", None)

        parent = sys.modules["ms_watermark_stub_parent"]
        self.assertIs(getattr(parent, "leaf"), child)

    def test_an_existing_entry_is_replaced_never_reused(self) -> None:
        # Reusing whatever sits under a generic name is exactly how unverified
        # code would get a foothold; the caller snapshots and restores instead.
        foreign = types.ModuleType("ms_watermark_stub_foreign")
        foreign.__path__ = ["/nowhere/b"]  # type: ignore[attr-defined]
        with patch.dict(sys.modules, {"ms_watermark_stub_foreign": foreign}):
            fresh = cf._make_pkg("ms_watermark_stub_foreign", Path("/nowhere/b"))
            self.assertIsNot(fresh, foreign)
            self.assertIs(sys.modules["ms_watermark_stub_foreign"], fresh)

    def test_missing_parent_is_an_explicit_error(self) -> None:
        with self.assertRaises(RuntimeError):
            cf._make_pkg("ms_watermark_absent_parent.child", Path("/nowhere/c"))
        sys.modules.pop("ms_watermark_absent_parent.child", None)


class ModuleStubTests(unittest.TestCase):
    """Training-only imports are faked deterministically, never trusted from the process."""

    def test_wdnet_stubs_are_created_and_reported(self) -> None:
        created = cf._install_module_stubs("wdnet")
        self.addCleanup(lambda: [sys.modules.pop(n, None) for n in created])

        self.assertEqual(created, {"dataloader", "tensorboardX", "vgg"})
        self.assertIsNone(sys.modules["tensorboardX"].SummaryWriter)

    def test_a_pre_existing_module_is_shadowed_for_the_import(self) -> None:
        # `vgg` / `dataloader` / `tensorboardX` are generic enough that a foreign
        # module of that name would otherwise be executed by the closure's
        # `from vgg import Vgg16`. It is shadowed here and restored by the caller.
        sentinel = types.ModuleType("tensorboardX")
        with patch.dict(sys.modules, {"tensorboardX": sentinel}):
            created = cf._install_module_stubs("wdnet")
            self.assertIn("tensorboardX", created)
            self.assertIsNot(sys.modules["tensorboardX"], sentinel)

    def test_splitnet_stub_is_namespaced_under_the_owned_root(self) -> None:
        self.assertEqual(cf._MODULE_STUBS["splitnet"], (("scripts.models.vgg", "Vgg16"),))
        self.assertTrue(cf._is_owned("scripts.models.vgg", cf._OWNED_NAMESPACES["splitnet"]))


class OwnedNamespaceTests(unittest.TestCase):
    """Every name the loader registers must fall inside its owned roots."""

    def test_manifest_modules_and_stubs_are_all_owned(self) -> None:
        for model_id in cf.MODEL_IDS:
            roots = cf._OWNED_NAMESPACES[model_id]
            names = [entry.module for entry in cf._REPOS[model_id].files]
            names += [fullname for fullname, _attr in cf._MODULE_STUBS[model_id]]
            names += [fullname for fullname, _sub in cf._PACKAGE_STUBS[model_id]]
            names.append(cf._NETWORK_MODULE[model_id])
            for name in names:
                with self.subTest(model=model_id, module=name):
                    self.assertTrue(cf._is_owned(name, roots))

    def test_ownership_matches_on_dotted_children_only(self) -> None:
        self.assertTrue(cf._is_owned("src", ("src",)))
        self.assertTrue(cf._is_owned("src.networks.blocks", ("src",)))
        self.assertFalse(cf._is_owned("srcfoo", ("src",)))
        self.assertFalse(cf._is_owned("torch", ("src",)))

    def test_network_module_is_the_last_file_of_each_manifest(self) -> None:
        # The manifest is in dependency order, so the network module is loaded
        # last; a reordering that breaks that would break the import chain.
        for model_id in cf.MODEL_IDS:
            with self.subTest(model=model_id):
                self.assertEqual(
                    cf._REPOS[model_id].files[-1].module, cf._NETWORK_MODULE[model_id]
                )


# Synthetic closures: two shapes, matching the two real ones (a package tree
# like SLBR/SplitNet, and top-level modules like WDNet). Neither needs torch.
_SYNTHETIC_PACKAGE_SOURCES: dict[str, bytes] = {
    "src/utils/model_init.py": b"INIT_MARKER = 'verified-model-init'\n",
    "src/networks/blocks.py": (
        b"from src.utils.model_init import INIT_MARKER\n"
        b"BLOCK_MARKER = 'verified-blocks'\n"
    ),
    "src/networks/resunet.py": (
        b"from src.networks.blocks import BLOCK_MARKER, INIT_MARKER\n"
        b"class SLBR:\n"
        b"    def __init__(self, args, shared_depth, blocks, long_skip):\n"
        b"        self.args = args\n"
        b"        self.provenance = (INIT_MARKER, BLOCK_MARKER)\n"
    ),
}

_SYNTHETIC_TOPLEVEL_SOURCES: dict[str, bytes] = {
    "unet_parts.py": b"PARTS_MARKER = 'verified-unet-parts'\n",
    "WDNet.py": (
        b"from unet_parts import *\n"
        b"from tensorboardX import SummaryWriter\n"
        b"def generator(in_ch, out_ch):\n"
        b"    return ('wdnet', in_ch, out_ch, PARTS_MARKER, SummaryWriter)\n"
    ),
}


class _SyntheticClosureTestCase(_TempRootTestCase):
    """Installs a synthetic, hash-pinned closure on disk for one model id."""

    MODEL = "slbr"
    SOURCES = _SYNTHETIC_PACKAGE_SOURCES

    def setUp(self) -> None:
        super().setUp()
        real = cf._REPOS[self.MODEL]
        by_path = {entry.path: entry for entry in real.files}
        files = tuple(
            cf._RemoteFile(
                path, len(data), hashlib.sha256(data).hexdigest(), by_path[path].module
            )
            for path, data in self.SOURCES.items()
        )
        spec = cf._RepoSpec(real.owner, real.repo, real.commit, files)
        repos_patch = patch.dict(cf._REPOS, {self.MODEL: spec})
        repos_patch.start()
        self.addCleanup(repos_patch.stop)

        root = cf.code_root(self.MODEL)
        for path, data in self.SOURCES.items():
            dest = root / Path(path)
            dest.parent.mkdir(parents=True, exist_ok=True)
            dest.write_bytes(data)

        cf._loaded_modules.pop(self.MODEL, None)
        self.addCleanup(cf._loaded_modules.pop, self.MODEL, None)

    def _owned_names_now(self) -> set[str]:
        roots = cf._OWNED_NAMESPACES[self.MODEL]
        return {name for name in sys.modules if cf._is_owned(name, roots)}


class VerifiedExecutionTests(_SyntheticClosureTestCase):
    """The bytes that were hashed are the bytes that run, and nothing leaks."""

    def test_closure_loads_and_the_network_is_constructed(self) -> None:
        net = cf.build_network("slbr")
        self.assertEqual(net.provenance, ("verified-model-init", "verified-blocks"))

    def test_no_owned_name_survives_the_import(self) -> None:
        before = self._owned_names_now()
        cf.build_network("slbr")
        self.assertEqual(self._owned_names_now(), before)

    def test_a_pre_existing_entry_is_restored_and_never_reused(self) -> None:
        # The program root really can be on `sys.path` in this process
        # (`inpaint/lama.py::_prepare_runtime_paths` puts it there) and this
        # repository's root contains a directory named `src/`.
        sentinel = types.ModuleType("src")
        sentinel.MARKER = "outsider"  # type: ignore[attr-defined]
        with patch.dict(sys.modules, {"src": sentinel}):
            net = cf.build_network("slbr")
            self.assertEqual(net.provenance, ("verified-model-init", "verified-blocks"))
            self.assertIs(sys.modules["src"], sentinel)

    def test_a_second_construction_works_after_the_namespace_was_restored(self) -> None:
        first = cf.build_network("slbr")
        second = cf.build_network("slbr")
        self.assertEqual(first.provenance, second.provenance)

    def test_a_tampered_file_is_refused_even_if_a_prior_check_passed(self) -> None:
        # TOCTOU: `is_code_ready()` hashes and releases the files; the loader
        # must not act on that verdict but verify the bytes it is about to run.
        target = cf.code_root("slbr") / "src" / "networks" / "resunet.py"
        original = target.read_bytes()
        target.write_bytes(b"X" * len(original))

        with patch.object(cf, "is_code_ready", lambda _model_id: True):
            with self.assertRaises(RuntimeError) as caught:
                cf.build_network("slbr")

        self.assertIn("целостности", str(caught.exception))
        self.assertEqual(self._owned_names_now(), set())

    def test_a_missing_file_is_a_file_not_found_error(self) -> None:
        (cf.code_root("slbr") / "src" / "utils" / "model_init.py").unlink()
        with self.assertRaises(FileNotFoundError):
            cf.build_network("slbr")

    def test_read_verified_source_returns_exactly_the_hashed_bytes(self) -> None:
        entry = cf._REPOS["slbr"].files[0]
        path = cf.code_root("slbr") / Path(entry.path)
        self.assertEqual(cf._read_verified_source(path, entry), path.read_bytes())

    def test_read_verified_source_rejects_a_same_length_mutation(self) -> None:
        entry = cf._REPOS["slbr"].files[0]
        path = cf.code_root("slbr") / Path(entry.path)
        path.write_bytes(b"Z" * entry.size)
        with self.assertRaises(RuntimeError):
            cf._read_verified_source(path, entry)


class WdnetTopLevelLoadTests(_SyntheticClosureTestCase):
    """WDNet's modules are top-level upstream names — the riskiest case."""

    MODEL = "wdnet"
    SOURCES = _SYNTHETIC_TOPLEVEL_SOURCES

    def test_closure_loads_and_uses_the_verified_unet_parts(self) -> None:
        net = cf.build_network("wdnet")
        self.assertEqual(net[:4], ("wdnet", 3, 3, "verified-unet-parts"))

    def test_a_pre_existing_unet_parts_is_shadowed_and_then_restored(self) -> None:
        # `_exec_module_from_path` used to return whatever was registered under
        # this generic name, which defeated the verified-code closure entirely.
        foreign = types.ModuleType("unet_parts")
        foreign.PARTS_MARKER = "attacker"  # type: ignore[attr-defined]
        with patch.dict(sys.modules, {"unet_parts": foreign}):
            net = cf.build_network("wdnet")
            self.assertEqual(net[3], "verified-unet-parts")
            self.assertIs(sys.modules["unet_parts"], foreign)

    def test_training_only_stubs_do_not_stay_registered(self) -> None:
        # `vgg` and `dataloader` are far too generic to leave in a long-lived
        # backend process. Compared against the snapshot rather than asserted
        # absent, so a genuinely installed `tensorboardX` does not break this.
        before = {name: sys.modules[name] for name in self._owned_names_now()}
        cf.build_network("wdnet")
        after = {name: sys.modules[name] for name in self._owned_names_now()}
        self.assertEqual(after, before)
        self.assertNotIn("ms_watermark_wdnet_net", sys.modules)


if __name__ == "__main__":
    unittest.main()
