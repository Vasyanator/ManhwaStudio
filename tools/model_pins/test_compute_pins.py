"""Offline tests of ``compute_pins.py`` with a scripted fake opener (never touches the network).

File: tools/model_pins/test_compute_pins.py

Covers: plain and resumed downloads, a server that ignores Range, transient retries with the
injected sleep (budget reset after progress), short bodies, permanent 4xx, 416 on a complete
part, the Drive interstitial, HF revision/tree resolution with LFS and git-blob cross-checks,
Surya manifest expansion with path-traversal refusal, zip-member md5 cross-checks, GitHub asset
digest checks, per-source failure isolation and the scratch-inside-repo refusal.

Run: ``python3 -I -m unittest discover -s tools/model_pins -p 'test_*.py' -v``
"""

from __future__ import annotations

import email.message
import hashlib
import io
import json
import shutil
import sys
import tempfile
import unittest
import urllib.error
import urllib.request
import zipfile
from pathlib import Path
from typing import Any

sys.path.insert(0, str(Path(__file__).resolve().parent))

import compute_pins as cp  # noqa: E402  (path set up above)


def _headers(values: dict[str, str]) -> email.message.Message:
    msg = email.message.Message()
    for key, value in values.items():
        msg[key] = value
    return msg


class FakeResponse:
    """A minimal ``Response``; ``cut`` truncates the body to simulate a dropped connection."""

    def __init__(self, status: int, body: bytes, headers: dict[str, str] | None = None, cut: int | None = None) -> None:
        self.status = status
        self.headers = _headers(headers or {})
        self._body = io.BytesIO(body if cut is None else body[:cut])

    def read(self, amt: int = -1) -> bytes:
        return self._body.read(amt)

    def close(self) -> None:
        self._body.close()


class FakeOpener:
    """Scripted opener: per URL a queue of ``FakeResponse`` or HTTP status ints (raised)."""

    def __init__(self, script: dict[str, list[Any]]) -> None:
        self.script = {url: list(items) for url, items in script.items()}
        self.requests: list[tuple[str, str | None]] = []

    def __call__(self, request: urllib.request.Request, timeout: float) -> FakeResponse:
        url = request.full_url
        self.requests.append((url, request.get_header("Range")))
        queue = self.script.get(url)
        if not queue:
            raise AssertionError(f"unexpected request {url}")
        item = queue.pop(0)
        if isinstance(item, int):
            raise urllib.error.HTTPError(url, item, "scripted", _headers({}), None)
        if isinstance(item, tuple):
            code, headers = item
            raise urllib.error.HTTPError(url, code, "scripted", _headers(headers), None)
        return item


def ok(body: bytes, ctype: str = "application/octet-stream") -> FakeResponse:
    return FakeResponse(200, body, {"Content-Length": str(len(body)), "Content-Type": ctype})


def partial(body: bytes, start: int) -> FakeResponse:
    rest = body[start:]
    return FakeResponse(206, rest, {"Content-Length": str(len(rest)),
                                    "Content-Range": f"bytes {start}-{len(body) - 1}/{len(body)}"})


def sha(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


class Base(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = Path(tempfile.mkdtemp(prefix="ms-pins-test-"))
        self.sleeps: list[float] = []

    def tearDown(self) -> None:
        shutil.rmtree(self.tmp)

    def ctx(self, script: dict[str, list[Any]]) -> tuple[cp.Context, FakeOpener]:
        opener = FakeOpener(script)
        net = cp.Net(opener=opener, sleep=self.sleeps.append, policy=cp.RetryPolicy(max_attempts_without_progress=2))
        return cp.Context(net=net, scratch=self.tmp), opener


class FetchFileTest(Base):
    URL = "https://example.test/f.bin"
    BODY = bytes(range(256)) * 50

    def test_plain_download(self) -> None:
        ctx, _ = self.ctx({self.URL: [ok(self.BODY)]})
        dest = self.tmp / "f.bin"
        cp.fetch_file(ctx.net, self.URL, dest)
        self.assertEqual(dest.read_bytes(), self.BODY)
        self.assertFalse((self.tmp / "f.bin.part").exists())

    def test_resume_sends_range_and_appends(self) -> None:
        (self.tmp / "f.bin.part").write_bytes(self.BODY[:1000])
        ctx, opener = self.ctx({self.URL: [partial(self.BODY, 1000)]})
        cp.fetch_file(ctx.net, self.URL, self.tmp / "f.bin")
        self.assertEqual(opener.requests, [(self.URL, "bytes=1000-")])
        self.assertEqual((self.tmp / "f.bin").read_bytes(), self.BODY)

    def test_server_ignoring_range_restarts(self) -> None:
        (self.tmp / "f.bin.part").write_bytes(b"garbage")
        ctx, _ = self.ctx({self.URL: [ok(self.BODY)]})
        cp.fetch_file(ctx.net, self.URL, self.tmp / "f.bin")
        self.assertEqual((self.tmp / "f.bin").read_bytes(), self.BODY)

    def test_short_body_resumes_and_progress_resets_budget(self) -> None:
        cut1 = FakeResponse(200, self.BODY, {"Content-Length": str(len(self.BODY))}, cut=3000)
        script = [cut1, 503, partial(self.BODY, 3000)]
        ctx, opener = self.ctx({self.URL: script})
        cp.fetch_file(ctx.net, self.URL, self.tmp / "f.bin")
        self.assertEqual((self.tmp / "f.bin").read_bytes(), self.BODY)
        self.assertEqual([r for _, r in opener.requests], [None, "bytes=3000-", "bytes=3000-"])
        self.assertEqual(self.sleeps, [2.0, 4.0])

    def test_retry_after_is_honoured_and_capped(self) -> None:
        ctx, _ = self.ctx({self.URL: [(429, {"Retry-After": "500"}), ok(self.BODY)]})
        cp.fetch_file(ctx.net, self.URL, self.tmp / "f.bin")
        self.assertEqual(self.sleeps, [60.0])

    def test_budget_exhausted_without_progress(self) -> None:
        ctx, _ = self.ctx({self.URL: [503, 503, 503]})
        with self.assertRaises(cp.PinError):
            cp.fetch_file(ctx.net, self.URL, self.tmp / "f.bin")
        self.assertEqual(len(self.sleeps), 2)

    def test_permanent_404_is_not_retried(self) -> None:
        ctx, opener = self.ctx({self.URL: [404]})
        with self.assertRaises(cp.HttpStatusError):
            cp.fetch_file(ctx.net, self.URL, self.tmp / "f.bin")
        self.assertEqual(len(opener.requests), 1)
        self.assertEqual(self.sleeps, [])

    def test_416_on_complete_part_finishes(self) -> None:
        (self.tmp / "f.bin.part").write_bytes(self.BODY)
        ctx, _ = self.ctx({self.URL: [(416, {"Content-Range": f"bytes */{len(self.BODY)}"})]})
        cp.fetch_file(ctx.net, self.URL, self.tmp / "f.bin")
        self.assertEqual((self.tmp / "f.bin").read_bytes(), self.BODY)

    def test_overflow_discards_part(self) -> None:
        bad = FakeResponse(200, self.BODY, {"Content-Length": "10"})
        ctx, _ = self.ctx({self.URL: [bad]})
        with self.assertRaises(cp.PinError):
            cp.fetch_file(ctx.net, self.URL, self.tmp / "f.bin")
        self.assertFalse((self.tmp / "f.bin.part").exists())

    def test_206_starting_early_truncates_and_appends(self) -> None:
        (self.tmp / "f.bin.part").write_bytes(self.BODY[:2000])
        ctx, _ = self.ctx({self.URL: [partial(self.BODY, 1500)]})
        cp.fetch_file(ctx.net, self.URL, self.tmp / "f.bin")
        self.assertEqual((self.tmp / "f.bin").read_bytes(), self.BODY)


class SourceTest(Base):
    def test_zip_member_md5_cross_check_and_github_digest(self) -> None:
        member = b"weights" * 100
        buf = io.BytesIO()
        with zipfile.ZipFile(buf, "w") as zf:
            zf.writestr("m.pth", member)
        archive = buf.getvalue()
        url = "https://github.com/o/r/releases/download/v1/m.zip"
        api = "https://api.github.com/repos/o/r/releases/tags/v1"
        release = {"assets": [{"name": "m.zip", "size": len(archive), "digest": f"sha256:{sha(archive)}"}]}
        ctx, _ = self.ctx({url: [ok(archive)], api: [ok(json.dumps(release).encode(), "application/json")]})
        good_md5 = hashlib.md5(member).hexdigest()
        res = cp.process_source(ctx, {"id": "z", "kind": "url", "url": url, "zip_members": [{"member": "m.pth", "md5": good_md5}]})
        self.assertEqual(res["status"], "ok")
        row = res["rows"][0]
        self.assertEqual((row["member_size"], row["member_sha256"]), (len(member), sha(member)))
        self.assertTrue(row["checks"]["github_digest_match"])
        self.assertTrue(row["checks"]["member_md5_match"])

    def test_md5_mismatch_marks_source(self) -> None:
        buf = io.BytesIO()
        with zipfile.ZipFile(buf, "w") as zf:
            zf.writestr("m.pth", b"x")
        url = "https://example.test/m.zip"
        ctx, _ = self.ctx({url: [ok(buf.getvalue())]})
        res = cp.process_source(ctx, {"id": "z", "kind": "url", "url": url, "zip_members": [{"member": "m.pth", "md5": "0" * 32}]})
        self.assertEqual(res["status"], "mismatch")

    def test_expected_sha256_mismatch(self) -> None:
        url = "https://example.test/a.bin"
        ctx, _ = self.ctx({url: [ok(b"abc")]})
        res = cp.process_source(ctx, {"id": "a", "kind": "url", "url": url, "expected_sha256": "0" * 64})
        self.assertEqual(res["status"], "mismatch")
        self.assertFalse(res["rows"][0]["checks"]["expected_sha256_match"])

    def test_drive_interstitial_follows_confirm_form(self) -> None:
        base = cp.DRIVE_URL.format(file_id="ID1")
        html = b'<form><input name="confirm" value="t"><input name="uuid" value="u-1"></form>'
        payload = b"\x80\x02payload"
        ctx, opener = self.ctx({base: [ok(html, "text/html; charset=utf-8")], f"{base}&confirm=t&uuid=u-1": [ok(payload)]})
        res = cp.process_source(ctx, {"id": "w", "kind": "gdrive", "file_id": "ID1", "file_name": "w.pth"})
        self.assertEqual(res["status"], "ok", res)
        self.assertEqual(res["rows"][0]["sha256"], sha(payload))
        self.assertEqual(len(opener.requests), 2)

    def test_hf_repo_resolves_commit_and_cross_checks_tree(self) -> None:
        commit = "a" * 40
        cfg = b'{"x": 1}'
        weights = b"W" * 4096
        git = hashlib.sha1(f"blob {len(cfg)}\0".encode() + cfg).hexdigest()
        tree = [
            {"type": "file", "path": "unet/config.json", "size": len(cfg), "oid": git},
            {"type": "file", "path": "unet/model.fp16.safetensors", "size": len(weights), "oid": "z", "lfs": {"oid": sha(weights), "size": len(weights)}},
            {"type": "file", "path": "unet/model.safetensors", "size": 1, "oid": "y"},
            {"type": "directory", "path": "unet"},
        ]
        h = cp.HF_ENDPOINT
        script = {
            f"{h}/api/models/o/r/revision/main": [ok(json.dumps({"sha": commit}).encode())],
            f"{h}/api/models/o/r/tree/{commit}?recursive=1": [ok(json.dumps(tree).encode())],
            f"{h}/o/r/resolve/{commit}/unet/config.json": [ok(cfg)],
            f"{h}/o/r/resolve/{commit}/unet/model.fp16.safetensors": [ok(weights)],
        }
        ctx, _ = self.ctx(script)
        res = cp.process_source(ctx, {"id": "h", "kind": "hf_repo", "repo": "o/r", "allow_patterns": ["*/config.json", "*.fp16.safetensors"]})
        self.assertEqual(res["status"], "ok", res)
        self.assertEqual(res["commit"], commit)
        self.assertEqual([r["path"] for r in res["rows"]], ["unet/config.json", "unet/model.fp16.safetensors"])
        self.assertTrue(res["rows"][0]["checks"]["tree_git_blob_match"])
        self.assertTrue(res["rows"][1]["checks"]["tree_lfs_sha256_match"])

    def test_manifest_source_expands_files_and_refuses_traversal(self) -> None:
        base = "https://models.test/det/v1"
        ctx, _ = self.ctx({
            f"{base}/manifest.json": [ok(json.dumps({"files": ["config.json", "model.safetensors"]}).encode())],
            f"{base}/config.json": [ok(b"{}")],
            f"{base}/model.safetensors": [ok(b"M" * 10)],
        })
        res = cp.process_source(ctx, {"id": "s", "kind": "url_manifest", "base_url": "https://models.test", "path": "det/v1"})
        self.assertEqual(res["status"], "ok", res)
        self.assertEqual([r["path"] for r in res["rows"]], ["manifest.json", "config.json", "model.safetensors"])

        evil = "https://models.test/evil/v1"
        ctx2, _ = self.ctx({f"{evil}/manifest.json": [ok(json.dumps({"files": ["../../x"]}).encode())]})
        res2 = cp.process_source(ctx2, {"id": "e", "kind": "url_manifest", "base_url": "https://models.test", "path": "evil/v1"})
        self.assertEqual(res2["status"], "failed")
        self.assertIn("unsafe remote path", res2["error"])

    def test_failed_source_does_not_stop_run(self) -> None:
        good = "https://example.test/g.bin"
        ctx, _ = self.ctx({"https://example.test/missing.bin": [404], good: [ok(b"g")]})
        out = self.tmp / "pins.json"
        results = cp.run([{"id": "m", "kind": "url", "url": "https://example.test/missing.bin"},
                          {"id": "g", "kind": "url", "url": good}], ctx, out, self.tmp / "snip.txt")
        self.assertEqual([r["status"] for r in results], ["failed", "ok"])
        self.assertEqual(json.loads(out.read_text())["sources"][1]["rows"][0]["sha256"], sha(b"g"))
        self.assertIn("ExternalFile { path: \"g.bin\", size: 1,", (self.tmp / "snip.txt").read_text())

    def test_repeated_url_is_downloaded_once(self) -> None:
        url = "https://example.test/same.tgz"
        ctx, opener = self.ctx({url: [ok(b"same")]})
        for source_id in ("a", "b"):
            self.assertEqual(cp.process_source(ctx, {"id": source_id, "kind": "url", "url": url})["status"], "ok")
        self.assertEqual(len(opener.requests), 1)


class GuardTest(unittest.TestCase):
    def test_scratch_inside_repo_is_refused(self) -> None:
        with self.assertRaises(cp.PinError):
            cp.check_scratch(cp.REPO_ROOT / "target" / "pins")
        with self.assertRaises(cp.PinError):
            cp.check_scratch(cp.REPO_ROOT)

    def test_scratch_outside_repo_is_accepted(self) -> None:
        outside = Path(tempfile.gettempdir()) / "ms-pins-outside"
        self.assertEqual(cp.check_scratch(outside, repo_root=Path("/home/u/repo")), outside.resolve())

    def test_unsafe_paths(self) -> None:
        for bad in ("/etc/passwd", "a/../b", "", "a\\b", "C:/x", "a//b"):
            with self.assertRaises(cp.PinError, msg=bad):
                cp.safe_relpath(bad)
        self.assertEqual(cp.safe_relpath("a/b.json"), Path("a", "b.json"))

    def test_content_range_parsing(self) -> None:
        self.assertEqual(cp.parse_content_range("bytes 5-9/10"), (5, 10))
        self.assertEqual(cp.parse_content_range("bytes */10"), (None, 10))
        with self.assertRaises(cp.PinError):
            cp.parse_content_range("items 1-2/3")

    def test_bundled_sources_file_is_valid(self) -> None:
        sources = cp.load_sources(Path(__file__).resolve().parent / "sources.json")
        self.assertTrue(all(s["kind"] in cp.HANDLERS for s in sources))
        for s in sources:
            if "expected_sha256" in s:
                self.assertRegex(s["expected_sha256"], cp.HEX64)


if __name__ == "__main__":
    unittest.main()
