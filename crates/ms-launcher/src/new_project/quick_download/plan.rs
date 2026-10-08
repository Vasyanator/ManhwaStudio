/*
File: crates/ms-launcher/src/new_project/quick_download/plan.rs

Purpose:
Shared result/error types of the quick downloader and the host dispatch chain that maps a
normalized chapter URL to the site module able to build a download plan for it.

Key structures:
- SiteDownloadPlan
- PlannedImage - one page: primary URL, ordered fallback URLs, optional expected sha256
- QuickDownloadError

Key functions:
- build_site_download_plan()
- sha256_from_hex(), digest_hex() - the hex encoding of `PlannedImage::sha256`

Notes:
This file owns the ONLY host-name switch in the module. Every arm delegates to one file in
`sites/`; adding a site means adding a file there plus one arm here.
*/

use super::sites;
use super::url_util::extract_host;

/// Failure of a quick download step, carrying both a localized user-facing message
/// and a detailed technical message for the runtime log.
#[derive(Debug)]
pub(crate) struct QuickDownloadError {
    pub(crate) user_message: String,
    pub(crate) log_message: String,
}

/// Everything a site module resolved for one chapter: the pages in reading order and the
/// optional `Referer` header the site's CDN requires for every URL of those pages.
pub(crate) struct SiteDownloadPlan {
    pub(crate) images: Vec<PlannedImage>,
    pub(crate) referer: Option<String>,
}

/// One page of a plan, site-agnostic: where to fetch it and how to verify it.
///
/// The controller tries `url` first, then each entry of `fallbacks` in order (the same image
/// on another origin); a page fails only when every URL failed. `sha256`, when present, is the
/// digest the downloaded bytes must have — a mismatch counts as a failure of that URL, never as
/// a page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlannedImage {
    pub(crate) url: String,
    pub(crate) fallbacks: Vec<String>,
    pub(crate) sha256: Option<[u8; 32]>,
}

impl PlannedImage {
    /// A page with a single URL, no fallback and no expected digest — what every site without
    /// a mirror origin produces.
    pub(crate) fn single(url: String) -> Self {
        Self {
            url,
            fallbacks: Vec::new(),
            sha256: None,
        }
    }

    /// Wraps an ordered URL list into single-URL pages, preserving the order.
    pub(crate) fn from_urls(urls: Vec<String>) -> Vec<Self> {
        urls.into_iter().map(Self::single).collect()
    }

    /// The URLs to try for this page, primary first.
    pub(crate) fn candidate_urls(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.url.as_str()).chain(self.fallbacks.iter().map(String::as_str))
    }
}

/// Parses exactly 64 hexadecimal digits (either case) into a SHA-256 digest, the encoding of
/// `PlannedImage::sha256` in site responses. Returns `None` for any other length or character.
pub(crate) fn sha256_from_hex(text: &str) -> Option<[u8; 32]> {
    let digits = text.as_bytes();
    if digits.len() != 64 {
        return None;
    }
    let mut digest = [0_u8; 32];
    for (byte, pair) in digest.iter_mut().zip(digits.chunks_exact(2)) {
        let high = char::from(pair[0]).to_digit(16)?;
        let low = char::from(pair[1]).to_digit(16)?;
        *byte = u8::try_from(high * 16 + low).ok()?;
    }
    Some(digest)
}

/// Lower-case hexadecimal rendering of a digest, for log messages.
pub(crate) fn digest_hex(digest: &[u8]) -> String {
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Dispatches a normalized chapter/series URL to the matching site module.
///
/// # Errors
/// Returns `QuickDownloadError` if the host is not supported, or whatever error the
/// selected site module produced while resolving the chapter.
pub(crate) fn build_site_download_plan(url: &str) -> Result<SiteDownloadPlan, QuickDownloadError> {
    let host = extract_host(url).unwrap_or_default();
    if host.contains("comic.naver.com") {
        return sites::naver::comic_naver_plan(url);
    }
    if host.contains("webtoons.com") || host.contains("m.webtoons.com") {
        return sites::webtoons::webtoons_plan(url);
    }
    if host.contains("mangadex.org") {
        return sites::mangadex::mangadex_plan(url);
    }
    if host.contains("readcomiconline.li") {
        return sites::readcomiconline::readcomiconline_plan(url);
    }
    if host.contains("comicfury.com") || host.ends_with(".thecomicseries.com") {
        return sites::comicfury::comicfury_plan(url);
    }
    if host.contains("kuaikanmanhua.com") {
        return sites::kuaikan::kuaikan_plan(url);
    }
    if host.contains("bato.to") {
        return sites::bato::bato_plan(url);
    }
    // One module serves the whole mirror family; natomanga.com is one of its four hosts.
    if host.contains("nelomanga.net")
        || host.contains("natomanga.com")
        || host.contains("manganato.gg")
        || host.contains("mangakakalot.gg")
    {
        return sites::manganelo::manganelo_plan(url);
    }
    if host.contains("mangapark")
        || host.contains("comicpark")
        || host.contains("readpark")
        || host.contains("parkmanga")
        || host.contains("mpark.to")
    {
        return sites::mangapark::mangapark_plan(url);
    }
    if host.contains("weebdex.org") {
        return sites::weebdex::weebdex_plan(url);
    }
    if host.contains("mangataro.org") {
        return sites::mangataro::mangataro_plan(url);
    }
    if host.contains("danke.moe") {
        return sites::dankefuerslesen::dankefuerslesen_plan(url);
    }
    if host.contains("dynasty-scans.com") {
        return sites::dynastyscans::dynastyscans_plan(url);
    }
    if host.contains("kaliscan.me") {
        return sites::kaliscan::kaliscan_plan(url);
    }
    if host.contains("hentai2read.com") {
        return sites::hentai2read::hentai2read_plan(url);
    }
    if host.contains("tcbscans")
        || host.contains("onepiecechapters")
        || host.contains("tcb-backup")
    {
        return sites::tcbscans::tcbscans_plan(url);
    }
    if host.contains("rawkuma.") {
        return sites::rawkuma::rawkuma_plan(url);
    }
    if host.contains("mangafreak.") {
        return sites::mangafreak::mangafreak_plan(url);
    }
    if host.contains("dandadan.net") {
        return sites::dandadan::dandadan_plan(url);
    }
    if host.contains("hiperdex") || host.contains("hipertoon") {
        return sites::hiperdex::hiperdex_plan(url);
    }
    if host.contains("komikcast") {
        return sites::komikcast::komikcast_plan(url);
    }
    if host.contains("mangaread.org") {
        return sites::mangaread::mangaread_plan(url);
    }
    if host.contains("senmanga.com") {
        return sites::senmanga::senmanga_plan(url);
    }
    if host.contains("weebcentral.com") {
        return sites::weebcentral::weebcentral_plan(url);
    }

    Err(QuickDownloadError {
        user_message: t!("launcher.new_project.quick_dl.site_unsupported_error").to_string(),
        log_message: format!("unsupported quick download host '{host}' for '{url}'"),
    })
}

#[cfg(test)]
mod tests {
    use super::{PlannedImage, digest_hex, sha256_from_hex};

    const HEX: &str = "0123456789abcdef0123456789ABCDEF0123456789abcdef0123456789abcdef";

    #[test]
    fn sha256_from_hex_round_trips_through_digest_hex() {
        let digest = sha256_from_hex(HEX).expect("64 hex digits parse");
        assert_eq!(digest[0], 0x01);
        assert_eq!(digest[15], 0xef);
        assert_eq!(digest_hex(&digest), HEX.to_ascii_lowercase());
    }

    #[test]
    fn sha256_from_hex_rejects_wrong_length_and_non_hex() {
        assert_eq!(sha256_from_hex(&HEX[..62]), None);
        assert_eq!(sha256_from_hex(&format!("{HEX}00")), None);
        assert_eq!(sha256_from_hex(&HEX.replacen('a', "g", 1)), None);
        assert_eq!(sha256_from_hex(""), None);
    }

    #[test]
    fn planned_image_candidates_start_with_the_primary_url() {
        let single = PlannedImage::single("https://a/1.png".to_string());
        assert_eq!(single.candidate_urls().collect::<Vec<_>>(), ["https://a/1.png"]);
        let with_fallbacks = PlannedImage {
            url: "https://a/1.png".to_string(),
            fallbacks: vec!["https://b/1.png".to_string(), "https://c/1.png".to_string()],
            sha256: None,
        };
        assert_eq!(
            with_fallbacks.candidate_urls().collect::<Vec<_>>(),
            ["https://a/1.png", "https://b/1.png", "https://c/1.png"]
        );
        assert_eq!(
            PlannedImage::from_urls(vec!["x".to_string(), "y".to_string()]),
            [PlannedImage::single("x".to_string()), PlannedImage::single("y".to_string())]
        );
    }
}
