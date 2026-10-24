/*
File: browser-extension/src/modules/translate/sites/types.ts

Purpose:
The shape of one translator site's table: its dedicated-tab `SiteSpec`, the URL builder, its
character limit and settle time, and every DOM selector the page driver uses. Each site has
exactly ONE such table (`google.ts`, `deepl.ts`, `yandex.ts`); the background side and the
content-script driver both read it, so a selector fix is one edit.

Key items:
- TranslateSite, SiteSelectors
- RATE_LIMIT_TEXT_PATTERNS — banner texts shared by the sites

Notes:
- Every selector field is a FALLBACK LIST, tried in order; the first selector that matches an
  element wins. Sites rename their markup several times a year, so older generations stay in
  the list behind the current one.
- Selectors and limits come from open-source precedents (see the module MODULE_README.md) and
  are UNVERIFIED against today's pages; only the user's manual test can confirm them.
- Tables must stay plain data plus pure functions: they are bundled into the content script.
*/

import type { SiteSpec } from "../../../core/tabs";
import type { TranslateSiteKey } from "../types";

/** DOM hooks of one site; every entry is a fallback list (first match wins). */
export interface SiteSelectors {
  /** The source editor: a `<textarea>` or a contenteditable element. */
  readonly input: readonly string[];
  /** The element whose text is the translation. */
  readonly output: readonly string[];
  /** Any match means "the translation is still in progress". */
  readonly pending: readonly string[];
  /** Any match means an in-page challenge (captcha, Cloudflare check) the user must solve. */
  readonly challenge: readonly string[];
  /** Containers of error banners / toasts whose text is matched by `rateLimitTextPatterns`. */
  readonly rateLimitBanner: readonly string[];
  /**
   * Elements carrying the applied target language in a `lang` attribute; empty = no check.
   * A primary subtag different from the requested target fails `unsupported_language`.
   */
  readonly targetLang: readonly string[];
}

/** One translator site. */
export interface TranslateSite {
  readonly key: TranslateSiteKey;
  /** Dedicated-tab spec: `hostPatterns` = pages of the translator itself, `challengeUrlPatterns` = challenge pages. */
  readonly spec: SiteSpec;
  /** Longest text the site accepts in one go (characters, UTF-16 units). UNVERIFIED constant. */
  readonly maxChars: number;
  /** The output must stay unchanged this long before it is trusted (ms). */
  readonly settleMs: number;
  /** The translator page for a language pair (`source` may be `"auto"`). */
  pageUrl(source: string, target: string): string;
  readonly selectors: SiteSelectors;
  /**
   * When set and the output element has matching descendants, the output text is their
   * `textContent` joined by `\n` (one paragraph per line); otherwise its `innerText`.
   */
  readonly outputParagraphSelector: string | null;
  /** Substrings that mark a partial output (DeepL streams `[...]` for pending sentences). */
  readonly pendingMarkers: readonly string[];
  /** Whole-output placeholders shown while translating (e.g. "Translating..."). */
  readonly pendingTextPatterns: readonly RegExp[];
  /** Banner texts that mean the site refuses because of too many requests. */
  readonly rateLimitTextPatterns: readonly RegExp[];
}

/**
 * Banner texts that mean "too many requests" (English and Russian UI). Matched only against
 * banner / alert containers, never against the input or output, so a bubble that happens to
 * say "try again later" is not mistaken for a block. UNVERIFIED wording.
 */
export const RATE_LIMIT_TEXT_PATTERNS: readonly RegExp[] = [
  /too many requests/i,
  /too many translations/i,
  /слишком много (запросов|переводов)/i,
];
