/*
File: browser-extension/test/translate_module.test.ts

Purpose:
The `translate` module over fakes of the tab manager seams, the attention seams and the page
driver (`TranslatePageApi`): params validation, limits, registration and dispatch, page reuse
and navigation, challenge hold + retry, failure mapping, lost runs and cancellation.
*/

import assert from "node:assert/strict";
import { test } from "node:test";

import { AttentionController } from "../src/core/attention";
import { Dispatcher } from "../src/core/dispatcher";
import { BridgeFailure, ModuleRegistry } from "../src/core/modules";
import type { FeatureModule, ModuleContext } from "../src/core/modules";
import type { EventKind, ExtMessage } from "../src/core/protocol";
import { SiteTabManager } from "../src/core/tabs";
import type { DriverJob } from "../src/modules/translate/driver_protocol";
import { createTranslateModule } from "../src/modules/translate";
import type { TranslatePageApi } from "../src/modules/translate/page";
import type { TranslateSiteKey } from "../src/modules/translate/types";
import { FakeStore, FakeTabs, flush, sleep } from "./helpers";

const TIMINGS = { elementWaitMs: 50, clearWaitMs: 10, translateTimeoutMs: 50 };

/** FakeTabs that records every navigation target. */
class RecordingTabs extends FakeTabs {
  readonly navigations: string[] = [];

  override async navigate(tabId: number, url: string): Promise<void> {
    this.navigations.push(url);
    await super.navigate(tabId, url);
  }
}

/** One scripted driver answer: what `runDriver` resolves with (or how it fails). */
type Scripted = (job: DriverJob) => Promise<unknown>;

/** A driver answer resolving with `value`. */
function ret(value: unknown): Scripted {
  return async () => value;
}

/** A driver call that fails with `message` (e.g. a navigation removed the frame). */
function throws(message: string): Scripted {
  return async () => {
    throw new Error(message);
  };
}

/** A page whose driver answers from a script; URLs come from the fake tabs. */
class FakePage implements TranslatePageApi {
  readonly jobs: DriverJob[] = [];
  readonly cancels: number[] = [];
  readonly unreadable = new Set<number>();
  challengeVisible = false;
  injectFails = false;
  injections = 0;

  constructor(
    private readonly tabs: FakeTabs,
    private readonly script: Scripted[],
  ) {}

  async tabUrl(tabId: number): Promise<string | undefined | null> {
    const tab = this.tabs.tabs.get(tabId);
    if (tab === undefined) return null;
    return this.unreadable.has(tabId) ? undefined : tab.url;
  }

  async injectDriver(): Promise<void> {
    this.injections += 1;
    if (this.injectFails) throw new Error("Cannot access contents of the page");
  }

  async runDriver(_tabId: number, job: DriverJob): Promise<unknown> {
    this.jobs.push(job);
    const next = this.script.shift();
    if (next === undefined) throw new Error("the test script has no more driver answers");
    return next(job);
  }

  async cancelDriver(tabId: number): Promise<void> {
    this.cancels.push(tabId);
  }

  async probeChallenge(_tabId: number, _site: TranslateSiteKey): Promise<unknown> {
    return this.challengeVisible;
  }
}

interface Harness {
  readonly tabs: RecordingTabs;
  readonly page: FakePage;
  readonly module: FeatureModule;
  readonly events: Array<{ kind: EventKind; data: unknown }>;
  ctx(signal?: AbortSignal): ModuleContext;
}

function harness(script: Scripted[], attentionTimeoutMs = 2_000): Harness {
  const tabs = new RecordingTabs();
  const page = new FakePage(tabs, script);
  const manager = new SiteTabManager(tabs, new FakeStore());
  const attention = new AttentionController(tabs, 5);
  const events: Array<{ kind: EventKind; data: unknown }> = [];
  const module = createTranslateModule(page, { attentionTimeoutMs, timings: TIMINGS });
  return {
    tabs,
    page,
    module,
    events,
    ctx: (signal = new AbortController().signal) => ({ signal, tabs: manager, attention, emit: (kind, data) => events.push({ kind, data }) }),
  };
}

function params(site: TranslateSiteKey, text: string, source = "ja", target = "ru"): unknown {
  return { site, source, target, text };
}

async function rejectsWith(promise: Promise<unknown>, code: string): Promise<BridgeFailure> {
  try {
    await promise;
  } catch (error: unknown) {
    assert.ok(error instanceof BridgeFailure, `expected BridgeFailure, got ${String(error)}`);
    assert.equal(error.code, code, error.message);
    return error;
  }
  assert.fail(`expected a '${code}' failure`);
}

test("module identity: translate v1 over google / deepl / yandex", () => {
  const { module } = harness([]);
  assert.equal(module.name, "translate");
  assert.equal(module.version, 1);
  assert.deepEqual([...module.sites], ["google", "deepl", "yandex"]);
});

test("malformed params fail bad_params without touching a tab", async () => {
  const h = harness([]);
  const bad: unknown[] = [
    null,
    "text",
    [],
    {},
    { site: "bing", source: "ja", target: "ru", text: "x" },
    { site: "google", source: "ja", target: "ru" },
    { site: "google", source: "ja", target: "ru", text: 5 },
    { site: "google", source: "", target: "ru", text: "x" },
    { site: "google", source: "ja", target: "auto", text: "x" },
    { site: "google", source: "ja", target: "r u", text: "x" },
    { site: "google", source: "ja", target: "ru", text: "x", extra: true },
  ];
  for (const p of bad) await rejectsWith(h.module.handle("translate", p, h.ctx()), "bad_params");
  assert.equal(h.tabs.created, 0);
});

test("unknown method fails unknown_method", async () => {
  const h = harness([]);
  await rejectsWith(h.module.handle("detect", params("google", "x"), h.ctx()), "unknown_method");
});

test("text over the site limit fails too_long; a blank text answers empty at once", async () => {
  const h = harness([]);
  await rejectsWith(h.module.handle("translate", params("deepl", "a".repeat(1501)), h.ctx()), "too_long");
  await rejectsWith(h.module.handle("translate", params("google", "a".repeat(5001)), h.ctx()), "too_long");
  assert.deepEqual(await h.module.handle("translate", params("deepl", " \n "), h.ctx()), { text: "" });
  assert.equal(h.tabs.created, 0);
});

test("a translation opens the pair's page once and reuses it", async () => {
  const h = harness([ret({ kind: "ok", text: "один" }), ret({ kind: "ok", text: "два" })]);
  assert.deepEqual(await h.module.handle("translate", params("google", "一"), h.ctx()), { text: "один" });
  assert.deepEqual(await h.module.handle("translate", params("google", "二"), h.ctx()), { text: "два" });
  assert.deepEqual(h.tabs.navigations, ["https://translate.google.com/?sl=ja&tl=ru&op=translate"]);
  assert.equal(h.page.jobs.length, 2);
  assert.deepEqual(h.page.jobs[0], { site: "google", target: "ru", text: "一", ...TIMINGS });
  assert.equal(h.tabs.created, 1);
});

test("another pair navigates again; a fragment-only change hops through about:blank", async () => {
  const h = harness([ret({ kind: "ok", text: "a" }), ret({ kind: "ok", text: "b" })]);
  await h.module.handle("translate", params("deepl", "x", "ko", "ru"), h.ctx());
  await h.module.handle("translate", params("deepl", "y", "ja", "ru"), h.ctx());
  assert.deepEqual(h.tabs.navigations, [
    "https://www.deepl.com/en/translator#ko/ru/",
    "about:blank",
    "https://www.deepl.com/en/translator#ja/ru/",
  ]);
});

test("registered and dispatched like any module", async () => {
  const h = harness([ret({ kind: "ok", text: "привет" })]);
  const registry = new ModuleRegistry();
  registry.register(h.module);
  registry.seal();
  assert.deepEqual(registry.announce(), [{ name: "translate", version: 1, sites: ["google", "deepl", "yandex"] }]);
  const sent: ExtMessage[] = [];
  const ctx = h.ctx();
  const dispatcher = new Dispatcher(registry, { tabs: ctx.tabs, attention: ctx.attention }, (frame) => sent.push(frame));
  dispatcher.handleRequest({ type: "request", id: 7, module: "translate", method: "translate", params: params("yandex", "hello", "en"), timeout_ms: 1000 });
  dispatcher.handleRequest({ type: "request", id: 8, module: "translate", method: "translate", params: { site: "yandex" }, timeout_ms: 1000 });
  await sleep(30);
  assert.deepEqual(sent.find((m) => m.type === "response" && m.id === 7), { type: "response", id: 7, ok: true, result: { text: "привет" } });
  const bad = sent.find((m) => m.type === "response" && m.id === 8);
  assert.ok(bad !== undefined && bad.type === "response" && !bad.ok && bad.error.code === "bad_params");
});

test("an in-page challenge holds for the user, then the run is retried once", async () => {
  const h = harness([ret({ kind: "challenge" }), ret({ kind: "ok", text: "готово" })]);
  h.page.challengeVisible = true;
  const call = h.module.handle("translate", params("deepl", "x"), h.ctx());
  await sleep(20);
  assert.deepEqual(h.events.map((e) => e.kind), ["attention"]);
  assert.deepEqual(h.events[0]?.data, { reason: "deepl_challenge", timeout_ms: 2_000 });
  assert.ok(h.tabs.focused.length > 0, "the tab was brought to the user");
  h.page.challengeVisible = false;
  assert.deepEqual(await call, { text: "готово" });
  assert.deepEqual(h.events.map((e) => e.kind), ["attention", "attention_cleared"]);
  assert.deepEqual(h.events[1]?.data, { reason: "deepl_challenge", outcome: "cleared" });
  assert.equal(h.page.jobs.length, 2);
});

test("a challenge that comes back after the hold fails attention_timeout", async () => {
  const h = harness([ret({ kind: "challenge" }), ret({ kind: "challenge" })]);
  await rejectsWith(h.module.handle("translate", params("deepl", "x"), h.ctx()), "attention_timeout");
  assert.equal(h.page.jobs.length, 2);
});

test("an unsolved challenge fails attention_timeout after the hold's bound", async () => {
  const h = harness([ret({ kind: "challenge" })], 30);
  h.page.challengeVisible = true;
  await rejectsWith(h.module.handle("translate", params("deepl", "x"), h.ctx()), "attention_timeout");
  assert.deepEqual(h.events.map((e) => e.kind), ["attention", "attention_cleared"]);
  assert.deepEqual(h.events[1]?.data, { reason: "deepl_challenge", outcome: "timeout" });
});

test("an off-site page (unreadable URL, e.g. google /sorry) holds before any run", async () => {
  const h = harness([ret({ kind: "ok", text: "ok" })]);
  const ctx = h.ctx();
  const lease = await ctx.tabs.acquire({ key: "google", hostPatterns: [], challengeUrlPatterns: [] }, ctx.signal);
  const tabId = lease.tabId;
  lease.release();
  h.page.unreadable.add(tabId);
  const call = h.module.handle("translate", params("google", "x"), ctx);
  await sleep(20);
  assert.equal(h.page.jobs.length, 0, "no driver run on a page that needs the user");
  assert.deepEqual(h.events.map((e) => e.kind), ["attention"]);
  h.page.unreadable.delete(tabId);
  assert.deepEqual(await call, { text: "ok" });
});

test("driver failures map to their codes", async () => {
  const cases: Array<[unknown, string]> = [
    [{ kind: "rate_limited", message: "Too many requests" }, "rate_limited"],
    [{ kind: "site_changed", role: "input" }, "site_changed"],
    [{ kind: "unsupported_language", applied: "en" }, "unsupported_language"],
    [{ kind: "timeout", stage: "translation" }, "timeout"],
    [{ kind: "cancelled" }, "internal"],
  ];
  for (const [result, code] of cases) {
    const h = harness([ret(result)]);
    await rejectsWith(h.module.handle("translate", params("google", "x"), h.ctx()), code);
  }
});

test("an unsupported language forgets the page so the next request reloads it", async () => {
  const h = harness([ret({ kind: "unsupported_language", applied: "en" }), ret({ kind: "ok", text: "ok" })]);
  await rejectsWith(h.module.handle("translate", params("google", "x"), h.ctx()), "unsupported_language");
  await h.module.handle("translate", params("google", "x"), h.ctx());
  assert.equal(h.tabs.navigations.length, 2);
});

test("a lost run is retried; the second lost run fails internal", async () => {
  const retried = harness([throws("Frame with ID 0 was removed"), ret({ kind: "ok", text: "ok" })]);
  assert.deepEqual(await retried.module.handle("translate", params("google", "x"), retried.ctx()), { text: "ok" });
  assert.equal(retried.tabs.navigations.length, 2, "the page is loaded fresh after a lost run");
  const lost = harness([ret(null), ret("garbage")]);
  await rejectsWith(lost.module.handle("translate", params("google", "x"), lost.ctx()), "internal");
  const refused = harness([]);
  refused.page.injectFails = true;
  await rejectsWith(refused.module.handle("translate", params("google", "x"), refused.ctx()), "internal");
  assert.equal(refused.page.jobs.length, 0);
});

test("a closed dedicated tab fails tab_closed", async () => {
  const h = harness([
    async () => {
      for (const id of [...h.tabs.tabs.keys()]) h.tabs.close(id);
      throw new Error("No tab with id");
    },
  ]);
  await rejectsWith(h.module.handle("translate", params("google", "x"), h.ctx()), "tab_closed");
});

test("cancel aborts the running driver, tells the page and releases the tab", async () => {
  const h = harness([() => new Promise(() => undefined), ret({ kind: "ok", text: "next" })]);
  const controller = new AbortController();
  const call = h.module.handle("translate", params("google", "x"), h.ctx(controller.signal));
  await sleep(20);
  assert.equal(h.page.jobs.length, 1);
  controller.abort(new BridgeFailure("cancelled", "the app cancelled the request"));
  await rejectsWith(call, "cancelled");
  await flush();
  assert.equal(h.page.cancels.length, 1);
  assert.deepEqual(await h.module.handle("translate", params("google", "y"), h.ctx()), { text: "next" }, "the lease was released");
});

test("an already aborted request never acquires the tab", async () => {
  const h = harness([]);
  const controller = new AbortController();
  controller.abort();
  await rejectsWith(h.module.handle("translate", params("google", "x"), h.ctx(controller.signal)), "cancelled");
  assert.equal(h.tabs.created, 0);
});
