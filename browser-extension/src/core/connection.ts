/*
File: browser-extension/src/core/connection.ts

Purpose:
The extension's one connection to the running app: read `bridge.json` (never cached), open
`ws://127.0.0.1:<port>/ms-bridge`, run the frozen handshake, keep the link alive and route
frames after `welcome` to the dispatcher. Reconnects with capped exponential backoff; the
`alarms` wake-up (background.ts) restarts a sleeping service worker.

Key items:
- BridgeConnection — state machine idle -> connecting -> handshake -> ready | outdated, and
  backoff between attempts
- ConnectionDeps — every browser seam (config read, socket, extension info, reload, logger),
  faked in tests; `chromeConnectionDeps()` is the production set
- backoffDelayMs() — 1, 2, 4, 8, 16, 30, 30 ... seconds
- detectBrowser() — browser name/version for `hello.browser`

Notes (decision D1):
- `hello` is sent even when `challenge.protocol` differs from PROTOCOL_VERSION; the app then
  answers `welcome{state:"outdated"}`. An outdated connection answers only `ping` and obeys
  `reload`; a request on it gets an `internal` error (the app never sends one).
- `reload` calls `chrome.runtime.reload()` in every state after `welcome`: the app sends it
  after unpacking a newer bundle.
- Pings every 20 s keep the MV3 service worker alive (Chrome >= 116 counts WebSocket traffic);
  a link silent for 60 s is dropped and redialled.
- Frames are handled strictly in arrival order (one promise chain), because the handshake
  verification is asynchronous.
*/

import { computeAuth, clientAuthMessage, randomNonceHex, serverAuthMessage, verifyAuth } from "./auth";
import { AttentionController } from "./attention";
import { Dispatcher } from "./dispatcher";
import type { DispatcherServices } from "./dispatcher";
import type { ModuleRegistry } from "./modules";
import {
  BRIDGE_HOST,
  BRIDGE_PATH,
  BUILD_INFO_FILE_NAME,
  CONFIG_FILE_NAME,
  PROTOCOL_VERSION,
  parseAppMessage,
  parseBridgeConfigFile,
} from "./protocol";
import type { AppMessage, BridgeConfigFile, BrowserInfo, ExtMessage, HelloMessage } from "./protocol";
import { SiteTabManager } from "./tabs";

/** Deadline from socket open to a verified `welcome`. */
export const HANDSHAKE_TIMEOUT_MS = 5_000;
/** Keep-alive ping period (below the 30 s MV3 idle limit). */
export const PING_INTERVAL_MS = 20_000;
/** A link with no frame from the app for this long is dropped. */
export const SILENCE_LIMIT_MS = 60_000;
/** First reconnect delay; doubles per failed attempt. */
export const BACKOFF_INITIAL_MS = 1_000;
/** Reconnect delay cap. */
export const BACKOFF_MAX_MS = 30_000;
/** Name of the `chrome.alarms` alarm that wakes the worker to reconnect. */
export const WAKE_ALARM_NAME = "ms-bridge-wake";

/**
 * WebSocket close codes the extension uses. A browser client may only send 1000 or 3000-4999
 * (`WebSocket.close` throws otherwise), so the RFC 6455 meanings 1002 / 1003 / 1008 are sent
 * as 4002 / 4003 / 4008.
 */
const CLOSE_NORMAL = 1000;
const CLOSE_PROTOCOL_ERROR = 4002;
const CLOSE_UNSUPPORTED_DATA = 4003;
const CLOSE_POLICY = 4008;

/** Delay before reconnect attempt number `attempt` (0-based): 1 s doubling, capped at 30 s. */
export function backoffDelayMs(attempt: number): number {
  const exponent = Math.min(Math.max(0, Math.floor(attempt)), 16);
  return Math.min(BACKOFF_INITIAL_MS * 2 ** exponent, BACKOFF_MAX_MS);
}

/** One user-agent brand (`navigator.userAgentData.brands`). */
export interface BrandVersion {
  readonly brand: string;
  readonly version: string;
}

/**
 * Names the browser for the app UI: the first real brand of `navigator.userAgentData`
 * (skipping "Chromium" and the GREASE "Not...Brand" entries), else a user-agent match, else
 * "Chromium".
 */
export function detectBrowser(userAgent: string, brands: readonly BrandVersion[]): BrowserInfo {
  const userAgentRules: ReadonlyArray<readonly [RegExp, string]> = [
    [/YaBrowser\/([\d.]+)/, "Yandex Browser"],
    [/Edg\/([\d.]+)/, "Microsoft Edge"],
    [/OPR\/([\d.]+)/, "Opera"],
    [/Vivaldi\/([\d.]+)/, "Vivaldi"],
    [/Firefox\/([\d.]+)/, "Firefox"],
  ];
  for (const [pattern, name] of userAgentRules) {
    const match = pattern.exec(userAgent);
    if (match?.[1] !== undefined) return { name, version: match[1] };
  }
  const real = brands.find((b) => b.brand !== "Chromium" && !/not.*brand/i.test(b.brand));
  if (real !== undefined) return { name: real.brand, version: real.version };
  const chrome = /Chrome\/([\d.]+)/.exec(userAgent);
  return { name: "Chromium", version: chrome?.[1] ?? "unknown" };
}

/** Identity fields of `hello` that come from the installed extension. */
export interface ExtensionInfo {
  readonly buildId: string;
  readonly extVersion: string;
  readonly browser: BrowserInfo;
}

/** Text-frame socket handlers. */
export interface SocketHandlers {
  onOpen(): void;
  onText(text: string): void;
  onBinary(): void;
  onClose(code: number, reason: string): void;
}

/** An open (or opening) socket. */
export interface SocketHandle {
  send(text: string): void;
  close(code: number, reason: string): void;
}

/** Structured logger; production writes to the service-worker console. */
export interface Logger {
  info(message: string, context?: Record<string, unknown>): void;
  warn(message: string, context?: Record<string, unknown>): void;
  error(message: string, context?: Record<string, unknown>): void;
}

/** Every browser seam of the connection. */
export interface ConnectionDeps {
  /** Reads and validates `bridge.json`; rejects when it is missing or malformed. */
  readConfig(): Promise<BridgeConfigFile>;
  openSocket(url: string, handlers: SocketHandlers): SocketHandle;
  extensionInfo(): Promise<ExtensionInfo>;
  /** `chrome.runtime.reload()`. */
  reload(): void;
  readonly services: DispatcherServices;
  readonly logger: Logger;
}

/** Connection state; `ready` and `outdated` are the two post-handshake states (D1). */
export type ConnectionState = "idle" | "connecting" | "handshake" | "ready" | "outdated" | "backoff";

interface Link {
  readonly generation: number;
  readonly socket: SocketHandle;
  readonly config: BridgeConfigFile;
  nonce: string | null;
  clientNonce: string | null;
  dispatcher: Dispatcher | null;
  lastReceived: number;
}

/** The app link of one service-worker lifetime. */
export class BridgeConnection {
  private stateValue: ConnectionState = "idle";
  private link: Link | null = null;
  private generation = 0;
  private attempt = 0;
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  private handshakeTimer: ReturnType<typeof setTimeout> | null = null;
  private pingTimer: ReturnType<typeof setInterval> | null = null;
  private inbox: Promise<void> = Promise.resolve();

  constructor(
    private readonly registry: ModuleRegistry,
    private readonly deps: ConnectionDeps,
  ) {}

  /** Current state (for tests and diagnostics). */
  get state(): ConnectionState {
    return this.stateValue;
  }

  /** Seals module registration and dials the app. Idempotent. */
  start(): void {
    this.registry.seal();
    this.wake();
  }

  /** Dials now when idle or waiting in backoff (alarm / startup wake-up); otherwise no-op. */
  wake(): void {
    if (this.stateValue !== "idle" && this.stateValue !== "backoff") return;
    this.clearReconnectTimer();
    this.connect().catch((error: unknown) => {
      this.deps.logger.error("bridge connect attempt failed unexpectedly", { error: String(error) });
      this.scheduleReconnect();
    });
  }

  private async connect(): Promise<void> {
    this.stateValue = "connecting";
    let config: BridgeConfigFile;
    try {
      config = await this.deps.readConfig();
    } catch (error: unknown) {
      // No or unreadable `bridge.json`: the app has not started its bridge (yet). Normal
      // while the app is closed, so it is an info line, and the attempt is retried.
      this.deps.logger.info("bridge config unavailable; will retry", { error: String(error) });
      this.scheduleReconnect();
      return;
    }
    const generation = ++this.generation;
    const url = `ws://${BRIDGE_HOST}:${config.port}${BRIDGE_PATH}`;
    const socket = this.deps.openSocket(url, {
      onOpen: () => this.onOpen(generation),
      onText: (text) => this.enqueue(generation, text),
      onBinary: () => this.onBinary(generation),
      onClose: (code, reason) => this.onClose(generation, code, reason),
    });
    this.link = { generation, socket, config, nonce: null, clientNonce: null, dispatcher: null, lastReceived: Date.now() };
    this.handshakeTimer = setTimeout(() => {
      if (this.link?.generation !== generation) return;
      this.deps.logger.warn("bridge handshake timed out", { url });
      this.dropLink(CLOSE_POLICY, "handshake timeout");
    }, HANDSHAKE_TIMEOUT_MS);
  }

  private isCurrent(generation: number): boolean {
    return this.link !== null && this.link.generation === generation;
  }

  private onOpen(generation: number): void {
    if (!this.isCurrent(generation)) return;
    this.stateValue = "handshake";
  }

  private onBinary(generation: number): void {
    if (!this.isCurrent(generation)) return;
    this.deps.logger.warn("binary frame from the app is not part of protocol v1; closing");
    this.dropLink(CLOSE_UNSUPPORTED_DATA, "binary frames are not supported");
  }

  private onClose(generation: number, code: number, reason: string): void {
    if (!this.isCurrent(generation)) return;
    this.deps.logger.info("bridge connection closed", { code, reason, state: this.stateValue });
    this.teardownLink();
    this.scheduleReconnect();
  }

  private enqueue(generation: number, text: string): void {
    this.inbox = this.inbox
      .then(() => this.handleFrame(generation, text))
      .catch((error: unknown) => {
        if (!this.isCurrent(generation)) return;
        this.deps.logger.error("bridge frame handling failed; reconnecting", { error: String(error) });
        this.dropLink(CLOSE_PROTOCOL_ERROR, "frame handling failed");
      });
  }

  /** Waits until every frame received so far has been handled (tests). */
  settled(): Promise<void> {
    return this.inbox;
  }

  private async handleFrame(generation: number, text: string): Promise<void> {
    const link = this.link;
    if (link === null || link.generation !== generation) return;
    link.lastReceived = Date.now();
    let decoded: unknown;
    try {
      decoded = JSON.parse(text);
    } catch (error: unknown) {
      this.deps.logger.warn("app frame is not JSON; closing", { error: String(error) });
      this.dropLink(CLOSE_PROTOCOL_ERROR, "invalid JSON");
      return;
    }
    const message = parseAppMessage(decoded);
    if (message === null) {
      this.onUnreadable(decoded);
      return;
    }
    await this.handleMessage(link, message);
  }

  private onUnreadable(decoded: unknown): void {
    const type: unknown = typeof decoded === "object" && decoded !== null ? Reflect.get(decoded, "type") : undefined;
    if (this.stateValue === "outdated") {
      // Another protocol version may send frames this build cannot read; only the frozen
      // messages matter on an outdated link.
      this.deps.logger.info("ignoring a frame of another protocol version", { type: String(type) });
      return;
    }
    if (this.stateValue === "ready") {
      this.deps.logger.warn("ignoring a malformed frame from the app", { type: String(type) });
      return;
    }
    this.deps.logger.warn("malformed handshake frame; closing", { type: String(type) });
    this.dropLink(CLOSE_PROTOCOL_ERROR, "malformed handshake frame");
  }

  private async handleMessage(link: Link, message: AppMessage): Promise<void> {
    switch (message.type) {
      case "challenge":
        await this.onChallenge(link, message.protocol, message.nonce);
        return;
      case "welcome": {
        if (link.nonce === null || link.clientNonce === null || link.dispatcher !== null || this.stateValue !== "handshake") {
          this.deps.logger.warn("unexpected welcome; closing");
          this.dropLink(CLOSE_PROTOCOL_ERROR, "unexpected welcome");
          return;
        }
        const genuine = await verifyAuth(link.config.secret, serverAuthMessage(link.nonce, link.clientNonce), message.server_auth);
        if (!this.isCurrent(link.generation)) return;
        if (!genuine) {
          this.deps.logger.error("the app failed server authentication; closing (port squatted or stale secret?)");
          this.dropLink(CLOSE_POLICY, "server authentication failed");
          return;
        }
        this.onWelcome(link, message.state === "ready" && message.protocol === PROTOCOL_VERSION, message.app_version);
        return;
      }
      case "reject":
        this.deps.logger.error("the app rejected the handshake", { code: message.code, message: message.message });
        this.dropLink(CLOSE_NORMAL, "rejected");
        return;
      case "request":
        if (this.stateValue === "ready" && link.dispatcher !== null) {
          link.dispatcher.handleRequest(message);
          return;
        }
        this.sendFrame(link, {
          type: "response",
          id: message.id,
          ok: false,
          error: { code: "internal", message: "the extension is not ready for requests (outdated protocol or handshake pending)" },
        });
        return;
      case "cancel":
        link.dispatcher?.cancel(message.id);
        return;
      case "reload":
        if (this.stateValue !== "ready" && this.stateValue !== "outdated") {
          this.deps.logger.warn("reload before welcome ignored");
          return;
        }
        this.deps.logger.info("the app asked the extension to reload");
        this.deps.reload();
        return;
      case "ping":
        this.sendFrame(link, { type: "pong" });
        return;
      case "pong":
        return;
      default: {
        const unreachable: never = message;
        throw new Error(`unhandled app message ${String(unreachable)}`);
      }
    }
  }

  private async onChallenge(link: Link, appProtocol: number, nonce: string): Promise<void> {
    if (link.nonce !== null || this.stateValue !== "handshake") {
      this.deps.logger.warn("unexpected challenge; closing");
      this.dropLink(CLOSE_PROTOCOL_ERROR, "unexpected challenge");
      return;
    }
    if (appProtocol !== PROTOCOL_VERSION) {
      // D1: still answer; the app decides (`welcome{state:"outdated"}`) and can send reload.
      this.deps.logger.warn("the app speaks another bridge protocol version", { app: appProtocol, extension: PROTOCOL_VERSION });
    }
    const clientNonce = randomNonceHex();
    link.nonce = nonce;
    link.clientNonce = clientNonce;
    const info = await this.deps.extensionInfo();
    const auth = await computeAuth(link.config.secret, clientAuthMessage(nonce, clientNonce));
    if (!this.isCurrent(link.generation)) return;
    const hello: HelloMessage = {
      type: "hello",
      protocol: PROTOCOL_VERSION,
      build_id: info.buildId,
      ext_version: info.extVersion,
      browser: info.browser,
      client_nonce: clientNonce,
      auth,
      modules: this.registry.announce(),
    };
    this.sendFrame(link, hello);
  }

  private onWelcome(link: Link, ready: boolean, appVersion: string): void {
    this.clearHandshakeTimer();
    this.attempt = 0;
    if (ready) {
      this.stateValue = "ready";
      link.dispatcher = new Dispatcher(this.registry, this.deps.services, (frame) => this.sendFrame(link, frame));
    } else {
      this.stateValue = "outdated";
    }
    this.deps.logger.info("bridge connected", { state: this.stateValue, appVersion });
    this.pingTimer = setInterval(() => this.onPingTick(link.generation), PING_INTERVAL_MS);
  }

  private onPingTick(generation: number): void {
    const link = this.link;
    if (link === null || link.generation !== generation) return;
    if (Date.now() - link.lastReceived > SILENCE_LIMIT_MS) {
      this.deps.logger.warn("the app went silent; redialling", { silentMs: Date.now() - link.lastReceived });
      this.dropLink(CLOSE_POLICY, "silence");
      return;
    }
    this.sendFrame(link, { type: "ping" });
  }

  private sendFrame(link: Link, frame: ExtMessage): void {
    if (!this.isCurrent(link.generation)) {
      this.deps.logger.warn("frame for a closed connection dropped", { type: frame.type });
      return;
    }
    link.socket.send(JSON.stringify(frame));
  }

  /** Closes the current link (if any) and schedules a reconnect. */
  private dropLink(code: number, reason: string): void {
    const link = this.link;
    if (link === null) return;
    this.teardownLink();
    try {
      link.socket.close(code, reason);
    } catch (error: unknown) {
      this.deps.logger.warn("closing the socket failed", { error: String(error) });
    }
    this.scheduleReconnect();
  }

  private teardownLink(): void {
    this.link?.dispatcher?.abortAll();
    this.link = null;
    this.clearHandshakeTimer();
    if (this.pingTimer !== null) clearInterval(this.pingTimer);
    this.pingTimer = null;
  }

  private scheduleReconnect(): void {
    this.clearReconnectTimer();
    const delay = backoffDelayMs(this.attempt);
    this.attempt += 1;
    this.stateValue = "backoff";
    this.reconnectTimer = setTimeout(() => {
      this.reconnectTimer = null;
      this.wake();
    }, delay);
  }

  private clearReconnectTimer(): void {
    if (this.reconnectTimer !== null) clearTimeout(this.reconnectTimer);
    this.reconnectTimer = null;
  }

  private clearHandshakeTimer(): void {
    if (this.handshakeTimer !== null) clearTimeout(this.handshakeTimer);
    this.handshakeTimer = null;
  }

  /** Stops everything without reconnecting (tests). */
  stop(): void {
    this.clearReconnectTimer();
    const link = this.link;
    this.teardownLink();
    link?.socket.close(CLOSE_NORMAL, "stopped");
    this.stateValue = "idle";
  }
}

// ---------------------------------------------------------------------------------------------
// Production seams
// ---------------------------------------------------------------------------------------------

/** Console logger with a fixed prefix. */
export const consoleLogger: Logger = {
  info: (message, context) => console.info(`[ms-bridge] ${message}`, context ?? {}),
  warn: (message, context) => console.warn(`[ms-bridge] ${message}`, context ?? {}),
  error: (message, context) => console.error(`[ms-bridge] ${message}`, context ?? {}),
};

/** Reads `bridge.json` from the unpacked folder, bypassing every cache (the app rewrites it). */
async function readBridgeConfig(): Promise<BridgeConfigFile> {
  const response = await fetch(chrome.runtime.getURL(CONFIG_FILE_NAME), { cache: "no-store" });
  if (!response.ok) throw new Error(`${CONFIG_FILE_NAME}: HTTP ${response.status}`);
  const parsed = parseBridgeConfigFile(await response.json());
  if (parsed === null) throw new Error(`${CONFIG_FILE_NAME} is malformed`);
  return parsed;
}

/** `build_id` from the bundle's `build-info.json`, or "unknown" for a dev build without it. */
async function readBuildId(logger: Logger): Promise<string> {
  try {
    const response = await fetch(chrome.runtime.getURL(BUILD_INFO_FILE_NAME), { cache: "no-store" });
    if (!response.ok) throw new Error(`HTTP ${response.status}`);
    const body: unknown = await response.json();
    const buildId: unknown = typeof body === "object" && body !== null ? Reflect.get(body, "build_id") : undefined;
    if (typeof buildId !== "string") throw new Error("build_id missing");
    return buildId;
  } catch (error: unknown) {
    // A folder built by `npm run build` (not by the app) has no build-info.json; the app then
    // reads the extension as foreign-built and may send `reload` after re-unpacking.
    logger.warn(`${BUILD_INFO_FILE_NAME} unreadable; announcing build_id "unknown"`, { error: String(error) });
    return "unknown";
  }
}

function readBrands(): BrandVersion[] {
  const data: unknown = Reflect.get(navigator, "userAgentData");
  if (typeof data !== "object" || data === null) return [];
  const brands: unknown = Reflect.get(data, "brands");
  if (!Array.isArray(brands)) return [];
  const out: BrandVersion[] = [];
  for (const entry of brands) {
    if (typeof entry !== "object" || entry === null) continue;
    const brand: unknown = Reflect.get(entry, "brand");
    const version: unknown = Reflect.get(entry, "version");
    if (typeof brand === "string" && typeof version === "string") out.push({ brand, version });
  }
  return out;
}

function openWebSocket(url: string, handlers: SocketHandlers): SocketHandle {
  const ws = new WebSocket(url);
  ws.binaryType = "arraybuffer";
  ws.addEventListener("open", () => handlers.onOpen());
  ws.addEventListener("message", (event: MessageEvent<unknown>) => {
    if (typeof event.data === "string") handlers.onText(event.data);
    else handlers.onBinary();
  });
  ws.addEventListener("close", (event: CloseEvent) => handlers.onClose(event.code, event.reason));
  // An `error` event is always followed by `close`, which drives the reconnect.
  ws.addEventListener("error", () => consoleLogger.info("bridge socket error", { url }));
  return {
    send: (text) => ws.send(text),
    close: (code, reason) => ws.close(code, reason),
  };
}

/** The production dependency set over chrome.* and the service-worker globals. */
export function chromeConnectionDeps(): ConnectionDeps {
  let cachedInfo: Promise<ExtensionInfo> | null = null;
  return {
    readConfig: readBridgeConfig,
    openSocket: openWebSocket,
    extensionInfo: () => {
      cachedInfo ??= readBuildId(consoleLogger).then((buildId) => ({
        buildId,
        extVersion: chrome.runtime.getManifest().version,
        browser: detectBrowser(navigator.userAgent, readBrands()),
      }));
      return cachedInfo;
    },
    reload: () => chrome.runtime.reload(),
    services: { tabs: new SiteTabManager(), attention: new AttentionController() },
    logger: consoleLogger,
  };
}
