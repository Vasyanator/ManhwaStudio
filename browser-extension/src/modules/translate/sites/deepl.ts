/*
File: browser-extension/src/modules/translate/sites/deepl.ts

Purpose:
The ONE table of the DeepL web translator (www.deepl.com): URL builder, limit, selectors.

Notes:
- URL: `https://www.deepl.com/en/translator#<src>/<tgt>/`. `auto` cannot be named in the
  fragment: DeepL drops a fragment it does not understand as a whole, target included
  (eggplants/deepl-cli). An auto source therefore pins a stand-in (`en`, or `de` when the
  target is English); DeepL's detection replaces it with the text's real language.
- Hooks: `d-textarea[data-testid=translator-source-input|translator-target-input]` ("the
  stable hooks", deepl-cli, TWP, deeplbox) with the older `dl-test` / `lmt__*` generations
  behind them. The output is read per `<p>` joined by `\n` (deeplbox).
- Streaming: partial output carries the `[...]` marker (deepl-cli).
- Challenges: Cloudflare's `#challenge-widget` overlay / turnstile iframe in the page, and
  the `__cf_chl` / `cdn-cgi/challenge-platform` interstitial URLs.
- The cookie-consent dialog (in a shadow root) is never clicked by the extension: accepting
  is the user's decision, and text inserted through the DOM still translates under it.
- The target `d-textarea` carries the applied target language in `lang` (deepl-cli), which
  catches a dropped fragment.
- 1500 characters is the anonymous web limit (deepl-cli `max_length`, DeepL support for
  logged-out Write); UNVERIFIED today. All selectors UNVERIFIED against today's page.
*/

import { AUTO_SOURCE } from "../types";
import type { TranslateSite } from "./types";
import { RATE_LIMIT_TEXT_PATTERNS } from "./types";

/** The source pinned in the fragment for an auto source; see the file header. */
export function deeplFragmentSource(source: string, target: string): string {
  if (source !== AUTO_SOURCE) return source;
  return target.split("-")[0]?.toLowerCase() === "en" ? "de" : "en";
}

/** DeepL web translator. */
export const DEEPL_SITE: TranslateSite = {
  key: "deepl",
  spec: {
    key: "deepl",
    hostPatterns: ["https://www.deepl.com/*"],
    challengeUrlPatterns: ["*/cdn-cgi/challenge-platform/*", "*__cf_chl*"],
  },
  maxChars: 1500,
  settleMs: 800,
  pageUrl(source: string, target: string): string {
    const src = encodeURIComponent(deeplFragmentSource(source, target));
    return `https://www.deepl.com/en/translator#${src}/${encodeURIComponent(target)}/`;
  },
  selectors: {
    input: [
      "d-textarea[data-testid=\"translator-source-input\"] div[contenteditable=\"true\"]",
      "d-textarea[data-testid=\"translator-source-input\"] div[role=\"textbox\"]",
      "[data-testid=\"translator-source-input\"] [contenteditable=\"true\"]",
      "d-textarea[dl-test=\"translator-source-input\"] div[contenteditable=\"true\"]",
      "d-textarea.lmt__source_textarea div[contenteditable=\"true\"]",
      "textarea.lmt__source_textarea",
    ],
    output: [
      "d-textarea[data-testid=\"translator-target-input\"]",
      "d-textarea[aria-labelledby=\"translation-target-heading\"]",
      "[data-testid=\"translator-target-input\"]",
      "d-textarea[dl-test=\"translator-target-input\"]",
      "d-textarea.lmt__target_textarea",
      "textarea.lmt__target_textarea",
    ],
    pending: ["d-textarea[data-testid=\"translator-target-input\"][aria-busy=\"true\"]"],
    challenge: [
      "#challenge-widget",
      "#challenge-running",
      "#challenge-stage",
      "iframe[src*=\"challenges.cloudflare.com\"]",
      ".cf-turnstile",
    ],
    rateLimitBanner: ["[data-testid*=\"error\"]", "[data-testid*=\"toast\"]", "[role=\"alert\"]", "[aria-live=\"assertive\"]"],
    targetLang: ["d-textarea[data-testid=\"translator-target-input\"]", "[data-testid=\"translator-target-input\"]"],
  },
  outputParagraphSelector: "p",
  pendingMarkers: ["[...]"],
  pendingTextPatterns: [],
  rateLimitTextPatterns: RATE_LIMIT_TEXT_PATTERNS,
};
