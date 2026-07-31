/*
File: panel/char_table/charset.rs

Purpose:
The character set of the typing tab's character-table window: one
`&'static [char]` per group plus the `CharGroup` table that names them.

GENERATED FILE - do not edit by hand.
Regenerate with `python3 tools/gen_char_table.py` after changing the group
ranges in that script; the output is checked in and the generator is never
a runtime dependency.

Filters applied by the generator (all three mandatory, see the script):
  1. unassigned codepoints                    - dropped 6
  2. Cc/Cf/Zs/Zl/Zp/Mn/Me (invisible)         - dropped 0
  3. not drawable by any bundled `fonts/ui`   - dropped 7
  plus 0 codepoints claimed by an earlier group.

Key types:
- `CharGroup` (a group's stable key + its characters)

Key functions:
- `groups` (the whole table, in tab order)
- `group_by_key` (lookup by the persisted group key)

Notes:
`key` is the stable NON-LOCALIZED group identity: it is persisted in
`TextTab.char_table_last_group` and forms the i18n key suffix
(`typing.char_table.group.<key>_label`). The favorites tab is NOT a group
here - it is a UI concept backed by `favorites.rs`, not by a character list.
Characters are emitted as `\u{...}` escapes so the file stays ASCII and
unambiguous regardless of the reader's font.
*/

/// One tab of the character table: a stable key plus its characters.
///
/// `key` is persisted (`TextTab.char_table_last_group`) and is the i18n key
/// suffix; it must never be localized or renamed without a migration.
#[derive(Debug, Clone, Copy)]
pub(in crate::tabs::typing::panel) struct CharGroup {
    /// Stable, non-localized group identity (`"arrows"`, `"lines"`, ...).
    pub(in crate::tabs::typing::panel) key: &'static str,
    /// The group's characters, in display order. Never empty.
    pub(in crate::tabs::typing::panel) chars: &'static [char],
}

/// Characters of the `arrows` group (189 entries).
const ARROWS_CHARS: &[char] = &[
    '\u{2190}', '\u{2191}', '\u{2192}', '\u{2193}', '\u{2194}', '\u{2195}', '\u{2196}', '\u{2197}',
    '\u{2198}', '\u{2199}', '\u{219A}', '\u{219B}', '\u{219C}', '\u{219D}', '\u{219E}', '\u{219F}',
    '\u{21A0}', '\u{21A1}', '\u{21A2}', '\u{21A3}', '\u{21A4}', '\u{21A5}', '\u{21A6}', '\u{21A7}',
    '\u{21A8}', '\u{21A9}', '\u{21AA}', '\u{21AB}', '\u{21AC}', '\u{21AD}', '\u{21AE}', '\u{21AF}',
    '\u{21B0}', '\u{21B1}', '\u{21B2}', '\u{21B3}', '\u{21B4}', '\u{21B5}', '\u{21B6}', '\u{21B7}',
    '\u{21B8}', '\u{21B9}', '\u{21BA}', '\u{21BB}', '\u{21BC}', '\u{21BD}', '\u{21BE}', '\u{21BF}',
    '\u{21C0}', '\u{21C1}', '\u{21C2}', '\u{21C3}', '\u{21C4}', '\u{21C5}', '\u{21C6}', '\u{21C7}',
    '\u{21C8}', '\u{21C9}', '\u{21CA}', '\u{21CB}', '\u{21CC}', '\u{21CD}', '\u{21CE}', '\u{21CF}',
    '\u{21D0}', '\u{21D1}', '\u{21D2}', '\u{21D3}', '\u{21D4}', '\u{21D5}', '\u{21D6}', '\u{21D7}',
    '\u{21D8}', '\u{21D9}', '\u{21DA}', '\u{21DB}', '\u{21DC}', '\u{21DD}', '\u{21DE}', '\u{21DF}',
    '\u{21E0}', '\u{21E1}', '\u{21E2}', '\u{21E3}', '\u{21E4}', '\u{21E5}', '\u{21E6}', '\u{21E7}',
    '\u{21E8}', '\u{21E9}', '\u{21EA}', '\u{21EB}', '\u{21EC}', '\u{21ED}', '\u{21EE}', '\u{21EF}',
    '\u{21F0}', '\u{21F1}', '\u{21F2}', '\u{21F3}', '\u{21F4}', '\u{21F5}', '\u{21F6}', '\u{21F7}',
    '\u{21F8}', '\u{21F9}', '\u{21FA}', '\u{21FB}', '\u{21FC}', '\u{21FD}', '\u{21FE}', '\u{21FF}',
    '\u{27F0}', '\u{27F1}', '\u{27F2}', '\u{27F3}', '\u{27F4}', '\u{27F5}', '\u{27F6}', '\u{27F7}',
    '\u{27F8}', '\u{27F9}', '\u{27FA}', '\u{27FB}', '\u{27FC}', '\u{27FD}', '\u{27FE}', '\u{27FF}',
    '\u{2794}', '\u{2795}', '\u{2796}', '\u{2797}', '\u{2798}', '\u{2799}', '\u{279A}', '\u{279B}',
    '\u{279C}', '\u{279D}', '\u{279E}', '\u{279F}', '\u{27A0}', '\u{27A1}', '\u{27A2}', '\u{27A3}',
    '\u{27A4}', '\u{27A5}', '\u{27A6}', '\u{27A7}', '\u{27A8}', '\u{27A9}', '\u{27AA}', '\u{27AB}',
    '\u{27AC}', '\u{27AD}', '\u{27AE}', '\u{27AF}', '\u{27B0}', '\u{27B1}', '\u{27B2}', '\u{27B3}',
    '\u{27B4}', '\u{27B5}', '\u{27B6}', '\u{27B7}', '\u{27B8}', '\u{27B9}', '\u{27BA}', '\u{27BB}',
    '\u{27BC}', '\u{27BD}', '\u{27BE}', '\u{2B00}', '\u{2B01}', '\u{2B02}', '\u{2B03}', '\u{2B04}',
    '\u{2B05}', '\u{2B06}', '\u{2B07}', '\u{2B08}', '\u{2B09}', '\u{2B0A}', '\u{2B0B}', '\u{2B0C}',
    '\u{2B0D}', '\u{2B0E}', '\u{2B0F}', '\u{2B10}', '\u{2B11}',
];

/// Characters of the `lines` group (160 entries).
const LINES_CHARS: &[char] = &[
    '\u{2500}', '\u{2501}', '\u{2502}', '\u{2503}', '\u{2504}', '\u{2505}', '\u{2506}', '\u{2507}',
    '\u{2508}', '\u{2509}', '\u{250A}', '\u{250B}', '\u{250C}', '\u{250D}', '\u{250E}', '\u{250F}',
    '\u{2510}', '\u{2511}', '\u{2512}', '\u{2513}', '\u{2514}', '\u{2515}', '\u{2516}', '\u{2517}',
    '\u{2518}', '\u{2519}', '\u{251A}', '\u{251B}', '\u{251C}', '\u{251D}', '\u{251E}', '\u{251F}',
    '\u{2520}', '\u{2521}', '\u{2522}', '\u{2523}', '\u{2524}', '\u{2525}', '\u{2526}', '\u{2527}',
    '\u{2528}', '\u{2529}', '\u{252A}', '\u{252B}', '\u{252C}', '\u{252D}', '\u{252E}', '\u{252F}',
    '\u{2530}', '\u{2531}', '\u{2532}', '\u{2533}', '\u{2534}', '\u{2535}', '\u{2536}', '\u{2537}',
    '\u{2538}', '\u{2539}', '\u{253A}', '\u{253B}', '\u{253C}', '\u{253D}', '\u{253E}', '\u{253F}',
    '\u{2540}', '\u{2541}', '\u{2542}', '\u{2543}', '\u{2544}', '\u{2545}', '\u{2546}', '\u{2547}',
    '\u{2548}', '\u{2549}', '\u{254A}', '\u{254B}', '\u{254C}', '\u{254D}', '\u{254E}', '\u{254F}',
    '\u{2550}', '\u{2551}', '\u{2552}', '\u{2553}', '\u{2554}', '\u{2555}', '\u{2556}', '\u{2557}',
    '\u{2558}', '\u{2559}', '\u{255A}', '\u{255B}', '\u{255C}', '\u{255D}', '\u{255E}', '\u{255F}',
    '\u{2560}', '\u{2561}', '\u{2562}', '\u{2563}', '\u{2564}', '\u{2565}', '\u{2566}', '\u{2567}',
    '\u{2568}', '\u{2569}', '\u{256A}', '\u{256B}', '\u{256C}', '\u{256D}', '\u{256E}', '\u{256F}',
    '\u{2570}', '\u{2571}', '\u{2572}', '\u{2573}', '\u{2574}', '\u{2575}', '\u{2576}', '\u{2577}',
    '\u{2578}', '\u{2579}', '\u{257A}', '\u{257B}', '\u{257C}', '\u{257D}', '\u{257E}', '\u{257F}',
    '\u{2580}', '\u{2581}', '\u{2582}', '\u{2583}', '\u{2584}', '\u{2585}', '\u{2586}', '\u{2587}',
    '\u{2588}', '\u{2589}', '\u{258A}', '\u{258B}', '\u{258C}', '\u{258D}', '\u{258E}', '\u{258F}',
    '\u{2590}', '\u{2591}', '\u{2592}', '\u{2593}', '\u{2594}', '\u{2595}', '\u{2596}', '\u{2597}',
    '\u{2598}', '\u{2599}', '\u{259A}', '\u{259B}', '\u{259C}', '\u{259D}', '\u{259E}', '\u{259F}',
];

/// Characters of the `shapes` group (158 entries).
const SHAPES_CHARS: &[char] = &[
    '\u{25A0}', '\u{25A1}', '\u{25A2}', '\u{25A3}', '\u{25A4}', '\u{25A5}', '\u{25A6}', '\u{25A7}',
    '\u{25A8}', '\u{25A9}', '\u{25AA}', '\u{25AB}', '\u{25AC}', '\u{25AD}', '\u{25AE}', '\u{25AF}',
    '\u{25B0}', '\u{25B1}', '\u{25B2}', '\u{25B3}', '\u{25B4}', '\u{25B5}', '\u{25B6}', '\u{25B7}',
    '\u{25B8}', '\u{25B9}', '\u{25BA}', '\u{25BB}', '\u{25BC}', '\u{25BD}', '\u{25BE}', '\u{25BF}',
    '\u{25C0}', '\u{25C1}', '\u{25C2}', '\u{25C3}', '\u{25C4}', '\u{25C5}', '\u{25C6}', '\u{25C7}',
    '\u{25C8}', '\u{25C9}', '\u{25CA}', '\u{25CB}', '\u{25CC}', '\u{25CD}', '\u{25CE}', '\u{25CF}',
    '\u{25D0}', '\u{25D1}', '\u{25D2}', '\u{25D3}', '\u{25D4}', '\u{25D5}', '\u{25D6}', '\u{25D7}',
    '\u{25D8}', '\u{25D9}', '\u{25DA}', '\u{25DB}', '\u{25DC}', '\u{25DD}', '\u{25DE}', '\u{25DF}',
    '\u{25E0}', '\u{25E1}', '\u{25E2}', '\u{25E3}', '\u{25E4}', '\u{25E5}', '\u{25E6}', '\u{25E7}',
    '\u{25E8}', '\u{25E9}', '\u{25EA}', '\u{25EB}', '\u{25EC}', '\u{25ED}', '\u{25EE}', '\u{25EF}',
    '\u{25F0}', '\u{25F1}', '\u{25F2}', '\u{25F3}', '\u{25F4}', '\u{25F5}', '\u{25F6}', '\u{25F7}',
    '\u{25F8}', '\u{25F9}', '\u{25FA}', '\u{25FB}', '\u{25FC}', '\u{25FD}', '\u{25FE}', '\u{25FF}',
    '\u{2B12}', '\u{2B13}', '\u{2B14}', '\u{2B15}', '\u{2B16}', '\u{2B17}', '\u{2B18}', '\u{2B19}',
    '\u{2B1A}', '\u{2B1B}', '\u{2B1C}', '\u{2B1D}', '\u{2B1E}', '\u{2B1F}', '\u{2B20}', '\u{2B21}',
    '\u{2B22}', '\u{2B23}', '\u{2B24}', '\u{2B25}', '\u{2B26}', '\u{2B27}', '\u{2B28}', '\u{2B29}',
    '\u{2B2A}', '\u{2B2B}', '\u{2B2C}', '\u{2B2D}', '\u{2B2E}', '\u{2B2F}', '\u{2B30}', '\u{2B31}',
    '\u{2B32}', '\u{2B33}', '\u{2B34}', '\u{2B35}', '\u{2B36}', '\u{2B37}', '\u{2B38}', '\u{2B39}',
    '\u{2B3A}', '\u{2B3B}', '\u{2B3C}', '\u{2B3D}', '\u{2B3E}', '\u{2B3F}', '\u{2B40}', '\u{2B41}',
    '\u{2B42}', '\u{2B43}', '\u{2B44}', '\u{2B45}', '\u{2B46}', '\u{2B47}', '\u{2B48}', '\u{2B49}',
    '\u{2B4A}', '\u{2B4B}', '\u{2B4C}', '\u{2B4D}', '\u{2B4E}', '\u{2B4F}',
];

/// Characters of the `math` group (304 entries).
const MATH_CHARS: &[char] = &[
    '\u{2200}', '\u{2201}', '\u{2202}', '\u{2203}', '\u{2204}', '\u{2205}', '\u{2206}', '\u{2207}',
    '\u{2208}', '\u{2209}', '\u{220A}', '\u{220B}', '\u{220C}', '\u{220D}', '\u{220E}', '\u{220F}',
    '\u{2210}', '\u{2211}', '\u{2212}', '\u{2213}', '\u{2214}', '\u{2215}', '\u{2216}', '\u{2217}',
    '\u{2218}', '\u{2219}', '\u{221A}', '\u{221B}', '\u{221C}', '\u{221D}', '\u{221E}', '\u{221F}',
    '\u{2220}', '\u{2221}', '\u{2222}', '\u{2223}', '\u{2224}', '\u{2225}', '\u{2226}', '\u{2227}',
    '\u{2228}', '\u{2229}', '\u{222A}', '\u{222B}', '\u{222C}', '\u{222D}', '\u{222E}', '\u{222F}',
    '\u{2230}', '\u{2231}', '\u{2232}', '\u{2233}', '\u{2234}', '\u{2235}', '\u{2236}', '\u{2237}',
    '\u{2238}', '\u{2239}', '\u{223A}', '\u{223B}', '\u{223C}', '\u{223D}', '\u{223E}', '\u{223F}',
    '\u{2240}', '\u{2241}', '\u{2242}', '\u{2243}', '\u{2244}', '\u{2245}', '\u{2246}', '\u{2247}',
    '\u{2248}', '\u{2249}', '\u{224A}', '\u{224B}', '\u{224C}', '\u{224D}', '\u{224E}', '\u{224F}',
    '\u{2250}', '\u{2251}', '\u{2252}', '\u{2253}', '\u{2254}', '\u{2255}', '\u{2256}', '\u{2257}',
    '\u{2258}', '\u{2259}', '\u{225A}', '\u{225B}', '\u{225C}', '\u{225D}', '\u{225E}', '\u{225F}',
    '\u{2260}', '\u{2261}', '\u{2262}', '\u{2263}', '\u{2264}', '\u{2265}', '\u{2266}', '\u{2267}',
    '\u{2268}', '\u{2269}', '\u{226A}', '\u{226B}', '\u{226C}', '\u{226D}', '\u{226E}', '\u{226F}',
    '\u{2270}', '\u{2271}', '\u{2272}', '\u{2273}', '\u{2274}', '\u{2275}', '\u{2276}', '\u{2277}',
    '\u{2278}', '\u{2279}', '\u{227A}', '\u{227B}', '\u{227C}', '\u{227D}', '\u{227E}', '\u{227F}',
    '\u{2280}', '\u{2281}', '\u{2282}', '\u{2283}', '\u{2284}', '\u{2285}', '\u{2286}', '\u{2287}',
    '\u{2288}', '\u{2289}', '\u{228A}', '\u{228B}', '\u{228C}', '\u{228D}', '\u{228E}', '\u{228F}',
    '\u{2290}', '\u{2291}', '\u{2292}', '\u{2293}', '\u{2294}', '\u{2295}', '\u{2296}', '\u{2297}',
    '\u{2298}', '\u{2299}', '\u{229A}', '\u{229B}', '\u{229C}', '\u{229D}', '\u{229E}', '\u{229F}',
    '\u{22A0}', '\u{22A1}', '\u{22A2}', '\u{22A3}', '\u{22A4}', '\u{22A5}', '\u{22A6}', '\u{22A7}',
    '\u{22A8}', '\u{22A9}', '\u{22AA}', '\u{22AB}', '\u{22AC}', '\u{22AD}', '\u{22AE}', '\u{22AF}',
    '\u{22B0}', '\u{22B1}', '\u{22B2}', '\u{22B3}', '\u{22B4}', '\u{22B5}', '\u{22B6}', '\u{22B7}',
    '\u{22B8}', '\u{22B9}', '\u{22BA}', '\u{22BB}', '\u{22BC}', '\u{22BD}', '\u{22BE}', '\u{22BF}',
    '\u{22C0}', '\u{22C1}', '\u{22C2}', '\u{22C3}', '\u{22C4}', '\u{22C5}', '\u{22C6}', '\u{22C7}',
    '\u{22C8}', '\u{22C9}', '\u{22CA}', '\u{22CB}', '\u{22CC}', '\u{22CD}', '\u{22CE}', '\u{22CF}',
    '\u{22D0}', '\u{22D1}', '\u{22D2}', '\u{22D3}', '\u{22D4}', '\u{22D5}', '\u{22D6}', '\u{22D7}',
    '\u{22D8}', '\u{22D9}', '\u{22DA}', '\u{22DB}', '\u{22DC}', '\u{22DD}', '\u{22DE}', '\u{22DF}',
    '\u{22E0}', '\u{22E1}', '\u{22E2}', '\u{22E3}', '\u{22E4}', '\u{22E5}', '\u{22E6}', '\u{22E7}',
    '\u{22E8}', '\u{22E9}', '\u{22EA}', '\u{22EB}', '\u{22EC}', '\u{22ED}', '\u{22EE}', '\u{22EF}',
    '\u{22F0}', '\u{22F1}', '\u{22F2}', '\u{22F3}', '\u{22F4}', '\u{22F5}', '\u{22F6}', '\u{22F7}',
    '\u{22F8}', '\u{22F9}', '\u{22FA}', '\u{22FB}', '\u{22FC}', '\u{22FD}', '\u{22FE}', '\u{22FF}',
    '\u{2070}', '\u{2071}', '\u{2074}', '\u{2075}', '\u{2076}', '\u{2077}', '\u{2078}', '\u{2079}',
    '\u{207A}', '\u{207B}', '\u{207C}', '\u{207D}', '\u{207E}', '\u{207F}', '\u{2080}', '\u{2081}',
    '\u{2082}', '\u{2083}', '\u{2084}', '\u{2085}', '\u{2086}', '\u{2087}', '\u{2088}', '\u{2089}',
    '\u{208A}', '\u{208B}', '\u{208C}', '\u{208D}', '\u{208E}', '\u{2090}', '\u{2091}', '\u{2092}',
    '\u{2093}', '\u{2094}', '\u{2095}', '\u{2096}', '\u{2097}', '\u{2098}', '\u{2099}', '\u{209A}',
    '\u{209B}', '\u{209C}', '\u{00B1}', '\u{00D7}', '\u{00F7}', '\u{2032}', '\u{2033}', '\u{2044}',
];

/// Characters of the `typography` group (47 entries).
const TYPOGRAPHY_CHARS: &[char] = &[
    '\u{2010}', '\u{2011}', '\u{2012}', '\u{2013}', '\u{2014}', '\u{2015}', '\u{2018}', '\u{2019}',
    '\u{201A}', '\u{201B}', '\u{201C}', '\u{201D}', '\u{201E}', '\u{201F}', '\u{2039}', '\u{203A}',
    '\u{00AB}', '\u{00BB}', '\u{2020}', '\u{2021}', '\u{2022}', '\u{2023}', '\u{2027}', '\u{2030}',
    '\u{2031}', '\u{00B6}', '\u{00A7}', '\u{00A9}', '\u{00AE}', '\u{2122}', '\u{00B0}', '\u{00B7}',
    '\u{00A1}', '\u{00BF}', '\u{203C}', '\u{203D}', '\u{2047}', '\u{2048}', '\u{2049}', '\u{2026}',
    '\u{2025}', '\u{203B}', '\u{2042}', '\u{00A6}', '\u{00AC}', '\u{2E2E}', '\u{2E18}',
];

/// Characters of the `currency` group (37 entries).
const CURRENCY_CHARS: &[char] = &[
    '\u{20A0}', '\u{20A1}', '\u{20A2}', '\u{20A3}', '\u{20A4}', '\u{20A5}', '\u{20A6}', '\u{20A7}',
    '\u{20A8}', '\u{20A9}', '\u{20AA}', '\u{20AB}', '\u{20AC}', '\u{20AD}', '\u{20AE}', '\u{20AF}',
    '\u{20B0}', '\u{20B1}', '\u{20B2}', '\u{20B3}', '\u{20B4}', '\u{20B5}', '\u{20B6}', '\u{20B7}',
    '\u{20B8}', '\u{20B9}', '\u{20BA}', '\u{20BB}', '\u{20BC}', '\u{20BD}', '\u{20BE}', '\u{20BF}',
    '\u{0024}', '\u{00A2}', '\u{00A3}', '\u{00A5}', '\u{00A4}',
];

/// Characters of the `music` group (77 entries).
const MUSIC_CHARS: &[char] = &[
    '\u{2669}', '\u{266A}', '\u{266B}', '\u{266C}', '\u{266D}', '\u{266E}', '\u{266F}', '\u{1D100}',
    '\u{1D101}', '\u{1D102}', '\u{1D103}', '\u{1D104}', '\u{1D105}', '\u{1D106}', '\u{1D107}', '\u{1D10B}',
    '\u{1D10C}', '\u{1D10D}', '\u{1D10E}', '\u{1D10F}', '\u{1D110}', '\u{1D111}', '\u{1D112}', '\u{1D113}',
    '\u{1D11E}', '\u{1D11F}', '\u{1D120}', '\u{1D121}', '\u{1D122}', '\u{1D12A}', '\u{1D12B}', '\u{1D12C}',
    '\u{1D12D}', '\u{1D12E}', '\u{1D12F}', '\u{1D130}', '\u{1D131}', '\u{1D132}', '\u{1D133}', '\u{1D134}',
    '\u{1D135}', '\u{1D13B}', '\u{1D13C}', '\u{1D13D}', '\u{1D13E}', '\u{1D13F}', '\u{1D140}', '\u{1D141}',
    '\u{1D142}', '\u{1D15D}', '\u{1D15E}', '\u{1D15F}', '\u{1D160}', '\u{1D161}', '\u{1D162}', '\u{1D163}',
    '\u{1D164}', '\u{1D16A}', '\u{1D16B}', '\u{1D16C}', '\u{1D183}', '\u{1D184}', '\u{1D19B}', '\u{1D19C}',
    '\u{1D19D}', '\u{1D19E}', '\u{1D19F}', '\u{1D1A0}', '\u{1D1A1}', '\u{1D1A2}', '\u{1D1A3}', '\u{1D1A4}',
    '\u{1D1A5}', '\u{1D1A6}', '\u{1D1A7}', '\u{1D1A8}', '\u{1D1A9}',
];

/// Characters of the `technical` group (271 entries).
const TECHNICAL_CHARS: &[char] = &[
    '\u{2300}', '\u{2301}', '\u{2302}', '\u{2303}', '\u{2304}', '\u{2305}', '\u{2306}', '\u{2307}',
    '\u{2308}', '\u{2309}', '\u{230A}', '\u{230B}', '\u{230C}', '\u{230D}', '\u{230E}', '\u{230F}',
    '\u{2310}', '\u{2311}', '\u{2312}', '\u{2313}', '\u{2314}', '\u{2315}', '\u{2316}', '\u{2317}',
    '\u{2318}', '\u{2319}', '\u{231A}', '\u{231B}', '\u{231C}', '\u{231D}', '\u{231E}', '\u{231F}',
    '\u{2320}', '\u{2321}', '\u{2322}', '\u{2323}', '\u{2324}', '\u{2325}', '\u{2326}', '\u{2327}',
    '\u{2328}', '\u{2329}', '\u{232A}', '\u{232B}', '\u{232C}', '\u{232D}', '\u{232E}', '\u{232F}',
    '\u{2330}', '\u{2331}', '\u{2332}', '\u{2333}', '\u{2334}', '\u{2335}', '\u{2336}', '\u{2337}',
    '\u{2338}', '\u{2339}', '\u{233A}', '\u{233B}', '\u{233C}', '\u{233D}', '\u{233E}', '\u{233F}',
    '\u{2340}', '\u{2341}', '\u{2342}', '\u{2343}', '\u{2344}', '\u{2345}', '\u{2346}', '\u{2347}',
    '\u{2348}', '\u{2349}', '\u{234A}', '\u{234B}', '\u{234C}', '\u{234D}', '\u{234E}', '\u{234F}',
    '\u{2350}', '\u{2351}', '\u{2352}', '\u{2353}', '\u{2354}', '\u{2355}', '\u{2356}', '\u{2357}',
    '\u{2358}', '\u{2359}', '\u{235A}', '\u{235B}', '\u{235C}', '\u{235D}', '\u{235E}', '\u{235F}',
    '\u{2360}', '\u{2361}', '\u{2362}', '\u{2363}', '\u{2364}', '\u{2365}', '\u{2366}', '\u{2367}',
    '\u{2368}', '\u{2369}', '\u{236A}', '\u{236B}', '\u{236C}', '\u{236D}', '\u{236E}', '\u{236F}',
    '\u{2370}', '\u{2371}', '\u{2372}', '\u{2373}', '\u{2374}', '\u{2375}', '\u{2376}', '\u{2377}',
    '\u{2378}', '\u{2379}', '\u{237A}', '\u{237B}', '\u{237C}', '\u{237D}', '\u{237E}', '\u{237F}',
    '\u{2380}', '\u{2381}', '\u{2382}', '\u{2383}', '\u{2384}', '\u{2385}', '\u{2386}', '\u{2387}',
    '\u{2388}', '\u{2389}', '\u{238A}', '\u{238B}', '\u{238C}', '\u{238D}', '\u{238E}', '\u{238F}',
    '\u{2390}', '\u{2391}', '\u{2392}', '\u{2393}', '\u{2394}', '\u{2395}', '\u{2396}', '\u{2397}',
    '\u{2398}', '\u{2399}', '\u{239A}', '\u{239B}', '\u{239C}', '\u{239D}', '\u{239E}', '\u{239F}',
    '\u{23A0}', '\u{23A1}', '\u{23A2}', '\u{23A3}', '\u{23A4}', '\u{23A5}', '\u{23A6}', '\u{23A7}',
    '\u{23A8}', '\u{23A9}', '\u{23AA}', '\u{23AB}', '\u{23AC}', '\u{23AD}', '\u{23AE}', '\u{23AF}',
    '\u{23B0}', '\u{23B1}', '\u{23B2}', '\u{23B3}', '\u{23B4}', '\u{23B5}', '\u{23B6}', '\u{23BE}',
    '\u{23BF}', '\u{23C0}', '\u{23C1}', '\u{23C2}', '\u{23C3}', '\u{23C4}', '\u{23C5}', '\u{23C6}',
    '\u{23C7}', '\u{23C8}', '\u{23C9}', '\u{23CA}', '\u{23CB}', '\u{23CC}', '\u{23CD}', '\u{23CE}',
    '\u{23CF}', '\u{23D0}', '\u{23D1}', '\u{23D2}', '\u{23D3}', '\u{23D4}', '\u{23D5}', '\u{23D6}',
    '\u{23D7}', '\u{23D8}', '\u{23D9}', '\u{23DA}', '\u{23DB}', '\u{23DC}', '\u{23DD}', '\u{23DE}',
    '\u{23DF}', '\u{23E0}', '\u{23E1}', '\u{23E2}', '\u{23E3}', '\u{23E4}', '\u{23E5}', '\u{23E6}',
    '\u{23E7}', '\u{23E8}', '\u{23E9}', '\u{23EA}', '\u{23EB}', '\u{23EC}', '\u{23ED}', '\u{23EE}',
    '\u{23EF}', '\u{23F0}', '\u{23F1}', '\u{23F2}', '\u{23F3}', '\u{23F4}', '\u{23F5}', '\u{23F6}',
    '\u{23F7}', '\u{23F8}', '\u{23F9}', '\u{23FA}', '\u{23FB}', '\u{23FC}', '\u{23FD}', '\u{23FE}',
    '\u{23FF}', '\u{2701}', '\u{2702}', '\u{2703}', '\u{2704}', '\u{2706}', '\u{2707}', '\u{2708}',
    '\u{2709}', '\u{270D}', '\u{270E}', '\u{270F}', '\u{2710}', '\u{2711}', '\u{2712}', '\u{2713}',
    '\u{2714}', '\u{2715}', '\u{2716}', '\u{2717}', '\u{2718}', '\u{274C}', '\u{274E}',
];

/// Characters of the `game` group (86 entries).
const GAME_CHARS: &[char] = &[
    '\u{2654}', '\u{2655}', '\u{2656}', '\u{2657}', '\u{2658}', '\u{2659}', '\u{265A}', '\u{265B}',
    '\u{265C}', '\u{265D}', '\u{265E}', '\u{265F}', '\u{2660}', '\u{2661}', '\u{2662}', '\u{2663}',
    '\u{2664}', '\u{2665}', '\u{2666}', '\u{2667}', '\u{2680}', '\u{2681}', '\u{2682}', '\u{2683}',
    '\u{2684}', '\u{2685}', '\u{1F0A0}', '\u{1F0A1}', '\u{1F0A2}', '\u{1F0A3}', '\u{1F0A4}', '\u{1F0A5}',
    '\u{1F0A6}', '\u{1F0A7}', '\u{1F0A8}', '\u{1F0A9}', '\u{1F0AA}', '\u{1F0AB}', '\u{1F0AC}', '\u{1F0AD}',
    '\u{1F0AE}', '\u{1F0B1}', '\u{1F0B2}', '\u{1F0B3}', '\u{1F0B4}', '\u{1F0B5}', '\u{1F0B6}', '\u{1F0B7}',
    '\u{1F0B8}', '\u{1F0B9}', '\u{1F0BA}', '\u{1F0BB}', '\u{1F0BC}', '\u{1F0BD}', '\u{1F0BE}', '\u{1F0BF}',
    '\u{1F0C1}', '\u{1F0C2}', '\u{1F0C3}', '\u{1F0C4}', '\u{1F0C5}', '\u{1F0C6}', '\u{1F0C7}', '\u{1F0C8}',
    '\u{1F0C9}', '\u{1F0CA}', '\u{1F0CB}', '\u{1F0CC}', '\u{1F0CD}', '\u{1F0CE}', '\u{1F0CF}', '\u{1F0D1}',
    '\u{1F0D2}', '\u{1F0D3}', '\u{1F0D4}', '\u{1F0D5}', '\u{1F0D6}', '\u{1F0D7}', '\u{1F0D8}', '\u{1F0D9}',
    '\u{1F0DA}', '\u{1F0DB}', '\u{1F0DC}', '\u{1F0DD}', '\u{1F0DE}', '\u{1F0DF}',
];

/// Characters of the `stars_weather` group (39 entries).
const STARS_WEATHER_CHARS: &[char] = &[
    '\u{2600}', '\u{2601}', '\u{2602}', '\u{2603}', '\u{2604}', '\u{2605}', '\u{2606}', '\u{263C}',
    '\u{263D}', '\u{263E}', '\u{263F}', '\u{2721}', '\u{2722}', '\u{2723}', '\u{2724}', '\u{2725}',
    '\u{2726}', '\u{2727}', '\u{2728}', '\u{2729}', '\u{272A}', '\u{272B}', '\u{272C}', '\u{272D}',
    '\u{272E}', '\u{272F}', '\u{2730}', '\u{2731}', '\u{2732}', '\u{2733}', '\u{2734}', '\u{26C4}',
    '\u{26C5}', '\u{26C6}', '\u{26C7}', '\u{26C8}', '\u{2609}', '\u{2744}', '\u{2B50}',
];

/// Characters of the `emoji` group (329 entries).
const EMOJI_CHARS: &[char] = &[
    '\u{1F300}', '\u{1F301}', '\u{1F302}', '\u{1F303}', '\u{1F304}', '\u{1F305}', '\u{1F306}', '\u{1F307}',
    '\u{1F308}', '\u{1F309}', '\u{1F30A}', '\u{1F30B}', '\u{1F30C}', '\u{1F30D}', '\u{1F30E}', '\u{1F30F}',
    '\u{1F310}', '\u{1F311}', '\u{1F312}', '\u{1F313}', '\u{1F314}', '\u{1F315}', '\u{1F316}', '\u{1F317}',
    '\u{1F318}', '\u{1F319}', '\u{1F31A}', '\u{1F31B}', '\u{1F31C}', '\u{1F31D}', '\u{1F31E}', '\u{1F31F}',
    '\u{1F320}', '\u{1F321}', '\u{1F332}', '\u{1F333}', '\u{1F334}', '\u{1F335}', '\u{1F336}', '\u{1F337}',
    '\u{1F338}', '\u{1F339}', '\u{1F33A}', '\u{1F33B}', '\u{1F33C}', '\u{1F33D}', '\u{1F33E}', '\u{1F33F}',
    '\u{1F340}', '\u{1F341}', '\u{1F342}', '\u{1F343}', '\u{1F345}', '\u{1F346}', '\u{1F347}', '\u{1F348}',
    '\u{1F349}', '\u{1F34A}', '\u{1F34B}', '\u{1F34C}', '\u{1F34D}', '\u{1F34E}', '\u{1F34F}', '\u{1F350}',
    '\u{1F351}', '\u{1F352}', '\u{1F353}', '\u{1F354}', '\u{1F355}', '\u{1F356}', '\u{1F357}', '\u{1F358}',
    '\u{1F359}', '\u{1F35A}', '\u{1F35B}', '\u{1F35C}', '\u{1F35D}', '\u{1F35E}', '\u{1F35F}', '\u{1F360}',
    '\u{1F361}', '\u{1F362}', '\u{1F363}', '\u{1F364}', '\u{1F365}', '\u{1F366}', '\u{1F367}', '\u{1F368}',
    '\u{1F369}', '\u{1F36A}', '\u{1F36B}', '\u{1F36C}', '\u{1F36D}', '\u{1F36E}', '\u{1F36F}', '\u{1F380}',
    '\u{1F381}', '\u{1F382}', '\u{1F383}', '\u{1F384}', '\u{1F385}', '\u{1F386}', '\u{1F387}', '\u{1F388}',
    '\u{1F389}', '\u{1F38A}', '\u{1F38B}', '\u{1F38C}', '\u{1F38D}', '\u{1F38E}', '\u{1F38F}', '\u{1F390}',
    '\u{1F391}', '\u{1F392}', '\u{1F393}', '\u{1F3A0}', '\u{1F3A1}', '\u{1F3A2}', '\u{1F3A3}', '\u{1F3A4}',
    '\u{1F3A5}', '\u{1F3A6}', '\u{1F3A7}', '\u{1F3A8}', '\u{1F3A9}', '\u{1F3AA}', '\u{1F3AB}', '\u{1F3AC}',
    '\u{1F3AD}', '\u{1F3AE}', '\u{1F3AF}', '\u{1F3B0}', '\u{1F3B1}', '\u{1F3B2}', '\u{1F3B3}', '\u{1F3B4}',
    '\u{1F3B5}', '\u{1F3B6}', '\u{1F3B7}', '\u{1F3B8}', '\u{1F3B9}', '\u{1F3BA}', '\u{1F3BB}', '\u{1F3BC}',
    '\u{1F3BD}', '\u{1F3BE}', '\u{1F3BF}', '\u{1F3C0}', '\u{1F3C1}', '\u{1F3C2}', '\u{1F3C3}', '\u{1F3C4}',
    '\u{1F400}', '\u{1F401}', '\u{1F402}', '\u{1F403}', '\u{1F404}', '\u{1F405}', '\u{1F406}', '\u{1F407}',
    '\u{1F408}', '\u{1F409}', '\u{1F40A}', '\u{1F40B}', '\u{1F40C}', '\u{1F40D}', '\u{1F40E}', '\u{1F40F}',
    '\u{1F410}', '\u{1F411}', '\u{1F412}', '\u{1F413}', '\u{1F414}', '\u{1F415}', '\u{1F416}', '\u{1F417}',
    '\u{1F418}', '\u{1F419}', '\u{1F41A}', '\u{1F41B}', '\u{1F41C}', '\u{1F41D}', '\u{1F41E}', '\u{1F41F}',
    '\u{1F420}', '\u{1F421}', '\u{1F422}', '\u{1F423}', '\u{1F424}', '\u{1F425}', '\u{1F426}', '\u{1F427}',
    '\u{1F428}', '\u{1F429}', '\u{1F42A}', '\u{1F42B}', '\u{1F42C}', '\u{1F42D}', '\u{1F42E}', '\u{1F42F}',
    '\u{1F430}', '\u{1F431}', '\u{1F432}', '\u{1F433}', '\u{1F434}', '\u{1F435}', '\u{1F436}', '\u{1F437}',
    '\u{1F438}', '\u{1F439}', '\u{1F43A}', '\u{1F43B}', '\u{1F43C}', '\u{1F43D}', '\u{1F43E}', '\u{1F43F}',
    '\u{1F600}', '\u{1F601}', '\u{1F602}', '\u{1F603}', '\u{1F604}', '\u{1F605}', '\u{1F606}', '\u{1F607}',
    '\u{1F608}', '\u{1F609}', '\u{1F60A}', '\u{1F60B}', '\u{1F60C}', '\u{1F60D}', '\u{1F60E}', '\u{1F60F}',
    '\u{1F610}', '\u{1F611}', '\u{1F612}', '\u{1F613}', '\u{1F614}', '\u{1F615}', '\u{1F616}', '\u{1F617}',
    '\u{1F618}', '\u{1F619}', '\u{1F61A}', '\u{1F61B}', '\u{1F61C}', '\u{1F61D}', '\u{1F61E}', '\u{1F61F}',
    '\u{1F620}', '\u{1F621}', '\u{1F622}', '\u{1F623}', '\u{1F624}', '\u{1F625}', '\u{1F626}', '\u{1F627}',
    '\u{1F628}', '\u{1F629}', '\u{1F62A}', '\u{1F62B}', '\u{1F62C}', '\u{1F62D}', '\u{1F62E}', '\u{1F62F}',
    '\u{1F630}', '\u{1F631}', '\u{1F632}', '\u{1F633}', '\u{1F634}', '\u{1F635}', '\u{1F636}', '\u{1F637}',
    '\u{1F638}', '\u{1F639}', '\u{1F63A}', '\u{1F63B}', '\u{1F63C}', '\u{1F63D}', '\u{1F63E}', '\u{1F63F}',
    '\u{1F640}', '\u{1F641}', '\u{1F642}', '\u{1F643}', '\u{1F644}', '\u{1F645}', '\u{1F646}', '\u{1F647}',
    '\u{1F648}', '\u{1F649}', '\u{1F64A}', '\u{1F64B}', '\u{1F64C}', '\u{1F64D}', '\u{1F64E}', '\u{1F64F}',
    '\u{1F910}', '\u{1F911}', '\u{1F912}', '\u{1F913}', '\u{1F914}', '\u{1F915}', '\u{1F916}', '\u{1F917}',
    '\u{1F918}', '\u{1F919}', '\u{1F91A}', '\u{1F91B}', '\u{1F91C}', '\u{1F91D}', '\u{1F91E}', '\u{1F91F}',
    '\u{1F920}', '\u{1F921}', '\u{1F922}', '\u{1F923}', '\u{1F924}', '\u{1F925}', '\u{1F926}', '\u{1F927}',
    '\u{1F928}', '\u{1F929}', '\u{1F92A}', '\u{1F92B}', '\u{1F92C}', '\u{1F92D}', '\u{1F92E}', '\u{1F92F}',
    '\u{2764}',
];

/// The whole character table in tab order (1697 characters total).
const GROUPS: &[CharGroup] = &[
    CharGroup { key: "arrows", chars: ARROWS_CHARS },
    CharGroup { key: "lines", chars: LINES_CHARS },
    CharGroup { key: "shapes", chars: SHAPES_CHARS },
    CharGroup { key: "math", chars: MATH_CHARS },
    CharGroup { key: "typography", chars: TYPOGRAPHY_CHARS },
    CharGroup { key: "currency", chars: CURRENCY_CHARS },
    CharGroup { key: "music", chars: MUSIC_CHARS },
    CharGroup { key: "technical", chars: TECHNICAL_CHARS },
    CharGroup { key: "game", chars: GAME_CHARS },
    CharGroup { key: "stars_weather", chars: STARS_WEATHER_CHARS },
    CharGroup { key: "emoji", chars: EMOJI_CHARS },
];

/// All character groups, in the order their tabs are shown.
#[must_use]
pub(super) fn groups() -> &'static [CharGroup] {
    GROUPS
}

/// Looks a group up by its stable `key`, or `None` when no group has that key
/// (e.g. a persisted key from a newer build, or the favorites tab's own key).
#[must_use]
pub(super) fn group_by_key(key: &str) -> Option<&'static CharGroup> {
    GROUPS.iter().find(|group| group.key == key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn no_group_is_empty() {
        assert!(!groups().is_empty(), "the table must have groups");
        for group in groups() {
            assert!(
                !group.chars.is_empty(),
                "group {} must not be empty",
                group.key
            );
        }
    }

    #[test]
    fn no_invisible_or_combining_characters() {
        // Filter 2 of the generator: an invisible or combining cell is a cell
        // the user can neither see nor click meaningfully.
        for group in groups() {
            for &ch in group.chars {
                assert!(
                    !ch.is_control(),
                    "control character U+{:04X} in group {}",
                    u32::from(ch),
                    group.key
                );
                assert!(
                    !ch.is_whitespace(),
                    "whitespace character U+{:04X} in group {}",
                    u32::from(ch),
                    group.key
                );
                // The generator drops Cf/Mn/Me; these are the ranges that
                // would have carried them into the curated blocks.
                let cp = u32::from(ch);
                let combining = (0x0300..=0x036F).contains(&cp)
                    || (0x1AB0..=0x1AFF).contains(&cp)
                    || (0x20D0..=0x20FF).contains(&cp)
                    || (0xFE00..=0xFE0F).contains(&cp)
                    || (0x1D165..=0x1D169).contains(&cp)
                    || (0x1D16D..=0x1D182).contains(&cp)
                    || (0x1D185..=0x1D18B).contains(&cp)
                    || (0x1D1AA..=0x1D1AD).contains(&cp);
                assert!(
                    !combining,
                    "combining character U+{cp:04X} in group {}",
                    group.key
                );
                // Format characters (Cf) are neither control nor whitespace in
                // Rust's classification, so guard the two ranges the curated
                // blocks touch explicitly.
                let format_char = (0x200B..=0x200F).contains(&cp)
                    || (0x2028..=0x202E).contains(&cp)
                    || (0x2060..=0x2064).contains(&cp);
                assert!(
                    !format_char,
                    "format character U+{cp:04X} in group {}",
                    group.key
                );
            }
        }
    }

    #[test]
    fn characters_are_unique_across_groups() {
        let mut owner: HashMap<char, &str> = HashMap::new();
        for group in groups() {
            for &ch in group.chars {
                if let Some(previous) = owner.insert(ch, group.key) {
                    panic!(
                        "U+{:04X} appears in both {previous} and {}",
                        u32::from(ch),
                        group.key
                    );
                }
            }
        }
    }

    #[test]
    fn group_lookup_matches_the_table() {
        for group in groups() {
            let found = group_by_key(group.key).map(|found| found.key);
            assert_eq!(found, Some(group.key));
        }
        assert!(group_by_key("favorites").is_none());
    }
}
