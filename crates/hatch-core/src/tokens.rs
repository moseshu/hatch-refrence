//! CJK-aware token estimation.
//!
//! The classic `chars / 4` heuristic undercounts Chinese text by 3–6x (one Han
//! character is roughly one token for most tokenizers). Underestimating means
//! compaction fires too late and requests overflow the context window, so we
//! count CJK and full-width characters individually and everything else at 4
//! characters per token. Provider-reported usage remains the source of truth
//! for accounting; this is only for budgeting before a request is sent.

/// True for Han, Kana, Hangul, CJK punctuation and full-width forms.
pub fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x3000..=0x303F   // CJK symbols & punctuation
        | 0x3040..=0x30FF // Hiragana, Katakana
        | 0x3400..=0x4DBF // CJK ext A
        | 0x4E00..=0x9FFF // CJK unified
        | 0xAC00..=0xD7AF // Hangul
        | 0xF900..=0xFAFF // CJK compatibility
        | 0xFF00..=0xFFEF // half/full-width forms
        | 0x20000..=0x2FA1F)
}

/// Estimated token count of `text`.
pub fn estimate_tokens(text: &str) -> usize {
    let mut cjk = 0usize;
    let mut other = 0usize;
    for c in text.chars() {
        if is_cjk(c) {
            cjk += 1;
        } else {
            other += 1;
        }
    }
    cjk + other.div_ceil(4)
}

/// Flat token estimate charged to the context budget for one image part
/// in a request. Images have no characters for `estimate_tokens` to
/// count, so without this the budget math would pretend they are free
/// and compaction would fire too late.
///
/// 1500 covers a typical viewport screenshot (~1280x800) on every
/// provider hatch talks to: OpenAI bills about 85 + 170 per 512px tile
/// (~1105 for that size), Gemini a flat 258 per image, Anthropic roughly
/// width x height / 750 (~1365). A very tall full-page capture can cost
/// more — provider-reported usage stays the source of truth for billing;
/// this number only drives pre-send budgeting and compaction, and it
/// errs on the side of dropping history early.
pub const IMAGE_TOKENS_ESTIMATE: usize = 1500;

/// Estimated token cost of one image part (see [`IMAGE_TOKENS_ESTIMATE`]).
pub fn estimate_image_tokens() -> usize {
    IMAGE_TOKENS_ESTIMATE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_is_quarter() {
        assert_eq!(estimate_tokens("abcdefgh"), 2);
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abc"), 1);
    }

    #[test]
    fn chinese_counts_per_char() {
        // chars/4 would say 2; real tokenizers say ~8.
        assert_eq!(estimate_tokens("明天下午三点开会"), 8);
        assert_eq!(estimate_tokens("会议 meeting"), 2 + 2);
        assert_eq!(estimate_tokens("，。"), 2);
    }
}
