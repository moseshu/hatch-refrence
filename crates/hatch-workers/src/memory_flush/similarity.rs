//! Text similarity for near-duplicate claims: Dice coefficient over character
//! bigrams of the normalised text (case, whitespace and punctuation removed).
//! Works the same for Chinese and English without a tokenizer.

use std::collections::HashMap;

pub fn normalise(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

fn bigrams(s: &str) -> HashMap<(char, char), usize> {
    let chars: Vec<char> = s.chars().collect();
    let mut m = HashMap::new();
    if chars.len() == 1 {
        *m.entry((chars[0], '\0')).or_insert(0) += 1;
    }
    for w in chars.windows(2) {
        *m.entry((w[0], w[1])).or_insert(0) += 1;
    }
    m
}

/// 1.0 for identical normalised text, 0.0 for nothing in common.
pub fn similarity(a: &str, b: &str) -> f64 {
    let (a, b) = (normalise(a), normalise(b));
    if a == b {
        return 1.0;
    }
    let (ba, bb) = (bigrams(&a), bigrams(&b));
    let total: usize = ba.values().sum::<usize>() + bb.values().sum::<usize>();
    if total == 0 {
        return 0.0;
    }
    let common: usize = ba.iter().map(|(k, n)| (*n).min(*bb.get(k).unwrap_or(&0))).sum();
    2.0 * common as f64 / total as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn punctuation_and_case_do_not_matter_meaning_changes_do() {
        assert_eq!(similarity("喜欢简短的回复。", "喜欢简短的回复"), 1.0);
        assert_eq!(similarity("Prefers SHORT replies", "prefers short replies!"), 1.0);
        assert!(similarity("周三下午不开会", "周三下午可以开会") < 0.75);
        assert!(similarity("回复邮件请用中文，语气简洁", "回复邮件请用中文，语气要简洁") > 0.85);
        assert_eq!(similarity("", "abc"), 0.0);
    }
}
