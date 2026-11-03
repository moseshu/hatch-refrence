//! CJK-aware lexical retrieval.
//!
//! Chinese has no spaces, so plain keyword matching misses "开会" in "下午开会".
//! The tokenizer emits ASCII words plus CJK unigrams *and* bigrams; BM25 then
//! ranks documents so that bigram matches (more specific) dominate unigram
//! noise. This is the same family as the ranked lexical retrieval observed in
//! the reference product, and needs no embedding model.

use hatch_core::tokens::is_cjk;
use std::collections::HashMap;

/// Lower-cased ASCII/latin words + CJK unigrams + CJK bigrams.
pub fn tokenize(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut word = String::new();
    let mut run: Vec<char> = Vec::new();
    let flush_run = |run: &mut Vec<char>, out: &mut Vec<String>| {
        for (i, c) in run.iter().enumerate() {
            out.push(c.to_string());
            if i + 1 < run.len() {
                out.push(format!("{c}{}", run[i + 1]));
            }
        }
        run.clear();
    };
    for c in text.chars() {
        if is_cjk(c) && !matches!(c as u32, 0x3000..=0x303F | 0xFF00..=0xFF0F | 0xFF1A..=0xFF20) {
            if !word.is_empty() {
                out.push(std::mem::take(&mut word));
            }
            run.push(c);
        } else if c.is_alphanumeric() || c == '@' || (c == '.' && !word.is_empty()) {
            flush_run(&mut run, &mut out);
            word.extend(c.to_lowercase());
        } else {
            flush_run(&mut run, &mut out);
            if !word.is_empty() {
                out.push(std::mem::take(&mut word).trim_end_matches('.').to_string());
            }
        }
    }
    flush_run(&mut run, &mut out);
    if !word.is_empty() {
        out.push(word.trim_end_matches('.').to_string());
    }
    out.retain(|t| !t.is_empty());
    out
}

/// In-memory BM25 index (k1 = 1.2, b = 0.75).
pub struct Bm25Index {
    docs: Vec<(String, HashMap<String, usize>, usize)>,
    df: HashMap<String, usize>,
    avgdl: f64,
}

impl Bm25Index {
    pub fn build<'a>(docs: impl IntoIterator<Item = (&'a str, String)>) -> Self {
        let mut out = Vec::new();
        let mut df: HashMap<String, usize> = HashMap::new();
        let mut total = 0usize;
        for (id, text) in docs {
            let toks = tokenize(&text);
            total += toks.len();
            let mut tf: HashMap<String, usize> = HashMap::new();
            for t in toks.iter() {
                *tf.entry(t.clone()).or_default() += 1;
            }
            for t in tf.keys() {
                *df.entry(t.clone()).or_default() += 1;
            }
            out.push((id.to_string(), tf, toks.len()));
        }
        let avgdl = if out.is_empty() { 1.0 } else { (total as f64 / out.len() as f64).max(1.0) };
        Bm25Index { docs: out, df, avgdl }
    }

    pub fn len(&self) -> usize {
        self.docs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.docs.is_empty()
    }

    /// Top-`k` `(id, score)` with score > 0, best first.
    pub fn query(&self, q: &str, k: usize) -> Vec<(String, f64)> {
        let (k1, b) = (1.2, 0.75);
        let n = self.docs.len() as f64;
        let mut qt = tokenize(q);
        qt.sort();
        qt.dedup();
        let mut scored: Vec<(String, f64)> = self
            .docs
            .iter()
            .filter_map(|(id, tf, len)| {
                let mut s = 0.0;
                for t in &qt {
                    let Some(&f) = tf.get(t) else { continue };
                    let df = *self.df.get(t).unwrap_or(&0) as f64;
                    let idf = ((n - df + 0.5) / (df + 0.5) + 1.0).ln();
                    let f = f as f64;
                    // Bigrams carry more meaning than single characters.
                    let weight = if t.chars().count() == 1 && is_cjk(t.chars().next().unwrap()) { 0.3 } else { 1.0 };
                    s += weight * idf * f * (k1 + 1.0) / (f + k1 * (1.0 - b + b * *len as f64 / self.avgdl));
                }
                (s > 0.0).then(|| (id.clone(), s))
            })
            .collect();
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(k);
        scored
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenizes_mixed_text() {
        let t = tokenize("下午开会 Meeting@3pm，见 bob@x.com");
        assert!(t.contains(&"开会".to_string()));
        assert!(t.contains(&"会".to_string()));
        assert!(t.contains(&"meeting@3pm".to_string()));
        assert!(t.contains(&"bob@x.com".to_string()));
        assert!(!t.iter().any(|x| x == "，"));
    }

    #[test]
    fn chinese_bm25_prefers_bigram_matches() {
        let idx = Bm25Index::build(vec![
            ("a", "用户喜欢简短的回复，不要客套".to_string()),
            ("b", "张总是重要客户，邮件要当天回复".to_string()),
            ("c", "每周一上午有部门例会".to_string()),
        ]);
        let hits = idx.query("回复风格要简短", 3);
        assert_eq!(hits[0].0, "a");
        let hits = idx.query("例会时间", 3);
        assert_eq!(hits[0].0, "c");
        assert!(idx.query("zzz", 3).is_empty());
    }

    #[test]
    fn english_words() {
        let idx = Bm25Index::build(vec![
            ("x", "Alice prefers morning meetings".to_string()),
            ("y", "Bob owns the budget".to_string()),
        ]);
        assert_eq!(idx.query("who owns budget?", 1)[0].0, "y");
    }
}
