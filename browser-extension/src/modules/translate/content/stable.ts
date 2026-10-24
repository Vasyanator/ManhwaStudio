/*
File: browser-extension/src/modules/translate/content/stable.ts

Purpose:
Pure completion detector of a translator page's output: decides when the text shown is the
finished translation of the text just inserted. No DOM, no timers: the driver feeds it
observations with a timestamp, so tests drive it on a fake timeline.

Key items:
- StableOutput

Rule (a candidate becomes the result once it is unchanged for `settleMs`):
- a candidate is non-empty (after trim) and not pending (the driver reports DeepL's `[...]`,
  pending selectors and "Translating..." placeholders as pending);
- it must differ from `baseline`, the output shown before the input was cleared, so a stale
  translation is never returned for the new text — UNLESS an empty output was observed after
  clearing: then whatever appears is new, even when it equals the baseline (the same text
  sent twice translates the same).
*/

/** Output-stability detector for one translation run. */
export class StableOutput {
  private sawEmpty: boolean;
  private candidate: string | null = null;
  private since = 0;

  /**
   * `baseline` is the output before clearing, or `null` when the output was already seen
   * empty after clearing (then no output is stale).
   */
  constructor(
    private readonly settleMs: number,
    private readonly baseline: string | null,
  ) {
    this.sawEmpty = baseline === null || baseline.trim() === "";
  }

  /** Records the output `text` seen at `nowMs`; `pending` = the page says it is still working. */
  observe(text: string, pending: boolean, nowMs: number): void {
    if (text.trim() === "") {
      this.sawEmpty = true;
      this.candidate = null;
      return;
    }
    const eligible = !pending && (this.sawEmpty || text !== this.baseline);
    if (!eligible) {
      this.candidate = null;
      return;
    }
    if (text !== this.candidate) {
      this.candidate = text;
      this.since = nowMs;
    }
  }

  /** The settled translation at `nowMs`, or `null` while there is none yet. */
  settled(nowMs: number): string | null {
    if (this.candidate === null || nowMs - this.since < this.settleMs) return null;
    return this.candidate;
  }

  /** Milliseconds until the current candidate settles, or `null` without a candidate. */
  remainingMs(nowMs: number): number | null {
    if (this.candidate === null) return null;
    return Math.max(0, this.settleMs - (nowMs - this.since));
  }
}
