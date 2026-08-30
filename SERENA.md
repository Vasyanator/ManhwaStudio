# SERENA.md — using the Serena MCP server in this repository

This document REPLACES Serena's own `initial_instructions`. If the `mcp__serena__*` tools are
listed in your session, read this file and do not call `initial_instructions` — its manual
overstates the tools and contains claims that are false in this harness (section 7).

Everything below was measured on this repository. Where a number appears, it came from a real
call, not from the tool descriptions.

---

## 1. What it is, and what it costs

Serena is an MCP server backed by language servers — here `rust` (rust-analyzer) and `python`
(pyright), configured in `.serena/project.yml`. It answers questions about *symbols* rather than
*text*.

Its tools are **deferred**: their schemas must be loaded with `ToolSearch("select:...")` before
any call. Budget one extra round-trip and roughly 5 KB of schema text.

Measured cost in one working session: resident memory of the Serena and rust-analyzer processes
grew from 1.9 GB to 6.5 GB. The first call of a session also pays language-server startup. Do not
start it for a one-line documentation fix.

Serena serialises tool calls internally, so issuing several in one batch is safe — they cannot
race — and saves round-trips. Batch aggressively.

---

## 2. Facts you must know BEFORE the first call

These are not subtleties; each one has already produced a wrong result or a failed call.

**Line numbers are 0-based**, while every other tool here is 1-based.

**`body_location.start_line` points at the doc comment, not at the declaration.** Measured on
`set_locale` in `crates/ms-i18n/src/catalog.rs`: Serena reports `start_line: 309`, which is line
310 one-based — the first `///` line. The actual `pub fn set_locale` is on line **322**, twelve
lines further down. Consequence: **never copy a line number out of Serena into a document, a
commit message, or a code comment.** Re-derive it with grep.

**`get_symbols_overview` emits no line numbers at all.** It maps a file; it does not locate
anything in it.

**Rust default depth is useless.** `src/backend_ipc/client.rs` (1759 lines) at default depth
returns about 130 characters — the names `wasm_stub` and `inner` and nothing else, because the
whole file lives inside two modules. It reads as "this file is nearly empty". At `depth=2` the
same call returns the full map for about 1.6 KB. **Always pass `depth` explicitly, 2 or more.**

**Method paths include the impl block.** `CallError/is_interrupted` fails; the correct path is
`impl CallError/is_interrupted`. Nested in a module it becomes `inner/impl BackendClient/call`.
Overloads and cfg twins are disambiguated with an index: `name[0]`, `name[1]`.

**`relative_path` must name the file where the symbol is DEFINED, not one that re-exports it.**
`find_symbol("set_locale", "crates/ms-i18n/src/lib.rs")` returns `[]`; the same query against
`crates/ms-i18n/src/catalog.rs` finds it.

---

## 3. The cfg blind spot — the failure that makes you WRONG

This project builds for `x86_64-unknown-linux-gnu`, `x86_64-pc-windows-gnu` and `wasm32`.
rust-analyzer resolves only the active target, and the granularity of what disappears is decided
by **where the `cfg` attribute sits**:

| Attribute position | Visible to Serena? |
|---|---|
| `#[cfg(...)] mod foo;` on the module declaration | **No.** The file is never loaded. Everything in it is invisible both as a source and as a target of references. |
| `#[cfg(...)] fn foo` on an item inside a loaded file | **Yes.** The item is indexed and appears in reference results. |

Verified in both directions:

- `src/storage.rs:70` declares `pub fn install` under `#[cfg(target_arch = "wasm32")]`. It has one
  real caller, `src/web_entry.rs:38`. `find_referencing_symbols` returns `{}` — because
  `src/main.rs:113` gates `mod web_entry` at the module declaration, so that file does not exist
  for the linux target.
- That same `install` function *was* returned as a referencing symbol when searching for
  references to `Storage`, proving a cfg-gated item inside a loaded file is indexed normally.

**The failure is silent.** An empty result from a real blind spot is byte-identical to an empty
result meaning "genuinely unreferenced". You cannot tell them apart from the output.

Therefore, two absolute rules:

1. **An empty Serena reference result is never evidence that a symbol is unused.** Confirm with
   `grep` before removing, renaming or changing the signature of anything.
2. **Before trusting any reference search, check how the enclosing module is declared.** One grep
   for `mod <name>` in the parent answers it.

Symbol *listing* is unaffected — `get_symbols_overview` and `find_symbol` are syntactic and do
show both cfg branches (they list `wasm_stub` and `inner` side by side).

---

## 4. Tools

### Navigation

| Tool | Use it for | Notes |
|---|---|---|
| `get_symbols_overview` | Mapping a file before reading it | Pass `depth` ≥ 2. No line numbers. |
| `find_symbol` | Reading one item: `include_body=true`, or `include_info=true` for signature + doc only | `depth=1` on an impl lists its methods with line ranges |
| `find_referencing_symbols` | Who calls this, and **from which function** | The one thing nothing else does. Subject to section 3. |
| `find_implementations` | All implementations of a trait | Precise and cheap; pass the file where the trait is *declared* |
| `find_declaration` | Jump from a usage to the definition, by regex with one capture group | Not obvious from the tool list, genuinely useful |
| `get_diagnostics_for_file` | Fast single-file sanity check | Active target only, no clippy. **Not** a substitute for section 16 verification |

**Large results degrade usefully.** When the answer exceeds `max_answer_chars`, Serena returns a
`file -> reference count` map instead of the full listing. Measured on `StorageError`: 94
references across 9 files, in place of 29 KB of output. This is the cheapest way to size a
refactor before starting it — use it deliberately by setting a small `max_answer_chars`.

### Editing

| Tool | Use it for |
|---|---|
| `replace_symbol_body` | Rewriting a whole function/method/class |
| `insert_before_symbol` / `insert_after_symbol` | Adding an item next to an existing one |
| `replace_content` | A few lines inside a larger symbol; regex mode with `start.*?end` avoids quoting long spans |
| `replace_in_files` | The SAME edit across many files, in one call. Use `dry_run=true` first: it returns a per-occurrence diff with ids, then apply all or a chosen subset |
| `rename_symbol` | Reference-aware rename across the codebase |
| `safe_delete_symbol` | Deleting a symbol *only if* nothing references it |

`safe_delete_symbol` is the most valuable of these: asked to delete a referenced class, it
refused and returned the referencing lines. That guard does not exist in ordinary editing.

`rename_symbol` updates code references only. Verified: renaming a class updated its definition,
a type annotation and a constructor call, and left the mention inside a docstring untouched. That
is correct behaviour, but it means **a rename is not finished until you grep for the old name in
comments, docs, locale files and `MODULE_README.md`.**

### Memory and onboarding — DO NOT USE

`write_memory`, `edit_memory`, `delete_memory`, `rename_memory`, `read_memory`, `list_memories`,
`onboarding`.

Serena keeps its own memory store in `.serena/memories`. It is empty and onboarding has never been
run. **Keep it that way.** This project deliberately has no persistent agent memory: project
knowledge belongs in `README_AGENT.md`, `MODULE_README.md`, file headers and `dev-docs/`, where it
is revised together with the code. A memory store is a copy nobody revises — an audit of the 48
memories this project used to keep found roughly a quarter of them factually wrong, naming
functions that no longer existed and features described as unbuilt that had shipped. Serena's
store has exactly the same failure mode. Do not call `onboarding`; it exists to populate it.

---

## 5. When to use Serena, and when not to

**Use it for:**

- listing the implementations of a trait;
- sizing a refactor before starting it (the count-map degradation above);
- finding out which *functions* call something, not just which lines;
- a reference-aware rename or a guarded delete, in code with no `cfg` involvement;
- mapping a large file before deciding what to read.

**Do not use it for:**

- anything reachable from `wasm32` or `windows` code — section 3;
- producing `file:line` citations for documentation — section 2;
- first-pass discovery when you do not yet know the symbol's name;
- text: `t!`/`tf!`/`tp!` keys, locale JSON, comments, `MODULE_README.md`, `dev-docs/`,
  `egui-docs/` — Serena cannot see any of it, and its `search_for_pattern` is not exposed here;
- a small, well-localised change where you already know the file and the line.

**Honest expectation.** In a controlled head-to-head on this repository — locating every call site
of a function, and describing an unfamiliar type — plain `grep` won both tasks: fewer calls,
comparable output size, and one call site that Serena missed entirely because of section 3. The
token advantage of a symbol map over a grep map measured about 2x, not an order of magnitude, and
the grep map carried line numbers that Serena's did not. Serena is a specialist tool with a narrow
edge, not a general replacement for search.

---

## 6. Recipes

**Map a file you are about to change.**
`get_symbols_overview(path, depth=2)` → pick the item → `find_symbol(name_path, path,
include_body=true)`. Do not read the whole file.

**Size a refactor.**
`find_referencing_symbols(name_path, path, max_answer_chars=4000)` → read the `file -> count` map
→ decide → then grep to confirm nothing cfg-gated is hiding.

**Rename safely.**
`find_referencing_symbols` → grep the same name to catch cfg-gated call sites → `rename_symbol` →
grep the OLD name again for comments, docs and locale keys.

**Delete safely.**
`safe_delete_symbol`. If it refuses, it hands you the reference list. If it succeeds, still grep
the name once: a caller in a cfg-gated module would not have stopped it.

---

## 7. Claims in Serena's own manual that are FALSE or wrong here

Its `initial_instructions` is written for a different harness. Specifically:

- **"Built-in `Read` is FORBIDDEN for discovery" and "`Edit` is FORBIDDEN".** Not in this project.
  Reading documentation, grepping for text, and verifying cfg-gated code with grep are required
  work here — see `AGENTS.md` section 3.
- **"Your built-in tools will deny such edits (they will assume you haven't read the content)."**
  Tested: a symbol body was read through `find_symbol(include_body=true)`, never through the
  built-in `Read`, and a subsequent built-in `Edit` on that file succeeded on the first attempt.
  This harness only requires a prior read for files outside the working directory.
- **"Trust the refactoring tools; do not re-run the build or tests to confirm."** Section 16 of
  `AGENTS.md` is not negotiable: `cargo check-all` and `cargo clippy --all-targets -- -D warnings`
  after any Rust change. Serena cannot see the two non-active build targets this project must
  compile for, which is precisely why the check exists.
- **Its cost framing** ("much more efficient than your own tools for most coding scenarios").
  Measured at about 2x on file mapping, and a net loss on both benchmark tasks. Use section 5.
