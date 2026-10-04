/*
File: crates/ms-ai-api/tests/live_image_edit.rs

Purpose:
Opt-in live check of one cloud image-edit provider through the public run path
(`image_edit::run_image_edit`: prepare -> adapter -> native executor -> finish), asserting the
size-exact guarantee end to end: the finished image is exactly the source size and the provider
answered exactly the sent size `(k * W, k * H)`. Skips with one line unless
`MS_IMAGE_EDIT_LIVE_PROVIDER`, `MS_IMAGE_EDIT_LIVE_KEY` and `MS_IMAGE_EDIT_LIVE_MODEL` are set,
so it passes on a fresh clone. A real run is a PAID request.

Run:
MS_IMAGE_EDIT_LIVE_PROVIDER=openai MS_IMAGE_EDIT_LIVE_KEY=sk-... MS_IMAGE_EDIT_LIVE_MODEL=gpt-image-2 \
  cargo test -p ms-ai-api --test live_image_edit -- --nocapture

Optional variables:
- MS_IMAGE_EDIT_LIVE_BASE_URL : base URL of the user's own server (provider `openai_compatible`).
- MS_IMAGE_EDIT_LIVE_REGION   : region id of a provider with a region list.
- MS_IMAGE_EDIT_LIVE_SIZE     : source size `WxH` (default: the offer's first table size, else
                                1024x1024); it must satisfy the offer's rule at the chosen `k`.
- MS_IMAGE_EDIT_LIVE_UPSCALE  : the integer upscale `k` (default 1).
- MS_IMAGE_EDIT_LIVE_KEEP     : when set, the finished PNG is written under the system temp
                                directory (path printed); otherwise no file is written anywhere.

Notes:
The key comes from the environment only: the OS credential store is never read or written.
The key is never printed.
*/
#![cfg(not(target_arch = "wasm32"))]

use ms_ai_api::image_edit::codec::encode_rgb_png;
use ms_ai_api::image_edit::{CancelFlag, EndpointChoice, ImageEditProvider, ImageEditRequest, MaskBlend, RgbaRegion, lookup, run_image_edit};

const PROVIDER_ENV: &str = "MS_IMAGE_EDIT_LIVE_PROVIDER";
const KEY_ENV: &str = "MS_IMAGE_EDIT_LIVE_KEY";
const MODEL_ENV: &str = "MS_IMAGE_EDIT_LIVE_MODEL";
const BASE_URL_ENV: &str = "MS_IMAGE_EDIT_LIVE_BASE_URL";
const REGION_ENV: &str = "MS_IMAGE_EDIT_LIVE_REGION";
const SIZE_ENV: &str = "MS_IMAGE_EDIT_LIVE_SIZE";
const UPSCALE_ENV: &str = "MS_IMAGE_EDIT_LIVE_UPSCALE";
const KEEP_ENV: &str = "MS_IMAGE_EDIT_LIVE_KEEP";
/// Default source size for offers without a size table.
const DEFAULT_SIDE: u32 = 1024;

#[test]
fn live_provider_returns_exactly_the_source_size() {
    let (Ok(provider_key), Ok(key), Ok(model)) = (std::env::var(PROVIDER_ENV), std::env::var(KEY_ENV), std::env::var(MODEL_ENV)) else {
        eprintln!("live_image_edit: {PROVIDER_ENV} / {KEY_ENV} / {MODEL_ENV} are not all set; skipping the live image-edit check");
        return;
    };
    let provider = ImageEditProvider::from_key(&provider_key).unwrap_or_else(|| panic!("unknown provider id {provider_key:?}"));
    let offer = lookup(provider, &model).unwrap_or_else(|error| panic!("{error:?}"));
    let endpoint = match (std::env::var(BASE_URL_ENV), std::env::var(REGION_ENV)) {
        (Ok(url), _) => EndpointChoice::BaseUrl(url),
        (Err(_), Ok(region)) => EndpointChoice::Region(provider.region(&region).unwrap_or_else(|| panic!("unknown region {region:?}")).id),
        (Err(_), Err(_)) => EndpointChoice::Default,
    };
    let upscale: u8 = std::env::var(UPSCALE_ENV).map_or(1, |raw| raw.trim().parse().unwrap_or_else(|error| panic!("{UPSCALE_ENV}={raw:?}: {error}")));
    let (width, height) = match std::env::var(SIZE_ENV) {
        Ok(raw) => parse_size(&raw),
        Err(_) => offer.rule.sizes.first().map_or((DEFAULT_SIDE, DEFAULT_SIDE), |&(w, h)| (w / u32::from(upscale), h / u32::from(upscale))),
    };
    let (image, mask) = test_scene(width, height);
    let request = ImageEditRequest { provider, model_id: model.clone(), endpoint, prompt: "Fill the white square with a solid red circle. Keep everything else unchanged.".to_string(), image, mask: Some(mask), blend: MaskBlend::default(), upscale };
    println!("live_image_edit: {provider_key} / {model}: {width}x{height}, k={upscale}");
    let outcome = run_image_edit(&request, &key, &CancelFlag::new(), |stage| println!("live_image_edit: stage {stage:?}")).unwrap_or_else(|error| panic!("run failed: {error:?} ({error})"));
    assert_eq!((outcome.image.width(), outcome.image.height()), (width, height), "the finished image must be the source size");
    assert_eq!(outcome.sent_size, (width * u32::from(upscale), height * u32::from(upscale)), "the provider must answer the sent size");
    assert!(outcome.image.pixels().chunks_exact(4).all(|pixel| pixel[3] == 255));
    println!("live_image_edit: ok, sent and returned {:?}", outcome.sent_size);
    if std::env::var_os(KEEP_ENV).is_some() {
        let rgb: Vec<u8> = outcome.image.pixels().chunks_exact(4).flat_map(|pixel| [pixel[0], pixel[1], pixel[2]]).collect();
        let png = encode_rgb_png(&rgb, width, height).unwrap_or_else(|error| panic!("{error:?}"));
        let path = std::env::temp_dir().join(format!("ms_image_edit_live_{}.png", std::process::id()));
        std::fs::write(&path, png).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        println!("live_image_edit: kept {}", path.display());
    }
}

/// Parses `WxH`.
fn parse_size(raw: &str) -> (u32, u32) {
    let parsed = raw.trim().split_once(['x', 'X']).and_then(|(w, h)| Some((w.trim().parse().ok()?, h.trim().parse().ok()?)));
    parsed.unwrap_or_else(|| panic!("{SIZE_ENV}={raw:?} is not WxH"))
}

/// A light-grey page with a white square in the middle, and the mask of that square.
fn test_scene(width: u32, height: u32) -> (RgbaRegion, Vec<u8>) {
    let (x0, x1, y0, y1) = (width / 3, width * 2 / 3, height / 3, height * 2 / 3);
    let inside = |x: u32, y: u32| (x0..x1).contains(&x) && (y0..y1).contains(&y);
    let mut pixels = Vec::new();
    let mut mask = Vec::new();
    for y in 0..height {
        for x in 0..width {
            let value = if inside(x, y) { 255 } else { 200 };
            pixels.extend_from_slice(&[value, value, value, 255]);
            mask.push(if inside(x, y) { 255 } else { 0 });
        }
    }
    let image = RgbaRegion::new(width, height, pixels).unwrap_or_else(|error| panic!("{error:?}"));
    (image, mask)
}
