/*
File: browser-extension/src/core/attention.ts

Purpose:
User attention for site challenges (captcha, Cloudflare check, "unusual traffic" pages). The
extension never solves a challenge: it brings the dedicated tab and its window to the front,
tells the app (`attention` event; the app suspends its deadline and shows "solve the check in
the browser"), waits until the challenge is gone or the attention timeout runs out, and always
closes the episode with an `attention_cleared` event.

Key items:
- AttentionController.hold() (pinned for WP-4)
- AttentionApi / chromeAttentionApi() — the chrome seam (focus, tab events), faked in tests
- ClearProbe — optional module check "is the challenge gone?"

Notes:
- Clear detection: with a `ClearProbe`, the probe is asked every `pollMs` and after every
  completed load of the tab; without one, the first completed load of the tab after `hold`
  started counts as cleared (solving a challenge redirects back to the site).
- `attention` data: `{reason, timeout_ms}`; `attention_cleared` data:
  `{reason, outcome: "cleared" | "timeout" | "cancelled" | "tab_closed"}`.
*/

import { BridgeFailure, failureFromAbort } from "./modules";
import type { EventKind } from "./protocol";
import type { TabChange } from "./tabs";

/** The event sink a held request reports through (`ModuleContext.emit`). */
export type AttentionEmit = (kind: EventKind, data: unknown) => void;

/** Resolves true once the challenge is gone. A rejection is logged and counts as "not yet". */
export type ClearProbe = () => Promise<boolean>;

/** The browser surface attention needs; injectable for tests. */
export interface AttentionApi {
  /** Activates the tab and focuses (and flashes) its window. */
  focusTab(tabId: number): Promise<void>;
  onUpdated(listener: (tabId: number, change: TabChange) => void): () => void;
  onRemoved(listener: (tabId: number) => void): () => void;
}

/** Default poll interval of a `ClearProbe`. */
export const ATTENTION_POLL_MS = 1_000;

type Outcome = "cleared" | "timeout" | "cancelled" | "tab_closed";

/** Runs attention episodes; stateless between calls, so one instance serves every request. */
export class AttentionController {
  constructor(
    private readonly api: AttentionApi = chromeAttentionApi(),
    private readonly pollMs: number = ATTENTION_POLL_MS,
  ) {}

  /**
   * Focuses `tabId`, emits `attention`, and resolves once the challenge is cleared. Rejects
   * with `BridgeFailure("attention_timeout")` after `timeoutMs`, `tab_closed` when the tab is
   * closed, or the abort failure when `signal` aborts. Emits `attention_cleared` on every exit.
   */
  async hold(tabId: number, reason: string, timeoutMs: number, signal: AbortSignal, emit: AttentionEmit, isCleared?: ClearProbe): Promise<void> {
    if (signal.aborted) throw failureFromAbort(signal);
    emit("attention", { reason, timeout_ms: timeoutMs });
    let outcome: Outcome = "cancelled";
    const cleanups: Array<() => void> = [];
    try {
      await this.api.focusTab(tabId).catch((error: unknown) => {
        // Focus is a courtesy: the app already shows the hint, so a failed focus (window
        // closed meanwhile, ...) must not end the episode. It is logged and the wait goes on.
        console.warn("[ms-bridge] could not focus the challenge tab", { tabId, reason, error: String(error) });
      });
      outcome = await new Promise<Outcome>((resolve, reject) => {
        let finished = false;
        const finish = (result: Outcome): void => {
          if (finished) return;
          finished = true;
          resolve(result);
        };
        let probing = false;
        const probe = (): void => {
          if (isCleared === undefined || probing || finished) return;
          probing = true;
          isCleared()
            .then((cleared) => {
              if (cleared) finish("cleared");
            })
            .catch((error: unknown) => {
              console.warn("[ms-bridge] attention probe failed; still waiting", { tabId, reason, error: String(error) });
            })
            .finally(() => {
              probing = false;
            });
        };
        cleanups.push(
          this.api.onUpdated((id, change) => {
            if (id !== tabId || change.status !== "complete") return;
            if (isCleared === undefined) finish("cleared");
            else probe();
          }),
        );
        cleanups.push(
          this.api.onRemoved((id) => {
            if (id === tabId) finish("tab_closed");
          }),
        );
        const onAbort = (): void => {
          if (finished) return;
          finished = true;
          reject(failureFromAbort(signal));
        };
        signal.addEventListener("abort", onAbort, { once: true });
        cleanups.push(() => signal.removeEventListener("abort", onAbort));
        const timer = setTimeout(() => finish("timeout"), timeoutMs);
        cleanups.push(() => clearTimeout(timer));
        if (isCleared !== undefined) {
          const poll = setInterval(probe, this.pollMs);
          cleanups.push(() => clearInterval(poll));
          probe();
        }
      });
    } finally {
      for (const cleanup of cleanups) cleanup();
      emit("attention_cleared", { reason, outcome });
    }
    switch (outcome) {
      case "cleared":
        return;
      case "timeout":
        throw new BridgeFailure("attention_timeout", `the ${reason} check was not solved in time`);
      case "tab_closed":
        throw new BridgeFailure("tab_closed", "the tab with the check was closed");
      case "cancelled":
        throw failureFromAbort(signal);
      default: {
        const unreachable: never = outcome;
        throw new Error(`unhandled attention outcome: ${String(unreachable)}`);
      }
    }
  }
}

/** `AttentionApi` over `chrome.tabs` / `chrome.windows`. */
export function chromeAttentionApi(): AttentionApi {
  return {
    async focusTab(tabId) {
      const tab = await chrome.tabs.update(tabId, { active: true });
      if (tab?.windowId !== undefined) {
        await chrome.windows.update(tab.windowId, { focused: true, drawAttention: true });
      }
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
