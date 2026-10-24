/*
File: browser-extension/src/modules/translate/content/driver.ts

Purpose:
The page driver of the translate module: a content script that the module injects
programmatically (never declared in the manifest) into its own dedicated translator tab only.
It installs `globalThis.__msTranslateDriver` (see driver_protocol.ts) in the extension's
isolated world; the module then calls `run(job)` through `chrome.scripting.executeScript`.

Key items:
- createDriver() — the DriverApi installed as the global
- watchPage() — MutationObserver-driven wait with one-shot timers
- insertText() — clear / insert the source text (execCommand, then a setter fallback)
- readOutput(), challengePresent(), rateLimitMessage() — table-driven page reads

Run flow (one run per document at a time; a new run cancels an older one):
1. wait for the input and output hooks (or a challenge / rate-limit banner);
2. remember the output, clear the input, wait (bounded) until the output is empty;
3. insert the text: `execCommand("insertText")`, else the native value setter (textarea) or
   one `<p>` per line (contenteditable) plus an `input` event; the input must then hold the
   same lines, else `site_changed`;
4. feed every DOM change into `StableOutput` until the output settled, then check the applied
   target language (DeepL) and return the normalized text.

Notes:
- No `setInterval` / timer chains: Chrome throttles chained timers of a page hidden for more
  than 5 minutes to once a minute, while MutationObserver callbacks and a single non-nested
  `setTimeout` are only clamped to 1 s in a background tab.
- The driver never clicks consent dialogs and never touches a challenge; it only reports one.
- Selectors come only from the site tables (`../sites`); an invalid selector is logged and
  treated as "no match".
*/

import { DRIVER_GLOBAL_VERSION } from "../driver_protocol";
import type { DriverApi, DriverJob, DriverResult } from "../driver_protocol";
import { siteFor } from "../sites";
import type { TranslateSite } from "../sites/types";
import type { TranslateSiteKey } from "../types";
import { isPendingText, normalizeOutput, primaryLanguageDiffers, sameLinesLoosely } from "./normalize";
import { StableOutput } from "./stable";

const LOG_PREFIX = "[ms-translate]";

/** One step of a page wait: finish with a value, or keep waiting (optionally re-check after a delay). */
type Step<R> = { readonly done: R } | { readonly recheckMs: number | null };

function done<R>(value: R): Step<R> {
  return { done: value };
}

const KEEP_WAITING: Step<never> = { recheckMs: null };

/** `querySelectorAll` that logs and yields nothing for an invalid selector. */
function queryAll(root: ParentNode, selector: string): Element[] {
  try {
    return Array.from(root.querySelectorAll(selector));
  } catch (error: unknown) {
    console.error(`${LOG_PREFIX} invalid selector in a site table`, { selector, error: String(error) });
    return [];
  }
}

/** First element matched by the first selector (in list order) that matches anything. */
function queryFirst(selectors: readonly string[]): Element | null {
  for (const selector of selectors) {
    const found = queryAll(document, selector)[0];
    if (found !== undefined) return found;
  }
  return null;
}

function anyMatch(selectors: readonly string[]): boolean {
  return queryFirst(selectors) !== null;
}

function asHTMLElement(element: Element | null): HTMLElement | null {
  return element instanceof HTMLElement ? element : null;
}

function isTextField(element: HTMLElement): element is HTMLTextAreaElement | HTMLInputElement {
  return element instanceof HTMLTextAreaElement || element instanceof HTMLInputElement;
}

/** Text of an editor element: a field's value, else its rendered text. */
function readInput(element: HTMLElement): string {
  return isTextField(element) ? element.value : element.innerText;
}

/** The normalized output text of `site` ("" while there is no output element). */
function readOutput(site: TranslateSite): string {
  const element = asHTMLElement(queryFirst(site.selectors.output));
  if (element === null) return "";
  if (isTextField(element)) return normalizeOutput(element.value);
  if (site.outputParagraphSelector !== null) {
    const paragraphs = queryAll(element, site.outputParagraphSelector);
    if (paragraphs.length > 0) return normalizeOutput(paragraphs.map((p) => p.textContent ?? "").join("\n"));
  }
  return normalizeOutput(element.innerText);
}

/** True when the site's in-page challenge is visible. */
function challengePresent(site: TranslateSite): boolean {
  return anyMatch(site.selectors.challenge);
}

/** The text of a visible "too many requests" banner, or `null`. */
function rateLimitMessage(site: TranslateSite): string | null {
  for (const selector of site.selectors.rateLimitBanner) {
    for (const banner of queryAll(document, selector)) {
      const text = (banner.textContent ?? "").replace(/\s+/g, " ").trim();
      if (text !== "" && site.rateLimitTextPatterns.some((pattern) => pattern.test(text))) return text;
    }
  }
  return null;
}

/** A blocking page state that ends any wait: challenge or rate-limit banner. */
function blockingState(site: TranslateSite): DriverResult | null {
  if (challengePresent(site)) return { kind: "challenge" };
  const message = rateLimitMessage(site);
  if (message !== null) return { kind: "rate_limited", message };
  return null;
}

/**
 * Waits until `step` finishes. `step` runs now, after every DOM mutation and at its own
 * re-check delay; the wait ends with `onTimeout()` after `timeoutMs` and with `cancelled`
 * when `signal` aborts. A throwing step ends the wait with that error.
 */
function watchPage<R>(step: () => Step<R>, timeoutMs: number, onTimeout: () => R, signal: AbortSignal): Promise<R | DriverResult> {
  return new Promise<R | DriverResult>((resolve, reject) => {
    let finished = false;
    let recheck: ReturnType<typeof setTimeout> | undefined;
    const cleanups: Array<() => void> = [];
    const finish = (settle: () => void): void => {
      if (finished) return;
      finished = true;
      clearTimeout(recheck);
      for (const cleanup of cleanups) cleanup();
      settle();
    };
    const evaluate = (): void => {
      if (finished) return;
      let outcome: Step<R>;
      try {
        outcome = step();
      } catch (error: unknown) {
        finish(() => reject(error));
        return;
      }
      if ("done" in outcome) {
        const value = outcome.done;
        finish(() => resolve(value));
        return;
      }
      clearTimeout(recheck);
      recheck = outcome.recheckMs === null ? undefined : setTimeout(evaluate, outcome.recheckMs);
    };
    const observer = new MutationObserver(evaluate);
    observer.observe(document.documentElement, { subtree: true, childList: true, characterData: true, attributes: true });
    cleanups.push(() => observer.disconnect());
    const deadline = setTimeout(() => finish(() => resolve(onTimeout())), timeoutMs);
    cleanups.push(() => clearTimeout(deadline));
    const onAbort = (): void => finish(() => resolve({ kind: "cancelled" }));
    signal.addEventListener("abort", onAbort, { once: true });
    cleanups.push(() => signal.removeEventListener("abort", onAbort));
    if (signal.aborted) onAbort();
    else evaluate();
  });
}

/** Selects the whole content of an editor so the next edit command replaces it. */
function selectAll(element: HTMLElement): void {
  element.focus();
  if (isTextField(element)) {
    element.select();
    return;
  }
  const selection = window.getSelection();
  if (selection === null) return;
  const range = document.createRange();
  range.selectNodeContents(element);
  selection.removeAllRanges();
  selection.addRange(range);
}

/** Replaces the editor content without editing commands: value setter or `<p>` per line. */
function setDirectly(element: HTMLElement, text: string): void {
  if (isTextField(element)) {
    // The PROTOTYPE setter, not the element's own: frameworks (React) wrap the instance
    // property to track the value and would otherwise swallow the following input event.
    const proto = element instanceof HTMLTextAreaElement ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype;
    const setter = Object.getOwnPropertyDescriptor(proto, "value")?.set;
    if (setter !== undefined) setter.call(element, text);
    else element.value = text;
  } else {
    const paragraphs = text.split("\n").map((line) => {
      const p = document.createElement("p");
      if (line === "") p.appendChild(document.createElement("br"));
      else p.textContent = line;
      return p;
    });
    element.replaceChildren(...paragraphs);
  }
  element.dispatchEvent(new InputEvent("input", { bubbles: true, inputType: text === "" ? "deleteContentBackward" : "insertText", data: text === "" ? null : text }));
  element.dispatchEvent(new Event("change", { bubbles: true }));
}

/**
 * Puts `text` into the editor (an empty text clears it). Tries the editing command first (it
 * reaches the page as a real `beforeinput` / `input` pair, like typed or IME text), then the
 * direct fallback. Returns false when the editor does not hold `text` line for line after both.
 */
function insertText(element: HTMLElement, text: string): boolean {
  selectAll(element);
  // `execCommand` is deprecated but still the only way to produce a native editing input from
  // script; its boolean only says whether the command ran, so the content is checked after.
  const ran = text === "" ? document.execCommand("delete") : document.execCommand("insertText", false, text);
  const matches = (): boolean => (text === "" ? readInput(element).trim() === "" : sameLinesLoosely(readInput(element), text));
  if (ran && matches()) return true;
  console.debug(`${LOG_PREFIX} editing command did not land; setting the editor directly`, { ran, length: text.length });
  setDirectly(element, text);
  return matches();
}

/** The driver installed as `globalThis.__msTranslateDriver`. */
function createDriver(): DriverApi {
  let active: AbortController | null = null;

  async function run(job: DriverJob): Promise<DriverResult> {
    active?.abort();
    const controller = new AbortController();
    active = controller;
    try {
      return await runJob(job, controller.signal);
    } finally {
      if (active === controller) active = null;
    }
  }

  async function runJob(job: DriverJob, signal: AbortSignal): Promise<DriverResult> {
    const site = siteFor(job.site);

    // 1. The editor and the output exist (the page hydrates after `load`).
    const ready = await watchPage<"ready" | DriverResult>(
      () => {
        const blocked = blockingState(site);
        if (blocked !== null) return done(blocked);
        return anyMatch(site.selectors.input) && anyMatch(site.selectors.output) ? done("ready") : KEEP_WAITING;
      },
      job.elementWaitMs,
      () => ({ kind: "site_changed", role: anyMatch(site.selectors.input) ? "output" : "input" }),
      signal,
    );
    if (ready !== "ready") return ready;

    // 2. Clear the input; an output seen empty afterwards proves the next output is new.
    const before = readOutput(site);
    const input = asHTMLElement(queryFirst(site.selectors.input));
    if (input === null) return { kind: "site_changed", role: "input" };
    if (!insertText(input, "")) return { kind: "site_changed", role: "input" };
    const cleared = await watchPage<"empty" | "not_empty" | DriverResult>(
      () => {
        const blocked = blockingState(site);
        if (blocked !== null) return done(blocked);
        return readOutput(site) === "" ? done("empty") : KEEP_WAITING;
      },
      job.clearWaitMs,
      () => "not_empty",
      signal,
    );
    if (cleared !== "empty" && cleared !== "not_empty") return cleared;
    const detector = new StableOutput(site.settleMs, cleared === "empty" ? null : before);

    // 3. Insert the text (the editor may have been re-rendered by the clear).
    const editor = asHTMLElement(queryFirst(site.selectors.input));
    if (editor === null || !insertText(editor, job.text)) return { kind: "site_changed", role: "input" };

    // 4. Wait for a settled translation.
    return watchPage<DriverResult>(
      () => {
        const blocked = blockingState(site);
        if (blocked !== null) return done(blocked);
        const now = performance.now();
        const text = readOutput(site);
        const pending = anyMatch(site.selectors.pending) || isPendingText(text, site.pendingMarkers, site.pendingTextPatterns);
        detector.observe(text, pending, now);
        const settled = detector.settled(now);
        if (settled === null) return { recheckMs: detector.remainingMs(now) };
        const applied = queryFirst(site.selectors.targetLang)?.getAttribute("lang") ?? "";
        if (applied.trim() !== "" && primaryLanguageDiffers(applied, job.target)) {
          return done({ kind: "unsupported_language", applied });
        }
        return done({ kind: "ok", text: settled });
      },
      job.translateTimeoutMs,
      () => ({ kind: "timeout", stage: "translation" }),
      signal,
    );
  }

  return {
    version: DRIVER_GLOBAL_VERSION,
    run,
    cancel(): void {
      active?.abort();
    },
    challengePresent(key: TranslateSiteKey): boolean {
      return challengePresent(siteFor(key));
    },
  };
}

// Install once per document; a second injection into the same document keeps the instance
// (and its active run) unless it speaks another driver version.
if (globalThis.__msTranslateDriver?.version !== DRIVER_GLOBAL_VERSION) {
  globalThis.__msTranslateDriver = createDriver();
}
