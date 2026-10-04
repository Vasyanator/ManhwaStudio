/*
File: crates/ms-ai-api/tests/live_compatible.rs

Purpose:
Opt-in live check of the two compatible services against a real server (llama.cpp, vLLM, ...)
through the crate's public paths: `load_metadata` (model list), `build_client` + `model_iden`
(chat), all with an EMPTY API key. Skips with one line when `MS_AI_API_LIVE_URL` is unset, so
it passes on a fresh clone; writes no file anywhere.

Run:
MS_AI_API_LIVE_URL=http://127.0.0.1:8080 cargo test -p ms-ai-api --test live_compatible -- --nocapture

Notes:
The server must serve one model that accepts images (the image check asks for the colour of an
in-memory solid red PNG). `load_metadata` reads (never writes) the key stored for exactly that
service + base URL in the OS credential store; such a key is then used for the listing only.
Thinking models answer with a reasoning block first, so `max_tokens` is generous and the visible
text must carry no `<think>`.
*/
#![cfg(not(target_arch = "wasm32"))]

use ms_ai_api::client::{block_on, build_client};
use ms_ai_api::genai::chat::{ChatMessage, ChatOptions, ChatRequest, ChatResponse, ContentPart};
use ms_ai_api::model_id::model_iden;
use ms_ai_api::{AiApiService, AiApiTarget, base64_encode, load_metadata};

/// Environment variable naming the server's base URL (e.g. `http://127.0.0.1:8080`).
const URL_ENV: &str = "MS_AI_API_LIVE_URL";
/// Room for a thinking model's reasoning plus the short answer.
const MAX_TOKENS: u32 = 16_384;
/// Side of the generated solid-colour test image, pixels.
const IMAGE_SIDE: u32 = 32;

#[test]
fn compatible_services_list_models_and_chat_without_a_key() {
    let Ok(url) = std::env::var(URL_ENV) else {
        eprintln!("live_compatible: {URL_ENV} is not set; skipping the live compatible-service check");
        return;
    };
    for service in [AiApiService::OpenAiCompatible, AiApiService::AnthropicCompatible] {
        check_service(service, &url);
    }
}

/// Lists the models of `service` at `url`, then sends a text chat and an image chat to the first
/// listed model with an empty key, printing every answer.
fn check_service(service: AiApiService, url: &str) {
    let target = AiApiTarget::new(service, url).unwrap_or_else(|error| panic!("{service:?}: invalid {URL_ENV} {url:?}: {error:?}"));
    let metadata = load_metadata(&target).unwrap_or_else(|error| panic!("{service:?}: model listing failed: {error:?}"));
    println!("[{service:?}] endpoint {:?}, key stored: {}, models: {:?}", target.endpoint(), metadata.key_configured, metadata.models);
    let model = metadata.models.first().unwrap_or_else(|| panic!("{service:?}: the server listed no model")).clone();

    let text_request = ChatRequest::default()
        .with_system("You are a concise assistant.")
        .append_message(ChatMessage::user("Reply with exactly one word: the capital of France."));
    let text = chat(&target, &model, text_request);
    println!("[{service:?}] text answer: {text:?}");

    let image_request = ChatRequest::default().append_message(ChatMessage::user(vec![
        ContentPart::from_text("What colour is this image? Answer with one word."),
        ContentPart::from_binary_base64("image/png", base64_encode(&solid_png(IMAGE_SIDE, [255, 0, 0])), Some("red.png".to_string())),
    ]));
    let colour = chat(&target, &model, image_request);
    println!("[{service:?}] image answer: {colour:?}");
}

/// Sends `request` to `model` at `target` with an EMPTY key and returns the trimmed visible text,
/// asserting it is non-empty and free of inline reasoning.
fn chat(target: &AiApiTarget, model: &str, request: ChatRequest) -> String {
    let service = target.service();
    let client = build_client(target, String::new());
    let iden = model_iden(service, model).unwrap_or_else(|error| panic!("{service:?}: model id {model:?}: {error:?}"));
    let options = ChatOptions::default().with_max_tokens(MAX_TOKENS);
    let response: ChatResponse = block_on(async move { client.exec_chat(iden, request, Some(&options)).await })
        .unwrap_or_else(|error| panic!("{service:?}: runtime: {error}"))
        .unwrap_or_else(|error| panic!("{service:?}: chat failed: {error}"));
    let reasoning_chars = response.reasoning_content.as_deref().map_or(0, |reasoning| reasoning.chars().count());
    println!("[{service:?}] reasoning chars kept out of the text: {reasoning_chars}");
    let text = response.first_text().unwrap_or("").trim().to_string();
    assert!(!text.is_empty(), "{service:?}: empty visible text (reasoning chars: {reasoning_chars})");
    assert!(!text.contains("<think>") && !text.contains("</think>"), "{service:?}: reasoning leaked into the text: {text:?}");
    text
}

/// A `side` x `side` 8-bit RGB PNG filled with `rgb`, encoded in memory with stored (uncompressed)
/// deflate blocks, so the test needs no image crate.
fn solid_png(side: u32, rgb: [u8; 3]) -> Vec<u8> {
    let side_px = usize::try_from(side).unwrap_or_else(|error| panic!("image side {side}: {error}"));
    // Each scanline: filter byte 0 (None) followed by the RGB pixels; every row is identical.
    let row = std::iter::once(0).chain(rgb.iter().copied().cycle().take(side_px * 3)).collect::<Vec<u8>>();
    let raw = row.repeat(side_px);

    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&side.to_be_bytes());
    ihdr.extend_from_slice(&side.to_be_bytes());
    // Bit depth 8, colour type 2 (RGB), compression 0, filter 0, interlace 0.
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);

    let mut png = vec![0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'];
    push_chunk(&mut png, *b"IHDR", &ihdr);
    push_chunk(&mut png, *b"IDAT", &zlib_stored(&raw));
    push_chunk(&mut png, *b"IEND", &[]);
    png
}

/// Appends one PNG chunk: big-endian length, type, data, CRC-32 over type + data.
fn push_chunk(png: &mut Vec<u8>, kind: [u8; 4], data: &[u8]) {
    let len = u32::try_from(data.len()).unwrap_or_else(|error| panic!("chunk too large: {error}"));
    png.extend_from_slice(&len.to_be_bytes());
    let start = png.len();
    png.extend_from_slice(&kind);
    png.extend_from_slice(data);
    let crc = crc32(&png[start..]);
    png.extend_from_slice(&crc.to_be_bytes());
}

/// A zlib stream of `data` in stored deflate blocks (at most 65535 bytes each) with its Adler-32.
fn zlib_stored(data: &[u8]) -> Vec<u8> {
    // CMF 0x78 (deflate, 32K window), FLG 0x01 (check bits; no dictionary, fastest level).
    let mut out = vec![0x78, 0x01];
    let mut blocks = data.chunks(usize::from(u16::MAX)).peekable();
    if blocks.peek().is_none() {
        out.extend_from_slice(&[1, 0, 0, 0xff, 0xff]);
    }
    while let Some(block) = blocks.next() {
        let is_last = blocks.peek().is_none();
        out.push(u8::from(is_last));
        let len = u16::try_from(block.len()).unwrap_or_else(|error| panic!("stored block too large: {error}"));
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(block);
    }
    let (mut a, mut b) = (1_u32, 0_u32);
    for byte in data {
        a = (a + u32::from(*byte)) % 65_521;
        b = (b + a) % 65_521;
    }
    out.extend_from_slice(&((b << 16) | a).to_be_bytes());
    out
}

/// CRC-32 (IEEE, reflected, polynomial 0xEDB88320) as PNG chunks use it.
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xffff_ffff_u32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 { (crc >> 1) ^ 0xedb8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

#[test]
fn crc32_matches_the_png_reference_value() {
    // The CRC of an IEND chunk is fixed by the PNG specification: AE 42 60 82.
    assert_eq!(crc32(b"IEND"), 0xae42_6082);
}
