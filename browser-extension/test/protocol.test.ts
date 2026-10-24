/*
File: browser-extension/test/protocol.test.ts

Purpose:
Golden-fixture tests of the wire protocol guards: every fixture parses as the message its file
name says, malformed frames are refused, and the closed sets stay closed.
*/

import assert from "node:assert/strict";
import { test } from "node:test";

import {
  ERROR_CODES,
  EVENT_KINDS,
  PROTOCOL_VERSION,
  isErrorCode,
  parseAppMessage,
  parseBridgeConfigFile,
  parseExtMessage,
} from "../src/core/protocol";
import { fixture, fixtureNames } from "./helpers";

/** Fixture -> direction and expected `type`; every fixture file must be listed here. */
const FIXTURES: Record<string, { readonly from: "app" | "ext"; readonly type: string }> = {
  "challenge.json": { from: "app", type: "challenge" },
  "welcome_ready.json": { from: "app", type: "welcome" },
  "welcome_outdated.json": { from: "app", type: "welcome" },
  "reject.json": { from: "app", type: "reject" },
  "request.json": { from: "app", type: "request" },
  "cancel.json": { from: "app", type: "cancel" },
  "reload.json": { from: "app", type: "reload" },
  "ping.json": { from: "app", type: "ping" },
  "pong.json": { from: "ext", type: "pong" },
  "hello.json": { from: "ext", type: "hello" },
  "event_attention.json": { from: "ext", type: "event" },
  "event_progress.json": { from: "ext", type: "event" },
  "response_ok.json": { from: "ext", type: "response" },
  "response_error.json": { from: "ext", type: "response" },
};

test("every message fixture parses as its declared type and round-trips", () => {
  for (const [name, expected] of Object.entries(FIXTURES)) {
    const raw = fixture(name);
    const parsed = expected.from === "app" ? parseAppMessage(raw) : parseExtMessage(raw);
    assert.ok(parsed !== null, `${name} must parse`);
    assert.equal(parsed.type, expected.type, name);
    // The guards keep every field: re-serializing gives the fixture back.
    assert.deepEqual(JSON.parse(JSON.stringify(parsed)), raw, `${name} round-trip`);
  }
});

test("every fixture file is covered by a test", () => {
  const covered = new Set([...Object.keys(FIXTURES), "auth_vector.json", "bridge_config.json"]);
  for (const name of fixtureNames()) assert.ok(covered.has(name), `fixture ${name} has no test`);
});

test("fixtures speak the current protocol version", () => {
  for (const name of ["challenge.json", "hello.json", "welcome_ready.json", "bridge_config.json"]) {
    const raw = fixture(name);
    assert.ok(typeof raw === "object" && raw !== null);
    assert.equal(Reflect.get(raw, "protocol"), PROTOCOL_VERSION, name);
  }
});

test("bridge.json fixture parses; malformed configs are refused", () => {
  assert.deepEqual(parseBridgeConfigFile(fixture("bridge_config.json")), fixture("bridge_config.json"));
  const secret = "ab".repeat(32);
  assert.equal(parseBridgeConfigFile({ protocol: 1, port: 0, secret }), null);
  assert.equal(parseBridgeConfigFile({ protocol: 1, port: 70000, secret }), null);
  assert.equal(parseBridgeConfigFile({ protocol: 1, port: 5000, secret: "AB".repeat(32) }), null);
  assert.equal(parseBridgeConfigFile({ protocol: 1, port: 5000, secret: "ab" }), null);
  assert.equal(parseBridgeConfigFile([]), null);
});

test("a challenge of another protocol version is still readable (frozen handshake)", () => {
  const parsed = parseAppMessage({ type: "challenge", protocol: 7, nonce: "0".repeat(32), future_field: true });
  assert.deepEqual(parsed, { type: "challenge", protocol: 7, nonce: "0".repeat(32) });
});

test("malformed app frames are refused", () => {
  const bad: unknown[] = [
    null,
    "ping",
    { type: "nope" },
    { type: "challenge", protocol: 1, nonce: "short" },
    { type: "request", id: -1, module: "m", method: "x", params: {}, timeout_ms: 1 },
    { type: "request", id: 1.5, module: "m", method: "x", params: {}, timeout_ms: 1 },
    { type: "request", id: 1, module: "m", method: "x", timeout_ms: 1 },
    { type: "welcome", protocol: 1, app_version: "1", expected_build_id: 5, state: "ready", server_auth: "0".repeat(64) },
    { type: "welcome", protocol: 1, app_version: "1", expected_build_id: null, state: "maybe", server_auth: "0".repeat(64) },
    { type: "reject", code: "teapot", message: "" },
    { type: "cancel", id: "7" },
  ];
  for (const frame of bad) assert.equal(parseAppMessage(frame), null, JSON.stringify(frame));
});

test("error responses accept only the closed code set", () => {
  assert.equal(parseExtMessage({ type: "response", id: 1, ok: false, error: { code: "teapot", message: "" } }), null);
  assert.equal(parseExtMessage({ type: "response", id: 1, ok: false, error: { code: "busy", message: "", retry_after_ms: -5 } }), null);
  assert.ok(parseExtMessage({ type: "response", id: 1, ok: false, error: { code: "busy", message: "" } }) !== null);
});

test("closed sets have no duplicates", () => {
  assert.equal(new Set(ERROR_CODES).size, ERROR_CODES.length);
  assert.equal(new Set(EVENT_KINDS).size, EVENT_KINDS.length);
  for (const code of ERROR_CODES) assert.ok(isErrorCode(code));
  assert.ok(!isErrorCode("outdated"));
});
