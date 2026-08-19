# Module: src/launcher/new_project/quick_download/sites

## Purpose
Per-site chapter resolvers of the quick downloader. This is the ONLY place in the module that may
know a concrete host, URL shape, API endpoint, HTML marker, or per-site encoding.

## Architecture
One file per supported site, no shared state, no cross-site imports. Each file exposes exactly one
entry point:

```rust
pub(crate) fn <site>_plan(url: &str) -> Result<SiteDownloadPlan, QuickDownloadError>
```

It receives the normalized chapter/series URL, performs whatever fetches that site needs
(`super::super::http`), extracts the ordered page URLs, and returns a `SiteDownloadPlan` with the
optional `Referer` the site's CDN requires. It must not download images itself — the controller does
that.

Every module is split into a thin `*_plan` that does the I/O and a PURE parser
(`parse_<site>_images(...) -> Vec<String>`, plus any id/slug extraction) that takes already-fetched
text. The pure half is what the unit tests exercise, so no test touches the network.

`plan.rs` dispatches to these functions; nothing else calls them.

## Files and submodules
Sites whose page list comes from a JSON or GraphQL API:
- `mangadex.rs`: mangadex.org `at-home` API, latest-chapter feed pick.
- `weebdex.rs`: weebdex.org via `api.weebdex.org/chapter/{id}`; needs `Referer` + `Origin`.
- `mangataro.rs`: mangataro.org `/auth/chapter-content?chapter_id=`.
- `dankefuerslesen.rs`: danke.moe `/api/series/{slug}/`; URL `-` maps to JSON key `.`.
- `mangapark.rs`: the mangapark/comicpark/readpark/parkmanga/mpark mirror family, GraphQL POST to
  `/apo/`. Origin is taken from the input URL, so every mirror works without a host table.

Sites whose page list is an inline script literal (shared scanners in `../html.rs`):
- `manganelo.rs`: the four-mirror family nelomanga.net / natomanga.com / manganato.gg /
  mangakakalot.gg, keyed on `var cdns` + `var chapterImages`.
- `dynastyscans.rs`: dynasty-scans.com `var pages`.
- `kaliscan.rs`: kaliscan.me `var chapImages` (a comma-separated string, not an array).
- `hentai2read.rs`: hentai2read.com `'images'`, CDN base differs from the site root. Adult site.
- `kuaikan.rs`: kuaikanmanhua.com inline JSON blob scan.
- `bato.rs`: bato.to Astro island props with the legacy `imgHttps` fallback.

Sites scraped from page markup:
- `naver.rs`: comic.naver.com viewer HTML; pages are the `id="content_image_*"` images under
  the episode's own path (the episode's list thumbnail shares that path), `IMAG<a>_<b>` reading
  order.
- `webtoons.rs`: webtoons.com; pages are scoped to the `#_imageList` container of the desktop
  viewer (the rest of the document is the episode-thumbnail strip), a `m.webtoons.com` link is
  rewritten to `www.`, a series URL goes through the mobile episode-list API; canvas vs. webtoon.
- `tcbscans.rs`: tcbscans/onepiecechapters/tcb-backup mirrors, `fixed-ratio-content` images.
- `rawkuma.rs`: rawkuma.net/.com, `<img>` minus WordPress chrome prefixes.
- `mangafreak.rs`: mangafreak.me and its rotating `ww<N>.` prefixes, fixed CDN path prefix.
- `dandadan.rs`: `*.dandadan.net`, `<figure>` tier with an `aligncenter` fallback.
- `hiperdex.rs`: the hiperdex/hipertoon mirror family, `id="image-N"` with `data-src` preferred.
- `komikcast.rs`: the komikcast mirror family, images scoped to `main-reading-area`.
- `mangaread.rs`: mangaread.org, `id="image-N"` inside `reading-content`.
- `senmanga.rs`: raw.senmanga.com; requires the fixed `Cookie: viewer=1` (all-pages viewer).
- `weebcentral.rs`: weebcentral.com; second request to `/images?...` with the HX-* headers.
- `comicfury.rs`: comicfury.com and `*.thecomicseries.com` mirrors, archive walk.
- `readcomiconline.rs`: readcomiconline.li `lstImages.push(...)` scan and its scrambled-URL decoder.

## Contracts and invariants
- Every user-facing failure goes through `QuickDownloadError` with BOTH a localized `user_message`
  and a technical `log_message`. User-facing text must come from the i18n catalog (`t!`/`tf!`, keys
  under `launcher.new_project.quick_dl.*`) — never a literal string.
- **Prefer the shared failure keys** (`invalid_url_error`, `no_chapters_error`,
  `no_chapter_images_error`, `unexpected_json_error`) over a per-site key: a new key costs an edit in
  all five locale catalogs and is enforced in both directions by the i18n tests. The `log_message` is
  where the site-specific detail belongs — name the URL and the marker/endpoint that was expected.
- Generic primitives (URL, HTML, JS-literal scanning, base64, HTTP) must be imported from the layer
  above, not re-implemented per site. A helper that turns out to be site-agnostic gets promoted to
  `../html.rs` / `../url_util.rs` rather than copied.
- A site module must not import another site module.
- Page order is the site's responsibility: return the URLs already in reading order.
- A series URL is resolved to a chapter only where that site's list page makes it unambiguous
  (naver, webtoons, mangadex, manganelo, comicfury, kuaikan). Elsewhere a series URL is rejected up
  front with `invalid_url_error` naming the accepted shape — inventing a chapter-ordering rule is
  worse than a clear error.
- `path_contains` strips the host AND the leading slash, so a first-segment check like
  `path_contains(url, "/chapter/")` never matches. Use `path_segments` for that.

## Editing map
- To add a site: create `sites/<site>.rs` with `<site>_plan` plus its pure parser and tests, declare
  it in `sites/mod.rs`, add one arm to `build_site_download_plan` in `../plan.rs`, and append the
  host to the `supported_sites_hint` locale string (see the parent MODULE_README for the on-disk
  catalog caveat).
- To fix a broken site: only its file should change.
