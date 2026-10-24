/*
File: browser-extension/src/core/protocol.ts

Purpose:
The extension side of the ManhwaStudio bridge wire protocol: constants, message shapes, the
closed error-code and event-kind sets, and the type guards that turn an untrusted JSON frame
into a typed message. Mirror of the Rust `ms_browser_bridge::protocol`; a Rust test reads this
file (`PROTOCOL_VERSION`, `CONFIG_FILE_NAME`, `ERROR_CODES`) and `protocol-fixtures/*.json`.

Key items:
- PROTOCOL_VERSION, CONFIG_FILE_NAME, BUILD_INFO_FILE_NAME, BRIDGE_PATH, HANDSHAKE_LABEL,
  MAX_TEXT_FRAME_BYTES
- ERROR_CODES / ErrorCode, EVENT_KINDS / EventKind, PEER_STATES / PeerState
- AppMessage (app -> extension), ExtMessage (extension -> app)
- parseAppMessage(), parseBridgeConfigFile(), isErrorCode()

Versioning (decision D1):
- `PROTOCOL_VERSION` is the ONLY compatibility gate. Any change of a message shape, a field's
  meaning, the handshake strings or a closed set bumps it here AND in
  `ms_browser_bridge::protocol`.
- FROZEN FOREVER, across every protocol version: `challenge`, `hello`, `welcome`, `reject`,
  `reload`, `ping`, `pong`. A peer of another protocol version must still be able to
  authenticate, learn that it is outdated and be told to reload, so these shapes never change
  (new optional fields only). Everything else may change with a version bump.
- `build_id` (the bundle content hash) is NOT a compatibility gate: it only tells the app that
  the unpacked folder or the running extension is stale (the app then re-unpacks and sends
  `reload`).
- Each feature module carries its own `version`; the app uses a module only when it announces
  exactly the version the app speaks.
- Request ids are JSON numbers in `1..=Number.MAX_SAFE_INTEGER` (Rust `MAX_WIRE_ID`): the app's
  counter wraps inside that range, so `isWireInteger` accepts every id it sends.
- App-side limit (not a shape): an `event` message longer than 64 KiB (Rust
  `MAX_EVENT_MESSAGE_BYTES`) ends the connection, as does a flood of attention events the app's
  requester has not taken yet; surplus `progress` events are dropped by the app.

Reserved for a later protocol version (not implemented, not accepted in v1): binary frames
`[u32 BE header_len][JSON {type:"chunk",id,seq,final}][bytes]` from the extension to the app,
with `{"type":"ack","id","seq"}` flow control from the app (the future `download` module).
Version 1 closes the connection on any binary frame.
*/

/** Wire protocol version; the only compatibility gate between app and extension (D1). */
export const PROTOCOL_VERSION = 1;

/** Discovery file the app writes into the unpacked extension folder: `{protocol, port, secret}`. */
export const CONFIG_FILE_NAME = "bridge.json";

/**
 * Bundle file `{build_id, protocol}` that the app's build (`ms-browser-ext` build.rs) adds to
 * the unpacked extension; the extension announces its `build_id` in `hello`. Mirrored by
 * `ms_browser_ext::BUILD_INFO_FILE_NAME`.
 */
export const BUILD_INFO_FILE_NAME = "build-info.json";

/** WebSocket path of the app's loopback server. */
export const BRIDGE_PATH = "/ms-bridge";

/** Loopback host the app binds; the only host the extension connects to. */
export const BRIDGE_HOST = "127.0.0.1";

/**
 * Domain-separation label of the handshake HMAC strings:
 * client auth = HMAC(secret, `${HANDSHAKE_LABEL}|client|${nonce}|${client_nonce}`),
 * server auth = HMAC(secret, `${HANDSHAKE_LABEL}|server|${nonce}|${client_nonce}`).
 * The HMAC key is the 32 raw bytes the 64-hex `secret` decodes to (not its ASCII text).
 */
export const HANDSHAKE_LABEL = "ms-bridge-v1";

/** Largest text frame either side accepts (the app's tungstenite `max_message_size`). */
export const MAX_TEXT_FRAME_BYTES = 4 * 1024 * 1024;

/** Closed set of remote error codes a module call can fail with. Mirrored in Rust. */
export const ERROR_CODES = [
  "cancelled",
  "timeout",
  "unknown_module",
  "unknown_method",
  "bad_params",
  "busy",
  "attention_timeout",
  "rate_limited",
  "site_changed",
  "tab_closed",
  "unsupported_language",
  "too_long",
  "internal",
] as const;

/** One remote error code; see `ERROR_CODES`. */
export type ErrorCode = (typeof ERROR_CODES)[number];

/** Closed set of event kinds a running request may emit to the app. */
export const EVENT_KINDS = ["attention", "attention_cleared", "progress"] as const;

/** One event kind; see `EVENT_KINDS`. */
export type EventKind = (typeof EVENT_KINDS)[number];

/** Handshake outcome the app reports in `welcome.state`. */
export const PEER_STATES = ["ready", "outdated"] as const;

/** `ready`: requests may come. `outdated`: protocol differs; only `ping` and `reload` come. */
export type PeerState = (typeof PEER_STATES)[number];

/** Codes of `reject`: bad HMAC, or a `hello` the app could not read at all. */
export const REJECT_CODES = ["auth_failed", "protocol_mismatch"] as const;

/** One reject code; see `REJECT_CODES`. */
export type RejectCode = (typeof REJECT_CODES)[number];

// ---------------------------------------------------------------------------------------------
// Messages app -> extension
// ---------------------------------------------------------------------------------------------

/** First frame of every connection (frozen). `nonce` is 32 hex chars. */
export interface ChallengeMessage {
  readonly type: "challenge";
  readonly protocol: number;
  readonly nonce: string;
}

/** Successful handshake answer (frozen). `server_auth` proves the app knows the secret. */
export interface WelcomeMessage {
  readonly type: "welcome";
  readonly protocol: number;
  readonly app_version: string;
  /** The app's embedded bundle `build_id`; informational, `null` when the app has no bundle. */
  readonly expected_build_id: string | null;
  readonly state: PeerState;
  readonly server_auth: string;
}

/** Failed handshake (frozen); the app closes with 1008 right after it. */
export interface RejectMessage {
  readonly type: "reject";
  readonly code: RejectCode;
  readonly message: string;
}

/**
 * One module call. `id` is in `1..=Number.MAX_SAFE_INTEGER` and unique among the app's in-flight
 * requests; `timeout_ms` is the app's own deadline.
 */
export interface RequestMessage {
  readonly type: "request";
  readonly id: number;
  readonly module: string;
  readonly method: string;
  readonly params: unknown;
  readonly timeout_ms: number;
}

/** The app stopped waiting for request `id`; the extension aborts the module call. */
export interface CancelMessage {
  readonly type: "cancel";
  readonly id: number;
}

/** Reload the extension (frozen): the app unpacked a newer bundle. */
export interface ReloadMessage {
  readonly type: "reload";
}

/** Keep-alive probe (frozen); either side may send it. */
export interface PingMessage {
  readonly type: "ping";
}

/** Keep-alive answer (frozen). */
export interface PongMessage {
  readonly type: "pong";
}

/** Every frame the app may send. */
export type AppMessage =
  | ChallengeMessage
  | WelcomeMessage
  | RejectMessage
  | RequestMessage
  | CancelMessage
  | ReloadMessage
  | PingMessage
  | PongMessage;

// ---------------------------------------------------------------------------------------------
// Messages extension -> app
// ---------------------------------------------------------------------------------------------

/** A feature module as announced in `hello.modules`. */
export interface ModuleAnnouncement {
  readonly name: string;
  readonly version: number;
  readonly sites: readonly string[];
}

/** Browser identity shown by the app UI. */
export interface BrowserInfo {
  readonly name: string;
  readonly version: string;
}

/**
 * Answer to `challenge` (frozen). Sent even when `challenge.protocol` differs from
 * `PROTOCOL_VERSION`: the app then answers `welcome{state:"outdated"}` and may send `reload`.
 */
export interface HelloMessage {
  readonly type: "hello";
  readonly protocol: number;
  readonly build_id: string;
  readonly ext_version: string;
  readonly browser: BrowserInfo;
  readonly client_nonce: string;
  readonly auth: string;
  readonly modules: readonly ModuleAnnouncement[];
}

/** Progress / attention notification for the in-flight request `id` (at most 64 KiB as JSON). */
export interface EventMessage {
  readonly type: "event";
  readonly id: number;
  readonly kind: EventKind;
  readonly data: unknown;
}

/** Error payload of a failed response. */
export interface RemoteError {
  readonly code: ErrorCode;
  readonly message: string;
  /** Present only with `rate_limited`: how long the site asked to wait. */
  readonly retry_after_ms?: number;
}

/** Successful module call result. */
export interface ResponseOkMessage {
  readonly type: "response";
  readonly id: number;
  readonly ok: true;
  readonly result: unknown;
}

/** Failed module call. */
export interface ResponseErrorMessage {
  readonly type: "response";
  readonly id: number;
  readonly ok: false;
  readonly error: RemoteError;
}

/** Every frame the extension may send. */
export type ExtMessage =
  | HelloMessage
  | EventMessage
  | ResponseOkMessage
  | ResponseErrorMessage
  | PingMessage
  | PongMessage;

/** Parsed `bridge.json`. `port` is a TCP port, `secret` is 64 lowercase hex chars. */
export interface BridgeConfigFile {
  readonly protocol: number;
  readonly port: number;
  readonly secret: string;
}

// ---------------------------------------------------------------------------------------------
// Guards
// ---------------------------------------------------------------------------------------------

type JsonObject = Readonly<Record<string, unknown>>;

function isObject(value: unknown): value is JsonObject {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/** True for a non-negative integer JavaScript can represent exactly (request ids, u32 fields). */
export function isWireInteger(value: unknown): value is number {
  return typeof value === "number" && Number.isSafeInteger(value) && value >= 0;
}

/** True when `value` is a lowercase hex string of exactly `length` characters. */
export function isHex(value: unknown, length: number): value is string {
  return typeof value === "string" && value.length === length && /^[0-9a-f]*$/.test(value);
}

/** True when `value` is one of the closed `ERROR_CODES`. */
export function isErrorCode(value: unknown): value is ErrorCode {
  return typeof value === "string" && (ERROR_CODES as readonly string[]).includes(value);
}

/** True when `value` is one of the closed `EVENT_KINDS`. */
export function isEventKind(value: unknown): value is EventKind {
  return typeof value === "string" && (EVENT_KINDS as readonly string[]).includes(value);
}

function isPeerState(value: unknown): value is PeerState {
  return typeof value === "string" && (PEER_STATES as readonly string[]).includes(value);
}

function isRejectCode(value: unknown): value is RejectCode {
  return typeof value === "string" && (REJECT_CODES as readonly string[]).includes(value);
}

/**
 * Reads a `challenge` leniently: only `type`, `protocol` and `nonce` are required, so a
 * challenge of ANY protocol version is understood (the handshake is frozen). Returns `null`
 * when the frame is not a readable challenge.
 */
function parseChallenge(obj: JsonObject): ChallengeMessage | null {
  const { protocol, nonce } = obj;
  if (!isWireInteger(protocol) || !isHex(nonce, 32)) return null;
  return { type: "challenge", protocol, nonce };
}

function parseWelcome(obj: JsonObject): WelcomeMessage | null {
  const { protocol, app_version, expected_build_id, state, server_auth } = obj;
  if (!isWireInteger(protocol) || typeof app_version !== "string") return null;
  if (expected_build_id !== null && typeof expected_build_id !== "string") return null;
  if (!isPeerState(state) || !isHex(server_auth, 64)) return null;
  return { type: "welcome", protocol, app_version, expected_build_id, state, server_auth };
}

function parseReject(obj: JsonObject): RejectMessage | null {
  const { code, message } = obj;
  if (!isRejectCode(code) || typeof message !== "string") return null;
  return { type: "reject", code, message };
}

function parseRequest(obj: JsonObject): RequestMessage | null {
  const { id, module, method, params, timeout_ms } = obj;
  if (!isWireInteger(id) || typeof module !== "string" || typeof method !== "string") return null;
  if (!isWireInteger(timeout_ms) || !("params" in obj)) return null;
  return { type: "request", id, module, method, params, timeout_ms };
}

function parseCancel(obj: JsonObject): CancelMessage | null {
  const { id } = obj;
  if (!isWireInteger(id)) return null;
  return { type: "cancel", id };
}

/**
 * Narrows one decoded JSON frame from the app into an `AppMessage`.
 * Returns `null` for anything that is not a well-formed message of this protocol version
 * (unknown `type`, missing or mistyped fields); the caller decides how to react.
 */
export function parseAppMessage(value: unknown): AppMessage | null {
  if (!isObject(value)) return null;
  const type = value["type"];
  if (typeof type !== "string") return null;
  switch (type) {
    case "challenge":
      return parseChallenge(value);
    case "welcome":
      return parseWelcome(value);
    case "reject":
      return parseReject(value);
    case "request":
      return parseRequest(value);
    case "cancel":
      return parseCancel(value);
    case "reload":
      return { type: "reload" };
    case "ping":
      return { type: "ping" };
    case "pong":
      return { type: "pong" };
    default:
      return null;
  }
}

function parseAnnouncement(value: unknown): ModuleAnnouncement | null {
  if (!isObject(value)) return null;
  const { name, version, sites } = value;
  if (typeof name !== "string" || !isWireInteger(version) || !Array.isArray(sites)) return null;
  const siteList: string[] = [];
  for (const site of sites) {
    if (typeof site !== "string") return null;
    siteList.push(site);
  }
  return { name, version, sites: siteList };
}

function parseHello(obj: JsonObject): HelloMessage | null {
  const { protocol, build_id, ext_version, browser, client_nonce, auth, modules } = obj;
  if (!isWireInteger(protocol) || typeof build_id !== "string" || typeof ext_version !== "string") return null;
  if (!isObject(browser) || typeof browser["name"] !== "string" || typeof browser["version"] !== "string") return null;
  if (!isHex(client_nonce, 32) || !isHex(auth, 64) || !Array.isArray(modules)) return null;
  const announced: ModuleAnnouncement[] = [];
  for (const entry of modules) {
    const parsed = parseAnnouncement(entry);
    if (parsed === null) return null;
    announced.push(parsed);
  }
  return {
    type: "hello",
    protocol,
    build_id,
    ext_version,
    browser: { name: browser["name"], version: browser["version"] },
    client_nonce,
    auth,
    modules: announced,
  };
}

function parseRemoteError(value: unknown): RemoteError | null {
  if (!isObject(value)) return null;
  const { code, message, retry_after_ms } = value;
  if (!isErrorCode(code) || typeof message !== "string") return null;
  if (retry_after_ms === undefined) return { code, message };
  if (!isWireInteger(retry_after_ms)) return null;
  return { code, message, retry_after_ms };
}

/**
 * Narrows one decoded JSON frame into an `ExtMessage` (what the extension sends). Used by the
 * fixture tests to prove the golden files match the types; the extension itself only builds
 * these messages. Returns `null` for anything malformed.
 */
export function parseExtMessage(value: unknown): ExtMessage | null {
  if (!isObject(value)) return null;
  const type = value["type"];
  if (typeof type !== "string") return null;
  switch (type) {
    case "hello":
      return parseHello(value);
    case "event": {
      const { id, kind, data } = value;
      if (!isWireInteger(id) || !isEventKind(kind) || !("data" in value)) return null;
      return { type: "event", id, kind, data };
    }
    case "response": {
      const { id, ok } = value;
      if (!isWireInteger(id)) return null;
      if (ok === true) {
        if (!("result" in value)) return null;
        return { type: "response", id, ok: true, result: value["result"] };
      }
      if (ok === false) {
        const error = parseRemoteError(value["error"]);
        if (error === null) return null;
        return { type: "response", id, ok: false, error };
      }
      return null;
    }
    case "ping":
      return { type: "ping" };
    case "pong":
      return { type: "pong" };
    default:
      return null;
  }
}

/**
 * Narrows the decoded `bridge.json`. `protocol` is read but NOT compared here: the handshake
 * is frozen, so the extension connects to an app of any protocol version and lets the app
 * decide (`welcome.state`). Returns `null` when the file is malformed.
 */
export function parseBridgeConfigFile(value: unknown): BridgeConfigFile | null {
  if (!isObject(value)) return null;
  const { protocol, port, secret } = value;
  if (!isWireInteger(protocol) || !isWireInteger(port) || port < 1 || port > 65535) return null;
  if (!isHex(secret, 64)) return null;
  return { protocol, port, secret };
}
