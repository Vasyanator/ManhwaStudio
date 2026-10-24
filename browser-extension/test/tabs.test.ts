/*
File: browser-extension/test/tabs.test.ts

Purpose:
SiteTabManager contracts over fake tabs: one dedicated tab per site reused across leases and
service-worker restarts, FIFO exclusive leases, cancel while waiting, navigation outcomes, and
the glob rule of site patterns.
*/

import assert from "node:assert/strict";
import { test } from "node:test";

import { BridgeFailure } from "../src/core/modules";
import { SiteTabManager, urlMatchesAny, urlMatchesPattern } from "../src/core/tabs";
import type { SiteSpec } from "../src/core/tabs";
import { FakeStore, FakeTabs, flush, sleep } from "./helpers";

const DEEPL: SiteSpec = { key: "deepl", hostPatterns: ["https://www.deepl.com/*"], challengeUrlPatterns: ["*challenge*"] };
const GOOGLE: SiteSpec = { key: "google", hostPatterns: ["https://translate.google.com/*"], challengeUrlPatterns: ["https://www.google.com/sorry*"] };

function signal(): AbortSignal {
  return new AbortController().signal;
}

test("glob patterns match the whole URL with * as any run", () => {
  assert.ok(urlMatchesPattern("https://www.deepl.com/translator#ja/ru/", "https://www.deepl.com/*"));
  assert.ok(!urlMatchesPattern("https://www.deepl.com.evil.io/", "https://www.deepl.com/*"));
  assert.ok(urlMatchesPattern("https://www.google.com/sorry/index?x=1", "https://www.google.com/sorry*"));
  assert.ok(!urlMatchesPattern("https://xwww.deepl.com/", "https://www.deepl.com/*"));
  assert.ok(urlMatchesPattern("a.b", "a.b"));
  assert.ok(!urlMatchesPattern("axb", "a.b"), "dots are literal");
  assert.ok(urlMatchesAny("https://www.google.com/sorry", GOOGLE.challengeUrlPatterns));
});

test("one dedicated tab per site, reused across leases and manager restarts", async () => {
  const tabs = new FakeTabs();
  const store = new FakeStore();
  const manager = new SiteTabManager(tabs, store);
  const a = await manager.acquire(DEEPL, signal());
  a.release();
  const b = await manager.acquire(DEEPL, signal());
  assert.equal(b.tabId, a.tabId);
  b.release();
  const g = await manager.acquire(GOOGLE, signal());
  assert.notEqual(g.tabId, a.tabId);
  g.release();
  // A restarted service worker (new manager, same session storage) reuses the tab.
  const again = await new SiteTabManager(tabs, store).acquire(DEEPL, signal());
  assert.equal(again.tabId, a.tabId);
  assert.equal(tabs.created, 2);
  // A tab the user closed is replaced.
  tabs.close(again.tabId);
  again.release();
  const fresh = await manager.acquire(DEEPL, signal());
  assert.notEqual(fresh.tabId, a.tabId);
  fresh.release();
});

test("leases are exclusive and FIFO per site; release is idempotent", async () => {
  const manager = new SiteTabManager(new FakeTabs(), new FakeStore());
  const order: string[] = [];
  const first = await manager.acquire(DEEPL, signal());
  const second = manager.acquire(DEEPL, signal()).then((lease) => {
    order.push("second");
    return lease;
  });
  const third = manager.acquire(DEEPL, signal()).then((lease) => {
    order.push("third");
    return lease;
  });
  // Another site is not blocked.
  const other = await manager.acquire(GOOGLE, signal());
  other.release();
  await flush();
  assert.deepEqual(order, []);
  first.release();
  first.release();
  const secondLease = await second;
  await flush();
  assert.deepEqual(order, ["second"]);
  secondLease.release();
  (await third).release();
  assert.deepEqual(order, ["second", "third"]);
});

test("a waiter whose request is cancelled leaves the queue with the abort failure", async () => {
  const manager = new SiteTabManager(new FakeTabs(), new FakeStore());
  const holder = await manager.acquire(DEEPL, signal());
  const controller = new AbortController();
  const waiting = manager.acquire(DEEPL, controller.signal);
  controller.abort(new BridgeFailure("cancelled", "app cancelled"));
  await assert.rejects(waiting, (error: unknown) => error instanceof BridgeFailure && error.code === "cancelled");
  holder.release();
  // The cancelled waiter did not take the lease: the next acquire gets it at once.
  const next = await manager.acquire(DEEPL, signal());
  next.release();
});

test("navigate resolves on load complete and fails on close, timeout and cancel", async () => {
  const tabs = new FakeTabs();
  const store = new FakeStore();
  const manager = new SiteTabManager(tabs, store, 30);
  const lease = await manager.acquire(DEEPL, signal());
  await lease.navigate("https://www.deepl.com/translator#ja/ru/", signal());

  tabs.autoComplete = false;
  await assert.rejects(lease.navigate("https://www.deepl.com/a", signal()), (e: unknown) => e instanceof BridgeFailure && e.code === "timeout");

  const controller = new AbortController();
  const cancelled = lease.navigate("https://www.deepl.com/b", controller.signal);
  controller.abort();
  await assert.rejects(cancelled, (e: unknown) => e instanceof BridgeFailure && e.code === "cancelled");

  const closing = lease.navigate("https://www.deepl.com/c", signal());
  await sleep(1);
  tabs.close(lease.tabId);
  await assert.rejects(closing, (e: unknown) => e instanceof BridgeFailure && e.code === "tab_closed");
  assert.equal(store.values.size, 0, "a closed tab is forgotten");
  lease.release();
});
