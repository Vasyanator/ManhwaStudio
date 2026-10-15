# Module: crates/ms-custom-fonts/examples

## Purpose
Development diagnostics of the custom font compiler, built as Cargo examples of the
`ms-custom-fonts` package. Never part of the application.

## Files and submodules
- `hybrid_gallery.rs`: compiles hybrids of the given font files at several weight sets
  (subset of the sample text, or the whole face with `--full`), renders the sources and the
  hybrids one under another through `ms-text-render` into `<out_dir>/<name>.png`, prints
  compile timings (subset ms; full glyph count, seconds, ms/glyph, size) and fallback reports;
  `--save-fonts` also writes the compiled faces. Labels use the bundled Noto Sans located from
  `CARGO_MANIFEST_DIR`.

## Contracts and invariants
- Writes only inside the output directory given on the command line.
- Missing fonts fail visibly (non-zero exit, message on stderr); no placeholder output.
- Run it in release (`cargo run --release -p ms-custom-fonts --example hybrid_gallery -- ...`):
  debug builds are an order of magnitude slower, so their timings mean nothing.
