/*
File: browser-extension/test/attention.test.ts

Purpose:
AttentionController contracts over fake tabs: focus + `attention` event, clearing by load or
by probe, timeout -> `attention_timeout`, cancel and tab close, and `attention_cleared` on
every exit.
*/

import assert from "node:assert/strict";
import { test } from "node:test";

import { AttentionController } from "../src/core/attention";
import { BridgeFailure } from "../src/core/modules";
import type { EventKind } from "../src/core/protocol";
import { FakeTabs, sleep } from "./helpers";

function recorder(): { events: Array<[EventKind, unknown]>; emit: (kind: EventKind, data: unknown) => void } {
  const events: Array<[EventKind, unknown]> = [];
  return { events, emit: (kind, data) => events.push([kind, data]) };
}

function isFailure(code: string): (error: unknown) => boolean {
  return (error: unknown) => error instanceof BridgeFailure && error.code === code;
}

test("without a probe, the next completed load clears the episode", async () => {
  const tabs = new FakeTabs();
  const tabId = await tabs.createDedicated();
  const { events, emit } = recorder();
  const held = new AttentionController(tabs, 5).hold(tabId, "captcha", 1000, new AbortController().signal, emit);
  await sleep(2);
  assert.deepEqual(tabs.focused, [tabId]);
  tabs.complete(tabId);
  await held;
  assert.deepEqual(events, [
    ["attention", { reason: "captcha", timeout_ms: 1000 }],
    ["attention_cleared", { reason: "captcha", outcome: "cleared" }],
  ]);
});

test("a probe is polled until it reports the challenge gone; probe errors keep waiting", async () => {
  const tabs = new FakeTabs();
  const tabId = await tabs.createDedicated();
  const { events, emit } = recorder();
  let calls = 0;
  const probe = async (): Promise<boolean> => {
    calls += 1;
    if (calls === 1) throw new Error("page is navigating");
    return calls >= 3;
  };
  await new AttentionController(tabs, 2).hold(tabId, "cloudflare", 1000, new AbortController().signal, emit, probe);
  assert.ok(calls >= 3);
  assert.deepEqual(events.at(-1), ["attention_cleared", { reason: "cloudflare", outcome: "cleared" }]);
});

test("timeout, tab close and cancel reject with their codes and still close the episode", async () => {
  const tabs = new FakeTabs();
  const controller = new AttentionController(tabs, 5);

  const t1 = await tabs.createDedicated();
  const r1 = recorder();
  await assert.rejects(controller.hold(t1, "captcha", 15, new AbortController().signal, r1.emit), isFailure("attention_timeout"));
  assert.deepEqual(r1.events.at(-1), ["attention_cleared", { reason: "captcha", outcome: "timeout" }]);

  const t2 = await tabs.createDedicated();
  const r2 = recorder();
  const closing = controller.hold(t2, "captcha", 1000, new AbortController().signal, r2.emit);
  await sleep(2);
  tabs.close(t2);
  await assert.rejects(closing, isFailure("tab_closed"));
  assert.deepEqual(r2.events.at(-1), ["attention_cleared", { reason: "captcha", outcome: "tab_closed" }]);

  const t3 = await tabs.createDedicated();
  const r3 = recorder();
  const abort = new AbortController();
  const cancelled = controller.hold(t3, "captcha", 1000, abort.signal, r3.emit);
  await sleep(2);
  abort.abort(new BridgeFailure("cancelled", "app cancelled"));
  await assert.rejects(cancelled, isFailure("cancelled"));
  assert.deepEqual(r3.events.at(-1), ["attention_cleared", { reason: "captcha", outcome: "cancelled" }]);
  assert.equal(tabs.updated.size + tabs.removed.size, 0, "every listener was removed");
});
