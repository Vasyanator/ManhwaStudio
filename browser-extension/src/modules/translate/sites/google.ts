/*
File: browser-extension/src/modules/translate/sites/google.ts

Purpose:
The ONE table of Google Translate (translate.google.com): URL builder, limit, selectors.

Notes:
- URL: `https://translate.google.com/?sl=<src>&tl=<tgt>&op=translate`; `sl=auto` detects.
- Challenges leave the site: `/sorry` ("unusual traffic") on www.google.com and the EU
  consent page on consent.google.com. The extension has no host permission there, so the
  module treats any URL outside `hostPatterns` (or an unreadable one) as "needs the user".
- Selectors: OneMoreGres/ScreenTranslator `translators/google.js` (`textarea.er8xn`,
  `span.HwtZe`) plus attribute hooks; Google's class names are obfuscated and drift.
  All UNVERIFIED against today's page.
- 5000 characters is the documented web limit (2016); UNVERIFIED today. The app splits below it.
*/

import type { TranslateSite } from "./types";
import { RATE_LIMIT_TEXT_PATTERNS } from "./types";

/** Google Translate web page. */
export const GOOGLE_SITE: TranslateSite = {
  key: "google",
  spec: {
    key: "google",
    hostPatterns: ["https://translate.google.com/*"],
    challengeUrlPatterns: ["https://www.google.com/sorry*", "https://translate.google.com/sorry*", "https://consent.google.com/*"],
  },
  maxChars: 5000,
  settleMs: 800,
  pageUrl(source: string, target: string): string {
    const query = new URLSearchParams({ sl: source, tl: target, op: "translate" });
    return `https://translate.google.com/?${query.toString()}`;
  },
  selectors: {
    input: ["textarea[jsname=\"BJE2fc\"]", "textarea.er8xn", "c-wiz textarea[aria-label]", "c-wiz textarea"],
    output: ["span.HwtZe[lang]", "span[jsname=\"jqKxS\"]", "div.lRu31 span[lang]", "div[aria-live=\"polite\"] span[lang]"],
    pending: [],
    challenge: ["form#captcha-form", "iframe[src*=\"recaptcha\"]"],
    rateLimitBanner: ["[role=\"alert\"]", "[aria-live=\"assertive\"]"],
    targetLang: [],
  },
  outputParagraphSelector: null,
  pendingMarkers: [],
  // Google shows a localized "Translating..." placeholder while a long text is in flight.
  pendingTextPatterns: [/^(translating|перевод|traduciendo|traduction|traduzindo)\s*(\.\.\.|…)$/i],
  rateLimitTextPatterns: RATE_LIMIT_TEXT_PATTERNS,
};
