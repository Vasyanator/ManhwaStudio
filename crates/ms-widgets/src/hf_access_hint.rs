/*
FILE HEADER (widgets/hf_access_hint.rs)
- Purpose: a small "How do I get access?" caption whose hover tooltip walks a non-expert
  through getting into a GATED Hugging Face model, step by step, with every page one click
  away: (1) create an account, (2) open the model repository and accept its licence,
  (3) create a Read access token, (4) paste it into the app's token field.
- Key items:
  - `HfAccessHint`: the widget. `new(id_salt, save_button_label)`, one `repo(name, repo)`
    per gated repository the model needs, `show(ui)`.
  - `hf_repo_url()`: the one builder of a repository page URL from a repo id (or a URL
    handed in as is); `HF_JOIN_URL`, `HF_TOKEN_SETTINGS_URL`, `HF_URL_PREFIX`: the Hugging
    Face pages every consumer links to, owned here so no two places spell them.
- Interaction contract: the tooltip is egui's own `Response::on_hover_ui`
  (egui-0.36.2/src/response.rs:665 -> `Tooltip::for_enabled`, containers/tooltip.rs:53). A
  tooltip that drew an interactive widget on its previous frame becomes interactable
  (`had_interactive_widgets`, tooltip.rs:135, :207) and STAYS OPEN while the pointer is inside
  it or moving towards it (`should_show_tooltip`, tooltip.rs:221, :274-291). The hyperlinks are
  such widgets, so the user can move into the tooltip and click them.
- No literal user-visible text: every line is a `widgets.hf_access_hint.*` key; repository ids
  are backend identifiers and stay literal. Nothing here blocks: links open through egui's
  `OpenUrl` output, handled by the platform integration.
*/

use eframe::egui;

/// Where a Hugging Face account is created.
pub const HF_JOIN_URL: &str = "https://huggingface.co/join";

/// Where a Hugging Face access token is created.
pub const HF_TOKEN_SETTINGS_URL: &str = "https://huggingface.co/settings/tokens";

/// The site prefix a repository id is appended to for its page.
pub const HF_URL_PREFIX: &str = "https://huggingface.co/";

/// Width the tooltip text wraps at, in logical points: several sentences per step read as a
/// column, not as one line across the screen.
const TOOLTIP_WIDTH_PT: f32 = 360.0;

/// The page of a Hugging Face repository.
///
/// `repo` is a repository id (`black-forest-labs/FLUX.2-klein-9B`), surrounding whitespace and
/// slashes ignored; a value that already is an `http(s)://` URL is returned as is, so a caller
/// may hand over either.
#[must_use]
pub fn hf_repo_url(repo: &str) -> String {
    let repo = repo.trim();
    if repo.starts_with("https://") || repo.starts_with("http://") {
        return repo.to_string();
    }
    format!("{HF_URL_PREFIX}{}", repo.trim_matches('/'))
}

/// The step-by-step "how to get access to a gated Hugging Face model" hint.
///
/// Draws one small underlined caption; hovering it opens a tooltip with the four steps and a
/// clickable link for each page. Build it per frame:
/// `HfAccessHint::new("my_salt", save_label).repo("FLUX.2 klein 9B", "black-forest-labs/FLUX.2-klein-9B").show(ui)`.
#[derive(Debug, Clone)]
pub struct HfAccessHint<'a> {
    id_salt: &'a str,
    save_button_label: &'a str,
    repos: Vec<(&'a str, &'a str)>,
}

impl<'a> HfAccessHint<'a> {
    /// A hint with no repository yet.
    ///
    /// `id_salt` keeps the caption's id (and with it the tooltip's) stable across language
    /// switches and distinct from a second hint in the same `Ui`. `save_button_label` is the
    /// caption of the button that stores the pasted token, named in step 4 — the hint never
    /// spells another widget's caption itself.
    #[must_use]
    pub fn new(id_salt: &'a str, save_button_label: &'a str) -> Self {
        Self { id_salt, save_button_label, repos: Vec::new() }
    }

    /// Adds one gated repository whose licence must be accepted: `name` is what the user
    /// knows the model as, `repo` its id (or page URL, see [`hf_repo_url`]).
    #[must_use]
    pub fn repo(mut self, name: &'a str, repo: &'a str) -> Self {
        self.repos.push((name, repo));
        self
    }

    /// Draws the caption and attaches the tooltip; returns the caption's response.
    pub fn show(self, ui: &mut egui::Ui) -> egui::Response {
        let Self { id_salt, save_button_label, repos } = self;
        ui.push_id(id_salt, |ui| {
            ui.add(
                egui::Label::new(egui::RichText::new(t!("widgets.hf_access_hint.trigger_label")).small().underline())
                    .selectable(false)
                    .sense(egui::Sense::hover()),
            )
            .on_hover_ui(|ui| draw_steps(ui, save_button_label, &repos))
        })
        .inner
    }
}

/// The tooltip body: the four steps, each with its page.
fn draw_steps(ui: &mut egui::Ui, save_button_label: &str, repos: &[(&str, &str)]) {
    ui.set_max_width(TOOLTIP_WIDTH_PT);
    ui.label(egui::RichText::new(t!("widgets.hf_access_hint.title")).strong());

    ui.label(t!("widgets.hf_access_hint.step_account"));
    ui.hyperlink_to(t!("widgets.hf_access_hint.join_link"), HF_JOIN_URL);

    ui.separator();
    ui.label(t!("widgets.hf_access_hint.step_licence"));
    for (name, repo) in repos {
        // The id stays literal: it is what the page itself is titled with.
        ui.hyperlink_to(tf!("widgets.hf_access_hint.repo_link", name = name, repo = repo.trim()), hf_repo_url(repo));
    }

    ui.separator();
    ui.label(t!("widgets.hf_access_hint.step_token"));
    ui.hyperlink_to(t!("widgets.hf_access_hint.token_link"), HF_TOKEN_SETTINGS_URL);

    ui.separator();
    ui.label(tf!("widgets.hf_access_hint.step_paste", save = save_button_label));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_repo_id_becomes_its_page_and_a_url_is_kept() {
        assert_eq!(hf_repo_url("black-forest-labs/FLUX.2-klein-9B"), "https://huggingface.co/black-forest-labs/FLUX.2-klein-9B");
        assert_eq!(hf_repo_url("  /black-forest-labs/flux-3-image-klein/ "), "https://huggingface.co/black-forest-labs/flux-3-image-klein");
        assert_eq!(hf_repo_url("https://huggingface.co/org/model"), "https://huggingface.co/org/model");
        assert_eq!(hf_repo_url("http://mirror.example/org/model"), "http://mirror.example/org/model");
    }

    #[test]
    fn the_fixed_pages_are_the_hugging_face_ones() {
        assert_eq!(HF_JOIN_URL, "https://huggingface.co/join");
        assert_eq!(HF_TOKEN_SETTINGS_URL, "https://huggingface.co/settings/tokens");
        assert!(hf_repo_url("a/b").starts_with(HF_URL_PREFIX));
    }

    /// Every step is in BOTH hand-maintained catalogs with the same placeholders: a unit
    /// test cannot see a missing key through `t!`, which falls back to the key itself.
    #[test]
    fn every_step_is_in_en_and_ru_with_the_same_placeholders() {
        let catalog = |tag: &str| -> serde_json::Value {
            let (_, source) = ms_i18n::embedded_locales()
                .iter()
                .find(|(name, _)| *name == tag)
                .unwrap_or_else(|| panic!("locale `{tag}` is embedded"));
            serde_json::from_str(source).unwrap_or_else(|err| panic!("locale `{tag}`: {err}"))
        };
        let (en, ru) = (catalog("en"), catalog("ru"));
        for (key, placeholders) in [
            ("trigger_label", &[][..]),
            ("title", &[]),
            ("step_account", &[]),
            ("join_link", &[]),
            ("step_licence", &[]),
            ("repo_link", &["{name}", "{repo}"]),
            ("step_token", &[]),
            ("token_link", &[]),
            ("step_paste", &["{save}"]),
        ] {
            let key = format!("widgets.hf_access_hint.{key}");
            for (tag, catalog) in [("en", &en), ("ru", &ru)] {
                let text = catalog.get(&key).and_then(serde_json::Value::as_str).unwrap_or_else(|| panic!("`{tag}` lacks `{key}`"));
                assert!(!text.trim().is_empty(), "`{tag}`: `{key}` is empty");
                for placeholder in placeholders {
                    assert!(text.contains(placeholder), "`{tag}`: `{key}` must carry `{placeholder}`");
                }
            }
        }
    }
}
