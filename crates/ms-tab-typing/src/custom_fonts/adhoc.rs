/*
File: crates/ms-tab-typing/src/custom_fonts/adhoc.rs

Purpose:
Render names of AD-HOC (layer-owned) custom fonts and the process-global interner that maps a
render name back to its recipe.

Why it exists:
Every ad-hoc layer persists the same identity (`MSCustom-adhoc-hybrid`) next to its own recipe,
but `FontProvider::resolve(name)` is name-only. So a layer renders through a per-recipe RENDER
name, `MSCustom-adhoc-<16 hex>`, a pure function of the canonical recipe JSON; the provider
recovers the recipe from that name through this interner. The render name is never persisted
and never written into `text_params.font`.

Key functions:
- `render_name`: recipe -> render name (and interns the recipe).
- `recipe_for_render_name`: render name -> recipe, for names minted in this process.

Notes:
The interner is a process-global memo (global §5 allows it as a content-addressed cache): the
mapping name -> recipe can never change (the name IS a digest of the recipe), and it is bounded
by the number of distinct recipes rendered in one session — a few per tuned layer. Hashing a
recipe (a few hundred bytes) is cheap enough for the GUI thread.
*/

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use ms_custom_fonts::CustomFontRecipe;
use sha2::{Digest, Sha256};

use super::identity::{ADHOC_RENDER_NAME_PREFIX, IDENTITY_HEX_LEN, hex_lower, is_adhoc_render_name};
use crate::panel::fonts::normalize_font_identity;

/// Domain separator of the render-name digest.
const RENDER_NAME_DOMAIN: &[u8] = b"ms-tab-typing/adhoc-render-name\0";

/// Normalized render name -> recipe.
type Interner = Mutex<HashMap<String, Arc<CustomFontRecipe>>>;

/// The process-global interner.
fn interner() -> &'static Interner {
    static INTERNER: OnceLock<Interner> = OnceLock::new();
    INTERNER.get_or_init(Interner::default)
}

/// The render name of an ad-hoc recipe: `MSCustom-adhoc-<16 hex of SHA-256(canonical JSON)>`.
///
/// Pure in its result (equal recipes give equal names in every process); as a side effect the
/// recipe is interned so `recipe_for_render_name` can recover it in this process.
#[must_use]
pub(crate) fn render_name(recipe: &Arc<CustomFontRecipe>) -> String {
    let name = render_name_of(recipe);
    // A poisoned interner only means another thread panicked mid-insert of a pure memo; its
    // map is still a valid name -> recipe table, so recover it.
    let mut map = interner().lock().unwrap_or_else(PoisonError::into_inner);
    map.entry(normalize_font_identity(&name)).or_insert_with(|| Arc::clone(recipe));
    name
}

/// The recipe behind a render name minted by `render_name` in this process, or `None` (not a
/// render name, or never interned here).
#[must_use]
pub(crate) fn recipe_for_render_name(name: &str) -> Option<Arc<CustomFontRecipe>> {
    if !is_adhoc_render_name(name) {
        return None;
    }
    let map = interner().lock().unwrap_or_else(PoisonError::into_inner);
    map.get(&normalize_font_identity(name)).cloned()
}

/// The render name without interning.
fn render_name_of(recipe: &CustomFontRecipe) -> String {
    // `to_json` is the recipe's canonical form; serde_json serializes object keys sorted
    // (`preserve_order` is banned workspace-wide, ARCHITECTURE.md §12), so the bytes are canonical.
    let canonical = recipe.to_json().to_string();
    let mut hasher = Sha256::new();
    hasher.update(RENDER_NAME_DOMAIN);
    hasher.update(canonical.as_bytes());
    let digest: [u8; 32] = hasher.finalize().into();
    format!("{ADHOC_RENDER_NAME_PREFIX}{}", hex_lower(&digest[..IDENTITY_HEX_LEN / 2]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::custom_fonts::identity::is_saved_identity;
    use ms_custom_fonts::{CustomFontKind, HybridParams, HybridSource, MissingGlyphPolicy};

    fn recipe(weight_b: f32) -> Arc<CustomFontRecipe> {
        Arc::new(CustomFontRecipe {
            kind: CustomFontKind::Hybrid(HybridParams {
                sources: vec![
                    HybridSource { identity: "NotoSans-Regular".to_owned(), weight: 1.0, missing_override: None },
                    HybridSource { identity: "NotoSans-Bold".to_owned(), weight: weight_b, missing_override: None },
                ],
                missing_default: MissingGlyphPolicy::NoInfluence,
                algorithm: ms_custom_fonts::BlendAlgorithm::Auto,
                property_weights: None,
            }),
        })
    }

    #[test]
    fn render_name_is_deterministic_and_per_recipe() {
        let a = render_name(&recipe(1.0));
        assert_eq!(a, render_name(&recipe(1.0)));
        assert_ne!(a, render_name(&recipe(2.0)));
        assert!(is_adhoc_render_name(&a));
        assert!(!is_saved_identity(&a));
    }

    #[test]
    fn interner_round_trips_and_ignores_foreign_names() {
        let original = recipe(3.5);
        let name = render_name(&original);
        assert_eq!(recipe_for_render_name(&name).as_deref(), Some(&*original));
        // The provider compares names normalized (lowercase, trimmed).
        assert_eq!(recipe_for_render_name(&format!("  {}  ", name.to_ascii_uppercase())).as_deref(), Some(&*original));
        assert!(recipe_for_render_name("MSCustom-adhoc-hybrid").is_none());
        assert!(recipe_for_render_name("MSCustom-adhoc-ffffffffffffffff").is_none());
        assert!(recipe_for_render_name("NotoSans-Regular").is_none());
    }
}
