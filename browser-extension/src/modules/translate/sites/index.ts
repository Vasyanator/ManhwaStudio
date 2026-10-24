/*
File: browser-extension/src/modules/translate/sites/index.ts

Purpose:
Site key -> table lookup for the translate module (background and content script alike).
*/

import type { TranslateSiteKey } from "../types";
import { DEEPL_SITE } from "./deepl";
import { GOOGLE_SITE } from "./google";
import type { TranslateSite } from "./types";
import { YANDEX_SITE } from "./yandex";

/** Every site table, keyed by site key. */
export const TRANSLATE_SITES: Readonly<Record<TranslateSiteKey, TranslateSite>> = {
  google: GOOGLE_SITE,
  deepl: DEEPL_SITE,
  yandex: YANDEX_SITE,
};

/** The table of `key`. */
export function siteFor(key: TranslateSiteKey): TranslateSite {
  return TRANSLATE_SITES[key];
}
