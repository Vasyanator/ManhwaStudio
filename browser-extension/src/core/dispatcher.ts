/*
File: browser-extension/src/core/dispatcher.ts

Purpose:
Routes the app's requests to feature modules and turns every outcome into exactly one
`response` frame. One `AbortController` per in-flight request id: the app's `cancel{id}`
aborts it (the module call ends as `cancelled`), a lost connection aborts everything without
answering (there is nobody to answer).

Key items:
- Dispatcher.handleRequest() / cancel() / abortAll()
- responseForError() — the error -> `RemoteError` mapping

Notes:
- The extension keeps NO timer of its own for `timeout_ms`: the app owns the deadline (and
  suspends it while an attention episode is open), and sends `cancel` when it expires.
- A result is answered only if its frame fits `MAX_TEXT_FRAME_BYTES`; a bigger one is answered
  `too_long` instead of being dropped by the app's frame cap.
- Events are forwarded only while their request is in flight.
*/

import type { AttentionController } from "./attention";
import { BridgeFailure, failureFromAbort } from "./modules";
import type { ModuleContext, ModuleRegistry } from "./modules";
import { MAX_TEXT_FRAME_BYTES } from "./protocol";
import type { EventKind, ExtMessage, RemoteError, RequestMessage, ResponseErrorMessage } from "./protocol";
import type { SiteTabManager } from "./tabs";

/** Shared services every module context carries. */
export interface DispatcherServices {
  readonly tabs: SiteTabManager;
  readonly attention: AttentionController;
}

/** Sends one frame to the app; may throw when the socket is gone (the caller logs it). */
export type FrameSender = (message: ExtMessage) => void;

interface InFlight {
  readonly controller: AbortController;
  /** Set when the connection is gone: the call is aborted and its outcome is not sent. */
  silent: boolean;
}

/** Maps a module failure to the wire error; `signal` decides whether it was a cancellation. */
export function responseForError(error: unknown, signal: AbortSignal): RemoteError {
  const failure = signal.aborted ? failureFromAbort(signal) : error;
  if (failure instanceof BridgeFailure) {
    if (failure.code === "rate_limited" && failure.retryAfterMs !== undefined) {
      return { code: failure.code, message: failure.message, retry_after_ms: Math.max(0, Math.round(failure.retryAfterMs)) };
    }
    return { code: failure.code, message: failure.message };
  }
  const message = failure instanceof Error ? failure.message : String(failure);
  return { code: "internal", message };
}

function errorResponse(id: number, error: RemoteError): ResponseErrorMessage {
  return { type: "response", id, ok: false, error };
}

/** Request router of one connection. */
export class Dispatcher {
  private readonly inFlight = new Map<number, InFlight>();

  constructor(
    private readonly registry: ModuleRegistry,
    private readonly services: DispatcherServices,
    private readonly send: FrameSender,
  ) {}

  /** Number of requests still running (for tests and diagnostics). */
  get inFlightCount(): number {
    return this.inFlight.size;
  }

  /** Starts `request`; its single response is sent when the module call settles. */
  handleRequest(request: RequestMessage): void {
    this.run(request).catch((error: unknown) => {
      // `run` handles every module outcome itself; reaching this means sending failed.
      console.error("[ms-bridge] could not answer a request", { id: request.id, module: request.module, method: request.method, error: String(error) });
    });
  }

  /** Aborts request `id` because the app cancelled it; a finished or unknown id is a no-op. */
  cancel(id: number): void {
    const entry = this.inFlight.get(id);
    if (entry === undefined) {
      console.debug("[ms-bridge] cancel for a request that already finished", { id });
      return;
    }
    entry.controller.abort(new BridgeFailure("cancelled", "the app cancelled the request"));
  }

  /** Aborts every running request without answering (the connection is gone). */
  abortAll(): void {
    for (const entry of this.inFlight.values()) {
      entry.silent = true;
      entry.controller.abort(new BridgeFailure("cancelled", "the connection to the app was lost"));
    }
  }

  private async run(request: RequestMessage): Promise<void> {
    const { id } = request;
    if (this.inFlight.has(id)) {
      this.send(errorResponse(id, { code: "internal", message: `request id ${id} is already in flight` }));
      return;
    }
    const module = this.registry.get(request.module);
    if (module === undefined) {
      this.send(errorResponse(id, { code: "unknown_module", message: `no module '${request.module}' in this extension` }));
      return;
    }
    const entry: InFlight = { controller: new AbortController(), silent: false };
    this.inFlight.set(id, entry);
    const { signal } = entry.controller;
    const ctx: ModuleContext = {
      signal,
      tabs: this.services.tabs,
      attention: this.services.attention,
      emit: (kind: EventKind, data: unknown) => this.emit(id, entry, kind, data),
    };
    let response: ExtMessage;
    try {
      const result = await module.handle(request.method, request.params, ctx);
      // An abort wins over a late result: the app was told nothing yet, and `cancelled` is
      // what it expects after sending `cancel`.
      response = signal.aborted
        ? errorResponse(id, responseForError(undefined, signal))
        : { type: "response", id, ok: true, result: result === undefined ? null : result };
    } catch (error: unknown) {
      const remote = responseForError(error, signal);
      if (remote.code === "internal") {
        console.error("[ms-bridge] module call failed", { id, module: request.module, method: request.method, error: remote.message });
      }
      response = errorResponse(id, remote);
    } finally {
      this.inFlight.delete(id);
    }
    if (entry.silent) return;
    this.send(fitToFrame(response));
  }

  private emit(id: number, entry: InFlight, kind: EventKind, data: unknown): void {
    if (this.inFlight.get(id) !== entry || entry.silent) {
      console.warn("[ms-bridge] event after the request finished was dropped", { id, kind });
      return;
    }
    this.send({ type: "event", id, kind, data });
  }
}

/** Replaces a response whose frame would exceed the app's cap with a `too_long` error. */
function fitToFrame(response: ExtMessage): ExtMessage {
  const bytes = new TextEncoder().encode(JSON.stringify(response)).length;
  if (bytes <= MAX_TEXT_FRAME_BYTES || response.type !== "response") return response;
  return errorResponse(response.id, { code: "too_long", message: `the result is ${bytes} bytes, over the ${MAX_TEXT_FRAME_BYTES}-byte frame limit` });
}
