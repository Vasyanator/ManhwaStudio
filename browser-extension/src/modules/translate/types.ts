/*
File: browser-extension/src/modules/translate/types.ts

Purpose:
The wire contract of the `translate` feature module (module version 1): the request params,
the response, and the validation that turns untrusted `params` into a typed request. The Rust
consumer (`ms-tab-translation` `machine_translators/browser.rs`) mirrors these shapes; the
contract is documented in this directory's MODULE_README.md.

Key items:
- TRANSLATE_MODULE_NAME / TRANSLATE_MODULE_VERSION / TRANSLATE_METHOD
- TRANSLATE_SITE_KEYS / TranslateSiteKey
- TranslateParams, TranslateResult
- parseTranslateParams() — params -> TranslateParams or BridgeFailure("bad_params")

Notes:
- ONE text per request. The app packs several bubbles one per line (line breaks inside a
  bubble are already flattened to spaces, decision D2) and splits the answer by lines.
- Language codes arrive in the site's own spelling (the app normalizes per service); only
  their shape is checked here. `source` may be "auto"; `target` may not.
- Any change of these shapes or meanings bumps TRANSLATE_MODULE_VERSION and the app's copy.
*/

import { BridgeFailure } from "../../core/modules";

/** Request namespace of this module (`request.module`). */
export const TRANSLATE_MODULE_NAME = "translate";

/** Module version; the app uses the module only when it speaks exactly this version. */
export const TRANSLATE_MODULE_VERSION = 1;

/** The one method of the module. */
export const TRANSLATE_METHOD = "translate";

/** Sites the module drives, in the app's service order. */
export const TRANSLATE_SITE_KEYS = ["google", "deepl", "yandex"] as const;

/** One translator site; see `TRANSLATE_SITE_KEYS`. */
export type TranslateSiteKey = (typeof TRANSLATE_SITE_KEYS)[number];

/** The source language value that asks the site to detect the language. */
export const AUTO_SOURCE = "auto";

/** Validated `translate` params: `{site, source, target, text}`. */
export interface TranslateParams {
  readonly site: TranslateSiteKey;
  /** Site spelling of the source language, or `"auto"`. */
  readonly source: string;
  /** Site spelling of the target language. */
  readonly target: string;
  /** The text to translate; lines are kept as lines. */
  readonly text: string;
}

/** `translate` result: the full translated text, line structure as the site returned it. */
export interface TranslateResult {
  readonly text: string;
}

/** BCP-47-like shape accepted for a language code (`ja`, `zh-CN`, `en-US`, `zh-hant`). */
const LANGUAGE_CODE_PATTERN = /^[A-Za-z]{2,3}(-[A-Za-z0-9]{2,8})*$/;

const PARAM_KEYS: ReadonlySet<string> = new Set(["site", "source", "target", "text"]);

/** True when `value` is one of `TRANSLATE_SITE_KEYS`. */
export function isTranslateSiteKey(value: unknown): value is TranslateSiteKey {
  return typeof value === "string" && (TRANSLATE_SITE_KEYS as readonly string[]).includes(value);
}

function badParams(message: string): BridgeFailure {
  return new BridgeFailure("bad_params", message);
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/**
 * Validates `params` of a `translate` request. Throws `BridgeFailure("bad_params")` when it is
 * not an object, has an unknown or missing field, an unknown site, a malformed language code,
 * `target == "auto"`, or a non-string text. Length limits are checked later, per site
 * (`too_long`).
 */
export function parseTranslateParams(params: unknown): TranslateParams {
  if (!isRecord(params)) throw badParams("translate params must be an object");
  for (const key of Object.keys(params)) {
    if (!PARAM_KEYS.has(key)) throw badParams(`unknown translate param '${key}'`);
  }
  const { site, source, target, text } = params;
  if (!isTranslateSiteKey(site)) throw badParams(`unknown site '${String(site)}'; expected one of ${TRANSLATE_SITE_KEYS.join(", ")}`);
  if (typeof source !== "string" || (source !== AUTO_SOURCE && !LANGUAGE_CODE_PATTERN.test(source))) {
    throw badParams(`malformed source language '${String(source)}'`);
  }
  if (typeof target !== "string" || !LANGUAGE_CODE_PATTERN.test(target)) {
    throw badParams(`malformed target language '${String(target)}'`);
  }
  if (typeof text !== "string") throw badParams("translate text must be a string");
  return { site, source, target, text };
}
