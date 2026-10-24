/*
File: browser-extension/test/translate_sites.test.ts

Purpose:
Translate site tables: URL builders per site, table sanity (every site has its hooks,
challenge detection and limits; patterns are well-formed), the tab-URL classification, and
the driver bundle path agreeing with build.mjs.
*/

import assert from "node:assert/strict";
import { test } from "node:test";

import { urlMatchesAny } from "../src/core/tabs";
import { DRIVER_FILE } from "../src/modules/translate/driver_protocol";
import { classifyTabUrl } from "../src/modules/translate";
import { TRANSLATE_SITES, siteFor } from "../src/modules/translate/sites";
import { deeplFragmentSource } from "../src/modules/translate/sites/deepl";
import { TRANSLATE_SITE_KEYS } from "../src/modules/translate/types";

test("Google URL carries sl / tl / op=translate", () => {
  assert.equal(siteFor("google").pageUrl("ja", "ru"), "https://translate.google.com/?sl=ja&tl=ru&op=translate");
  assert.equal(siteFor("google").pageUrl("auto", "zh-CN"), "https://translate.google.com/?sl=auto&tl=zh-CN&op=translate");
});

test("DeepL URL puts the pair in the fragment and pins a stand-in for auto", () => {
  assert.equal(siteFor("deepl").pageUrl("ko", "ru"), "https://www.deepl.com/en/translator#ko/ru/");
  assert.equal(siteFor("deepl").pageUrl("auto", "ru"), "https://www.deepl.com/en/translator#en/ru/");
  assert.equal(siteFor("deepl").pageUrl("auto", "en-US"), "https://www.deepl.com/en/translator#de/en-US/");
  assert.equal(deeplFragmentSource("auto", "EN"), "de");
  assert.equal(deeplFragmentSource("ja", "en"), "ja");
});

test("Yandex URL uses source_lang / target_lang and omits the source for auto", () => {
  assert.equal(siteFor("yandex").pageUrl("ko", "ru"), "https://translate.yandex.ru/?source_lang=ko&target_lang=ru");
  assert.equal(siteFor("yandex").pageUrl("auto", "en"), "https://translate.yandex.ru/?target_lang=en");
});

test("every site URL belongs to the site's own hosts", () => {
  for (const key of TRANSLATE_SITE_KEYS) {
    const site = siteFor(key);
    const url = site.pageUrl("auto", "ru");
    assert.ok(urlMatchesAny(url, site.spec.hostPatterns), `${key}: ${url}`);
    assert.ok(!urlMatchesAny(url, site.spec.challengeUrlPatterns), `${key}: page URL reads as a challenge`);
    assert.equal(classifyTabUrl(site, url), "site");
  }
});

test("every site table is complete and keyed consistently", () => {
  assert.deepEqual(Object.keys(TRANSLATE_SITES).sort(), [...TRANSLATE_SITE_KEYS].sort());
  for (const key of TRANSLATE_SITE_KEYS) {
    const site = siteFor(key);
    assert.equal(site.key, key);
    assert.equal(site.spec.key, key, `${key}: the tab-lease key must be the site key`);
    assert.ok(site.selectors.input.length > 0, `${key}: input selectors`);
    assert.ok(site.selectors.output.length > 0, `${key}: output selectors`);
    assert.ok(site.selectors.challenge.length > 0, `${key}: in-page challenge selectors`);
    assert.ok(site.spec.challengeUrlPatterns.length > 0, `${key}: challenge URL patterns`);
    assert.ok(site.spec.hostPatterns.length > 0, `${key}: host patterns`);
    assert.ok(site.selectors.rateLimitBanner.length > 0, `${key}: rate-limit banner selectors`);
    assert.ok(site.rateLimitTextPatterns.length > 0, `${key}: rate-limit texts`);
    assert.ok(Number.isInteger(site.maxChars) && site.maxChars >= 1000, `${key}: limit`);
    assert.ok(Number.isInteger(site.settleMs) && site.settleMs > 0, `${key}: settle`);
    const all = Object.values(site.selectors).flat();
    for (const selector of all) {
      assert.equal(typeof selector, "string");
      assert.ok(selector.trim() !== "" && selector === selector.trim(), `${key}: blank or padded selector '${selector}'`);
    }
    for (const pattern of [...site.spec.hostPatterns, ...site.spec.challengeUrlPatterns]) {
      assert.ok(pattern.includes("*"), `${key}: pattern '${pattern}' should be a glob`);
    }
  }
});

test("site limits are the documented web limits (the app splits below them)", () => {
  assert.equal(siteFor("deepl").maxChars, 1500);
  assert.equal(siteFor("google").maxChars, 5000);
  assert.equal(siteFor("yandex").maxChars, 5000);
});

test("tab URLs classify as site / needs_user / gone", () => {
  const google = siteFor("google");
  assert.equal(classifyTabUrl(google, "https://translate.google.com/?sl=ja&tl=ru&text=x"), "site");
  assert.equal(classifyTabUrl(google, "https://www.google.com/sorry/index?continue=x"), "needs_user");
  assert.equal(classifyTabUrl(google, "https://consent.google.com/m?continue=x"), "needs_user");
  assert.equal(classifyTabUrl(google, undefined), "needs_user", "unreadable = off-site");
  assert.equal(classifyTabUrl(google, null), "gone");
  assert.equal(classifyTabUrl(google, "about:blank"), "needs_user");
  const deepl = siteFor("deepl");
  assert.equal(classifyTabUrl(deepl, "https://www.deepl.com/en/translator?__cf_chl_tk=abc#ko/ru/"), "needs_user");
  assert.equal(classifyTabUrl(deepl, "https://www.deepl.com/en/translator#ko/ru/text"), "site");
  const yandex = siteFor("yandex");
  assert.equal(classifyTabUrl(yandex, "https://translate.yandex.ru/showcaptcha?cc=1"), "needs_user");
  assert.equal(classifyTabUrl(yandex, "https://translate.yandex.com/?target_lang=en"), "site");
});

test("the driver path follows build.mjs (modules/<module>/driver.js)", () => {
  assert.equal(DRIVER_FILE, "modules/translate/driver.js");
});
