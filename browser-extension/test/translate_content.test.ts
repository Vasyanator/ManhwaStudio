/*
File: browser-extension/test/translate_content.test.ts

Purpose:
Pure parts of the translate page driver: the StableOutput completion rule on a fake
timeline, output normalization and comparisons, pending detection, and the validation of
values that come back from the page (`parseDriverResult`). No DOM, no browser.
*/

import assert from "node:assert/strict";
import { test } from "node:test";

import { isPendingText, normalizeOutput, primaryLanguageDiffers, sameLinesLoosely, sameTextLoosely } from "../src/modules/translate/content/normalize";
import { StableOutput } from "../src/modules/translate/content/stable";
import { parseDriverResult } from "../src/modules/translate/driver_protocol";

test("a candidate settles only after settleMs without change", () => {
  const d = new StableOutput(800, null);
  d.observe("Привет", false, 0);
  assert.equal(d.settled(799), null);
  assert.equal(d.remainingMs(500), 300);
  assert.equal(d.settled(800), "Привет");
});

test("a change restarts the settle window (streamed output)", () => {
  const d = new StableOutput(800, null);
  d.observe("Hello", false, 0);
  d.observe("Hello world", false, 600);
  assert.equal(d.settled(900), null);
  d.observe("Hello world", false, 1000); // same text again: the window keeps running
  assert.equal(d.settled(1399), null);
  assert.equal(d.settled(1400), "Hello world");
});

test("pending output never settles, and resets the candidate", () => {
  const d = new StableOutput(500, null);
  d.observe("Hello [...]", true, 0);
  assert.equal(d.settled(10_000), null);
  assert.equal(d.remainingMs(10_000), null);
  d.observe("Hello", false, 10_000);
  d.observe("Hello [...]", true, 10_200);
  d.observe("Hello", false, 10_300);
  assert.equal(d.settled(10_700), null, "the window restarted after the pending glimpse");
  assert.equal(d.settled(10_800), "Hello");
});

test("the pre-clear output is stale until an empty output was seen", () => {
  const d = new StableOutput(100, "old translation");
  d.observe("old translation", false, 0);
  assert.equal(d.settled(1000), null, "stale output is not a result");
  d.observe("", false, 1000);
  d.observe("old translation", false, 1100);
  assert.equal(d.settled(1200), "old translation", "after an empty glimpse the same text is new");
});

test("a different output settles even when the output never emptied", () => {
  const d = new StableOutput(100, "old");
  d.observe("new", false, 0);
  assert.equal(d.settled(100), "new");
});

test("a whitespace-only baseline counts as already empty", () => {
  const d = new StableOutput(100, "  \n ");
  d.observe("x", false, 0);
  assert.equal(d.settled(100), "x");
});

test("empty output is never a result", () => {
  const d = new StableOutput(0, null);
  d.observe("   ", false, 0);
  assert.equal(d.settled(1000), null);
});

test("normalizeOutput unifies line breaks and spaces, keeps inner lines", () => {
  assert.equal(normalizeOutput("\r\n  \nA b  \r\nc​\rd\t\n\n"), "A b\nc\nd");
  assert.equal(normalizeOutput("one\n\nthree"), "one\n\nthree", "inner blank lines are kept");
  assert.equal(normalizeOutput(""), "");
  assert.equal(normalizeOutput(" \n  "), "");
  assert.equal(normalizeOutput("  lead stays"), "  lead stays", "leading spaces of a line are content");
});

test("loose comparisons ignore whitespace runs, line comparison keeps lines", () => {
  assert.ok(sameTextLoosely(" a \n b ", "a b"));
  assert.ok(!sameTextLoosely("a b", "ab"));
  assert.ok(sameLinesLoosely("a\n\nb  c\n", "a\nb c"));
  assert.ok(!sameLinesLoosely("a b", "a\nb"), "a lost line break is detected");
  assert.ok(sameLinesLoosely("x​\r\ny", "x\ny"));
});

test("primary language comparison is case- and region-insensitive", () => {
  assert.ok(!primaryLanguageDiffers("en-US", "en"));
  assert.ok(!primaryLanguageDiffers("ZH-hans", "zh"));
  assert.ok(!primaryLanguageDiffers("pt_BR", "pt-PT"));
  assert.ok(primaryLanguageDiffers("de", "en-GB"));
});

test("pending text: markers anywhere, placeholders as the whole output", () => {
  assert.ok(isPendingText("Hello [...] world", ["[...]"], []));
  assert.ok(isPendingText("  Translating...  ", [], [/^translating\s*(\.\.\.|…)$/i]));
  assert.ok(!isPendingText("Translating... is fun", [], [/^translating\s*(\.\.\.|…)$/i]));
  assert.ok(!isPendingText("done", ["[...]"], []));
});

test("parseDriverResult accepts every result shape and rejects anything else", () => {
  assert.deepEqual(parseDriverResult({ kind: "ok", text: "x" }), { kind: "ok", text: "x" });
  assert.deepEqual(parseDriverResult({ kind: "challenge", extra: 1 }), { kind: "challenge" });
  assert.deepEqual(parseDriverResult({ kind: "cancelled" }), { kind: "cancelled" });
  assert.deepEqual(parseDriverResult({ kind: "rate_limited", message: "m" }), { kind: "rate_limited", message: "m" });
  assert.deepEqual(parseDriverResult({ kind: "site_changed", role: "input" }), { kind: "site_changed", role: "input" });
  assert.deepEqual(parseDriverResult({ kind: "unsupported_language", applied: "de" }), { kind: "unsupported_language", applied: "de" });
  assert.deepEqual(parseDriverResult({ kind: "timeout", stage: "translation" }), { kind: "timeout", stage: "translation" });
  for (const bad of [null, undefined, 1, "ok", [], {}, { kind: "ok" }, { kind: "ok", text: 1 }, { kind: "nope" }, { kind: "timeout" }]) {
    assert.equal(parseDriverResult(bad), null, JSON.stringify(bad) ?? "undefined");
  }
});
