/*
File: crates/ms-ai-api/src/quota.rs

Purpose:
Provider-agnostic classification of an AI provider error text as "out of credits / quota /
rate limited", so a consumer can show a softer notice instead of a hard error.

Key functions:
- is_probable_quota_or_limit_error()
*/

/// Keyword fragments (lowercase) that strongly suggest an AI provider stopped the request because
/// the account is out of credits/quota or hit a usage/rate limit. Kept provider-agnostic so it
/// covers `OpenAI`, Anthropic, Gemini, `OpenRouter` and similar wordings, plus the HTTP 402/429 codes
/// these providers return for billing/rate problems.
const AI_QUOTA_LIMIT_ERROR_KEYWORDS: &[&str] = &[
    "insufficient_quota",
    "insufficient quota",
    "insufficient credit",
    "insufficient funds",
    "not enough credit",
    "out of credit",
    "no credits",
    "quota exceeded",
    "exceeded your current quota",
    "exceeded your quota",
    "usage limit",
    "monthly limit",
    "spending limit",
    "limit reached",
    "limit exceeded",
    "rate limit",
    "rate_limit",
    "ratelimit",
    "too many requests",
    "resource_exhausted",
    "resource exhausted",
    "credit balance is too low",
    "payment required",
    "billing",
    "status code: 402",
    "status code: 429",
    "status: 402",
    "status: 429",
    "error 402",
    "error 429",
    "http 402",
    "http 429",
];

/// Best-effort classification of an AI provider error string as a credit/quota/limit exhaustion
/// rather than a transient network glitch or a configuration mistake.
///
/// Matching is a case-insensitive substring scan over [`AI_QUOTA_LIMIT_ERROR_KEYWORDS`], so it stays
/// provider-agnostic. It is intentionally a heuristic: a false positive only changes a red error
/// toast into the softer "probably out of credits/limit" notice, and the full original error is
/// always still available to the user.
#[must_use]
pub fn is_probable_quota_or_limit_error(error: &str) -> bool {
    let haystack = error.to_ascii_lowercase();
    AI_QUOTA_LIMIT_ERROR_KEYWORDS
        .iter()
        .any(|keyword| haystack.contains(keyword))
}

#[cfg(test)]
mod tests {
    use super::is_probable_quota_or_limit_error;

    #[test]
    fn quota_limit_classifier_matches_common_provider_wordings() {
        let positives = [
            "AI перевод не выполнен: 429 Too Many Requests",
            "error: insufficient_quota - You exceeded your current quota",
            "Your credit balance is too low to access the Claude API",
            "RESOURCE_EXHAUSTED: Quota exceeded for gemini",
            "OpenRouter: Insufficient credits (status code: 402)",
        ];
        for error in positives {
            assert!(
                is_probable_quota_or_limit_error(error),
                "expected quota/limit match for: {error}"
            );
        }
    }

    #[test]
    fn quota_limit_classifier_ignores_unrelated_errors() {
        let negatives = [
            "AI вернул невалидный JSON: expected value at line 1",
            "Не удалось создать async runtime для AI перевода",
            "connection reset by peer",
        ];
        for error in negatives {
            assert!(
                !is_probable_quota_or_limit_error(error),
                "did not expect quota/limit match for: {error}"
            );
        }
    }
}
