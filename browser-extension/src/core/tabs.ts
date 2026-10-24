/*
File: browser-extension/src/core/tabs.ts

Purpose:
Dedicated site tabs for feature modules. Each site key (e.g. "deepl") gets ONE pinned,
inactive, non-discardable tab that the extension creates and reuses; the user's own tabs on
that site are never touched. A module drives the tab only while it holds the site's exclusive
lease, so two requests never type into the same page at once.

Key items:
- SiteSpec, SiteTabLease, SiteTabManager (pinned for WP-4)
- TabsApi / SessionStore — the chrome seams (`chromeTabsApi`, `chromeSessionStore`), faked in tests
- urlMatchesPattern() — the glob rule used by `hostPatterns` and `challengeUrlPatterns`

Notes:
- The tab id is kept in `chrome.storage.session` (key `ms_site_tab:<site>`), so a restarted
  service worker reuses the same tab within one browser session.
- Patterns are globs over the FULL URL: `*` matches any run of characters (including none),
  every other character matches itself, e.g. `https://www.deepl.com/*`.
- `navigate` resolves when the tab reports `status: "complete"` after the update (or at once
  when the tab already shows that URL completely, e.g. a fragment-only change); page drivers
  still wait for their own selectors.
*/

import { BridgeFailure, failureFromAbort } from "./modules";
import { isWireInteger } from "./protocol";

/** A site a module drives: its key, the URLs that belong to it and its challenge pages. */
export interface SiteSpec { readonly key: string; readonly hostPatterns: readonly string[]; readonly challengeUrlPatterns: readonly string[]; }

/** Exclusive use of a site's dedicated tab; `release` is idempotent and must always be called. */
export interface SiteTabLease { readonly tabId: number; navigate(url: string, signal: AbortSignal): Promise<void>; release(): void; }

/** What the manager needs to know about a tab. */
export interface TabInfo {
  readonly id: number;
  readonly windowId: number;
  readonly url: string | undefined;
  readonly status: string | undefined;
}

/** Fields of a `tabs.onUpdated` change the manager reads. */
export interface TabChange {
  readonly url?: string | undefined;
  readonly status?: string | undefined;
}

/** The chrome.tabs surface used by the core; injectable for tests. */
export interface TabsApi {
  /** The tab, or `null` when no tab with this id exists any more. */
  get(tabId: number): Promise<TabInfo | null>;
  /** Creates a pinned, inactive, non-discardable `about:blank` tab and returns its id. */
  createDedicated(): Promise<number>;
  /** Starts navigating `tabId` to `url`. */
  navigate(tabId: number, url: string): Promise<void>;
  /** Subscribes to tab changes; returns the unsubscribe function. */
  onUpdated(listener: (tabId: number, change: TabChange) => void): () => void;
  /** Subscribes to tab removal; returns the unsubscribe function. */
  onRemoved(listener: (tabId: number) => void): () => void;
}

/** Session-scoped key/value storage (chrome.storage.session); injectable for tests. */
export interface SessionStore {
  get(key: string): Promise<unknown>;
  set(key: string, value: unknown): Promise<void>;
  remove(key: string): Promise<void>;
}

/** Longest wait for one navigation before `navigate` fails with `timeout`. */
export const NAVIGATION_TIMEOUT_MS = 60_000;

/** True when `url` matches the glob `pattern` (see the file header for the rule). */
export function urlMatchesPattern(url: string, pattern: string): boolean {
  const escaped = pattern
    .split("*")
    .map((part) => part.replace(/[.+?^${}()|[\]\\]/g, "\\$&"))
    .join(".*");
  return new RegExp(`^${escaped}$`).test(url);
}

/** True when `url` matches any of `patterns`. */
export function urlMatchesAny(url: string, patterns: readonly string[]): boolean {
  return patterns.some((pattern) => urlMatchesPattern(url, pattern));
}

function storageKey(siteKey: string): string {
  return `ms_site_tab:${siteKey}`;
}

/** Owns the dedicated tab of every site and the per-site exclusive lease. */
export class SiteTabManager {
  private readonly held = new Set<string>();
  private readonly waiters = new Map<string, Array<() => void>>();

  constructor(
    private readonly api: TabsApi = chromeTabsApi(),
    private readonly store: SessionStore = chromeSessionStore(),
    private readonly navigationTimeoutMs: number = NAVIGATION_TIMEOUT_MS,
  ) {}

  /**
   * Waits for the site's lease (FIFO), then returns it with the site's dedicated tab, created
   * if missing. Rejects with the abort failure (`cancelled`) when `signal` aborts first.
   */
  async acquire(site: SiteSpec, signal: AbortSignal): Promise<SiteTabLease> {
    await this.lockSite(site.key, signal);
    try {
      if (signal.aborted) throw failureFromAbort(signal);
      const tabId = await this.ensureTab(site.key);
      return this.makeLease(site.key, tabId);
    } catch (error: unknown) {
      this.unlockSite(site.key);
      throw error;
    }
  }

  private lockSite(key: string, signal: AbortSignal): Promise<void> {
    if (signal.aborted) return Promise.reject(failureFromAbort(signal));
    if (!this.held.has(key)) {
      this.held.add(key);
      return Promise.resolve();
    }
    return new Promise<void>((resolve, reject) => {
      const queue = this.waiters.get(key) ?? [];
      this.waiters.set(key, queue);
      const wake = (): void => {
        signal.removeEventListener("abort", onAbort);
        resolve();
      };
      const onAbort = (): void => {
        const index = queue.indexOf(wake);
        if (index >= 0) queue.splice(index, 1);
        reject(failureFromAbort(signal));
      };
      queue.push(wake);
      signal.addEventListener("abort", onAbort, { once: true });
    });
  }

  /** Hands the lease to the next waiter (ownership moves without a free gap) or frees it. */
  private unlockSite(key: string): void {
    const next = this.waiters.get(key)?.shift();
    if (next !== undefined) {
      next();
      return;
    }
    this.waiters.delete(key);
    this.held.delete(key);
  }

  private async ensureTab(siteKey: string): Promise<number> {
    const stored = await this.store.get(storageKey(siteKey));
    if (isWireInteger(stored) && (await this.api.get(stored)) !== null) return stored;
    const tabId = await this.api.createDedicated();
    await this.store.set(storageKey(siteKey), tabId);
    return tabId;
  }

  private makeLease(siteKey: string, tabId: number): SiteTabLease {
    let released = false;
    return {
      tabId,
      navigate: (url: string, signal: AbortSignal) => this.navigate(siteKey, tabId, url, signal),
      release: () => {
        if (released) return;
        released = true;
        this.unlockSite(siteKey);
      },
    };
  }

  private async forgetTab(siteKey: string, tabId: number): Promise<void> {
    if ((await this.store.get(storageKey(siteKey))) === tabId) await this.store.remove(storageKey(siteKey));
  }

  private async navigate(siteKey: string, tabId: number, url: string, signal: AbortSignal): Promise<void> {
    if (signal.aborted) throw failureFromAbort(signal);
    let settle: { resolve: () => void; reject: (error: BridgeFailure) => void } | null = null;
    const done = new Promise<void>((resolve, reject) => {
      settle = { resolve, reject };
    });
    // Subscribe before starting the navigation so its `complete` cannot be missed.
    const offUpdated = this.api.onUpdated((id, change) => {
      if (id === tabId && change.status === "complete") settle?.resolve();
    });
    let removed = false;
    const offRemoved = this.api.onRemoved((id) => {
      if (id !== tabId) return;
      removed = true;
      settle?.reject(new BridgeFailure("tab_closed", `the dedicated ${siteKey} tab was closed`));
    });
    const onAbort = (): void => settle?.reject(failureFromAbort(signal));
    signal.addEventListener("abort", onAbort, { once: true });
    const timer = setTimeout(() => {
      settle?.reject(new BridgeFailure("timeout", `the ${siteKey} page did not finish loading`));
    }, this.navigationTimeoutMs);
    try {
      await this.api.navigate(tabId, url);
      const tab = await this.api.get(tabId);
      if (tab === null) throw new BridgeFailure("tab_closed", `the dedicated ${siteKey} tab was closed`);
      if (tab.url === url && tab.status === "complete") return;
      await done;
    } finally {
      offUpdated();
      offRemoved();
      signal.removeEventListener("abort", onAbort);
      clearTimeout(timer);
      if (removed) await this.forgetTab(siteKey, tabId);
    }
  }
}

/** `TabsApi` over `chrome.tabs`. */
export function chromeTabsApi(): TabsApi {
  return {
    async get(tabId) {
      try {
        const tab = await chrome.tabs.get(tabId);
        return { id: tabId, windowId: tab.windowId, url: tab.url ?? tab.pendingUrl, status: tab.status };
      } catch (error: unknown) {
        // `tabs.get` rejects only when no tab has this id (the id is a valid integer here);
        // that absence is the answer, not a failure.
        console.debug("[ms-bridge] tab is gone", { tabId, error: String(error) });
        return null;
      }
    },
    async createDedicated() {
      const tab = await chrome.tabs.create({ url: "about:blank", pinned: true, active: false });
      if (tab.id === undefined) throw new BridgeFailure("internal", "the browser created a tab without an id");
      await chrome.tabs.update(tab.id, { autoDiscardable: false });
      return tab.id;
    },
    async navigate(tabId, url) {
      await chrome.tabs.update(tabId, { url });
    },
    onUpdated(listener) {
      const wrapped = (tabId: number, change: chrome.tabs.OnUpdatedInfo): void => {
        listener(tabId, { url: change.url, status: change.status });
      };
      chrome.tabs.onUpdated.addListener(wrapped);
      return () => chrome.tabs.onUpdated.removeListener(wrapped);
    },
    onRemoved(listener) {
      const wrapped = (tabId: number): void => listener(tabId);
      chrome.tabs.onRemoved.addListener(wrapped);
      return () => chrome.tabs.onRemoved.removeListener(wrapped);
    },
  };
}

/** `SessionStore` over `chrome.storage.session`. */
export function chromeSessionStore(): SessionStore {
  return {
    async get(key) {
      const items = await chrome.storage.session.get(key);
      return items[key];
    },
    async set(key, value) {
      await chrome.storage.session.set({ [key]: value });
    },
    async remove(key) {
      await chrome.storage.session.remove(key);
    },
  };
}
