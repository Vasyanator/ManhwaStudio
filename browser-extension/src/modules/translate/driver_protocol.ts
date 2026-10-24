/*
File: browser-extension/src/modules/translate/driver_protocol.ts

Purpose:
The contract between the translate module (service worker) and its page driver (content
script `content/driver.ts`, injected into the dedicated site tab only). The background calls
the driver through `chrome.scripting.executeScript({func, args})`; Chrome awaits the promise
the driver returns and hands back its (JSON) value, which `parseDriverResult` re-validates.

Key items:
- DRIVER_FILE, DRIVER_GLOBAL_VERSION
- DriverJob, DriverResult, DriverApi (+ the `__msTranslateDriver` global it lives in)
- parseDriverResult()

Notes:
- Both executeScript calls run in the extension's ISOLATED world of the tab's top frame, so
  the page's own scripts never see the driver global, and the driver global set by the file
  injection is visible to the later `func` injections.
- A navigation of the tab destroys the global; the module injects the file again before every
  run (the driver keeps the first instance when it is injected twice into one document).
*/

import type { TranslateSiteKey } from "./types";

/** Extension-relative path of the bundled driver (`build.mjs`: modules/<module>/driver.js). */
export const DRIVER_FILE = "modules/translate/driver.js";

/** Version of the driver API; a mismatching instance in a document is replaced. */
export const DRIVER_GLOBAL_VERSION = 1;

/** One translation run inside the page. */
export interface DriverJob {
  readonly site: TranslateSiteKey;
  readonly target: string;
  readonly text: string;
  /** Longest wait for the editor to appear after a load (ms). */
  readonly elementWaitMs: number;
  /** Longest wait for the output to empty after clearing the input (ms). */
  readonly clearWaitMs: number;
  /** Longest wait for a settled translation after inserting the text (ms). */
  readonly translateTimeoutMs: number;
}

/**
 * What a run ended with. `site_changed.role` names the missing hook (`input` / `output`);
 * `cancelled` answers a `cancel()` or a newer `run()` in the same document (the module's
 * per-site lease makes that an orphan from a cancelled request); `timeout.stage` names the
 * step that ran out (`translation`; a missing hook ends as `site_changed`).
 */
export type DriverResult =
  | { readonly kind: "ok"; readonly text: string }
  | { readonly kind: "challenge" }
  | { readonly kind: "rate_limited"; readonly message: string }
  | { readonly kind: "site_changed"; readonly role: string }
  | { readonly kind: "unsupported_language"; readonly applied: string }
  | { readonly kind: "timeout"; readonly stage: string }
  | { readonly kind: "cancelled" };

/** The driver's API in the page's isolated world. */
export interface DriverApi {
  readonly version: number;
  run(job: DriverJob): Promise<DriverResult>;
  /** Ends the active run with `cancelled`; a no-op when none runs. */
  cancel(): void;
  /** True when the site's in-page challenge is visible now. */
  challengePresent(site: TranslateSiteKey): boolean;
}

declare global {
  // The driver instance of this document's isolated world (set by content/driver.ts).
  var __msTranslateDriver: DriverApi | undefined;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/**
 * Validates a value returned through `executeScript` as a `DriverResult`; `null` when it is
 * anything else (no driver in the document, a navigation ended the run, a foreign value).
 */
export function parseDriverResult(value: unknown): DriverResult | null {
  if (!isRecord(value)) return null;
  switch (value.kind) {
    case "ok":
      return typeof value.text === "string" ? { kind: "ok", text: value.text } : null;
    case "challenge":
      return { kind: "challenge" };
    case "cancelled":
      return { kind: "cancelled" };
    case "rate_limited":
      return typeof value.message === "string" ? { kind: "rate_limited", message: value.message } : null;
    case "site_changed":
      return typeof value.role === "string" ? { kind: "site_changed", role: value.role } : null;
    case "unsupported_language":
      return typeof value.applied === "string" ? { kind: "unsupported_language", applied: value.applied } : null;
    case "timeout":
      return typeof value.stage === "string" ? { kind: "timeout", stage: value.stage } : null;
    default:
      return null;
  }
}
