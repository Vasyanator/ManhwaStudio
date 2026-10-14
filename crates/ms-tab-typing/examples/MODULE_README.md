# Module: crates/ms-tab-typing/examples

## Purpose
Development prototypes next to the typing tab, built as Cargo examples of the
`ms-tab-typing` package. Not production entry points, never shipped or signed.

## Files and submodules
- `test_center_find.rs`: standalone egui tester of overlay placement and bubble-centre
  search (region growing from a click, shape validation, optical centre of an overlay).
  It is the prototype of the production search in `src/auto_typing.rs`; the two are
  separate copies, the tab never calls the example.

## Contracts and invariants
- Production behaviour lives in `src/`; a change proven here is ported to
  `src/auto_typing.rs`, never linked from it.
- Image analysis runs on a worker thread; missing fixture images
  (`old_or_test/test_center_find/`, resolved from the current directory) fail visibly.

## Editing map
- Run: `cargo run -p ms-tab-typing --example test_center_find` from the repository root.
