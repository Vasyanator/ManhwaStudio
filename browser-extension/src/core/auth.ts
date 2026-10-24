/*
File: browser-extension/src/core/auth.ts

Purpose:
The handshake cryptography of the bridge: HMAC-SHA256 over the frozen handshake strings with
WebCrypto, nonce generation and hex helpers. Mutual authentication: the extension proves it
knows the pairing secret (`hello.auth`), the app proves the same (`welcome.server_auth`), so a
local process squatting the port learns and commands nothing.

Key functions:
- clientAuthMessage() / serverAuthMessage() — the exact signed strings (see HANDSHAKE_LABEL)
- computeAuth() — hex HMAC of one string under the hex secret
- verifyAuth() — constant-time check through `crypto.subtle.verify`
- randomNonceHex() — 16 random bytes as 32 hex chars

Notes:
The HMAC key is the 32 raw bytes of the 64-hex `secret` from `bridge.json`.
`protocol-fixtures/auth_vector.json` pins one vector that the Rust side checks too.
*/

import { HANDSHAKE_LABEL, isHex } from "./protocol";

/** Thrown when a hex string is malformed; never carries the secret itself. */
export class HexError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "HexError";
  }
}

/** Decodes lowercase/uppercase hex into bytes. Throws `HexError` on odd length or bad digits. */
export function hexToBytes(hex: string): Uint8Array<ArrayBuffer> {
  if (hex.length % 2 !== 0 || !/^[0-9a-fA-F]*$/.test(hex)) {
    throw new HexError(`malformed hex string of length ${hex.length}`);
  }
  const out = new Uint8Array(hex.length / 2);
  for (let i = 0; i < out.length; i += 1) {
    out[i] = Number.parseInt(hex.slice(i * 2, i * 2 + 2), 16);
  }
  return out;
}

/** Encodes bytes as lowercase hex. */
export function bytesToHex(bytes: Uint8Array): string {
  let out = "";
  for (const byte of bytes) out += byte.toString(16).padStart(2, "0");
  return out;
}

/** String the extension signs: `ms-bridge-v1|client|<nonce>|<client_nonce>`. */
export function clientAuthMessage(nonce: string, clientNonce: string): string {
  return `${HANDSHAKE_LABEL}|client|${nonce}|${clientNonce}`;
}

/** String the app signs: `ms-bridge-v1|server|<nonce>|<client_nonce>`. */
export function serverAuthMessage(nonce: string, clientNonce: string): string {
  return `${HANDSHAKE_LABEL}|server|${nonce}|${clientNonce}`;
}

async function importKey(secretHex: string, usage: "sign" | "verify"): Promise<CryptoKey> {
  if (!isHex(secretHex, 64)) throw new HexError("pairing secret must be 64 lowercase hex chars");
  return crypto.subtle.importKey("raw", hexToBytes(secretHex), { name: "HMAC", hash: "SHA-256" }, false, [usage]);
}

/** Hex HMAC-SHA256 of `message` (UTF-8) under the 32-byte key `secretHex` decodes to. */
export async function computeAuth(secretHex: string, message: string): Promise<string> {
  const key = await importKey(secretHex, "sign");
  const mac = await crypto.subtle.sign("HMAC", key, new TextEncoder().encode(message));
  return bytesToHex(new Uint8Array(mac));
}

/**
 * Checks `authHex` against HMAC(secret, message) in constant time (`crypto.subtle.verify`).
 * Returns false for a malformed `authHex` instead of throwing: a peer's bad answer is a
 * failed check, not an extension error.
 */
export async function verifyAuth(secretHex: string, message: string, authHex: string): Promise<boolean> {
  if (!isHex(authHex, 64)) return false;
  const key = await importKey(secretHex, "verify");
  return crypto.subtle.verify("HMAC", key, hexToBytes(authHex), new TextEncoder().encode(message));
}

/** 16 cryptographically random bytes as 32 lowercase hex chars (`client_nonce`). */
export function randomNonceHex(): string {
  const bytes = new Uint8Array(16);
  crypto.getRandomValues(bytes);
  return bytesToHex(bytes);
}
