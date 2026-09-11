/*
File: src/launcher/new_project/quick_download/plan.rs

Purpose:
Shared result/error types of the quick downloader and the host dispatch chain that maps a
normalized chapter URL to the site module able to build a download plan for it.

Key structures:
- SiteDownloadPlan
- QuickDownloadError

Key functions:
- build_site_download_plan()

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

/// Everything a site module resolved for one chapter: the ordered image URLs and the
/// optional `Referer` header the site's CDN requires for those URLs.
pub(crate) struct SiteDownloadPlan {
    pub(crate) image_urls: Vec<String>,
    pub(crate) referer: Option<String>,
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
