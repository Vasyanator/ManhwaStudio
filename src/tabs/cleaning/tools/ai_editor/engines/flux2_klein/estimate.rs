/*
File: cleaning/tools/ai_editor/engines/flux2_klein/estimate.rs

Purpose:
The backend's RAM/VRAM forecast for the FLUX.2 klein engine: the `.estimate` answer, the
phase peaks its breakdown carries, and the two strings the panel renders from them.

Main responsibilities:
- own `Flux2Estimate` and the `peak_*` breakdown keys the backend spells its phases in;
- fetch and parse the forecast;
- turn a forecast into the one status line the panel shows and into its hover
  (`estimate_status_line`, `estimate_tooltip`), leading with the phase peaks
  (`split_estimate_peaks`).

Key structures:
- `Flux2Estimate`, `Flux2EstimatePeaks`

Key functions:
- `fetch_flux2_estimate()`, `parse_flux2_estimate()`
- `estimate_status_line()`, `estimate_tooltip()`, `split_estimate_peaks()`

Notes:
`draw_estimate_ui` — the drawing of these two strings — lives in `ui/components.rs`. The
forecast is re-armed by a settled REGION SIZE change or a settings change, never by a
move of the frame.
*/

use super::*;

/// Breakdown key of the denoising-loop peak in an `.estimate` answer.
pub(super) const FLUX2_BREAKDOWN_PEAK_DENOISE: &str = "peak_denoise";

/// Breakdown key of the VAE-decode peak in an `.estimate` answer. The forecast's VRAM
/// figure is the LARGEST of the peaks, not their sum.
pub(super) const FLUX2_BREAKDOWN_PEAK_DECODE: &str = "peak_decode";

/// Breakdown key of the prompt-encoding peak in an `.estimate` answer.
///
/// The text encoder is resident only while the prompt is encoded, so that phase is a
/// peak of its own rather than a term added to the others. A backend that does not
/// report it simply leaves the line out of the tooltip.
pub(super) const FLUX2_BREAKDOWN_PEAK_ENCODE: &str = "peak_encode";

/// The `.estimate` answer: the backend's own forecast for the current parameters.
///
/// Every figure here is COMPUTED BY THE BACKEND; this side only formats it.
#[derive(Debug, Clone, Default)]
pub(super) struct Flux2Estimate {
    pub(super) vram_bytes: u64,
    pub(super) ram_bytes: u64,
    pub(super) vram_free: u64,
    pub(super) ram_free: u64,
    pub(super) fits: bool,
    /// `(component, bytes)` pairs in KEY order, not in the backend's: `serde_json` is
    /// built without `preserve_order`, so an object parses into a `BTreeMap`. Nothing
    /// may depend on the position of an entry — the per-phase peaks are looked up by name.
    /// The keys are backend identifiers and stay literal.
    pub(super) breakdown: Vec<(String, u64)>,
}

/// Formats the memory forecast as ONE line: the predicted peaks against what is free and,
/// while `.status` has answered, against what the machine has in total.
///
/// The free figures are what `fits` was actually computed against; the totals turn "9.8
/// free" into a proportion the user can judge. They are DROPPED, not printed as "0.0",
/// while `.status` has not answered — that call fails on its own, and «из 0,0 ГиБ» would be
/// a lie rather than a missing figure.
///
/// A function of its own because two surfaces print this line — the always-visible
/// readiness line and «Память и скорость» — and a second copy of the formatting would let
/// the same forecast read differently in the two places.
#[must_use]
pub(super) fn estimate_status_line(estimate: &Flux2Estimate, status: Option<&Flux2Status>) -> String {
    let vram_total = status.map_or(0, |s| s.vram_total);
    let ram_total = status.map_or(0, |s| s.ram_total);
    if vram_total > 0 && ram_total > 0 {
        tf!(
            "cleaning.tools.flux2_klein.estimate_status",
            vram = format_gib(estimate.vram_bytes),
            vram_free = format_gib(estimate.vram_free),
            vram_total = format_gib(vram_total),
            ram = format_gib(estimate.ram_bytes),
            ram_free = format_gib(estimate.ram_free),
            ram_total = format_gib(ram_total)
        )
    } else {
        tf!(
            "cleaning.tools.flux2_klein.estimate_status_no_totals",
            vram = format_gib(estimate.vram_bytes),
            vram_free = format_gib(estimate.vram_free),
            ram = format_gib(estimate.ram_bytes),
            ram_free = format_gib(estimate.ram_free)
        )
    }
}

/// Builds the hover text of the forecast line: the per-phase peaks first, in pipeline
/// order (prompt encoding, denoise, VAE decode — the VRAM figure is the LARGEST of them,
/// not their sum), then every other breakdown entry the backend reported.
///
/// Breakdown keys are backend identifiers, so they stay literal; their captions and
/// the unit around them come from the locale, so every figure on this screen is
/// labelled in the same unit the figures are actually computed in (gibibytes).
pub(super) fn estimate_tooltip(estimate: &Flux2Estimate) -> String {
    let peaks = split_estimate_peaks(estimate);
    let mut lines = Vec::<String>::new();
    if let Some(bytes) = peaks.encode {
        lines.push(tf!(
            "cleaning.tools.flux2_klein.estimate_peak_encode",
            size = format_gib(bytes)
        ));
    }
    if let Some(bytes) = peaks.denoise {
        lines.push(tf!(
            "cleaning.tools.flux2_klein.estimate_peak_denoise",
            size = format_gib(bytes)
        ));
    }
    if let Some(bytes) = peaks.decode {
        lines.push(tf!(
            "cleaning.tools.flux2_klein.estimate_peak_decode",
            size = format_gib(bytes)
        ));
    }
    for (key, bytes) in peaks.others {
        lines.push(tf!(
            "cleaning.tools.flux2_klein.estimate_breakdown_entry",
            key = key,
            size = format_gib(bytes)
        ));
    }
    if lines.is_empty() {
        return t!("cleaning.tools.flux2_klein.estimate_no_breakdown_status").to_string();
    }
    lines.join("\n")
}

/// A breakdown split into its per-phase peaks and everything else, borrowed from the
/// [`Flux2Estimate`] it was read from.
///
/// A named struct rather than a tuple: three `Option<u64>` fields in a row are exactly
/// the shape where a positional swap survives review unnoticed.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Flux2EstimatePeaks<'a> {
    /// Peak of the prompt-encoding phase, `None` when the backend did not report one
    /// (a build predating the phase-wise forecast).
    pub(super) encode: Option<u64>,
    pub(super) denoise: Option<u64>,
    pub(super) decode: Option<u64>,
    /// Every breakdown entry that is not one of the peaks above, in the order the
    /// backend's object parsed into (key order — see [`Flux2Estimate::breakdown`]).
    pub(super) others: Vec<(&'a str, u64)>,
}

/// Splits a breakdown into its per-phase peaks and the remaining entries, which is the
/// order the tooltip lists them in. Each peak is `None` when the backend did not report
/// it, and none of them is repeated among `others`.
///
/// Separate from [`estimate_tooltip`] because the ORDER and the de-duplication are the
/// contract worth testing, while the rendered text depends on a loaded locale catalog.
pub(super) fn split_estimate_peaks(estimate: &Flux2Estimate) -> Flux2EstimatePeaks<'_> {
    let peak = |name: &str| {
        estimate
            .breakdown
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, bytes)| *bytes)
    };
    const PEAK_KEYS: [&str; 3] = [
        FLUX2_BREAKDOWN_PEAK_ENCODE,
        FLUX2_BREAKDOWN_PEAK_DENOISE,
        FLUX2_BREAKDOWN_PEAK_DECODE,
    ];
    let others = estimate
        .breakdown
        .iter()
        .filter(|(key, _)| !PEAK_KEYS.contains(&key.as_str()))
        .map(|(key, bytes)| (key.as_str(), *bytes))
        .collect();
    Flux2EstimatePeaks {
        encode: peak(FLUX2_BREAKDOWN_PEAK_ENCODE),
        denoise: peak(FLUX2_BREAKDOWN_PEAK_DENOISE),
        decode: peak(FLUX2_BREAKDOWN_PEAK_DECODE),
        others,
    }
}

/// Asks the backend to forecast the memory cost of one run at `width` x `height`.
pub(super) fn fetch_flux2_estimate(
    params: &Value,
    width: usize,
    height: usize,
) -> Result<Flux2Estimate, String> {
    let client = backend_ipc::shared_client().map_err(|_| ai_backend_offline_error().to_string())?;
    let (header, _blob) = client
        .call(
            backend_ipc::protocol::METHOD_INPAINT_FLUX2_KLEIN_ESTIMATE,
            json!({
                "params": params,
                "region_width": width,
                "region_height": height,
            }),
            &[],
            FLUX2_QUERY_TIMEOUT,
        )
        .map_err(map_flux2_call_error)?;
    Ok(parse_flux2_estimate(&header))
}

/// Parses an `.estimate` answer. The breakdown comes out in key order (see
/// [`Flux2Estimate::breakdown`]), so it must be read by name, never by position.
pub(super) fn parse_flux2_estimate(header: &Value) -> Flux2Estimate {
    let u64_field = |name: &str| header.get(name).and_then(Value::as_u64).unwrap_or_default();
    let breakdown = header
        .get("breakdown")
        .and_then(Value::as_object)
        .map(|map| {
            map.iter()
                .map(|(key, value)| (key.clone(), value.as_u64().unwrap_or_default()))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Flux2Estimate {
        vram_bytes: u64_field("vram_bytes"),
        ram_bytes: u64_field("ram_bytes"),
        vram_free: u64_field("vram_free"),
        ram_free: u64_field("ram_free"),
        fits: header.get("fits").and_then(Value::as_bool).unwrap_or(false),
        breakdown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimate_tooltip_leads_with_the_phase_peaks() {
        let estimate = Flux2Estimate {
            breakdown: vec![
                ("transformer".to_string(), 8_000_000_000),
                (FLUX2_BREAKDOWN_PEAK_DECODE.to_string(), 6_000_000_000),
                (FLUX2_BREAKDOWN_PEAK_DENOISE.to_string(), 9_000_000_000),
                (FLUX2_BREAKDOWN_PEAK_ENCODE.to_string(), 17_000_000_000),
            ],
            ..Flux2Estimate::default()
        };
        // The three peaks lead, and none of them is repeated among the remaining
        // entries. Asserted on the split rather than on the rendered text: every line
        // comes from a locale template, and a unit test runs without a loaded catalog.
        let peaks = split_estimate_peaks(&estimate);
        assert_eq!(peaks.encode, Some(17_000_000_000));
        assert_eq!(peaks.denoise, Some(9_000_000_000));
        assert_eq!(peaks.decode, Some(6_000_000_000));
        assert_eq!(peaks.others, vec![("transformer", 8_000_000_000)]);

        let tooltip = estimate_tooltip(&estimate);
        assert_eq!(tooltip.lines().count(), 4);
        // A backend that reported nothing still gets a line, not an empty tooltip.
        assert!(!estimate_tooltip(&Flux2Estimate::default()).is_empty());
        assert_eq!(
            split_estimate_peaks(&Flux2Estimate::default()),
            Flux2EstimatePeaks::default()
        );
    }

    #[test]
    fn a_breakdown_without_the_encode_peak_still_renders() {
        // The prompt-encoding phase is reported by a NEWER backend than the one that
        // introduced the other two peaks; an answer without it must lose exactly that
        // line and keep everything else.
        let estimate = Flux2Estimate {
            breakdown: vec![
                (FLUX2_BREAKDOWN_PEAK_DECODE.to_string(), 6_000_000_000),
                (FLUX2_BREAKDOWN_PEAK_DENOISE.to_string(), 9_000_000_000),
            ],
            ..Flux2Estimate::default()
        };
        let peaks = split_estimate_peaks(&estimate);
        assert!(peaks.encode.is_none());
        assert_eq!(peaks.denoise, Some(9_000_000_000));
        assert_eq!(estimate_tooltip(&estimate).lines().count(), 2);
    }

    #[test]
    fn estimate_parses_the_breakdown() {
        let header = json!({
            "vram_bytes": 10_000_000_000u64,
            "ram_bytes": 2_000_000_000u64,
            "vram_free": 15_000_000_000u64,
            "ram_free": 20_000_000_000u64,
            "fits": true,
            "breakdown": { "transformer": 8_000_000_000u64 }
        });
        let estimate = parse_flux2_estimate(&header);
        assert!(estimate.fits);
        assert_eq!(estimate.vram_bytes, 10_000_000_000);
        assert_eq!(estimate.breakdown.len(), 1);
        assert_eq!(estimate.breakdown[0].0, "transformer");
        // An empty answer must degrade, not panic.
        let empty = parse_flux2_estimate(&json!({}));
        assert!(!empty.fits);
        assert!(empty.breakdown.is_empty());
    }
}
