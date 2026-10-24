/*
File: browser-extension/src/modules/translate/content/normalize.ts

Purpose:
Pure text helpers of the page driver: the normalization applied to a finished translation
before it is returned, and the whitespace-insensitive comparison used to confirm that the
input really holds the inserted text.

Key items:
- normalizeOutput(), sameTextLoosely(), sameLinesLoosely(), primaryLanguageDiffers(),
  isPendingText()
*/

/** Zero-width characters editors leave in their DOM (ZWSP, ZWNJ, ZWJ, BOM). */
const ZERO_WIDTH = /[​-‍﻿]/g;

/**
 * The translation as returned to the app: CRLF / CR -> LF, NBSP -> space, zero-width
 * characters removed, trailing spaces of every line removed, leading and trailing blank
 * lines removed. Inner lines (including empty ones) are kept: the app splits by lines.
 */
export function normalizeOutput(raw: string): string {
  const lines = raw
    .replace(/\r\n?/g, "\n")
    .replace(/ /g, " ")
    .replace(ZERO_WIDTH, "")
    .split("\n")
    .map((line) => line.replace(/[ \t]+$/, ""));
  while (lines.length > 0 && lines[0]?.trim() === "") lines.shift();
  while (lines.length > 0 && lines[lines.length - 1]?.trim() === "") lines.pop();
  return lines.join("\n");
}

/** True when `a` and `b` are equal after collapsing every whitespace run to one space and trimming. */
export function sameTextLoosely(a: string, b: string): boolean {
  const squash = (s: string): string => s.replace(ZERO_WIDTH, "").replace(/\s+/g, " ").trim();
  return squash(a) === squash(b);
}

/**
 * True when `a` and `b` hold the same non-empty lines, each compared with
 * `sameTextLoosely`. Used to confirm an insertion kept the line structure the app relies on
 * (blank lines are ignored: editors render paragraphs with varying blank-line spacing).
 */
export function sameLinesLoosely(a: string, b: string): boolean {
  const lines = (s: string): string[] => s.replace(ZERO_WIDTH, "").split(/\r\n?|\n/).map((line) => line.replace(/\s+/g, " ").trim()).filter((line) => line !== "");
  const left = lines(a);
  const right = lines(b);
  return left.length === right.length && left.every((line, index) => line === right[index]);
}

/**
 * True when the language tag `applied` (a page's `lang` attribute) names a different language
 * than the requested `target`: their primary subtags differ, case-insensitively (`en-US` vs
 * `en` is the same language; `de` vs `en` is not).
 */
export function primaryLanguageDiffers(applied: string, target: string): boolean {
  const primary = (tag: string): string => (tag.trim().split(/[-_]/)[0] ?? "").toLowerCase();
  return primary(applied) !== primary(target);
}

/** True when `text` contains a pending marker or is a whole-output placeholder. */
export function isPendingText(text: string, markers: readonly string[], patterns: readonly RegExp[]): boolean {
  if (markers.some((marker) => text.includes(marker))) return true;
  const trimmed = text.trim();
  return patterns.some((pattern) => pattern.test(trimmed));
}
