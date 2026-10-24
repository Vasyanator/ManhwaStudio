/*
File: browser-extension/test/auth.test.ts

Purpose:
Pins the handshake HMAC: the golden `auth_vector.json` (computed independently with openssl,
also checked by the Rust bridge) must be reproduced, and verification must refuse tampering.
*/

import assert from "node:assert/strict";
import { test } from "node:test";

import {
  bytesToHex,
  clientAuthMessage,
  computeAuth,
  hexToBytes,
  randomNonceHex,
  serverAuthMessage,
  verifyAuth,
} from "../src/core/auth";
import { fixture } from "./helpers";

interface AuthVector {
  secret: string;
  nonce: string;
  client_nonce: string;
  client_auth: string;
  server_auth: string;
}

function authVector(): AuthVector {
  const raw = fixture("auth_vector.json");
  assert.ok(typeof raw === "object" && raw !== null);
  const field = (name: string): string => {
    const value: unknown = Reflect.get(raw, name);
    assert.equal(typeof value, "string", name);
    return String(value);
  };
  return {
    secret: field("secret"),
    nonce: field("nonce"),
    client_nonce: field("client_nonce"),
    client_auth: field("client_auth"),
    server_auth: field("server_auth"),
  };
}

test("handshake strings have the frozen shape", () => {
  assert.equal(clientAuthMessage("aa", "bb"), "ms-bridge-v1|client|aa|bb");
  assert.equal(serverAuthMessage("aa", "bb"), "ms-bridge-v1|server|aa|bb");
});

test("the golden auth vector is reproduced", async () => {
  const v = authVector();
  assert.equal(await computeAuth(v.secret, clientAuthMessage(v.nonce, v.client_nonce)), v.client_auth);
  assert.equal(await computeAuth(v.secret, serverAuthMessage(v.nonce, v.client_nonce)), v.server_auth);
});

test("verification accepts the genuine MAC and refuses tampering", async () => {
  const v = authVector();
  const message = serverAuthMessage(v.nonce, v.client_nonce);
  assert.equal(await verifyAuth(v.secret, message, v.server_auth), true);
  const flipped = (v.server_auth[0] === "0" ? "1" : "0") + v.server_auth.slice(1);
  assert.equal(await verifyAuth(v.secret, message, flipped), false);
  assert.equal(await verifyAuth(v.secret, message, v.client_auth), false);
  assert.equal(await verifyAuth(v.secret, message, "not hex"), false);
  const otherSecret = "ff".repeat(32);
  assert.equal(await verifyAuth(otherSecret, message, v.server_auth), false);
});

test("hello and welcome fixtures carry the vector's MACs", () => {
  const v = authVector();
  const hello = fixture("hello.json");
  const welcome = fixture("welcome_ready.json");
  const challenge = fixture("challenge.json");
  assert.ok(typeof hello === "object" && hello !== null && typeof welcome === "object" && welcome !== null);
  assert.ok(typeof challenge === "object" && challenge !== null);
  assert.equal(Reflect.get(challenge, "nonce"), v.nonce);
  assert.equal(Reflect.get(hello, "client_nonce"), v.client_nonce);
  assert.equal(Reflect.get(hello, "auth"), v.client_auth);
  assert.equal(Reflect.get(welcome, "server_auth"), v.server_auth);
});

test("hex helpers round-trip and reject garbage", () => {
  const bytes = new Uint8Array([0, 1, 127, 128, 255]);
  assert.equal(bytesToHex(bytes), "00017f80ff");
  assert.deepEqual([...hexToBytes("00017f80ff")], [...bytes]);
  assert.throws(() => hexToBytes("abc"));
  assert.throws(() => hexToBytes("zz"));
  assert.match(randomNonceHex(), /^[0-9a-f]{32}$/);
  assert.notEqual(randomNonceHex(), randomNonceHex());
});
