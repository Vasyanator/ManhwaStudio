/*
File: browser-extension/test/helpers.ts

Purpose:
Shared test helpers: golden fixture loading and fakes of the core's browser seams (tabs,
session storage, attention, socket). Bundled into each test file by `build.mjs --tests`.
*/

import { readFileSync, readdirSync } from "node:fs";
import { fileURLToPath } from "node:url";

import type { AttentionApi } from "../src/core/attention";
import type { SocketHandle, SocketHandlers } from "../src/core/connection";
import type { SessionStore, TabChange, TabInfo, TabsApi } from "../src/core/tabs";

/** Directory of the golden protocol fixtures (tests run from `.test-out/`). */
const FIXTURE_DIR = new URL("../protocol-fixtures/", import.meta.url);

/** Parsed JSON of `protocol-fixtures/<name>`. */
export function fixture(name: string): unknown {
  return JSON.parse(readFileSync(new URL(name, FIXTURE_DIR), "utf8"));
}

/** Every fixture file name. */
export function fixtureNames(): string[] {
  return readdirSync(fileURLToPath(FIXTURE_DIR)).filter((n) => n.endsWith(".json")).sort();
}

/** Resolves after `ms` real milliseconds. */
export function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

/** Lets every pending promise callback and zero-delay timer run. */
export async function flush(): Promise<void> {
  await sleep(0);
  await sleep(0);
}

type UpdatedListener = (tabId: number, change: TabChange) => void;
type RemovedListener = (tabId: number) => void;

/** In-memory tabs: `navigate` completes asynchronously unless `autoComplete` is false. */
export class FakeTabs implements TabsApi, AttentionApi {
  readonly tabs = new Map<number, TabInfo>();
  readonly updated = new Set<UpdatedListener>();
  readonly removed = new Set<RemovedListener>();
  readonly focused: number[] = [];
  autoComplete = true;
  created = 0;
  private nextId = 100;

  async get(tabId: number): Promise<TabInfo | null> {
    return this.tabs.get(tabId) ?? null;
  }

  async createDedicated(): Promise<number> {
    const id = this.nextId++;
    this.created += 1;
    this.tabs.set(id, { id, windowId: 1, url: "about:blank", status: "complete" });
    return id;
  }

  async navigate(tabId: number, url: string): Promise<void> {
    this.tabs.set(tabId, { id: tabId, windowId: 1, url, status: "loading" });
    if (this.autoComplete) setTimeout(() => this.complete(tabId), 1);
  }

  async focusTab(tabId: number): Promise<void> {
    this.focused.push(tabId);
  }

  /** Marks `tabId` loaded and fires `onUpdated{status:"complete"}`. */
  complete(tabId: number): void {
    const tab = this.tabs.get(tabId);
    if (tab === undefined) return;
    this.tabs.set(tabId, { ...tab, status: "complete" });
    for (const listener of [...this.updated]) listener(tabId, { status: "complete", url: tab.url });
  }

  /** Closes `tabId` and fires `onRemoved`. */
  close(tabId: number): void {
    this.tabs.delete(tabId);
    for (const listener of [...this.removed]) listener(tabId);
  }

  onUpdated(listener: UpdatedListener): () => void {
    this.updated.add(listener);
    return () => this.updated.delete(listener);
  }

  onRemoved(listener: RemovedListener): () => void {
    this.removed.add(listener);
    return () => this.removed.delete(listener);
  }
}

/** In-memory `SessionStore`. */
export class FakeStore implements SessionStore {
  readonly values = new Map<string, unknown>();

  async get(key: string): Promise<unknown> {
    return this.values.get(key);
  }

  async set(key: string, value: unknown): Promise<void> {
    this.values.set(key, value);
  }

  async remove(key: string): Promise<void> {
    this.values.delete(key);
  }
}

/** A socket the test drives: it records sent frames and can deliver app frames. */
export class FakeSocket implements SocketHandle {
  readonly sent: unknown[] = [];
  closed: { code: number; reason: string } | null = null;

  constructor(
    readonly url: string,
    readonly handlers: SocketHandlers,
  ) {}

  send(text: string): void {
    this.sent.push(JSON.parse(text));
  }

  close(code: number, reason: string): void {
    this.closed = { code, reason };
  }

  /** Delivers one app frame. */
  deliver(frame: unknown): void {
    this.handlers.onText(JSON.stringify(frame));
  }

  /** The last frame the extension sent. */
  last(): unknown {
    return this.sent[this.sent.length - 1];
  }
}
