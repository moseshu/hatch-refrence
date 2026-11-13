//! Pseudonymise personal identifiers before text is sent to a model.
//!
//! `secrets::redact` covers credentials, OTPs and card numbers, but an
//! email address is not a secret in that sense — it is an identifier
//! the model still needs to *distinguish* (two contacts sharing one
//! name must stay two people). So instead of deleting addresses, this
//! module maps each one to a stable code inside one task:
//!
//! * the mailbox owner is always `我 <me@example.invalid>`;
//! * every other address becomes `发件人A <sender-a@example.invalid>`,
//!   `发件人B <sender-b@example.invalid>`, … in address-sorted order,
//!   so the same address always maps to the same code wherever the
//!   mapping is rebuilt from the same set;
//! * `forward` rewrites a text into code space (known addresses and
//!   known names, plus any other email-like string, which gets a
//!   hash-derived fallback code — except an email-like token inside
//!   angle brackets, which is a message-id, not an address, and is
//!   left alone so citations survive); `reverse` maps codes back to
//!   the real names and addresses, locally, before anything is
//!   delivered to the user — including fallback codes this same
//!   mapper issued (it records them; a rebuilt mapper has no record
//!   of another instance's fallbacks, so the known set should cover
//!   every sender that matters). The mapping table itself is never
//!   sent to a model.
//!
//! Canonical build rule — every call site must build from the same
//! set, or the codes will disagree: owner + all mail participants +
//! all contacts, sorted by lowercased address. See
//! `hatch_triage::desensitize::mapper_for` for the database-backed
//! builder; `hatch-tools` builds the same set inline in `mail_search`.
//!
//! One ambiguity is inherent: a bare name shared by two addresses
//! (two contacts both called 李婷) cannot be told apart from the name
//! alone. `forward_with` takes the mail's sender as a hint and prefers
//! that entry for a bare name; without a hint the first entry wins.
//! The envelope header is always built from `code_for` (name and
//! address paired for that one entry), never from free-text
//! replacement, so a header can never mix two entries' codes.

use regex::Regex;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// The fake domain every code address lives under. `.invalid` can
/// never resolve, so a code can never be mistaken for a real address.
pub const CODE_DOMAIN: &str = "example.invalid";
/// Code address of the mailbox owner.
pub const OWNER_CODE_ADDR: &str = "me@example.invalid";
/// Code name of the mailbox owner.
pub const OWNER_CODE_NAME: &str = "我";

/// One mapped identity: a real address + name and its code form.
#[derive(Clone, Debug)]
struct Entry {
    real_addr: String,
    real_name: String,
    code_name: String,
    code_addr: String,
}

/// Stable per-task mapping from real addresses/names to code names and
/// code addresses. `Clone` so a worker can hand the same mapping to
/// the gather turn and to the artifact step.
#[derive(Clone, Debug)]
pub struct AddressMapper {
    entries: Vec<Entry>,
    /// lowercased real address → entry index.
    by_addr: HashMap<String, usize>,
    /// lowercased code address → entry index.
    by_code_addr: HashMap<String, usize>,
    /// code name → entry index (code names are unique by construction).
    by_code_name: HashMap<String, usize>,
    /// Matches any known non-owner real address, case-insensitively.
    addr_re: Option<Regex>,
    /// Matches any known real name.
    name_re: Option<Regex>,
    /// Matches any code address, case-insensitively.
    code_addr_re: Option<Regex>,
    /// Matches any code name.
    code_name_re: Option<Regex>,
    /// Fallback codes this instance issued (code → original text),
    /// so `reverse` can restore an unknown address the model echoed.
    /// Shared across clones: a worker that hands the same mapping to
    /// the gather turn and to the artifact step shares one record.
    /// A mapper rebuilt from scratch has an empty record — the
    /// fallback code is a hash and cannot be inverted without it.
    fallbacks: Arc<Mutex<HashMap<String, String>>>,
}

/// Letters for the n-th non-owner entry (0-based): a, …, z, aa, ab, …
fn letters(mut n: usize) -> String {
    let mut out = String::new();
    loop {
        out.push((b'a' + (n % 26) as u8) as char);
        n /= 26;
        if n == 0 {
            break;
        }
        n -= 1;
    }
    out.chars().rev().collect()
}

fn email_re() -> Regex {
    Regex::new(r"[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}").expect("email regex")
}

impl AddressMapper {
    /// Build the mapping. `contacts` are `(address, name)` pairs; they
    /// are deduplicated by lowercased address and sorted by it, so two
    /// builds from the same set assign the same codes. The owner is
    /// always entry zero and is never assigned a letter.
    pub fn build(owner_addr: &str, owner_name: &str, contacts: &[(String, String)]) -> Self {
        let mut entries = vec![Entry {
            real_addr: owner_addr.to_string(),
            real_name: owner_name.to_string(),
            code_name: OWNER_CODE_NAME.to_string(),
            code_addr: OWNER_CODE_ADDR.to_string(),
        }];
        let owner_lower = owner_addr.to_ascii_lowercase();
        let mut seen: Vec<(String, (String, String))> = contacts
            .iter()
            .filter(|(a, _)| !a.is_empty() && a.to_ascii_lowercase() != owner_lower)
            .map(|(a, n)| (a.to_ascii_lowercase(), (a.clone(), n.clone())))
            .collect();
        seen.sort_by(|a, b| a.0.cmp(&b.0));
        seen.dedup_by(|a, b| a.0 == b.0);
        for (i, (_, (addr, name))) in seen.into_iter().enumerate() {
            let code = letters(i);
            entries.push(Entry {
                real_addr: addr,
                real_name: name,
                code_name: format!("发件人{}", code.to_ascii_uppercase()),
                code_addr: format!("sender-{code}@{CODE_DOMAIN}"),
            });
        }
        let by_addr: HashMap<String, usize> = entries
            .iter()
            .enumerate()
            .map(|(i, e)| (e.real_addr.to_ascii_lowercase(), i))
            .collect();
        let by_code_addr: HashMap<String, usize> = entries
            .iter()
            .enumerate()
            .map(|(i, e)| (e.code_addr.clone(), i))
            .collect();
        let by_code_name: HashMap<String, usize> = entries
            .iter()
            .enumerate()
            .map(|(i, e)| (e.code_name.clone(), i))
            .collect();
        // Longest first, so "发件人AA" wins over "发件人A" and a long
        // address wins over one that is its prefix.
        let mut by_len: Vec<&Entry> = entries.iter().skip(1).collect();
        by_len.sort_by_key(|e| std::cmp::Reverse(e.real_addr.len()));
        let addr_re = if by_len.is_empty() {
            None
        } else {
            let pat = by_len
                .iter()
                .map(|e| regex::escape(&e.real_addr))
                .collect::<Vec<_>>()
                .join("|");
            Regex::new(&format!("(?i)(?:{pat})")).ok()
        };
        let mut by_name_len: Vec<&Entry> = entries.iter().collect();
        by_name_len.sort_by_key(|e| std::cmp::Reverse(e.real_name.chars().count()));
        let name_re = {
            let pat = by_name_len
                .iter()
                .filter(|e| !e.real_name.is_empty())
                .map(|e| regex::escape(&e.real_name))
                .collect::<Vec<_>>()
                .join("|");
            if pat.is_empty() {
                None
            } else {
                Regex::new(&format!("(?:{pat})")).ok()
            }
        };
        let mut by_code_len: Vec<&Entry> = entries.iter().collect();
        by_code_len.sort_by_key(|e| std::cmp::Reverse(e.code_addr.len()));
        let code_addr_re = {
            let pat = by_code_len
                .iter()
                .map(|e| regex::escape(&e.code_addr))
                .collect::<Vec<_>>()
                .join("|");
            Regex::new(&format!("(?i)(?:{pat})")).ok()
        };
        let mut by_code_name_len: Vec<&Entry> = entries.iter().skip(1).collect();
        by_code_name_len.sort_by_key(|e| std::cmp::Reverse(e.code_name.chars().count()));
        let code_name_re = {
            let pat = by_code_name_len
                .iter()
                // An entry with no known real name (a to-only address)
                // must never be reversed into an empty string.
                .filter(|e| !e.real_name.is_empty())
                .map(|e| regex::escape(&e.code_name))
                .collect::<Vec<_>>()
                .join("|");
            if pat.is_empty() {
                None
            } else {
                Regex::new(&format!("(?:{pat})")).ok()
            }
        };
        AddressMapper {
            entries,
            by_addr,
            by_code_addr,
            by_code_name,
            addr_re,
            name_re,
            code_addr_re,
            code_name_re,
            fallbacks: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Code name of the mailbox owner (`我`).
    pub fn owner_code_name(&self) -> &str {
        OWNER_CODE_NAME
    }

    /// Code address of the mailbox owner.
    pub fn owner_code_addr(&self) -> &str {
        OWNER_CODE_ADDR
    }

    /// `(code name, code address)` for a real address, if it is mapped.
    /// Unknown addresses get a hash-derived fallback from `forward`;
    /// this lookup is only for the known set.
    pub fn code_for(&self, addr: &str) -> Option<(&str, &str)> {
        let i = *self.by_addr.get(&addr.to_ascii_lowercase())?;
        let e = &self.entries[i];
        Some((&e.code_name, &e.code_addr))
    }

    /// Code name for a real address, if mapped.
    pub fn code_name_for(&self, addr: &str) -> Option<&str> {
        self.code_for(addr).map(|(n, _)| n)
    }

    /// Code address for a real address, if mapped.
    pub fn code_addr_for(&self, addr: &str) -> Option<&str> {
        self.code_for(addr).map(|(_, a)| a)
    }
}

mod forward;

#[cfg(test)]
mod tests;
