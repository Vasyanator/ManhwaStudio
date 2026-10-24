/*
File: browser-extension/src/modules/translate/sites/yandex.ts

Purpose:
The ONE table of Yandex Translate (translate.yandex.ru / .com): URL builder, limit, selectors.

Notes:
- URL: `https://translate.yandex.ru/?source_lang=<src>&target_lang=<tgt>`; an auto source
  omits `source_lang` so the page detects. UNVERIFIED: older pages used `lang=<src>-<tgt>`
  (OneMoreGres/ScreenTranslator); the site may also redirect .ru -> .com (both are hosts).
- Challenges: SmartCaptcha on `/showcaptcha` (same host) and its in-page checkbox / advanced
  forms.
- Hooks: `#fakeArea` (contenteditable editor) / `#textarea`, output `#translation`
  (ScreenTranslator reads `span.translation-word` inside it). All UNVERIFIED today.
- 5000 characters is a conservative guess (the 10000 figure is the Cloud API's); UNVERIFIED.
*/

import { AUTO_SOURCE } from "../types";
import type { TranslateSite } from "./types";
import { RATE_LIMIT_TEXT_PATTERNS } from "./types";

/** Yandex Translate web page. */
export const YANDEX_SITE: TranslateSite = {
  key: "yandex",
  spec: {
    key: "yandex",
    hostPatterns: ["https://translate.yandex.ru/*", "https://translate.yandex.com/*"],
    challengeUrlPatterns: ["https://translate.yandex.ru/showcaptcha*", "https://translate.yandex.com/showcaptcha*", "*/checkcaptcha*"],
  },
  maxChars: 5000,
  settleMs: 800,
  pageUrl(source: string, target: string): string {
    const query = new URLSearchParams();
    if (source !== AUTO_SOURCE) query.set("source_lang", source);
    query.set("target_lang", target);
    return `https://translate.yandex.ru/?${query.toString()}`;
  },
  selectors: {
    input: ["#fakeArea[contenteditable]", "#fakeArea", "textarea#textarea", "#textarea"],
    output: ["#translation", "#dstBox", "[data-testid=\"translation\"]"],
    pending: [],
    challenge: [".CheckboxCaptcha", ".AdvancedCaptcha", "#checkbox-captcha-form", "form[action*=\"checkcaptcha\"]", "iframe[src*=\"smartcaptcha\"]"],
    rateLimitBanner: ["[role=\"alert\"]", "[aria-live=\"assertive\"]"],
    targetLang: [],
  },
  outputParagraphSelector: null,
  pendingMarkers: [],
  pendingTextPatterns: [],
  rateLimitTextPatterns: RATE_LIMIT_TEXT_PATTERNS,
};
