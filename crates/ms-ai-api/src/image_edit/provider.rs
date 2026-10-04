/*
File: crates/ms-ai-api/src/image_edit/provider.rs

Purpose:
The image-edit provider catalogue: every hosted service (first-party, aggregator, Russian
reseller) plus the user's own OpenAI-compatible images server, with the data the rest of the
layer needs per provider: brand label, API shape (which adapter speaks to it), endpoint (one
base URL, a region list, or a user-given base URL), where its key lives, and its
availability from Russia.

Key structures:
- ImageEditProvider (`key()` ids are a persistence contract)
- ProviderInfo, ApiShape, EndpointKind, EndpointRegion, ProviderKeySlot
- RussiaStatus, RussiaNote, RussiaAccess

Notes:
Availability from Russia is per PROVIDER, as of 2026-10-04 (research classes Available ->
`Works`, Partially -> `PaymentIssues`, Blocked -> `Blocked`; sources:
`dev-docs/image_edit/services_{western,cn_ru,aggregators}.md`). A reseller has its own
status, independent of its upstream. Base URLs come from the providers' docs (cited in the
research notes or fetched 2026-10-04); the adapters append the endpoint paths. Brand labels
are not localized (as in `service.rs`); the compatible label, region labels, status and note
texts are.
*/

use crate::keys::NamedKeyUser;
use crate::service::AiApiService;

/// One image-edit provider. `key()` is persisted (tool settings, named key slots).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ImageEditProvider {
    OpenAi,
    Gemini,
    OpenRouter,
    BytePlus,
    Bfl,
    Xai,
    Ideogram,
    Recraft,
    Runway,
    Luma,
    DashScope,
    Tencent,
    Kling,
    Fal,
    Replicate,
    Together,
    DeepInfra,
    Runware,
    AimlApi,
    AiTunnel,
    ProxyApi,
    RouterAi,
    Polza,
    GenApi,
    /// The user's own server speaking the `OpenAI` images API at a user-given base URL.
    OpenAiCompatible,
}

/// The wire protocol family of a provider: which adapter builds its requests. Providers are
/// data; one adapter serves every provider of its shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ApiShape {
    /// `POST {base}/images/edits` (`OpenAI` images edits).
    OpenAiImages,
    /// Gemini `models/{id}:generateContent` with `imageConfig`.
    GeminiGenerate,
    /// `OpenRouter`-style `POST {base}/images` with `input_references`.
    OpenRouterImages,
    /// `BytePlus` `ModelArk` images generations.
    ArkImages,
    /// Black Forest Labs async: submit, poll `polling_url`, download `result.sample`.
    BflAsync,
    /// xAI images edits.
    XaiImages,
    /// Ideogram precise edit (multipart).
    IdeogramEdit,
    /// Recraft edit endpoints.
    RecraftEdit,
    /// Runway task API.
    RunwayTasks,
    /// Luma Agents generations API (`type: image_edit`).
    LumaGenerations,
    /// Alibaba Model Studio (`DashScope`) multimodal generation.
    DashScopeMultimodal,
    /// Tencent `TokenHub` image generation.
    TencentTokenHub,
    /// Kling image API.
    KlingImage,
    /// fal.ai queue: submit, poll `status_url`, fetch `response_url`, download.
    FalQueue,
    /// Replicate predictions.
    ReplicatePredictions,
    /// Together images generations with reference images.
    TogetherImages,
    /// Runware task API.
    RunwareTasks,
    /// Polza.ai async media API.
    PolzaMedia,
    /// `GenAPI` async network API.
    GenApiAsync,
}

/// One selectable region of a provider whose keys or hosts are per region.
#[derive(Debug, Clone, Copy)]
pub struct EndpointRegion {
    /// Frozen region id (part of a region-bound key's user name); non-empty, no `@`.
    pub id: &'static str,
    /// API base URL of the region (https, no trailing `/`).
    pub base_url: &'static str,
    /// Localized region name for the picker.
    pub label: fn() -> &'static str,
}

/// Where a provider's requests go.
#[derive(Debug, Clone, Copy)]
pub enum EndpointKind {
    /// One API base URL (https, no trailing `/`).
    Fixed(&'static str),
    /// A region list; the first entry is the default.
    Regions(&'static [EndpointRegion]),
    /// A user-given base URL (validated as an `AiApiTarget` of `AiApiService::OpenAiCompatible`).
    UserBaseUrl,
}

/// Where a provider's API key lives in the OS credential store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderKeySlot {
    /// The chat key of this hosted service, shared with OCR and machine translation.
    SharedChat(AiApiService),
    /// The per-URL key of `AiApiService::OpenAiCompatible`, shared with the chat-compatible
    /// connection for the same base URL; optional.
    SharedCompatible,
    /// A named slot `image_edit:{provider_key}`.
    Named,
    /// A named slot per region, `image_edit:{provider_key}@{region_id}` (keys are region-bound).
    NamedPerRegion,
}

/// How a provider can be used from Russia (as of 2026-10-04).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RussiaStatus {
    /// Registration, ruble payment and calls work directly.
    Works,
    /// Reachable, but payment needs a non-Russian card, crypto or an intermediary.
    PaymentIssues,
    /// The provider excludes Russia (country list, geo error, closed accounts).
    Blocked,
}

impl RussiaStatus {
    /// Localized badge text.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Works => t!("ai_api.image_edit.russia.works_status"),
            Self::PaymentIssues => t!("ai_api.image_edit.russia.payment_issues_status"),
            Self::Blocked => t!("ai_api.image_edit.russia.blocked_status"),
        }
    }
}

/// The reason behind a provider's `RussiaStatus` (badge tooltip).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RussiaNote {
    /// Russia is absent from the provider's supported-country list or the API answers with a
    /// geo error.
    CountryExcluded,
    /// The provider closed Russian accounts and IPs (secondary sources, no official statement).
    AccountsClosed,
    /// No Russia-specific block is known, but payment accepts only non-Russian cards.
    ForeignCardRequired,
    /// As `ForeignCardRequired`, but crypto top-up is also accepted.
    ForeignCardOrCrypto,
    /// A Russian reseller paid in rubles; its upstream excludes Russia, so models may vanish
    /// and every page passes through a third-party chain.
    RubleReseller,
}

impl RussiaNote {
    /// Localized tooltip text.
    #[must_use]
    pub fn text(self) -> &'static str {
        match self {
            Self::CountryExcluded => t!("ai_api.image_edit.russia.country_excluded_note"),
            Self::AccountsClosed => t!("ai_api.image_edit.russia.accounts_closed_note"),
            Self::ForeignCardRequired => t!("ai_api.image_edit.russia.foreign_card_note"),
            Self::ForeignCardOrCrypto => t!("ai_api.image_edit.russia.foreign_card_or_crypto_note"),
            Self::RubleReseller => t!("ai_api.image_edit.russia.ruble_reseller_note"),
        }
    }
}

/// A provider's availability from Russia: the badge status and its reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RussiaAccess {
    pub status: RussiaStatus,
    pub note: RussiaNote,
}

/// Static description of one provider.
#[derive(Debug, Clone, Copy)]
pub struct ProviderInfo {
    /// Display name (brand, not localized; the compatible entry is localized).
    pub label: &'static str,
    /// Which adapter speaks to it.
    pub shape: ApiShape,
    /// Where its requests go.
    pub endpoint: EndpointKind,
    /// Where its key lives.
    pub key_slot: ProviderKeySlot,
    /// Availability from Russia; `None` for the user's own server (no badge).
    pub russia: Option<RussiaAccess>,
    /// The provider's API documentation (linked next to the provider in the picker).
    pub docs_url: &'static str,
}

/// BFL regional hosts; one key works for all of them (`research_flux_qwen` §A).
const BFL_REGIONS: [EndpointRegion; 3] = [
    EndpointRegion { id: "global", base_url: "https://api.bfl.ai", label: || t!("ai_api.image_edit.region.global_label") },
    EndpointRegion { id: "eu", base_url: "https://api.eu.bfl.ai", label: || t!("ai_api.image_edit.region.eu_label") },
    EndpointRegion { id: "us", base_url: "https://api.us.bfl.ai", label: || t!("ai_api.image_edit.region.us_label") },
];

/// Alibaba Model Studio regions; keys are region-bound (`research_flux_qwen` §B: the
/// `dashscope-intl` (Singapore) and `dashscope` (Beijing) hosts, "still functional").
const DASHSCOPE_REGIONS: [EndpointRegion; 2] = [
    EndpointRegion { id: "intl", base_url: "https://dashscope-intl.aliyuncs.com", label: || t!("ai_api.image_edit.region.singapore_label") },
    EndpointRegion { id: "cn", base_url: "https://dashscope.aliyuncs.com", label: || t!("ai_api.image_edit.region.beijing_label") },
];

/// Shorthand for the status table below.
const fn access(status: RussiaStatus, note: RussiaNote) -> RussiaAccess {
    RussiaAccess { status, note }
}

const BLOCKED_COUNTRY: Option<RussiaAccess> = Some(access(RussiaStatus::Blocked, RussiaNote::CountryExcluded));
const PAYMENT_CARD: Option<RussiaAccess> = Some(access(RussiaStatus::PaymentIssues, RussiaNote::ForeignCardRequired));
const RUBLE_RESELLER: Option<RussiaAccess> = Some(access(RussiaStatus::Works, RussiaNote::RubleReseller));

impl ImageEditProvider {
    /// Every provider, in picker order (first-party, aggregators, Russian resellers, own server).
    pub const ALL: [Self; 25] = [
        Self::OpenAi,
        Self::Gemini,
        Self::Bfl,
        Self::Xai,
        Self::Ideogram,
        Self::Recraft,
        Self::Runway,
        Self::Luma,
        Self::DashScope,
        Self::Tencent,
        Self::Kling,
        Self::BytePlus,
        Self::OpenRouter,
        Self::Fal,
        Self::Replicate,
        Self::Together,
        Self::DeepInfra,
        Self::Runware,
        Self::AimlApi,
        Self::AiTunnel,
        Self::ProxyApi,
        Self::RouterAi,
        Self::Polza,
        Self::GenApi,
        Self::OpenAiCompatible,
    ];

    /// Stable persisted id (tool settings, named key slot user names). Never change a value.
    #[must_use]
    pub fn key(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::Gemini => "gemini",
            Self::OpenRouter => "openrouter",
            Self::BytePlus => "byteplus",
            Self::Bfl => "bfl",
            Self::Xai => "xai",
            Self::Ideogram => "ideogram",
            Self::Recraft => "recraft",
            Self::Runway => "runway",
            Self::Luma => "luma",
            Self::DashScope => "dashscope",
            Self::Tencent => "tencent",
            Self::Kling => "kling",
            Self::Fal => "fal",
            Self::Replicate => "replicate",
            Self::Together => "together",
            Self::DeepInfra => "deepinfra",
            Self::Runware => "runware",
            Self::AimlApi => "aimlapi",
            Self::AiTunnel => "aitunnel",
            Self::ProxyApi => "proxyapi",
            Self::RouterAi => "routerai",
            Self::Polza => "polza",
            Self::GenApi => "genapi",
            Self::OpenAiCompatible => "openai_compatible",
        }
    }

    /// Parses a persisted id (exact match). `None` for an unknown id: the caller decides the
    /// fallback, so a removed provider is never silently replaced here.
    #[must_use]
    pub fn from_key(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|provider| provider.key() == raw)
    }

    /// The provider's static description.
    #[must_use]
    pub fn info(self) -> ProviderInfo {
        let (label, shape, endpoint, key_slot, russia, docs_url) = match self {
            Self::OpenAi => ("OpenAI", ApiShape::OpenAiImages, EndpointKind::Fixed("https://api.openai.com/v1"), ProviderKeySlot::SharedChat(AiApiService::OpenAi), BLOCKED_COUNTRY, "https://developers.openai.com/api/reference/resources/images/methods/edit"),
            Self::Gemini => ("Google Gemini", ApiShape::GeminiGenerate, EndpointKind::Fixed("https://generativelanguage.googleapis.com/v1beta"), ProviderKeySlot::SharedChat(AiApiService::Gemini), BLOCKED_COUNTRY, "https://ai.google.dev/gemini-api/docs/image-generation"),
            Self::OpenRouter => ("OpenRouter", ApiShape::OpenRouterImages, EndpointKind::Fixed("https://openrouter.ai/api/v1"), ProviderKeySlot::SharedChat(AiApiService::OpenRouter), Some(access(RussiaStatus::Blocked, RussiaNote::AccountsClosed)), "https://openrouter.ai/docs/guides/overview/multimodal/image-generation"),
            Self::BytePlus => ("BytePlus ModelArk", ApiShape::ArkImages, EndpointKind::Fixed("https://ark.ap-southeast.bytepluses.com/api/v3"), ProviderKeySlot::Named, BLOCKED_COUNTRY, "https://docs.byteplus.com/en/docs/ModelArk"),
            Self::Bfl => ("Black Forest Labs", ApiShape::BflAsync, EndpointKind::Regions(&BFL_REGIONS), ProviderKeySlot::Named, PAYMENT_CARD, "https://docs.bfl.ai"),
            Self::Xai => ("xAI", ApiShape::XaiImages, EndpointKind::Fixed("https://api.x.ai/v1"), ProviderKeySlot::SharedChat(AiApiService::Xai), PAYMENT_CARD, "https://docs.x.ai/developers/model-capabilities/images/editing"),
            Self::Ideogram => ("Ideogram", ApiShape::IdeogramEdit, EndpointKind::Fixed("https://api.ideogram.ai"), ProviderKeySlot::Named, PAYMENT_CARD, "https://developer.ideogram.ai/api-reference/images/precise-edit/ideogram-4-5"),
            Self::Recraft => ("Recraft", ApiShape::RecraftEdit, EndpointKind::Fixed("https://external.api.recraft.ai/v1"), ProviderKeySlot::Named, PAYMENT_CARD, "https://www.recraft.ai/docs/api-reference/endpoints"),
            Self::Runway => ("Runway", ApiShape::RunwayTasks, EndpointKind::Fixed("https://api.dev.runwayml.com/v1"), ProviderKeySlot::Named, PAYMENT_CARD, "https://docs.dev.runwayml.com/api/"),
            // The Luma Agents API (docs.agents.lumalabs.ai, fetched 2026-10-04): the Uni image
            // models and inline image data; the legacy Dream Machine API has neither.
            Self::Luma => ("Luma AI", ApiShape::LumaGenerations, EndpointKind::Fixed("https://agents.lumalabs.ai/v1"), ProviderKeySlot::Named, PAYMENT_CARD, "https://docs.agents.lumalabs.ai/guides/images/editing/"),
            Self::DashScope => ("Alibaba Model Studio", ApiShape::DashScopeMultimodal, EndpointKind::Regions(&DASHSCOPE_REGIONS), ProviderKeySlot::NamedPerRegion, PAYMENT_CARD, "https://www.alibabacloud.com/help/en/model-studio/qwen-image-edit-api"),
            Self::Tencent => ("Tencent TokenHub", ApiShape::TencentTokenHub, EndpointKind::Fixed("https://tokenhub-intl.tencentcloudmaas.com/v1"), ProviderKeySlot::Named, PAYMENT_CARD, "https://intl.cloud.tencent.com/document/product/1300/83708"),
            Self::Kling => ("Kling AI", ApiShape::KlingImage, EndpointKind::Fixed("https://api-singapore.klingai.com"), ProviderKeySlot::Named, PAYMENT_CARD, "https://app.klingai.com/global/dev/document-api"),
            Self::Fal => ("fal.ai", ApiShape::FalQueue, EndpointKind::Fixed("https://queue.fal.run"), ProviderKeySlot::Named, PAYMENT_CARD, "https://fal.ai/docs/model-apis/model-endpoints/queue"),
            Self::Replicate => ("Replicate", ApiShape::ReplicatePredictions, EndpointKind::Fixed("https://api.replicate.com/v1"), ProviderKeySlot::Named, PAYMENT_CARD, "https://replicate.com/docs/reference/http"),
            // The API reference's server (docs.together.ai/reference/post-images-generations.md,
            // fetched 2026-10-04).
            Self::Together => ("Together AI", ApiShape::TogetherImages, EndpointKind::Fixed("https://api.together.ai/v1"), ProviderKeySlot::Named, PAYMENT_CARD, "https://docs.together.ai/docs/images-overview"),
            // The OpenAPI spec (https://api.deepinfra.com/openapi.json, fetched 2026-10-04) has
            // `POST /v1/images/edits` and no `/v1/openai/images/edits`.
            Self::DeepInfra => ("DeepInfra", ApiShape::OpenAiImages, EndpointKind::Fixed("https://api.deepinfra.com/v1"), ProviderKeySlot::Named, PAYMENT_CARD, "https://docs.deepinfra.com/api-reference/image-generation/openai-images-edits"),
            Self::Runware => ("Runware", ApiShape::RunwareTasks, EndpointKind::Fixed("https://api.runware.ai/v1"), ProviderKeySlot::Named, PAYMENT_CARD, "https://runware.ai/docs"),
            Self::AimlApi => ("AI/ML API", ApiShape::OpenAiImages, EndpointKind::Fixed("https://api.aimlapi.com/v1"), ProviderKeySlot::Named, Some(access(RussiaStatus::PaymentIssues, RussiaNote::ForeignCardOrCrypto)), "https://docs.aimlapi.com/api-references/image-models/openai/gpt-image-2"),
            Self::AiTunnel => ("AITunnel", ApiShape::OpenAiImages, EndpointKind::Fixed("https://api.aitunnel.ru/v1"), ProviderKeySlot::Named, RUBLE_RESELLER, "https://aitunnel.ru/docs/images"),
            Self::ProxyApi => ("ProxyAPI", ApiShape::OpenAiImages, EndpointKind::Fixed("https://api.proxyapi.ru/v1"), ProviderKeySlot::Named, RUBLE_RESELLER, "https://proxyapi.ru/docs/image-generation"),
            Self::RouterAi => ("RouterAI", ApiShape::OpenRouterImages, EndpointKind::Fixed("https://routerai.ru/api/v1"), ProviderKeySlot::Named, RUBLE_RESELLER, "https://routerai.ru/docs/guides/overview/multimodal/image-generation"),
            Self::Polza => ("Polza.ai", ApiShape::PolzaMedia, EndpointKind::Fixed("https://polza.ai/api"), ProviderKeySlot::Named, RUBLE_RESELLER, "https://polza.ai/docs/api-reference/images/generations"),
            Self::GenApi => ("GenAPI", ApiShape::GenApiAsync, EndpointKind::Fixed("https://api.gen-api.ru/api/v1"), ProviderKeySlot::Named, RUBLE_RESELLER, "https://gen-api.ru/model/gpt-image-2/api"),
            Self::OpenAiCompatible => (t!("ai_api.image_edit.provider.openai_compatible_label"), ApiShape::OpenAiImages, EndpointKind::UserBaseUrl, ProviderKeySlot::SharedCompatible, None, "https://raw.githubusercontent.com/leejet/stable-diffusion.cpp/master/examples/server/api.md"),
        };
        ProviderInfo { label, shape, endpoint, key_slot, russia, docs_url }
    }

    /// Whether a stored key is mandatory. Only the user's own server may run without one.
    #[must_use]
    pub fn requires_key(self) -> bool {
        match self.info().key_slot {
            ProviderKeySlot::SharedChat(_) | ProviderKeySlot::Named | ProviderKeySlot::NamedPerRegion => true,
            ProviderKeySlot::SharedCompatible => false,
        }
    }

    /// The named credential-store slot of this provider: `image_edit:{key}` for a `Named`
    /// provider, `image_edit:{key}@{region_id}` for a `NamedPerRegion` provider (`None` when
    /// `region_id` is `None`, since such a key is bound to one region). `None` for providers
    /// whose key is a shared chat key; `region_id` is ignored for a `Named` provider.
    #[must_use]
    pub fn named_key_user(self, region_id: Option<&'static str>) -> Option<NamedKeyUser> {
        match self.info().key_slot {
            ProviderKeySlot::Named => Some(NamedKeyUser::image_edit(self.key(), None)),
            ProviderKeySlot::NamedPerRegion => region_id.map(|region| NamedKeyUser::image_edit(self.key(), Some(region))),
            ProviderKeySlot::SharedChat(_) | ProviderKeySlot::SharedCompatible => None,
        }
    }

    /// Host suffixes, besides the origin of the call's base URL, where this provider's API
    /// key may go: hosts the provider's own API hands out in its answers. Only BFL: its
    /// `polling_url` must be used as returned and may name another regional `*.bfl.ai` host
    /// (<https://docs.bfl.ml/api_integration/integration_guidelines.md>, "Polling URL Usage";
    /// region identifiers change, so no fixed host list). Empty for everyone else. The
    /// executor accepts a suffix only over https on port 443 and on a dot boundary.
    #[must_use]
    pub fn auth_host_suffixes(self) -> &'static [&'static str] {
        match self {
            Self::Bfl => &["bfl.ai"],
            Self::OpenAi
            | Self::Gemini
            | Self::OpenRouter
            | Self::BytePlus
            | Self::Xai
            | Self::Ideogram
            | Self::Recraft
            | Self::Runway
            | Self::Luma
            | Self::DashScope
            | Self::Tencent
            | Self::Kling
            | Self::Fal
            | Self::Replicate
            | Self::Together
            | Self::DeepInfra
            | Self::Runware
            | Self::AimlApi
            | Self::AiTunnel
            | Self::ProxyApi
            | Self::RouterAi
            | Self::Polza
            | Self::GenApi
            | Self::OpenAiCompatible => &[],
        }
    }

    /// The region with frozen id `region_id`, for a provider with a region list.
    #[must_use]
    pub fn region(self, region_id: &str) -> Option<&'static EndpointRegion> {
        match self.info().endpoint {
            EndpointKind::Regions(regions) => regions.iter().find(|region| region.id == region_id),
            EndpointKind::Fixed(_) | EndpointKind::UserBaseUrl => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{EndpointKind, ImageEditProvider, ProviderKeySlot, RussiaStatus};
    use crate::service::AiApiService;

    #[test]
    fn characterize_provider_keys() {
        let keys: Vec<&str> = ImageEditProvider::ALL.iter().map(|provider| provider.key()).collect();
        assert_eq!(
            keys,
            [
                "openai", "gemini", "bfl", "xai", "ideogram", "recraft", "runway", "luma", "dashscope", "tencent", "kling", "byteplus", "openrouter", "fal", "replicate", "together", "deepinfra", "runware", "aimlapi", "aitunnel", "proxyapi", "routerai", "polza", "genapi", "openai_compatible"
            ]
        );
        for provider in ImageEditProvider::ALL {
            assert_eq!(ImageEditProvider::from_key(provider.key()), Some(provider));
            assert!(!provider.key().is_empty() && !provider.key().contains('@') && !provider.key().contains(':'));
        }
        assert_eq!(ImageEditProvider::from_key("unknown"), None);
    }

    #[test]
    fn russia_status_per_provider_matches_the_research_table() {
        use ImageEditProvider as P;
        let expected = [
            (P::OpenAi, Some(RussiaStatus::Blocked)),
            (P::Gemini, Some(RussiaStatus::Blocked)),
            (P::OpenRouter, Some(RussiaStatus::Blocked)),
            (P::BytePlus, Some(RussiaStatus::Blocked)),
            (P::Bfl, Some(RussiaStatus::PaymentIssues)),
            (P::Xai, Some(RussiaStatus::PaymentIssues)),
            (P::Ideogram, Some(RussiaStatus::PaymentIssues)),
            (P::Recraft, Some(RussiaStatus::PaymentIssues)),
            (P::Runway, Some(RussiaStatus::PaymentIssues)),
            (P::Luma, Some(RussiaStatus::PaymentIssues)),
            (P::DashScope, Some(RussiaStatus::PaymentIssues)),
            (P::Tencent, Some(RussiaStatus::PaymentIssues)),
            (P::Kling, Some(RussiaStatus::PaymentIssues)),
            (P::Fal, Some(RussiaStatus::PaymentIssues)),
            (P::Replicate, Some(RussiaStatus::PaymentIssues)),
            (P::Together, Some(RussiaStatus::PaymentIssues)),
            (P::DeepInfra, Some(RussiaStatus::PaymentIssues)),
            (P::Runware, Some(RussiaStatus::PaymentIssues)),
            (P::AimlApi, Some(RussiaStatus::PaymentIssues)),
            (P::AiTunnel, Some(RussiaStatus::Works)),
            (P::ProxyApi, Some(RussiaStatus::Works)),
            (P::RouterAi, Some(RussiaStatus::Works)),
            (P::Polza, Some(RussiaStatus::Works)),
            (P::GenApi, Some(RussiaStatus::Works)),
            (P::OpenAiCompatible, None),
        ];
        assert_eq!(expected.len(), ImageEditProvider::ALL.len());
        for (provider, status) in expected {
            assert_eq!(provider.info().russia.map(|access| access.status), status, "{provider:?}");
        }
    }

    #[test]
    fn shared_chat_keys_use_the_chat_service_entries() {
        let shared: Vec<(ImageEditProvider, AiApiService)> = ImageEditProvider::ALL
            .into_iter()
            .filter_map(|provider| match provider.info().key_slot {
                ProviderKeySlot::SharedChat(service) => Some((provider, service)),
                ProviderKeySlot::SharedCompatible | ProviderKeySlot::Named | ProviderKeySlot::NamedPerRegion => None,
            })
            .collect();
        assert_eq!(shared, [(ImageEditProvider::OpenAi, AiApiService::OpenAi), (ImageEditProvider::Gemini, AiApiService::Gemini), (ImageEditProvider::Xai, AiApiService::Xai), (ImageEditProvider::OpenRouter, AiApiService::OpenRouter)]);
        assert_eq!(ImageEditProvider::OpenAiCompatible.info().key_slot, ProviderKeySlot::SharedCompatible);
        assert!(!ImageEditProvider::OpenAiCompatible.requires_key());
        assert!(ImageEditProvider::Bfl.requires_key());
    }

    #[test]
    fn named_key_users_are_frozen() {
        assert_eq!(ImageEditProvider::Bfl.named_key_user(None).map(|user| user.as_str().to_string()).as_deref(), Some("image_edit:bfl"));
        assert_eq!(ImageEditProvider::Bfl.named_key_user(Some("eu")).map(|user| user.as_str().to_string()).as_deref(), Some("image_edit:bfl"));
        assert_eq!(ImageEditProvider::Fal.named_key_user(None).map(|user| user.as_str().to_string()).as_deref(), Some("image_edit:fal"));
        assert_eq!(ImageEditProvider::DashScope.named_key_user(Some("intl")).map(|user| user.as_str().to_string()).as_deref(), Some("image_edit:dashscope@intl"));
        assert!(ImageEditProvider::DashScope.named_key_user(None).is_none());
        assert!(ImageEditProvider::OpenAi.named_key_user(None).is_none());
        assert!(ImageEditProvider::OpenAiCompatible.named_key_user(None).is_none());
    }

    #[test]
    fn endpoints_are_https_without_trailing_slash_and_regions_are_frozen() {
        for provider in ImageEditProvider::ALL {
            let urls: Vec<&str> = match provider.info().endpoint {
                EndpointKind::Fixed(url) => vec![url],
                EndpointKind::Regions(regions) => {
                    assert!(!regions.is_empty(), "{provider:?}");
                    regions.iter().map(|region| region.base_url).collect()
                }
                EndpointKind::UserBaseUrl => {
                    assert_eq!(provider, ImageEditProvider::OpenAiCompatible);
                    Vec::new()
                }
            };
            for url in urls {
                assert!(url.starts_with("https://") && !url.ends_with('/'), "{provider:?}: {url}");
            }
            assert!(provider.info().docs_url.starts_with("https://"), "{provider:?}");
        }
        let region_ids = |provider: ImageEditProvider| match provider.info().endpoint {
            EndpointKind::Regions(regions) => regions.iter().map(|region| region.id).collect::<Vec<_>>(),
            EndpointKind::Fixed(_) | EndpointKind::UserBaseUrl => Vec::new(),
        };
        assert_eq!(region_ids(ImageEditProvider::Bfl), ["global", "eu", "us"]);
        assert_eq!(region_ids(ImageEditProvider::DashScope), ["intl", "cn"]);
        assert!(ImageEditProvider::DashScope.region("intl").is_some());
        assert!(ImageEditProvider::DashScope.region("eu").is_none());
        // Region-bound keys need a region list to bind to.
        for provider in ImageEditProvider::ALL {
            if provider.info().key_slot == ProviderKeySlot::NamedPerRegion {
                assert!(!region_ids(provider).is_empty(), "{provider:?}");
            }
        }
    }
}
