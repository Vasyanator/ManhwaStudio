/*
File: src/launcher/new_project/quick_download.rs

Purpose:
Background quick-download pipeline for supported chapter URLs in the New Project launcher.

Main responsibilities:
- detect supported hosts and extract chapter image URLs from site-specific pages or APIs;
- download and decode chapter images off the GUI thread;
- stream progress updates back to the launcher UI and convert results into ribbon pages.

Key structures:
- QuickDownloadController
- QuickDownloadEvent
- QuickDownloadSuccess

Notes:
This module mirrors the old Python quick downloader from `modules/downloader.py`, but keeps
all network and image decoding work in a worker thread so the egui window stays responsive.
*/

use crate::launcher::new_project::ribbon::{ImportedImage, RibbonPage, build_ribbon_pages};
use image::DynamicImage;
use rayon::prelude::*;
use serde_json::Value;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use ms_thread as thread;
#[cfg(not(target_arch = "wasm32"))]
use web_time::Duration;

#[cfg(not(target_arch = "wasm32"))]
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const DOWNLOAD_PARALLELISM: usize = 8;
#[cfg(not(target_arch = "wasm32"))]
const DEFAULT_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
     (KHTML, like Gecko) Chrome/115.0.0.0 Safari/537.36";

pub const SUPPORTED_SITES_TOOLTIP: &str = "\
Поддерживаемые сайты:\n\
comic.naver.com\n\
webtoons.com\n\
m.webtoons.com\n\
mangadex.org\n\
natomanga.com\n\
readcomiconline.li\n\
comicfury.com\n\
hecomicseries.com\n\
kuaikanmanhua.com\n\
bato.to";

#[derive(Debug)]
struct PendingQuickDownload {
    rx: Receiver<QuickDownloadWorkerEvent>,
}

pub struct QuickDownloadController {
    pending: Option<PendingQuickDownload>,
}

pub struct QuickDownloadSuccess {
    pub source_url: String,
    pub pages: Vec<RibbonPage>,
    pub downloaded_images: usize,
}

pub enum QuickDownloadEvent {
    Progress {
        stage: String,
        current: usize,
        total: usize,
    },
    Loaded(QuickDownloadSuccess),
    Failed {
        user_message: String,
        log_message: String,
    },
    WorkerDisconnected,
}

enum QuickDownloadWorkerEvent {
    Progress {
        stage: &'static str,
        current: usize,
        total: usize,
    },
    Finished(Result<LoadedQuickDownload, QuickDownloadError>),
}

struct LoadedQuickDownload {
    source_url: String,
    pages: Vec<RibbonPage>,
    downloaded_images: usize,
}

#[derive(Debug)]
struct QuickDownloadError {
    user_message: String,
    log_message: String,
}

struct SiteDownloadPlan {
    image_urls: Vec<String>,
    referer: Option<String>,
}

impl QuickDownloadController {
    pub fn new() -> Self {
        Self { pending: None }
    }

    pub fn is_loading(&self) -> bool {
        self.pending.is_some()
    }

    pub fn begin_download(&mut self, url: String) {
        self.pending = Some(PendingQuickDownload {
            rx: spawn_quick_download(url),
        });
    }

    pub fn poll(&mut self, ctx: &egui::Context) -> Option<QuickDownloadEvent> {
        let pending = self.pending.take()?;
        let mut last_progress = None;
        loop {
            match pending.rx.try_recv() {
                Ok(QuickDownloadWorkerEvent::Progress {
                    stage,
                    current,
                    total,
                }) => {
                    ctx.request_repaint();
                    last_progress = Some(QuickDownloadEvent::Progress {
                        stage: stage.to_string(),
                        current,
                        total,
                    });
                }
                Ok(QuickDownloadWorkerEvent::Finished(result)) => match result {
                    Ok(success) => {
                        ctx.request_repaint();
                        return Some(QuickDownloadEvent::Loaded(QuickDownloadSuccess {
                            source_url: success.source_url,
                            pages: success.pages,
                            downloaded_images: success.downloaded_images,
                        }));
                    }
                    Err(err) => {
                        return Some(QuickDownloadEvent::Failed {
                            user_message: err.user_message,
                            log_message: err.log_message,
                        });
                    }
                },
                Err(mpsc::TryRecvError::Empty) => {
                    self.pending = Some(pending);
                    return last_progress;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    return Some(QuickDownloadEvent::WorkerDisconnected);
                }
            }
        }
    }
}

fn spawn_quick_download(url: String) -> Receiver<QuickDownloadWorkerEvent> {
    let (tx, rx) = mpsc::channel();
    let tx_worker = tx.clone();
    let url_for_thread = url.clone();
    match thread::Builder::new()
        .name("new-project-quick-download".to_string())
        .spawn(move || {
            let result = load_quick_download(&url_for_thread, &tx_worker);
            if tx_worker
                .send(QuickDownloadWorkerEvent::Finished(result))
                .is_err()
            {
                crate::runtime_log::log_warn(
                    "[new-project] failed to send quick download result to UI",
                );
            }
        }) {
        Ok(_) => {}
        Err(err) => {
            crate::runtime_log::log_error(format!(
                "[new-project] failed to spawn quick downloader for '{url}': {err}"
            ));
            if tx
                .send(QuickDownloadWorkerEvent::Finished(Err(
                    QuickDownloadError {
                        user_message: "Не удалось запустить быстрый выкачиватель.".to_string(),
                        log_message: format!("failed to spawn quick downloader for '{url}': {err}"),
                    },
                )))
                .is_err()
            {
                crate::runtime_log::log_warn(
                    "[new-project] failed to deliver quick downloader spawn error",
                );
            }
        }
    }
    rx
}

fn load_quick_download(
    url: &str,
    progress_tx: &Sender<QuickDownloadWorkerEvent>,
) -> Result<LoadedQuickDownload, QuickDownloadError> {
    let normalized = normalize_http_url(url).map_err(|err| QuickDownloadError {
        user_message: "Ссылка для быстрого выкачивателя выглядит некорректной.".to_string(),
        log_message: format!("invalid quick download url '{url}': {err}"),
    })?;
    let plan = build_site_download_plan(&normalized)?;
    if plan.image_urls.is_empty() {
        return Err(QuickDownloadError {
            user_message: "На странице не удалось найти изображения главы.".to_string(),
            log_message: format!("quick downloader found zero images for '{normalized}'"),
        });
    }
    let images = download_images_ordered(&plan, progress_tx)?;
    let pages = build_ribbon_pages(images);
    Ok(LoadedQuickDownload {
        source_url: normalized,
        downloaded_images: pages.len(),
        pages,
    })
}

fn build_site_download_plan(url: &str) -> Result<SiteDownloadPlan, QuickDownloadError> {
    let host = extract_host(url).unwrap_or_default();
    if host.contains("comic.naver.com") {
        return comic_naver_plan(url);
    }
    if host.contains("webtoons.com") || host.contains("m.webtoons.com") {
        return webtoons_plan(url);
    }
    if host.contains("mangadex.org") {
        return mangadex_plan(url);
    }
    if host.contains("natomanga.com") {
        return natomanga_plan(url);
    }
    if host.contains("readcomiconline.li") {
        return readcomiconline_plan(url);
    }
    if host.contains("comicfury.com") || host.ends_with(".thecomicseries.com") {
        return comicfury_plan(url);
    }
    if host.contains("kuaikanmanhua.com") {
        return kuaikan_plan(url);
    }
    if host.contains("bato.to") {
        return bato_plan(url);
    }

    Err(QuickDownloadError {
        user_message: "Этот сайт пока не поддерживается быстрым выкачивателем.".to_string(),
        log_message: format!("unsupported quick download host '{host}' for '{url}'"),
    })
}

fn download_images_ordered(
    plan: &SiteDownloadPlan,
    progress_tx: &Sender<QuickDownloadWorkerEvent>,
) -> Result<Vec<ImportedImage>, QuickDownloadError> {
    let total = plan.image_urls.len();
    let downloaded = Arc::new(AtomicUsize::new(0));
    let referer = plan.referer.clone();
    let progress_tx = progress_tx.clone();

    let mut indexed = plan
        .image_urls
        .par_iter()
        .enumerate()
        .with_max_len(DOWNLOAD_PARALLELISM)
        .map(|(index, url)| {
            let image = download_image(url, referer.as_deref())?;
            let current = downloaded.fetch_add(1, Ordering::Relaxed) + 1;
            let _ = progress_tx.send(QuickDownloadWorkerEvent::Progress {
                stage: "download",
                current,
                total,
            });
            Ok::<(usize, ImportedImage), QuickDownloadError>((
                index,
                ImportedImage {
                    name: format!("{:04}.png", index + 1),
                    image,
                },
            ))
        })
        .collect::<Result<Vec<_>, _>>()?;

    indexed.sort_by_key(|(index, _)| *index);
    Ok(indexed.into_iter().map(|(_, image)| image).collect())
}

fn download_image(url: &str, referer: Option<&str>) -> Result<DynamicImage, QuickDownloadError> {
    let bytes = fetch_bytes(url, referer)?;
    image::load_from_memory(&bytes).map_err(|err| QuickDownloadError {
        user_message: "Не удалось декодировать одну из загруженных картинок.".to_string(),
        log_message: format!("failed to decode downloaded image '{url}': {err}"),
    })
}

fn comic_naver_plan(url: &str) -> Result<SiteDownloadPlan, QuickDownloadError> {
    let title_id = query_param(url, "titleId").ok_or_else(|| QuickDownloadError {
        user_message: "Ссылка Naver должна содержать titleId.".to_string(),
        log_message: format!("naver url '{url}' has no titleId"),
    })?;
    let episode_no = query_param(url, "no").ok_or_else(|| QuickDownloadError {
        user_message: "Ссылка Naver должна содержать номер главы.".to_string(),
        log_message: format!("naver url '{url}' has no no parameter"),
    })?;
    let html = fetch_text(url, None)?;
    let marker = format!("/webtoon/{title_id}/{episode_no}/");
    let mut items = Vec::new();
    for tag in extract_html_tags(&html) {
        if tag.is_end || !tag.name.eq_ignore_ascii_case("img") {
            continue;
        }
        let Some(src) = get_html_attr(tag.attrs, "src") else {
            continue;
        };
        if !src.contains(&marker) {
            continue;
        }
        let normalized = normalize_network_url(src, url);
        let order = naver_image_order(&normalized);
        items.push((order, normalized));
    }
    items.sort_by_key(|(order, _)| *order);
    Ok(SiteDownloadPlan {
        image_urls: dedupe_preserve(items.into_iter().map(|(_, url)| url).collect()),
        referer: None,
    })
}

fn bato_plan(url: &str) -> Result<SiteDownloadPlan, QuickDownloadError> {
    let html = fetch_text(url, None)?;
    let mut image_urls = extract_bato_astro_image_urls(&html);
    if image_urls.is_empty() {
        image_urls = extract_bato_script_image_urls(&html);
    }
    if image_urls.is_empty() {
        return Err(QuickDownloadError {
            user_message: "Не удалось найти изображения главы на bato.to.".to_string(),
            log_message: format!("bato page '{url}' has no image urls"),
        });
    }
    Ok(SiteDownloadPlan {
        image_urls,
        referer: Some("https://bato.to/".to_string()),
    })
}

fn webtoons_plan(url: &str) -> Result<SiteDownloadPlan, QuickDownloadError> {
    let chapter_url = if path_contains(url, "/viewer") {
        url.to_string()
    } else {
        let title_no = query_param(url, "title_no").ok_or_else(|| QuickDownloadError {
            user_message: "Ссылка Webtoons должна содержать title_no.".to_string(),
            log_message: format!("webtoons url '{url}' has no title_no"),
        })?;
        let webtoon_type = if url.contains("/canvas/") || url.contains("/challenge/") {
            "canvas"
        } else {
            "webtoon"
        };
        let api_url = format!(
            "https://m.webtoons.com/api/v1/{webtoon_type}/{title_no}/episodes?pageSize=2000"
        );
        let json = fetch_json_value(&api_url, Some("https://webtoons.com/"))?;
        let episodes = json
            .get("result")
            .and_then(|result| result.get("episodeList"))
            .and_then(Value::as_array)
            .ok_or_else(|| QuickDownloadError {
                user_message: "Не удалось получить список эпизодов Webtoons.".to_string(),
                log_message: format!("webtoons api '{api_url}' returned no episodeList"),
            })?;
        let last_viewer_link = episodes
            .last()
            .and_then(|episode| episode.get("viewerLink"))
            .and_then(Value::as_str)
            .ok_or_else(|| QuickDownloadError {
                user_message: "Не удалось определить эпизод для скачивания.".to_string(),
                log_message: format!("webtoons api '{api_url}' returned malformed viewerLink"),
            })?;
        format!("https://www.webtoons.com{last_viewer_link}")
    };

    let html = fetch_text(&chapter_url, Some("https://webtoons.com/"))?;
    let mut image_urls = Vec::new();
    for tag in extract_html_tags(&html) {
        if tag.is_end || !tag.name.eq_ignore_ascii_case("img") {
            continue;
        }
        let candidate = get_html_attr(tag.attrs, "data-url")
            .or_else(|| get_html_attr(tag.attrs, "data-src"))
            .or_else(|| get_html_attr(tag.attrs, "src"));
        let Some(src) = candidate else {
            continue;
        };
        let normalized = normalize_network_url(src, &chapter_url);
        if normalized.contains("/viewer/") || !looks_like_image_url(&normalized) {
            continue;
        }
        image_urls.push(normalized);
    }
    image_urls = dedupe_preserve(image_urls);
    if image_urls.is_empty() {
        return Err(QuickDownloadError {
            user_message: "Не удалось найти изображения эпизода Webtoons.".to_string(),
            log_message: format!("webtoons chapter '{chapter_url}' has no image urls"),
        });
    }
    Ok(SiteDownloadPlan {
        image_urls,
        referer: Some("https://www.webtoons.com/".to_string()),
    })
}

fn mangadex_plan(url: &str) -> Result<SiteDownloadPlan, QuickDownloadError> {
    let chapter_id = if let Some(id) = path_segment_after(url, "chapter") {
        id
    } else {
        let manga_id = path_segment_after(url, "title").ok_or_else(|| QuickDownloadError {
            user_message: "Ссылка MangaDex должна вести на тайтл или главу.".to_string(),
            log_message: format!("mangadex url '{url}' is neither title nor chapter"),
        })?;
        pick_latest_mangadex_chapter(&manga_id)?
    };

    let api_url = format!("https://api.mangadex.org/at-home/server/{chapter_id}");
    let json = fetch_json_value(&api_url, None)?;
    let base_url =
        json.get("baseUrl")
            .and_then(Value::as_str)
            .ok_or_else(|| QuickDownloadError {
                user_message: "MangaDex не вернул адрес сервера главы.".to_string(),
                log_message: format!("mangadex at-home '{api_url}' has no baseUrl"),
            })?;
    let chapter = json.get("chapter").ok_or_else(|| QuickDownloadError {
        user_message: "MangaDex не вернул данные главы.".to_string(),
        log_message: format!("mangadex at-home '{api_url}' has no chapter field"),
    })?;
    let hash = chapter
        .get("hash")
        .and_then(Value::as_str)
        .ok_or_else(|| QuickDownloadError {
            user_message: "MangaDex не вернул hash главы.".to_string(),
            log_message: format!("mangadex at-home '{api_url}' has no hash"),
        })?;
    let data = chapter
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| QuickDownloadError {
            user_message: "MangaDex не вернул список страниц главы.".to_string(),
            log_message: format!("mangadex at-home '{api_url}' has no chapter.data"),
        })?;
    let image_urls = data
        .iter()
        .filter_map(Value::as_str)
        .map(|name| format!("{base_url}/data/{hash}/{name}"))
        .collect::<Vec<_>>();
    Ok(SiteDownloadPlan {
        image_urls,
        referer: None,
    })
}

fn natomanga_plan(url: &str) -> Result<SiteDownloadPlan, QuickDownloadError> {
    let chapter_url = if path_contains(url, "/chapter") {
        url.to_string()
    } else {
        let html = fetch_text(url, None)?;
        let chapters = collect_anchor_hrefs_containing(&html, url, "/chapter/");
        chapters.last().cloned().ok_or_else(|| QuickDownloadError {
            user_message: "Не удалось получить список глав NatoManga.".to_string(),
            log_message: format!("natomanga series '{url}' has no chapter urls"),
        })?
    };
    let html = fetch_text(&chapter_url, None)?;
    let mut image_urls = Vec::new();
    for tag in extract_html_tags(&html) {
        if tag.is_end || !tag.name.eq_ignore_ascii_case("img") {
            continue;
        }
        let Some(src) = get_html_attr(tag.attrs, "src") else {
            continue;
        };
        if src.starts_with("https://natomanga.com") {
            continue;
        }
        let normalized = normalize_network_url(src, &chapter_url);
        if looks_like_image_url(&normalized) {
            image_urls.push(normalized);
        }
    }
    Ok(SiteDownloadPlan {
        image_urls: dedupe_preserve(image_urls),
        referer: None,
    })
}

fn readcomiconline_plan(url: &str) -> Result<SiteDownloadPlan, QuickDownloadError> {
    let chapter_url = if path_segment_count(url) <= 2 {
        let comic_id = path_segments(url)
            .get(1)
            .cloned()
            .ok_or_else(|| QuickDownloadError {
                user_message: "Ссылка ReadComicOnline выглядит неполной.".to_string(),
                log_message: format!("readcomiconline url '{url}' has not enough segments"),
            })?;
        let list_url = format!("https://readcomiconline.li/Comic/{comic_id}");
        let html = fetch_text(&list_url, None)?;
        let chapters = collect_anchor_hrefs_containing(&html, &list_url, "/Comic/");
        chapters.last().cloned().ok_or_else(|| QuickDownloadError {
            user_message: "Не удалось получить список глав комикса.".to_string(),
            log_message: format!("readcomiconline list '{list_url}' has no chapters"),
        })?
    } else {
        url.to_string()
    };

    let html = fetch_text(&chapter_url, None)?;
    let mut image_urls = Vec::new();
    let mut start = 0usize;
    while let Some(index) = html[start..].find("lstImages.push(") {
        let absolute_index = start + index + "lstImages.push(".len();
        let Some(quote) = html.as_bytes().get(absolute_index).copied() else {
            break;
        };
        if quote != b'"' && quote != b'\'' {
            start = absolute_index;
            continue;
        }
        let value_start = absolute_index + 1;
        let Some(end_offset) = find_quoted_end(&html[value_start..], quote) else {
            break;
        };
        let encoded = &html[value_start..value_start + end_offset];
        image_urls.push(readcomiconline_decode(encoded));
        start = value_start + end_offset + 1;
    }

    if image_urls.is_empty() {
        return Err(QuickDownloadError {
            user_message: "Не удалось найти страницы ReadComicOnline.".to_string(),
            log_message: format!("readcomiconline chapter '{chapter_url}' has no lstImages"),
        });
    }
    Ok(SiteDownloadPlan {
        image_urls,
        referer: None,
    })
}

fn comicfury_plan(url: &str) -> Result<SiteDownloadPlan, QuickDownloadError> {
    let comic_id = comicfury_id(url).ok_or_else(|| QuickDownloadError {
        user_message: "Не удалось определить комикс ComicFury по ссылке.".to_string(),
        log_message: format!("comicfury url '{url}' has no comic id"),
    })?;

    let archive_url = if url.contains("/read/") && url.contains("/comics/") {
        url.to_string()
    } else {
        format!("https://comicfury.com/read/{comic_id}/archive")
    };
    let html = fetch_text(&archive_url, None)?;
    let chapter_url = if archive_url == url {
        archive_url
    } else {
        collect_anchor_hrefs_containing(&html, &archive_url, &format!("/read/{comic_id}/comics/"))
            .last()
            .cloned()
            .ok_or_else(|| QuickDownloadError {
                user_message: "Не удалось получить список глав ComicFury.".to_string(),
                log_message: format!("comicfury archive '{archive_url}' has no chapter urls"),
            })?
    };

    let chapter_html = fetch_text(&chapter_url, None)?;
    let mut image_urls = Vec::new();
    for tag in extract_html_tags(&chapter_html) {
        if tag.is_end || !tag.name.eq_ignore_ascii_case("img") {
            continue;
        }
        let Some(src) = get_html_attr(tag.attrs, "src") else {
            continue;
        };
        let normalized = normalize_network_url(src, &chapter_url);
        if normalized.contains("/comic/") || normalized.contains("/comics/") {
            image_urls.push(normalized);
        }
    }
    image_urls = dedupe_preserve(image_urls);
    if image_urls.is_empty() {
        return Err(QuickDownloadError {
            user_message: "Не удалось найти изображения ComicFury.".to_string(),
            log_message: format!("comicfury chapter '{chapter_url}' has no matching images"),
        });
    }
    Ok(SiteDownloadPlan {
        image_urls,
        referer: None,
    })
}

fn kuaikan_plan(url: &str) -> Result<SiteDownloadPlan, QuickDownloadError> {
    let chapter_url = if url.contains("/web/topic/") {
        let html = fetch_text(url, None)?;
        let mut chapters = collect_https_json_strings(&html);
        chapters.retain(|item| item.contains("/webs/comic-next/"));
        chapters.last().cloned().ok_or_else(|| QuickDownloadError {
            user_message: "Не удалось получить список глав Kuaikan.".to_string(),
            log_message: format!("kuaikan topic '{url}' has no chapter urls"),
        })?
    } else {
        url.to_string()
    };
    let html = fetch_text(&chapter_url, None)?;
    let mut image_urls = collect_https_json_strings(&html);
    image_urls.retain(|item| looks_like_image_url(item));
    image_urls = dedupe_preserve(image_urls);
    if image_urls.is_empty() {
        return Err(QuickDownloadError {
            user_message: "Не удалось найти изображения Kuaikan.".to_string(),
            log_message: format!("kuaikan chapter '{chapter_url}' has no image urls"),
        });
    }
    Ok(SiteDownloadPlan {
        image_urls,
        referer: None,
    })
}

fn pick_latest_mangadex_chapter(manga_id: &str) -> Result<String, QuickDownloadError> {
    for language_filtered in [true, false] {
        let lang_param = if language_filtered {
            "&translatedLanguage[]=en"
        } else {
            ""
        };
        let api_url = format!(
            "https://api.mangadex.org/manga/{manga_id}/feed?limit=1{lang_param}\
             &order[volume]=desc&order[chapter]=desc"
        );
        let json = fetch_json_value(&api_url, None)?;
        if let Some(id) = json
            .get("data")
            .and_then(Value::as_array)
            .and_then(|items| items.first())
            .and_then(|entry| entry.get("id"))
            .and_then(Value::as_str)
        {
            return Ok(id.to_string());
        }
    }
    Err(QuickDownloadError {
        user_message: "Не удалось найти главы MangaDex для этого тайтла.".to_string(),
        log_message: format!("mangadex manga '{manga_id}' has no chapters"),
    })
}

#[cfg(not(target_arch = "wasm32"))]
fn fetch_text(url: &str, referer: Option<&str>) -> Result<String, QuickDownloadError> {
    let response = make_request(url, referer)?
        .into_string()
        .map_err(|err| QuickDownloadError {
            user_message: "Не удалось прочитать ответ сайта.".to_string(),
            log_message: format!("failed to read text response from '{url}': {err}"),
        })?;
    Ok(response)
}

/// Web stub: the direct downloader uses a native HTTP client (`ureq`) that is not
/// compiled for wasm. Returns a clear error instead of a fake response.
#[cfg(target_arch = "wasm32")]
fn fetch_text(_url: &str, _referer: Option<&str>) -> Result<String, QuickDownloadError> {
    Err(QuickDownloadError {
        user_message: "Загрузка глав недоступна в веб-версии.".to_string(),
        log_message: "quick download HTTP client is not available on the web build".to_string(),
    })
}

#[cfg(not(target_arch = "wasm32"))]
fn fetch_bytes(url: &str, referer: Option<&str>) -> Result<Vec<u8>, QuickDownloadError> {
    let response = make_request(url, referer)?;
    let mut reader = response.into_reader();
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut reader, &mut bytes).map_err(|err| QuickDownloadError {
        user_message: "Не удалось скачать одну из страниц.".to_string(),
        log_message: format!("failed to read binary response from '{url}': {err}"),
    })?;
    Ok(bytes)
}

/// Web stub twin of `fetch_bytes`; see `fetch_text`.
#[cfg(target_arch = "wasm32")]
fn fetch_bytes(_url: &str, _referer: Option<&str>) -> Result<Vec<u8>, QuickDownloadError> {
    Err(QuickDownloadError {
        user_message: "Загрузка глав недоступна в веб-версии.".to_string(),
        log_message: "quick download HTTP client is not available on the web build".to_string(),
    })
}

fn fetch_json_value(url: &str, referer: Option<&str>) -> Result<Value, QuickDownloadError> {
    let text = fetch_text(url, referer)?;
    serde_json::from_str::<Value>(&text).map_err(|err| QuickDownloadError {
        user_message: "Сайт вернул неожиданный JSON-ответ.".to_string(),
        log_message: format!("failed to parse json from '{url}': {err}; body={text}"),
    })
}

#[cfg(not(target_arch = "wasm32"))]
fn make_request(url: &str, referer: Option<&str>) -> Result<ureq::Response, QuickDownloadError> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(REQUEST_TIMEOUT)
        .timeout_read(REQUEST_TIMEOUT)
        .timeout_write(REQUEST_TIMEOUT)
        .build();
    let mut request = agent.get(url).set("User-Agent", DEFAULT_USER_AGENT);
    if let Some(referer) = referer {
        request = request.set("Referer", referer);
    }
    request.call().map_err(|err| match err {
        ureq::Error::Status(status, response) => {
            let body = response.into_string().unwrap_or_default();
            QuickDownloadError {
                user_message: format!("Сайт вернул ошибку {status} при загрузке главы."),
                log_message: format!("request '{url}' failed with status {status}; body={body}"),
            }
        }
        ureq::Error::Transport(transport) => QuickDownloadError {
            user_message: "Не удалось подключиться к сайту для загрузки главы.".to_string(),
            log_message: format!("request '{url}' failed: {transport}"),
        },
    })
}

fn normalize_http_url(url: &str) -> Result<String, String> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return Err("empty url".to_string());
    }
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        return Ok(trimmed.to_string());
    }
    if looks_like_host(trimmed) {
        return Ok(format!("https://{trimmed}"));
    }
    Err("missing http/https scheme or host".to_string())
}

fn looks_like_host(value: &str) -> bool {
    value.starts_with("www.") || value.contains('.')
}

fn extract_host(url: &str) -> Option<String> {
    let (_, rest) = url.split_once("://")?;
    let host = rest.split('/').next().unwrap_or_default();
    let host = host.split('@').next_back().unwrap_or(host);
    Some(host.split(':').next().unwrap_or(host).to_ascii_lowercase())
}

fn query_param(url: &str, key: &str) -> Option<String> {
    let query = url.split_once('?')?.1.split('#').next().unwrap_or_default();
    for pair in query.split('&') {
        let mut parts = pair.splitn(2, '=');
        let name = parts.next().unwrap_or_default();
        let value = parts.next().unwrap_or_default();
        if name == key {
            return Some(percent_decode(value));
        }
    }
    None
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
        {
            output.push((hi << 4) | lo);
            index += 3;
            continue;
        }
        output.push(if bytes[index] == b'+' {
            b' '
        } else {
            bytes[index]
        });
        index += 1;
    }
    String::from_utf8_lossy(&output).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn normalize_network_url(url: &str, base: &str) -> String {
    if url.starts_with("http://") || url.starts_with("https://") {
        return url.to_string();
    }
    if url.starts_with("//") {
        let scheme = if base.starts_with("http://") {
            "http:"
        } else {
            "https:"
        };
        return format!("{scheme}{url}");
    }
    let origin = origin_from_url(base);
    if url.starts_with('/') {
        return format!("{origin}{url}");
    }
    let base_dir = base.rsplit_once('/').map(|(left, _)| left).unwrap_or(base);
    format!("{base_dir}/{url}")
}

fn origin_from_url(url: &str) -> String {
    if let Some((scheme, rest)) = url.split_once("://") {
        let host = rest.split('/').next().unwrap_or_default();
        format!("{scheme}://{host}")
    } else {
        String::new()
    }
}

fn path_contains(url: &str, needle: &str) -> bool {
    let path = url
        .split_once("://")
        .map(|(_, rest)| {
            rest.split_once('/')
                .map(|(_, path)| path)
                .unwrap_or_default()
        })
        .unwrap_or_default();
    path.contains(needle)
}

fn path_segments(url: &str) -> Vec<String> {
    url.split_once("://")
        .map(|(_, rest)| {
            rest.split_once('/')
                .map(|(_, path)| path)
                .unwrap_or_default()
        })
        .unwrap_or_default()
        .split('?')
        .next()
        .unwrap_or_default()
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(str::to_string)
        .collect()
}

fn path_segment_after(url: &str, key: &str) -> Option<String> {
    let segments = path_segments(url);
    let index = segments.iter().position(|segment| segment == key)?;
    segments.get(index + 1).cloned()
}

fn path_segment_count(url: &str) -> usize {
    path_segments(url).len()
}

fn dedupe_preserve(values: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for value in values {
        if seen.insert(value.clone()) {
            out.push(value);
        }
    }
    out
}

fn looks_like_image_url(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    [".jpg", ".jpeg", ".png", ".webp", ".bmp", ".gif"]
        .iter()
        .any(|ext| lower.contains(ext))
}

fn collect_anchor_hrefs_containing(html: &str, base_url: &str, needle: &str) -> Vec<String> {
    let mut urls = Vec::new();
    for tag in extract_html_tags(html) {
        if tag.is_end || !tag.name.eq_ignore_ascii_case("a") {
            continue;
        }
        if let Some(href) = get_html_attr(tag.attrs, "href") {
            let normalized = normalize_network_url(href, base_url);
            if normalized.contains(needle) {
                urls.push(normalized);
            }
        }
    }
    dedupe_preserve(urls)
}

fn collect_https_json_strings(text: &str) -> Vec<String> {
    let mut urls = Vec::new();
    let bytes = text.as_bytes();
    let mut index = 0usize;
    while index + 8 < bytes.len() {
        let remaining = &text[index..];
        let next_http = remaining
            .find("https://")
            .or_else(|| remaining.find("http://"));
        let Some(offset) = next_http else {
            break;
        };
        let start = index + offset;
        let mut end = start;
        while end < bytes.len() {
            let byte = bytes[end];
            if byte == b'"'
                || byte == b'\''
                || byte == b','
                || byte == b']'
                || byte.is_ascii_whitespace()
            {
                break;
            }
            end += 1;
        }
        urls.push(text[start..end].to_string());
        index = end + 1;
    }
    urls
}

fn extract_bato_astro_image_urls(html: &str) -> Vec<String> {
    for tag in extract_html_tags(html) {
        if tag.is_end || !tag.name.eq_ignore_ascii_case("astro-island") {
            continue;
        }
        let Some(component_url) = get_html_attr(tag.attrs, "component-url") else {
            continue;
        };
        if !component_url.starts_with("/_astro/ImageList") {
            continue;
        }
        let Some(props_raw) = get_html_attr(tag.attrs, "props") else {
            continue;
        };
        let props = html_unescape(props_raw);
        let Ok(json) = serde_json::from_str::<Value>(&props) else {
            continue;
        };
        let Some(image_files) = json.get("imageFiles").and_then(Value::as_array) else {
            continue;
        };
        let Some(second) = image_files.get(1).and_then(Value::as_str) else {
            continue;
        };
        let Ok(entries) = serde_json::from_str::<Value>(second) else {
            continue;
        };
        let Some(entries) = entries.as_array() else {
            continue;
        };
        let urls = entries
            .iter()
            .filter_map(Value::as_array)
            .filter_map(|entry| entry.get(1))
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect::<Vec<_>>();
        if !urls.is_empty() {
            return urls;
        }
    }
    Vec::new()
}

fn extract_bato_script_image_urls(html: &str) -> Vec<String> {
    let marker = "imgHttps";
    let Some(marker_index) = html.find(marker) else {
        return Vec::new();
    };
    let remainder = &html[marker_index..];
    let Some(open) = remainder.find('[') else {
        return Vec::new();
    };
    let Some(close) = find_matching_square_bracket(&remainder[open..]) else {
        return Vec::new();
    };
    let payload = &remainder[open..open + close + 1];
    serde_json::from_str::<Vec<String>>(payload).unwrap_or_default()
}

fn find_matching_square_bracket(text: &str) -> Option<usize> {
    let mut depth = 0usize;
    let mut in_string = false;
    let mut quote = b'"';
    let bytes = text.as_bytes();
    let mut index = 0usize;
    while index < bytes.len() {
        let byte = bytes[index];
        if in_string {
            if byte == b'\\' {
                index += 2;
                continue;
            }
            if byte == quote {
                in_string = false;
            }
            index += 1;
            continue;
        }
        match byte {
            b'"' | b'\'' => {
                in_string = true;
                quote = byte;
            }
            b'[' => depth += 1,
            b']' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
        index += 1;
    }
    None
}

fn html_unescape(value: &str) -> String {
    value
        .replace("&quot;", "\"")
        .replace("&#34;", "\"")
        .replace("&#x27;", "'")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
}

fn naver_image_order(url: &str) -> (u32, u32) {
    let file_name = url.rsplit('/').next().unwrap_or_default();
    let name = file_name.split('.').next().unwrap_or_default();
    let digits = name
        .rsplit_once("IMAG")
        .map(|(_, right)| right)
        .unwrap_or_default();
    let mut parts = digits.split('_');
    let first = parts
        .next()
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(0);
    let second = parts
        .next()
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(0);
    (first, second)
}

fn comicfury_id(url: &str) -> Option<String> {
    let host = extract_host(url)?;
    if host == "comicfury.com" {
        if let Some(value) = query_param(url, "url") {
            return Some(value);
        }
        let segments = path_segments(url);
        if segments.first().map(String::as_str) == Some("read") {
            return segments.get(1).cloned();
        }
    }
    if host.ends_with(".thecomicseries.com") {
        return host.split('.').next().map(str::to_string);
    }
    None
}

fn readcomiconline_decode(url: &str) -> String {
    let url = url.replace("_x236", "d").replace("_x945", "g");
    if url.starts_with("https") {
        return url;
    }

    let (main, suffix) = url
        .split_once('?')
        .map_or((url.as_str(), ""), |(a, b)| (a, b));
    let contains_s0 = main.contains("=s0");
    let trimmed = if contains_s0 {
        main.get(..main.len().saturating_sub(3)).unwrap_or_default()
    } else {
        main.get(..main.len().saturating_sub(6)).unwrap_or_default()
    };
    let stage1 = format!(
        "{}{}",
        trimmed.get(4..22).unwrap_or_default(),
        trimmed.get(25..).unwrap_or_default()
    );
    let stage2 = format!(
        "{}{}",
        stage1
            .get(..stage1.len().saturating_sub(6))
            .unwrap_or_default(),
        stage1
            .get(stage1.len().saturating_sub(2)..)
            .unwrap_or_default()
    );
    let decoded = base64_decode(&stage2)
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default();
    let stage3 = format!(
        "{}{}",
        decoded.get(..13).unwrap_or_default(),
        decoded.get(17..).unwrap_or_default()
    );
    let suffix_param = if contains_s0 { "=s0" } else { "=s1600" };
    let final_url = format!(
        "{}{}",
        stage3
            .get(..stage3.len().saturating_sub(2))
            .unwrap_or_default(),
        suffix_param
    );
    if suffix.is_empty() {
        format!("https://2.bp.blogspot.com/{final_url}")
    } else {
        format!("https://2.bp.blogspot.com/{final_url}?{suffix}")
    }
}

fn base64_decode(input: &str) -> Option<Vec<u8>> {
    let mut buffer = 0u32;
    let mut bits = 0u32;
    let mut output = Vec::new();
    for byte in input.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            _ => continue,
        } as u32;
        buffer = (buffer << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            output.push(((buffer >> bits) & 0xFF) as u8);
        }
    }
    Some(output)
}

fn find_quoted_end(text: &str, quote: u8) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] == b'\\' {
            index += 2;
            continue;
        }
        if bytes[index] == quote {
            return Some(index);
        }
        index += 1;
    }
    None
}

struct HtmlTag<'a> {
    name: &'a str,
    attrs: &'a str,
    is_end: bool,
}

fn extract_html_tags(html: &str) -> Vec<HtmlTag<'_>> {
    let mut tags = Vec::new();
    let mut cursor = 0usize;
    while let Some(start_offset) = html[cursor..].find('<') {
        let start = cursor + start_offset;
        let Some(end_offset) = html[start..].find('>') else {
            break;
        };
        let end = start + end_offset;
        let raw = &html[start + 1..end];
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.starts_with('!') || trimmed.starts_with('?') {
            cursor = end + 1;
            continue;
        }
        let is_end = trimmed.starts_with('/');
        let content = if is_end {
            trimmed[1..].trim_start()
        } else {
            trimmed
        };
        let mut parts = content.splitn(2, char::is_whitespace);
        let name = parts.next().unwrap_or_default();
        let attrs = parts.next().unwrap_or_default();
        if !name.is_empty() {
            tags.push(HtmlTag {
                name,
                attrs,
                is_end,
            });
        }
        cursor = end + 1;
    }
    tags
}

fn get_html_attr<'a>(attrs: &'a str, attr_name: &str) -> Option<&'a str> {
    let bytes = attrs.as_bytes();
    let mut index = 0usize;
    while index < bytes.len() {
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        if index >= bytes.len() {
            break;
        }
        let name_start = index;
        while index < bytes.len() && !bytes[index].is_ascii_whitespace() && bytes[index] != b'=' {
            index += 1;
        }
        let name = &attrs[name_start..index];
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        if index >= bytes.len() || bytes[index] != b'=' {
            while index < bytes.len() && !bytes[index].is_ascii_whitespace() {
                index += 1;
            }
            continue;
        }
        index += 1;
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        let value = if index < bytes.len() && (bytes[index] == b'"' || bytes[index] == b'\'') {
            let quote = bytes[index];
            index += 1;
            let start = index;
            while index < bytes.len() && bytes[index] != quote {
                index += 1;
            }
            let value = &attrs[start..index];
            if index < bytes.len() {
                index += 1;
            }
            value
        } else {
            let start = index;
            while index < bytes.len() && !bytes[index].is_ascii_whitespace() {
                index += 1;
            }
            &attrs[start..index]
        };
        if name.eq_ignore_ascii_case(attr_name) {
            return Some(value);
        }
    }
    None
}
