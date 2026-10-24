/*
File: browser-extension/src/core/modules.ts

Purpose:
The feature-module platform of the extension. A feature (translate, later download, ...) is a
`FeatureModule` that owns ONE request namespace: the app addresses it by `request.module ==
name` and picks the operation with `request.method`. The core never knows what a module does;
it only routes requests, cancels them and forwards events.

Key items:
- FeatureModule, ModuleContext — the contract a module implements / receives (pinned for WP-4)
- BridgeFailure — the typed error a module throws to answer with a protocol `ErrorCode`
- ModuleRegistry — name -> module map, sealed when the connection starts
- registerModule() — registration into the process registry used by background.ts

Notes:
- Modules register at service-worker top level, BEFORE the bridge starts: `hello.modules` is
  built once per connection from the sealed registry, so a late registration would never be
  announced and is rejected instead.
- Events: `ctx.emit(kind, data)` sends `{type:"event", id, kind, data}` for the module's own
  in-flight request only. Binary chunks (future `download`) are reserved in protocol.ts.
*/

import type { AttentionController } from "./attention";
import type { ErrorCode, EventKind, ModuleAnnouncement } from "./protocol";
import { isWireInteger } from "./protocol";
import type { SiteTabManager } from "./tabs";

/**
 * Per-request services a module receives. `signal` aborts when the app cancels the request or
 * the connection drops; `emit` sends an event tied to this request (dropped once it finished).
 */
export interface ModuleContext { readonly signal: AbortSignal; readonly tabs: SiteTabManager; readonly attention: AttentionController; emit(kind: EventKind, data: unknown): void; }

/**
 * One feature. `name` is the request namespace and must be unique; `version` is announced and
 * the app uses the module only when it speaks exactly that version; `sites` is informational
 * (shown by the app). `handle` resolves with the JSON-serializable result or rejects; a
 * `BridgeFailure` maps to its code, an abort to `cancelled`, anything else to `internal`.
 * An unknown `method` must reject with `BridgeFailure("unknown_method", ...)`.
 */
export interface FeatureModule { readonly name: string; readonly version: number; readonly sites: readonly string[]; handle(method: string, params: unknown, ctx: ModuleContext): Promise<unknown>; }

/** A module-level failure carrying a protocol error code (and `retry_after_ms` for `rate_limited`). */
export class BridgeFailure extends Error {
  constructor(readonly code: ErrorCode, message: string, readonly retryAfterMs?: number) {
    super(message);
    this.name = "BridgeFailure";
  }
}

/**
 * The failure an aborted `signal` stands for: the `BridgeFailure` the dispatcher aborted with
 * (`cancelled`), or a generic `cancelled` failure for any other abort reason.
 */
export function failureFromAbort(signal: AbortSignal): BridgeFailure {
  const reason: unknown = signal.reason;
  if (reason instanceof BridgeFailure) return reason;
  return new BridgeFailure("cancelled", "the request was cancelled");
}

/** Thrown by `ModuleRegistry.register` for an invalid, duplicate or late registration. */
export class ModuleRegistrationError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "ModuleRegistrationError";
  }
}

/** Module names: lowercase ASCII words joined by `_` (they are wire namespaces). */
const MODULE_NAME_PATTERN = /^[a-z][a-z0-9_]*$/;

/** Name -> module map. Open for registration until `seal()`; the dispatcher reads it after. */
export class ModuleRegistry {
  private readonly modules = new Map<string, FeatureModule>();
  private sealed = false;

  /** Adds `module`. Throws `ModuleRegistrationError` on a bad name/version, a duplicate or after `seal()`. */
  register(module: FeatureModule): void {
    if (this.sealed) {
      throw new ModuleRegistrationError(`module '${module.name}' registered after the bridge started`);
    }
    if (!MODULE_NAME_PATTERN.test(module.name)) {
      throw new ModuleRegistrationError(`invalid module name '${module.name}'`);
    }
    if (!isWireInteger(module.version) || module.version < 1) {
      throw new ModuleRegistrationError(`module '${module.name}' has invalid version ${String(module.version)}`);
    }
    if (this.modules.has(module.name)) {
      throw new ModuleRegistrationError(`module '${module.name}' registered twice`);
    }
    this.modules.set(module.name, module);
  }

  /** Closes registration; idempotent. Called when the bridge starts. */
  seal(): void {
    this.sealed = true;
  }

  /** The module owning namespace `name`, if any. */
  get(name: string): FeatureModule | undefined {
    return this.modules.get(name);
  }

  /** `hello.modules`, in registration order. */
  announce(): ModuleAnnouncement[] {
    return [...this.modules.values()].map((m) => ({ name: m.name, version: m.version, sites: [...m.sites] }));
  }
}

/** The process registry of the service worker; `background.ts` registers into it. */
export const moduleRegistry = new ModuleRegistry();

/** Registers `m` in the process registry. Must run at service-worker top level, before the bridge starts. */
export function registerModule(m: FeatureModule): void {
  moduleRegistry.register(m);
}
