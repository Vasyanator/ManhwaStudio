# Module: browser-extension/src/modules/translate/

## Purpose
The `translate` feature module of the browser extension: machine translation on the Google,
DeepL and Yandex translator web pages, in the user's own browser session. The app's consumer
is `ms-tab-translation` (`machine_translators/browser.rs`); it reaches this module through the
bridge (`request.module = "translate"`). This directory covers `sites/` and `content/` too.

## Wire contract (module `translate`, version 1, method `translate`)
Any change of these shapes or meanings bumps `TRANSLATE_MODULE_VERSION` (`types.ts`) and the
app's copy; the app uses the module only when it is announced with exactly that version.

- **Params** `{site, source, target, text}` — all four required, no other keys:
  - `site`: `"google" | "deepl" | "yandex"`;
  - `source`: the site's own language code (`ja`, `zh-CN`, `en-US`, ...) or `"auto"`;
  - `target`: the site's own language code (never `"auto"`);
  - `text`: ONE text. The app packs several bubbles one per line (line breaks inside a bubble
    already flattened to spaces, decision D2) and splits the answer by lines.
  Codes arrive already in the site's spelling (the app normalizes per service); only their
  shape (`^[A-Za-z]{2,3}(-[A-Za-z0-9]{2,8})*$`) is checked here.
- **Result** `{text}`: the full translation, line structure as the site returned it,
  normalized (CRLF/CR -> LF, NBSP -> space, zero-width characters and trailing spaces removed,
  leading/trailing blank lines removed; inner blank lines kept). A blank `text` answers `""`
  without touching a tab.
- **Errors** (`BridgeFailure` codes):
  - `bad_params` — malformed params (see above); `unknown_method` — any other method;
  - `too_long` — `text.length` (UTF-16 units) over the site limit: DeepL 1500, Google 5000,
    Yandex 5000 (UNVERIFIED constants; the app splits below them);
  - `site_changed` — the input/output hooks were not found within 20 s, or the inserted text
    did not reach the editor line for line (selectors outdated);
  - `rate_limited` — a "too many requests" banner (no `retry_after_ms`: the sites give none);
  - `attention_timeout` — a challenge was not solved within 10 min, or came back after it was
    solved once in this request;
  - `unsupported_language` — DeepL applied another target language than requested (the
    fragment was dropped / the pair is not offered);
  - `timeout` — no settled translation 60 s after inserting the text;
  - `tab_closed` — the dedicated tab was closed; `cancelled` — the app cancelled;
  - `internal` — the driver produced no result twice (navigations, refused injection).
- **Events**: `attention` `{reason: "<site>_challenge", timeout_ms}` and `attention_cleared`
  `{reason, outcome}` (core `AttentionController`); the app suspends its deadline meanwhile.

## Architecture
```
index.ts (service worker)   createTranslateModule(page: TranslatePageApi)
  validate -> limit -> ctx.tabs.acquire(site.spec)   (one dedicated pinned tab per site, exclusive)
  openPage: navigate only when this module has not already loaded the pair there
  classifyTabUrl: site | needs_user (challenge / off-site / unreadable URL) | gone
  page.injectDriver -> page.runDriver(job) ---- chrome.scripting.executeScript ----+
  result -> {text} | ctx.attention.hold(... probe) + retry | BridgeFailure           |
                                                                                   v
content/driver.ts (IIFE, isolated world of the dedicated tab only)  globalThis.__msTranslateDriver
  wait for hooks -> clear input -> wait (bounded) for empty output -> insert text
  -> MutationObserver feeds StableOutput (content/stable.ts) -> lang check -> normalized text
```

## Files and submodules
- `types.ts`: module identity, params/result types, `parseTranslateParams`.
- `index.ts`: the module, page reuse, challenge hold + retry, result -> error mapping.
- `page.ts`: `TranslatePageApi` seam and its `chrome.tabs` / `chrome.scripting` implementation.
- `driver_protocol.ts`: `DriverJob` / `DriverResult` / `DriverApi`, the driver global, `DRIVER_FILE`.
- `sites/types.ts`: the `TranslateSite` table shape; `sites/{google,deepl,yandex}.ts`: ONE table
  per site (URL builder, limit, settle time, selector fallback lists); `sites/index.ts`: lookup.
- `content/driver.ts`: the page driver (DOM work only). `content/stable.ts`: pure completion
  detector. `content/normalize.ts`: pure text helpers.

## Contracts and invariants
- The driver is injected programmatically into the extension's own dedicated tab only; the
  manifest declares no content scripts, so the user's own translator tabs are never touched.
- Completion rule (`StableOutput`): non-empty, not pending (DeepL `[...]`, pending selectors,
  "Translating..." placeholders), unchanged for `settleMs` (800 ms), and different from the
  pre-clear output unless an empty output was seen after clearing.
- Input: `execCommand("insertText")` first, then the prototype value setter (textarea) or one
  `<p>` per line (contenteditable) plus `input`; the editor must then hold the same lines.
- The driver never clicks consent dialogs and never solves a challenge; it reports a challenge
  and the module hands the tab to the user (`attention.hold`), at most once per request.
- No `setInterval` in the driver: hidden pages get chained timers throttled to once a minute
  after 5 minutes; the driver waits on MutationObserver plus single one-shot timers.
- Without the "tabs" permission an off-site URL (Google `/sorry`, consent.google.com) is
  unreadable; unreadable counts as "needs the user". Do not widen permissions for this.
- Site tables are plain data + pure functions: they are bundled into the content script.

## Unverified (needs the user's manual test)
- Every selector, Google's "Translating..." placeholder, the rate-limit banner texts.
- Yandex URL parameters (`source_lang` / `target_lang`; auto = no `source_lang`) and its limit.
- DeepL: the stand-in source for `auto` (from eggplants/deepl-cli), the target `lang` attribute,
  whether a fragment change through about:blank re-applies the pair, whether DOM insertion
  triggers Cloudflare in a real profile.
- `execCommand` in a background (inactive) tab; speed in a hidden tab.
- The 1500 / 5000 / 5000 character limits.

## Editing map
- A site changed its markup: that site's table in `sites/<site>.ts` (prepend the new selector,
  keep the old ones behind it).
- A new site: `TRANSLATE_SITE_KEYS` (`types.ts`) + `sites/<site>.ts` + `sites/index.ts` + host
  permission in `manifest.json` + module version bump + the app's service list.
- Completion detection: `content/stable.ts`; text clean-up: `content/normalize.ts`.
- Retry / attention / error mapping: `index.ts`.
