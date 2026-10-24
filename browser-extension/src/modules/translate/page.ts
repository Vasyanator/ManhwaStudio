/*
File: browser-extension/src/modules/translate/page.ts

Purpose:
The browser seam of the translate module: reading the dedicated tab's URL and talking to the
page driver through `chrome.scripting`. The module logic (`index.ts`) depends only on the
`TranslatePageApi` interface, so tests run it with a fake page and no browser.

Key items:
- TranslatePageApi
- chromeTranslatePageApi() — the production implementation

Notes:
- Without the "tabs" permission, `tabs.get(...).url` is readable only on hosts the extension
  holds host permissions for (the three translators). An off-site page — Google's `/sorry`
  and consent pages, any redirect — therefore reads as `undefined`, which the module treats
  as "the page needs the user". The install-time permission set is not widened for this.
- Every function is injected into the tab's top frame, ISOLATED world (see driver_protocol.ts).
*/

import { DRIVER_FILE } from "./driver_protocol";
import type { DriverJob } from "./driver_protocol";
import type { TranslateSiteKey } from "./types";

/** What the module needs from the browser for one dedicated tab. */
export interface TranslatePageApi {
  /** The tab's URL; `undefined` when it is not readable (off-site page); `null` when the tab is gone. */
  tabUrl(tabId: number): Promise<string | undefined | null>;
  /** Injects the driver file into the tab's current document (idempotent per document). */
  injectDriver(tabId: number): Promise<void>;
  /** Runs `job` in the driver; resolves with the raw returned value (validate with `parseDriverResult`). */
  runDriver(tabId: number, job: DriverJob): Promise<unknown>;
  /** Asks the driver to end its active run with `cancelled`. */
  cancelDriver(tabId: number): Promise<void>;
  /** Raw answer of the driver's `challengePresent(site)`: `true`, `false`, or `null` without a driver. */
  probeChallenge(tabId: number, site: TranslateSiteKey): Promise<unknown>;
}

/** `TranslatePageApi` over `chrome.tabs` and `chrome.scripting`. */
export function chromeTranslatePageApi(): TranslatePageApi {
  return {
    async tabUrl(tabId) {
      let tab: chrome.tabs.Tab;
      try {
        tab = await chrome.tabs.get(tabId);
      } catch (error: unknown) {
        // `tabs.get` rejects only for an id with no tab; that absence is the answer.
        console.debug("[ms-translate] dedicated tab is gone", { tabId, error: String(error) });
        return null;
      }
      return tab.url;
    },
    async injectDriver(tabId) {
      await chrome.scripting.executeScript({ target: { tabId }, files: [DRIVER_FILE] });
    },
    async runDriver(tabId, job) {
      // The function is serialized into the page: it may only use its argument and globals.
      const results = await chrome.scripting.executeScript({
        target: { tabId },
        func: (j: DriverJob) => globalThis.__msTranslateDriver?.run(j) ?? null,
        args: [job],
      });
      return results[0]?.result;
    },
    async cancelDriver(tabId) {
      await chrome.scripting.executeScript({
        target: { tabId },
        func: () => {
          globalThis.__msTranslateDriver?.cancel();
          return null;
        },
      });
    },
    async probeChallenge(tabId, site) {
      const results = await chrome.scripting.executeScript({
        target: { tabId },
        func: (key: TranslateSiteKey) => globalThis.__msTranslateDriver?.challengePresent(key) ?? null,
        args: [site],
      });
      return results[0]?.result;
    },
  };
}
