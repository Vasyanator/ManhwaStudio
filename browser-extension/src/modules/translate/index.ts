/*
File: browser-extension/src/modules/translate/index.ts

Purpose:
The `translate` feature module (version 1, method `translate`, sites google / deepl / yandex):
translates one text on a translator web page in the extension's dedicated tab for that site,
using the user's own browser session. Contract (params, result, errors): MODULE_README.md of
this directory; types in `types.ts`.

Key items:
- createTranslateModule() — the FeatureModule (registered in background.ts)
- TranslateModuleOptions, ATTENTION_TIMEOUT_MS, DRIVER_TIMINGS, MAX_LOST_RUNS, classifyTabUrl()

Request flow (under the site's exclusive tab lease):
1. validate params (`bad_params`), check the site's limit (`too_long`); a blank text answers
   `""` without touching a tab;
2. open the pair's page in the dedicated tab unless this module already did (navigation memory
   per site and tab; a fragment-only change hops through about:blank so the page reloads);
3. an off-site or challenge URL -> attention hold, then retry;
4. inject the driver, run it (cancel follows `ctx.signal`), map its result:
   ok -> `{text}`; challenge -> attention hold, then retry; rate_limited / site_changed /
   unsupported_language / timeout -> the same `BridgeFailure` code.

Notes:
- One attention hold per request: a challenge after it fails `attention_timeout` ("the check
  came back") instead of holding again. A run lost to a navigation is retried; the
  MAX_LOST_RUNS-th lost run fails `internal`.
- The attention hold suspends the app's deadline (the app counts its own attention budget);
  the extension's own bound is ATTENTION_TIMEOUT_MS.
*/

import { BridgeFailure, failureFromAbort } from "../../core/modules";
import type { FeatureModule, ModuleContext } from "../../core/modules";
import type { SiteTabLease } from "../../core/tabs";
import { urlMatchesAny } from "../../core/tabs";
import { parseDriverResult } from "./driver_protocol";
import type { DriverJob, DriverResult } from "./driver_protocol";
import { chromeTranslatePageApi } from "./page";
import type { TranslatePageApi } from "./page";
import { siteFor } from "./sites";
import type { TranslateSite } from "./sites/types";
import { TRANSLATE_METHOD, TRANSLATE_MODULE_NAME, TRANSLATE_MODULE_VERSION, TRANSLATE_SITE_KEYS, parseTranslateParams } from "./types";
import type { TranslateParams, TranslateResult } from "./types";

/** The extension's own bound of one attention episode (the app's attention budget is 10 min too). */
export const ATTENTION_TIMEOUT_MS = 10 * 60_000;

/** Driver waits of one run (see `DriverJob`). The app's request deadline usually ends a run first. */
export const DRIVER_TIMINGS = { elementWaitMs: 20_000, clearWaitMs: 3_000, translateTimeoutMs: 60_000 } as const;

/** Driver runs lost to a navigation / refused injection before a request fails `internal`. */
export const MAX_LOST_RUNS = 2;

/** Test seams; production uses the defaults. */
export interface TranslateModuleOptions {
  readonly attentionTimeoutMs?: number;
  readonly timings?: { readonly elementWaitMs: number; readonly clearWaitMs: number; readonly translateTimeoutMs: number };
}

/** Where the dedicated tab is: the translator itself, a page that needs the user, or gone. */
type TabPlace = "site" | "needs_user" | "gone";

function stripFragment(url: string): string {
  const hash = url.indexOf("#");
  return hash < 0 ? url : url.slice(0, hash);
}

/** Classifies a tab URL for `site` (`undefined` = unreadable = off-site, see page.ts). */
export function classifyTabUrl(site: TranslateSite, url: string | undefined | null): TabPlace {
  if (url === null) return "gone";
  if (url === undefined) return "needs_user";
  if (urlMatchesAny(url, site.spec.challengeUrlPatterns)) return "needs_user";
  return urlMatchesAny(url, site.spec.hostPatterns) ? "site" : "needs_user";
}

/** Creates the `translate` module over `page` (the chrome implementation by default). */
export function createTranslateModule(page: TranslatePageApi = chromeTranslatePageApi(), options: TranslateModuleOptions = {}): FeatureModule {
  const attentionTimeoutMs = options.attentionTimeoutMs ?? ATTENTION_TIMEOUT_MS;
  const timings = options.timings ?? DRIVER_TIMINGS;
  // Site key -> the tab and URL this module last navigated it to; lost with the service
  // worker, which only costs one extra page load.
  const navigated = new Map<string, { readonly tabId: number; readonly url: string }>();

  async function place(site: TranslateSite, tabId: number): Promise<TabPlace> {
    return classifyTabUrl(site, await page.tabUrl(tabId));
  }

  /** Shows the pair's page in the lease's tab, navigating only when needed. */
  async function openPage(site: TranslateSite, lease: SiteTabLease, url: string, signal: AbortSignal): Promise<void> {
    const current = await page.tabUrl(lease.tabId);
    if (current === null) throw new BridgeFailure("tab_closed", `the dedicated ${site.key} tab was closed`);
    const known = navigated.get(site.key);
    if (known !== undefined && known.tabId === lease.tabId && known.url === url && classifyTabUrl(site, current) === "site") return;
    navigated.delete(site.key);
    if (current !== undefined && current !== url && stripFragment(current) === stripFragment(url)) {
      // Only the fragment differs: the browser would not reload, and the site applies the
      // language pair from the fragment at load time only.
      await lease.navigate("about:blank", signal);
    }
    await lease.navigate(url, signal);
    navigated.set(site.key, { tabId: lease.tabId, url });
  }

  /** Hands the tab to the user until the challenge is gone (core AttentionController). */
  async function holdForUser(site: TranslateSite, tabId: number, ctx: ModuleContext): Promise<void> {
    console.info("[ms-translate] the site needs the user (captcha / check)", { site: site.key, tabId });
    await ctx.attention.hold(tabId, `${site.key}_challenge`, attentionTimeoutMs, ctx.signal, (kind, data) => ctx.emit(kind, data), async () => {
      if ((await place(site, tabId)) !== "site") return false;
      await page.injectDriver(tabId);
      return (await page.probeChallenge(tabId, site.key)) === false;
    });
  }

  /** Runs `job` in the tab; resolves `null` when the run produced no valid result. Cancel follows `signal`. */
  function runDriver(tabId: number, job: DriverJob, signal: AbortSignal): Promise<DriverResult | null> {
    if (signal.aborted) return Promise.reject(failureFromAbort(signal));
    return new Promise<DriverResult | null>((resolve, reject) => {
      const onAbort = (): void => {
        page.cancelDriver(tabId).catch((error: unknown) => {
          // The page may already be gone; the request is answered `cancelled` regardless.
          console.warn("[ms-translate] could not cancel the page driver", { tabId, error: String(error) });
        });
        reject(failureFromAbort(signal));
      };
      signal.addEventListener("abort", onAbort, { once: true });
      page
        .runDriver(tabId, job)
        .then(
          (raw) => resolve(parseDriverResult(raw)),
          (error: unknown) => {
            // A navigation or a closed tab ends the injected call; the caller re-checks the tab.
            console.warn("[ms-translate] the page driver call failed", { tabId, site: job.site, error: String(error) });
            resolve(null);
          },
        )
        .finally(() => signal.removeEventListener("abort", onAbort));
    });
  }

  /** Injects the driver; false when the document refused it (navigation, error page). */
  async function inject(site: TranslateSite, tabId: number): Promise<boolean> {
    try {
      await page.injectDriver(tabId);
      return true;
    } catch (error: unknown) {
      console.warn("[ms-translate] could not inject the page driver", { tabId, site: site.key, error: String(error) });
      return false;
    }
  }

  async function translateInTab(request: TranslateParams, site: TranslateSite, lease: SiteTabLease, ctx: ModuleContext): Promise<TranslateResult> {
    const { signal } = ctx;
    const url = site.pageUrl(request.source, request.target);
    const job: DriverJob = { site: site.key, target: request.target, text: request.text, ...timings };
    // Bounded: every pass returns, throws, uses the one hold, or counts one lost run.
    let held = false;
    let lostRuns = 0;
    for (;;) {
      await openPage(site, lease, url, signal);
      let where = await place(site, lease.tabId);
      let result: DriverResult | null = null;
      if (where === "site") {
        if (await inject(site, lease.tabId)) result = await runDriver(lease.tabId, job, signal);
        if (signal.aborted) throw failureFromAbort(signal);
        // A run that produced nothing was most likely cut short by a navigation; look again.
        if (result === null) where = await place(site, lease.tabId);
      }
      if (where === "gone") throw new BridgeFailure("tab_closed", `the dedicated ${site.key} tab was closed`);
      if (where === "needs_user" || result?.kind === "challenge") {
        if (held) throw new BridgeFailure("attention_timeout", `the ${site.key} check came back after it was solved`);
        held = true;
        await holdForUser(site, lease.tabId, ctx);
        continue;
      }
      if (result === null) {
        navigated.delete(site.key);
        lostRuns += 1;
        if (lostRuns >= MAX_LOST_RUNS) throw new BridgeFailure("internal", `the ${site.key} page driver returned no result ${lostRuns} times`);
        continue;
      }
      switch (result.kind) {
        case "ok":
          return { text: result.text };
        case "rate_limited":
          throw new BridgeFailure("rate_limited", `${site.key}: ${result.message}`);
        case "site_changed":
          throw new BridgeFailure("site_changed", `the ${site.key} page has no usable ${result.role} (selectors outdated?)`);
        case "unsupported_language":
          // The page did not apply the pair; load it fresh next time.
          navigated.delete(site.key);
          throw new BridgeFailure("unsupported_language", `${site.key} translated into '${result.applied}' instead of '${request.target}'`);
        case "timeout":
          throw new BridgeFailure("timeout", `${site.key} did not finish the ${result.stage} step in time`);
        case "cancelled":
          // Not our signal (checked above): another run replaced this one in the document.
          throw new BridgeFailure("internal", `the ${site.key} page driver run was superseded`);
        default: {
          const unreachable: never = result;
          throw new Error(`unhandled driver result: ${JSON.stringify(unreachable)}`);
        }
      }
    }
  }

  return {
    name: TRANSLATE_MODULE_NAME,
    version: TRANSLATE_MODULE_VERSION,
    sites: TRANSLATE_SITE_KEYS,
    async handle(method: string, params: unknown, ctx: ModuleContext): Promise<unknown> {
      if (method !== TRANSLATE_METHOD) throw new BridgeFailure("unknown_method", `module '${TRANSLATE_MODULE_NAME}' has no method '${method}'`);
      const request = parseTranslateParams(params);
      const site = siteFor(request.site);
      if (request.text.length > site.maxChars) {
        throw new BridgeFailure("too_long", `${request.text.length} characters is over the ${site.key} limit of ${site.maxChars}`);
      }
      if (request.text.trim() === "") return { text: "" } satisfies TranslateResult;
      if (ctx.signal.aborted) throw failureFromAbort(ctx.signal);
      const lease = await ctx.tabs.acquire(site.spec, ctx.signal);
      try {
        return await translateInTab(request, site, lease, ctx);
      } finally {
        lease.release();
      }
    },
  };
}
