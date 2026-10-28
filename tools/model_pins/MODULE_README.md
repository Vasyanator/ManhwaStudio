# Module: tools/model_pins

## Purpose
Offline developer tool that downloads each upstream model / runtime source ONCE and records the
values the application's pinned download catalogs need: exact byte size, sha256, the resolved
40-hex commit of Hugging Face repositories, and size / sha256 / md5 of zip members. Nothing in
`src/` or `crates/` imports it; it never enters the runtime path. Its output is pasted by hand
into the owning catalogs (`ms_sysprobe::ai_models::external_catalog`,
`crates/ms-onnx-runtime/src/ort_manifest.json`, the Stable-Diffusion.cpp package rows in
`crates/ms-sdcpp/src/catalog.rs` (`sdcpp/*` sources: the release zips with every member), the
waifu2x URL table, the watermark weight table in `modules/ai_backend/watermark/service.py`).

## Architecture
```
sources.json -> load_sources -> process_source (per kind handler) -> rows + checks
                                   fetch_file (Range resume, retries) -> hash_file
             -> run: pins.json (+ snippets) rewritten after every source
```
- `fetch_file` stages into `<dest>.part`, resumes with `Range`, retries transient failures with
  capped exponential backoff (budget reset after progress), follows the project's
  "Network downloads" rules. `urllib` has a single socket timeout (60 s) for connect and read.
- One URL is downloaded once per run (`Context.fetched`), so rows sharing an archive (the two
  macOS ORT rows) or a commit-pinned HF file (config spec vs full inpaint spec) cost nothing extra.
- GitHub release URLs are cross-checked against the release API (`size`, `digest`); a null
  `digest` (older assets) is recorded as such, not as an error.

## Files and submodules
- `compute_pins.py`: the tool (stdlib only). Run:
  `python3 -I -B tools/model_pins/compute_pins.py --sources tools/model_pins/sources.json
  --scratch <dir outside the repo> --out <pins.json> [--snippets <file>] [--only <id>]...`.
  Exit `0` = every source ok, `1` = a source failed or mismatched, `2` = usage error
  (including a scratch path inside the repository).
- `sources.json`: the source list. Kinds:
  - `url`: `url`, optional `expected_sha256`, optional `zip_members` (`"*"` or
    `[{"member", "md5"?}]`);
  - `gdrive`: `file_id`, `file_name`, optional `approx_size` (handles the confirm form);
  - `hf_repo`: `repo`, `revision` (default `main`, resolved to a commit), and either exact
    `paths` or `allow_patterns` (fnmatch; `*` crosses `/`, as `huggingface_hub` matches);
    optional `expected_sha256` applied to every selected file;
  - `url_manifest`: `base_url` + `path`; downloads `manifest.json` and its `files` list (Surya).
  Every source has a unique `id` and a `note` naming the code it was derived from.
- `make_easyocr_fixture.py`: writes `crates/ms-sysprobe/fixtures/easyocr_scripts.json` (the
  mirror of easyocr's `config.py` tables, the `Reader.__init__` recognizer if-chain and the
  language-list cases that the Rust test of `ms_sysprobe::ai_models::easyocr` compares its table
  against). It EXECUTES the installed library's `config.py` (`runpy`; its tables are built by
  calls) and refuses any version other than the pinned one (`--easyocr-version`, default 1.7.2).
  Run: `python3 -I -B tools/model_pins/make_easyocr_fixture.py --venv venv --out
  crates/ms-sysprobe/fixtures/easyocr_scripts.json`; exit `0` = written, `2` = missing or
  unexpected library (nothing written). Byte-deterministic. Its `CHAIN` / `CASES` mirror
  `Reader.__init__` by hand: re-read that code and regenerate when the easyocr pin changes.
- `test_compute_pins.py`: stdlib `unittest` suite with a scripted fake opener and an injected
  sleep; no network. Run: `python3 -I -B -m unittest discover -s tools/model_pins -p 'test_*.py'`.

## Contracts and invariants
- **Downloads are untrusted data.** The tool only hashes files and reads zip member bytes; it
  never executes, imports or unpickles them. Remote-supplied relative paths (HF tree, Surya
  manifest) pass `safe_relpath`, so they cannot escape the scratch directory.
- **The scratch directory must be outside the repository** (`check_scratch` refuses otherwise);
  it holds several GB and is deleted by hand after the run.
- **A source never aborts the run.** A permanent failure is recorded as `status: "failed"` with
  the error; a check that disagrees (`*_match: false`) gives `status: "mismatch"` with all
  values still recorded.
- Each row: `path`, `url`, `size`, `sha256`, `checks`, plus `member`, `member_size`,
  `member_sha256`, `member_md5` for zip members. HF sources add `repo`, `revision`, `commit`.
- `sources.json` values are copied from the code that owns them (see each `note`); when that
  code changes a URL, a library version (easyocr 1.7.2, surya-ocr 0.17.1, diffusers 0.39.0) or a
  repo, update `sources.json` in the same change and re-run.

## Editing map
- To add a source of an existing kind, append it to `sources.json`.
- To add a source kind, add a handler to `HANDLERS` in `compute_pins.py` and a test.
- To change retry or timeout behaviour, see `RetryPolicy`, `TIMEOUT_S` and `fetch_file`.
- To change the pasted formats, see `render_snippets`.
