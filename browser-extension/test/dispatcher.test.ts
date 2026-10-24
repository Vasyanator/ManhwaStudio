/*
File: browser-extension/test/dispatcher.test.ts

Purpose:
Dispatcher and module-registry contracts with fake modules: one response per request,
error mapping, cancel -> `cancelled`, connection loss -> silent abort, events only while in
flight, oversized results -> `too_long`, and the registry's registration rules.
*/

import assert from "node:assert/strict";
import { test } from "node:test";

import { AttentionController } from "../src/core/attention";
import { Dispatcher } from "../src/core/dispatcher";
import { BridgeFailure, ModuleRegistry, failureFromAbort } from "../src/core/modules";
import type { FeatureModule, ModuleContext } from "../src/core/modules";
import { MAX_TEXT_FRAME_BYTES } from "../src/core/protocol";
import type { ExtMessage, RequestMessage } from "../src/core/protocol";
import { SiteTabManager } from "../src/core/tabs";
import { FakeStore, FakeTabs, flush, sleep } from "./helpers";

function services(): { tabs: SiteTabManager; attention: AttentionController } {
  const fake = new FakeTabs();
  return { tabs: new SiteTabManager(fake, new FakeStore()), attention: new AttentionController(fake, 5) };
}

/** A module whose `handle` behaviour the test sets per method. */
function fakeModule(name: string, handle: (method: string, params: unknown, ctx: ModuleContext) => Promise<unknown>): FeatureModule {
  return { name, version: 1, sites: ["site"], handle };
}

function request(id: number, module: string, method: string, params: unknown = {}): RequestMessage {
  return { type: "request", id, module, method, params, timeout_ms: 1000 };
}

/** Waits until a cancel-aware handler sees its abort. */
function untilAborted(signal: AbortSignal): Promise<never> {
  return new Promise((_, reject) => signal.addEventListener("abort", () => reject(failureFromAbort(signal)), { once: true }));
}

function setup(...modules: FeatureModule[]): { dispatcher: Dispatcher; sent: ExtMessage[] } {
  const registry = new ModuleRegistry();
  for (const m of modules) registry.register(m);
  registry.seal();
  const sent: ExtMessage[] = [];
  return { dispatcher: new Dispatcher(registry, services(), (frame) => sent.push(frame)), sent };
}

test("a successful call answers ok with the result; undefined becomes null", async () => {
  const { dispatcher, sent } = setup(
    fakeModule("echo", async (method, params) => (method === "echo" ? params : undefined)),
  );
  dispatcher.handleRequest(request(1, "echo", "echo", { a: 1 }));
  dispatcher.handleRequest(request(2, "echo", "nothing"));
  await flush();
  assert.deepEqual(sent, [
    { type: "response", id: 1, ok: true, result: { a: 1 } },
    { type: "response", id: 2, ok: true, result: null },
  ]);
  assert.equal(dispatcher.inFlightCount, 0);
});

test("unknown module, BridgeFailure and plain errors map to their codes", async () => {
  const { dispatcher, sent } = setup(
    fakeModule("m", async (method) => {
      if (method === "limited") throw new BridgeFailure("rate_limited", "slow down", 1500.4);
      if (method === "busy") throw new BridgeFailure("busy", "busy", 99);
      throw new Error("boom");
    }),
  );
  dispatcher.handleRequest(request(1, "absent", "x"));
  dispatcher.handleRequest(request(2, "m", "limited"));
  dispatcher.handleRequest(request(3, "m", "busy"));
  dispatcher.handleRequest(request(4, "m", "other"));
  await flush();
  assert.deepEqual(sent, [
    { type: "response", id: 1, ok: false, error: { code: "unknown_module", message: "no module 'absent' in this extension" } },
    { type: "response", id: 2, ok: false, error: { code: "rate_limited", message: "slow down", retry_after_ms: 1500 } },
    // `retry_after_ms` is only meaningful (and only sent) with `rate_limited`.
    { type: "response", id: 3, ok: false, error: { code: "busy", message: "busy" } },
    { type: "response", id: 4, ok: false, error: { code: "internal", message: "boom" } },
  ]);
});

test("cancel aborts the call and answers cancelled, even if the module resolves late", async () => {
  const { dispatcher, sent } = setup(
    fakeModule("m", async (method, _params, ctx) => {
      if (method === "wait") return untilAborted(ctx.signal);
      await new Promise<void>((resolve) => ctx.signal.addEventListener("abort", () => resolve(), { once: true }));
      return "late result";
    }),
  );
  dispatcher.handleRequest(request(1, "m", "wait"));
  dispatcher.handleRequest(request(2, "m", "late"));
  await flush();
  assert.equal(dispatcher.inFlightCount, 2);
  dispatcher.cancel(1);
  dispatcher.cancel(2);
  dispatcher.cancel(99);
  await flush();
  assert.deepEqual(
    sent.map((m) => (m.type === "response" && !m.ok ? [m.id, m.error.code] : m)),
    [
      [1, "cancelled"],
      [2, "cancelled"],
    ],
  );
});

test("abortAll aborts every call without answering", async () => {
  let aborted = 0;
  const { dispatcher, sent } = setup(
    fakeModule("m", async (_method, _params, ctx) => {
      try {
        return await untilAborted(ctx.signal);
      } finally {
        aborted += 1;
      }
    }),
  );
  dispatcher.handleRequest(request(1, "m", "x"));
  dispatcher.handleRequest(request(2, "m", "x"));
  await flush();
  dispatcher.abortAll();
  await flush();
  assert.equal(aborted, 2);
  assert.deepEqual(sent, []);
  assert.equal(dispatcher.inFlightCount, 0);
});

test("events are forwarded only while the request is in flight", async () => {
  const captured: ModuleContext[] = [];
  const { dispatcher, sent } = setup(
    fakeModule("m", async (_method, _params, ctx) => {
      ctx.emit("progress", { done: 1, total: 2 });
      captured.push(ctx);
      return "ok";
    }),
  );
  dispatcher.handleRequest(request(5, "m", "x"));
  await flush();
  const ctx = captured[0];
  assert.ok(ctx !== undefined);
  ctx.emit("progress", { done: 2, total: 2 });
  assert.deepEqual(sent, [
    { type: "event", id: 5, kind: "progress", data: { done: 1, total: 2 } },
    { type: "response", id: 5, ok: true, result: "ok" },
  ]);
});

test("a duplicate in-flight id is refused without disturbing the first call", async () => {
  const { dispatcher, sent } = setup(
    fakeModule("m", async () => {
      await sleep(5);
      return 1;
    }),
  );
  dispatcher.handleRequest(request(1, "m", "x"));
  dispatcher.handleRequest(request(1, "m", "x"));
  await sleep(20);
  assert.equal(sent.length, 2);
  assert.deepEqual(sent[0], { type: "response", id: 1, ok: false, error: { code: "internal", message: "request id 1 is already in flight" } });
  assert.deepEqual(sent[1], { type: "response", id: 1, ok: true, result: 1 });
});

test("a result over the frame cap is answered too_long", async () => {
  const { dispatcher, sent } = setup(fakeModule("m", async () => "x".repeat(MAX_TEXT_FRAME_BYTES)));
  dispatcher.handleRequest(request(1, "m", "x"));
  await flush();
  const response = sent[0];
  assert.ok(response !== undefined && response.type === "response" && !response.ok);
  assert.equal(response.error.code, "too_long");
});

test("the registry validates names, versions, duplicates and late registration", () => {
  const registry = new ModuleRegistry();
  const ok = fakeModule("translate", async () => null);
  registry.register(ok);
  assert.throws(() => registry.register(ok), /twice/);
  assert.throws(() => registry.register(fakeModule("Bad-Name", async () => null)), /invalid module name/);
  assert.throws(() => registry.register({ ...fakeModule("v0", async () => null), version: 0 }), /invalid version/);
  registry.register(fakeModule("download", async () => null));
  assert.deepEqual(registry.announce(), [
    { name: "translate", version: 1, sites: ["site"] },
    { name: "download", version: 1, sites: ["site"] },
  ]);
  registry.seal();
  assert.throws(() => registry.register(fakeModule("late", async () => null)), /after the bridge started/);
});
