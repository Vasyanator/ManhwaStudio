/*
File: browser-extension/test/connection.test.ts

Purpose:
BridgeConnection over a fake socket: the frozen handshake (hello sent for any protocol, mutual
HMAC), the ready and outdated states of decision D1, reload, ping, request routing, failure
paths (bad server auth, reject, binary frame, missing bridge.json, socket loss), plus the
backoff schedule and browser detection.
*/

import assert from "node:assert/strict";
import { test } from "node:test";

import { AttentionController } from "../src/core/attention";
import { clientAuthMessage, computeAuth, serverAuthMessage } from "../src/core/auth";
import { BridgeConnection, backoffDelayMs, detectBrowser } from "../src/core/connection";
import type { ConnectionDeps, Logger } from "../src/core/connection";
import { ModuleRegistry, failureFromAbort } from "../src/core/modules";
import type { FeatureModule } from "../src/core/modules";
import { PROTOCOL_VERSION, parseBridgeConfigFile } from "../src/core/protocol";
import type { BridgeConfigFile } from "../src/core/protocol";
import { SiteTabManager } from "../src/core/tabs";
import { FakeSocket, FakeStore, FakeTabs, fixture, flush } from "./helpers";

const NONCE = "0123456789abcdef0123456789abcdef";

function config(): BridgeConfigFile {
  const parsed = parseBridgeConfigFile(fixture("bridge_config.json"));
  assert.ok(parsed !== null);
  return parsed;
}

const quietLogger: Logger = { info: () => undefined, warn: () => undefined, error: () => undefined };

interface HarnessState {
  sockets: FakeSocket[];
  reloads: number;
  configReads: number;
  configMissing: boolean;
}

interface Harness extends HarnessState {
  readonly connection: BridgeConnection;
}

function harness(...modules: FeatureModule[]): Harness {
  const registry = new ModuleRegistry();
  for (const m of modules) registry.register(m);
  const fakeTabs = new FakeTabs();
  const h: HarnessState = { sockets: [], reloads: 0, configReads: 0, configMissing: false };
  const deps: ConnectionDeps = {
    readConfig: async () => {
      h.configReads += 1;
      if (h.configMissing) throw new Error("bridge.json: HTTP 404");
      return config();
    },
    openSocket: (url, handlers) => {
      const socket = new FakeSocket(url, handlers);
      h.sockets.push(socket);
      return socket;
    },
    extensionInfo: async () => ({ buildId: "b".repeat(32), extVersion: "1.0.0", browser: { name: "Test Browser", version: "1" } }),
    reload: () => {
      h.reloads += 1;
    },
    services: { tabs: new SiteTabManager(fakeTabs, new FakeStore()), attention: new AttentionController(fakeTabs, 5) },
    logger: quietLogger,
  };
  return Object.assign(h, { connection: new BridgeConnection(registry, deps) });
}

function socketOf(h: Harness): FakeSocket {
  const socket = h.sockets.at(-1);
  assert.ok(socket !== undefined, "a socket was opened");
  return socket;
}

/** Field reader over the `hello` the extension sent. */
type HelloFields = (name: string) => unknown;

/** Runs the handshake up to the `hello`; returns a reader of its fields. */
async function openAndChallenge(h: Harness, appProtocol: number): Promise<HelloFields> {
  h.connection.start();
  await flush();
  const socket = socketOf(h);
  assert.equal(socket.url, `ws://127.0.0.1:${config().port}/ms-bridge`);
  socket.handlers.onOpen();
  assert.equal(h.connection.state, "handshake");
  socket.deliver({ type: "challenge", protocol: appProtocol, nonce: NONCE });
  await h.connection.settled();
  const hello = socket.last();
  assert.ok(typeof hello === "object" && hello !== null);
  return (name: string): unknown => Reflect.get(hello, name);
}

async function welcome(h: Harness, hello: HelloFields, state: "ready" | "outdated", protocol = PROTOCOL_VERSION): Promise<void> {
  const clientNonce = String(hello("client_nonce"));
  const serverAuth = await computeAuth(config().secret, serverAuthMessage(NONCE, clientNonce));
  socketOf(h).deliver({ type: "welcome", protocol, app_version: "4321", expected_build_id: null, state, server_auth: serverAuth });
  await h.connection.settled();
}

const echoModule: FeatureModule = { name: "echo", version: 3, sites: ["x"], handle: async (_method, params) => params };

test("ready handshake: authenticated hello, welcome, ping, request, reload", async () => {
  const h = harness(echoModule);
  const hello = await openAndChallenge(h, PROTOCOL_VERSION);
  assert.equal(hello("type"), "hello");
  assert.equal(hello("protocol"), PROTOCOL_VERSION);
  assert.equal(hello("build_id"), "b".repeat(32));
  assert.deepEqual(hello("modules"), [{ name: "echo", version: 3, sites: ["x"] }]);
  assert.deepEqual(hello("browser"), { name: "Test Browser", version: "1" });
  const expectedAuth = await computeAuth(config().secret, clientAuthMessage(NONCE, String(hello("client_nonce"))));
  assert.equal(hello("auth"), expectedAuth);

  await welcome(h, hello, "ready");
  assert.equal(h.connection.state, "ready");
  const socket = socketOf(h);
  socket.deliver({ type: "ping" });
  await h.connection.settled();
  assert.deepEqual(socket.last(), { type: "pong" });
  socket.deliver({ type: "request", id: 9, module: "echo", method: "any", params: { t: "hi" }, timeout_ms: 1000 });
  await h.connection.settled();
  await flush();
  assert.deepEqual(socket.last(), { type: "response", id: 9, ok: true, result: { t: "hi" } });
  socket.deliver({ type: "reload" });
  await h.connection.settled();
  assert.equal(h.reloads, 1);
  h.connection.stop();
});

test("D1: another protocol still gets a hello; outdated link answers ping and obeys reload only", async () => {
  const h = harness(echoModule);
  const hello = await openAndChallenge(h, PROTOCOL_VERSION + 1);
  assert.equal(hello("type"), "hello");
  assert.equal(hello("protocol"), PROTOCOL_VERSION);
  await welcome(h, hello, "outdated", PROTOCOL_VERSION + 1);
  assert.equal(h.connection.state, "outdated");
  const socket = socketOf(h);
  socket.deliver({ type: "some_future_message", x: 1 });
  socket.deliver({ type: "request", id: 1, module: "echo", method: "any", params: {}, timeout_ms: 1000 });
  await h.connection.settled();
  const answer = socket.last();
  assert.ok(typeof answer === "object" && answer !== null);
  assert.equal(Reflect.get(answer, "ok"), false);
  assert.equal(Reflect.get(Reflect.get(answer, "error"), "code"), "internal");
  socket.deliver({ type: "ping" });
  socket.deliver({ type: "reload" });
  await h.connection.settled();
  assert.deepEqual(socket.last(), { type: "pong" });
  assert.equal(h.reloads, 1);
  assert.equal(socket.closed, null, "an outdated link stays open");
  h.connection.stop();
});

test("a welcome claiming ready for another protocol is treated as outdated", async () => {
  const h = harness();
  const hello = await openAndChallenge(h, PROTOCOL_VERSION);
  await welcome(h, hello, "ready", PROTOCOL_VERSION + 1);
  assert.equal(h.connection.state, "outdated");
  h.connection.stop();
});

test("a wrong server_auth closes the link and backs off", async () => {
  const h = harness();
  await openAndChallenge(h, PROTOCOL_VERSION);
  socketOf(h).deliver({ type: "welcome", protocol: 1, app_version: "1", expected_build_id: null, state: "ready", server_auth: "0".repeat(64) });
  await h.connection.settled();
  assert.equal(socketOf(h).closed?.code, 4008);
  assert.equal(h.connection.state, "backoff");
  assert.equal(h.reloads, 0);
  h.connection.stop();
});

test("reject, binary frames and malformed handshake frames close the link", async () => {
  const rejected = harness();
  await openAndChallenge(rejected, PROTOCOL_VERSION);
  socketOf(rejected).deliver({ type: "reject", code: "auth_failed", message: "bad mac" });
  await rejected.connection.settled();
  assert.equal(socketOf(rejected).closed?.code, 1000);
  assert.equal(rejected.connection.state, "backoff");
  rejected.connection.stop();

  const binary = harness();
  await openAndChallenge(binary, PROTOCOL_VERSION);
  socketOf(binary).handlers.onBinary();
  assert.equal(socketOf(binary).closed?.code, 4003);
  binary.connection.stop();

  const garbage = harness();
  garbage.connection.start();
  await flush();
  socketOf(garbage).handlers.onOpen();
  socketOf(garbage).handlers.onText("{not json");
  await garbage.connection.settled();
  assert.equal(socketOf(garbage).closed?.code, 4002);
  garbage.connection.stop();
});

test("missing bridge.json backs off; an alarm wake-up dials again at once", async () => {
  const h = harness();
  h.configMissing = true;
  h.connection.start();
  await flush();
  assert.equal(h.connection.state, "backoff");
  assert.equal(h.sockets.length, 0);
  h.configMissing = false;
  h.connection.wake();
  await flush();
  assert.equal(h.configReads, 2);
  assert.equal(h.sockets.length, 1);
  h.connection.wake();
  await flush();
  assert.equal(h.configReads, 2, "wake is a no-op while connected");
  h.connection.stop();
});

test("losing the socket aborts in-flight requests without answering and redials later", async () => {
  let aborted = false;
  const waiting: FeatureModule = {
    name: "wait",
    version: 1,
    sites: [],
    handle: (_method, _params, ctx) =>
      new Promise((_, reject) =>
        ctx.signal.addEventListener(
          "abort",
          () => {
            aborted = true;
            reject(failureFromAbort(ctx.signal));
          },
          { once: true },
        ),
      ),
  };
  const h = harness(waiting);
  const hello = await openAndChallenge(h, PROTOCOL_VERSION);
  await welcome(h, hello, "ready");
  const socket = socketOf(h);
  socket.deliver({ type: "request", id: 1, module: "wait", method: "x", params: null, timeout_ms: 1000 });
  await h.connection.settled();
  const sentBefore = socket.sent.length;
  socket.handlers.onClose(1006, "gone");
  await flush();
  assert.equal(aborted, true);
  assert.equal(socket.sent.length, sentBefore, "nothing is sent on a dead link");
  assert.equal(h.connection.state, "backoff");
  h.connection.stop();
});

test("backoff doubles from 1 s and caps at 30 s", () => {
  assert.deepEqual([0, 1, 2, 3, 4, 5, 6, 50].map(backoffDelayMs), [1000, 2000, 4000, 8000, 16000, 30000, 30000, 30000]);
});

test("browser detection prefers specific user agents, then real brands", () => {
  const chromeUa = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/141.0.0.0 Safari/537.36";
  assert.deepEqual(
    detectBrowser(chromeUa, [
      { brand: "Not)A;Brand", version: "99" },
      { brand: "Chromium", version: "141" },
      { brand: "Google Chrome", version: "141" },
    ]),
    { name: "Google Chrome", version: "141" },
  );
  assert.deepEqual(detectBrowser(`${chromeUa} YaBrowser/25.8.1.900 Yowser/2.5`, []), { name: "Yandex Browser", version: "25.8.1.900" });
  assert.deepEqual(detectBrowser(`${chromeUa} Edg/141.0.3537.57`, [{ brand: "Microsoft Edge", version: "141" }]), { name: "Microsoft Edge", version: "141.0.3537.57" });
  assert.deepEqual(detectBrowser(chromeUa, [{ brand: "Chromium", version: "141" }]), { name: "Chromium", version: "141.0.0.0" });
});
