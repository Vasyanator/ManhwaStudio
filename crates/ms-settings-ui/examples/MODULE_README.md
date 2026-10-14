# Module: crates/ms-settings-ui/examples

## Purpose
Development demos of the `ms-settings-ui` package. Not production entry points, never
shipped or signed.

## Files and submodules
- `tutorial_test/`: the tutorial overlay demo. It imports the crate's own
  `tutorial::engine`, so its `[[example]]` table carries
  `required-features = ["tutorial"]`. See `tutorial_test/MODULE_README.md`.

## Contracts and invariants
- A demo uses the crate's real modules, never a `#[path]` mount or a fork.
- Startup errors go to stderr.

## Editing map
- Run: `cargo run -p ms-settings-ui --example tutorial_test --features tutorial`.
