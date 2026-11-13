//! `forward` / `reverse` for [`super::AddressMapper`]: the text
//! rewriting. Split out of `super` to keep each file under the
//! 400-line rule; the methods live on the same struct, so the
//! public API is unchanged.

use super::{email_re, AddressMapper, CODE_DOMAIN, OWNER_CODE_ADDR};
use regex::Regex;
use sha2::{Digest, Sha256};
use std::collections::HashSet;

impl AddressMapper {
    /// Fallback code for an address outside the known set: stable
    /// across rebuilds (it is a hash of the address), so every call
    /// site agrees on it. `forward` records the code it issues, and
    /// `reverse` restores it — but only within the same mapper
    /// instance (clones share the record); a rebuilt mapper has no
    /// record and leaves the code alone. Prefer the known set for
    /// senders that matter.
    fn fallback_code(addr: &str) -> String {
        let digest = Sha256::digest(addr.to_ascii_lowercase().as_bytes());
        format!("sender-{}@{CODE_DOMAIN}", hex::encode(&digest[..4]))
    }

    /// Rewrite text into code space, with no sender hint. Prefer
    /// [`Self::forward_with`] when the text belongs to one mail, so a
    /// bare shared name resolves to that mail's sender.
    pub fn forward(&self, text: &str) -> String {
        self.forward_with(text, None)
    }

    /// Rewrite text into code space: known addresses → code addresses,
    /// known names → code names, any other email-like string → a
    /// hash-derived fallback code. Code addresses pass through
    /// unchanged, so `forward` is idempotent. `preferred` is the
    /// address of the mail this text comes from; a bare name shared by
    /// several addresses resolves to it when it carries that name.
    pub fn forward_with(&self, text: &str, preferred: Option<&str>) -> String {
        let mut out = text.to_string();
        // 1. Known addresses (owner handled separately below).
        let mut found: HashSet<usize> = HashSet::new();
        if let Some(re) = &self.addr_re {
            let by_addr = &self.by_addr;
            let entries = &self.entries;
            out = re
                .replace_all(&out, |caps: &regex::Captures| {
                    let m = caps.get(0).expect("addr match").as_str();
                    let i = by_addr[&m.to_ascii_lowercase()];
                    found.insert(i);
                    entries[i].code_addr.clone()
                })
                .into_owned();
        }
        // The owner address is entry zero and is not in `addr_re`;
        // replace it explicitly (it is the one address that must
        // never reach a model).
        let owner = &self.entries[0];
        if !owner.real_addr.is_empty() {
            let re = Regex::new(&format!("(?i){}", regex::escape(&owner.real_addr)))
                .expect("owner addr regex");
            if re.is_match(&out) {
                found.insert(0);
            }
            out = re
                .replace_all(&out, regex::NoExpand(OWNER_CODE_ADDR))
                .into_owned();
        }
        // 2. Known names. A name shared by several entries resolves
        // to the preferred sender, else to an entry whose address was
        // found in this text, else to the first entry carrying it.
        if let Some(re) = &self.name_re {
            let entries = &self.entries;
            let preferred_idx =
                preferred.and_then(|a| self.by_addr.get(&a.to_ascii_lowercase()).copied());
            out = re
                .replace_all(&out, |caps: &regex::Captures| {
                    let m = caps.get(0).expect("name match").as_str();
                    let candidates: Vec<usize> = entries
                        .iter()
                        .enumerate()
                        .filter(|(_, e)| e.real_name == m)
                        .map(|(i, _)| i)
                        .collect();
                    let pick = preferred_idx
                        .filter(|i| candidates.contains(i))
                        .or_else(|| candidates.iter().copied().find(|i| found.contains(i)))
                        .or_else(|| candidates.first().copied());
                    pick.map(|i| entries[i].code_name.clone())
                        .unwrap_or_else(|| m.to_string())
                })
                .into_owned();
        }
        // 3. Any other email-like string gets a hash-derived fallback
        // code; code addresses already end in the code domain and
        // pass through untouched. An email-like token inside angle
        // brackets is a message-id (`[<m201@corp.example>]`), not an
        // address — it is left alone so citations survive the round
        // trip (a known address in brackets was already replaced in
        // step 1). Issued fallback codes are recorded, so `reverse`
        // can restore an unknown address the model echoes.
        let email = email_re();
        let fallbacks = &self.fallbacks;
        out = email
            .replace_all(&out, |caps: &regex::Captures| {
                let m = caps.get(0).expect("email match");
                let s = m.as_str();
                if s.to_ascii_lowercase().ends_with(&format!("@{CODE_DOMAIN}")) {
                    return s.to_string();
                }
                let bracketed = m.start() > 0
                    && out.as_bytes()[m.start() - 1] == b'<'
                    && m.end() < out.len()
                    && out.as_bytes()[m.end()] == b'>';
                if bracketed {
                    return s.to_string();
                }
                let code = Self::fallback_code(s);
                fallbacks
                    .lock()
                    .expect("fallbacks lock")
                    .insert(code.clone(), s.to_string());
                code
            })
            .into_owned();
        out
    }

    /// Map codes back to real names and addresses. Pure local string
    /// replacement; nothing here is sent to a model. Fallback codes
    /// this same mapper issued are restored from its record; a
    /// fallback code from another mapper instance (a rebuild has an
    /// empty record) is left alone — the hash cannot be inverted.
    ///
    /// The owner's code name `我` is a pronoun, so a bare `我` in
    /// model output is left alone — it is only restored as part of
    /// the pair `我 <me@example.invalid>`, which the envelope header
    /// always produces. (A brief addressed to the owner may therefore
    /// keep a `请我确认`; that reads naturally for its reader.)
    pub fn reverse(&self, text: &str) -> String {
        let mut out = text.to_string();
        let owner = &self.entries[0];
        if !owner.real_name.is_empty() && !owner.real_addr.is_empty() {
            let pair = Regex::new(&format!("我\\s*<(?i){}>", regex::escape(OWNER_CODE_ADDR)))
                .expect("owner pair regex");
            let replacement = format!("{} <{}>", owner.real_name, owner.real_addr);
            out = pair
                .replace_all(&out, regex::NoExpand(&replacement))
                .into_owned();
        }
        if let Some(re) = &self.code_addr_re {
            let by_code_addr = &self.by_code_addr;
            let entries = &self.entries;
            out = re
                .replace_all(&out, |caps: &regex::Captures| {
                    let m = caps.get(0).expect("code addr match").as_str();
                    let i = by_code_addr[&m.to_ascii_lowercase()];
                    entries[i].real_addr.clone()
                })
                .into_owned();
        }
        // Code names of non-owner entries only — see the doc above
        // for why the owner's `我` is excluded.
        if let Some(re) = &self.code_name_re {
            let by_code_name = &self.by_code_name;
            let entries = &self.entries;
            out = re
                .replace_all(&out, |caps: &regex::Captures| {
                    let m = caps.get(0).expect("code name match").as_str();
                    let i = by_code_name[m];
                    entries[i].real_name.clone()
                })
                .into_owned();
        }
        // Fallback codes this mapper issued: restore the original
        // address the model echoed. (Codes from another instance
        // stay — a rebuild has no record of them.)
        for (code, original) in self.fallbacks.lock().expect("fallbacks lock").iter() {
            if out.contains(code.as_str()) {
                out = out.replace(code.as_str(), original);
            }
        }
        out
    }

    /// True if the text still carries a real address: a known one, or
    /// any email-like string outside the code domain.
    pub fn has_real_address(&self, text: &str) -> bool {
        if let Some(re) = &self.addr_re {
            if re.is_match(text) {
                return true;
            }
        }
        let owner = &self.entries[0];
        if !owner.real_addr.is_empty()
            && text
                .to_ascii_lowercase()
                .contains(&owner.real_addr.to_ascii_lowercase())
        {
            return true;
        }
        let email = email_re();
        let leaked = email.find_iter(text).any(|m| {
            !m.as_str()
                .to_ascii_lowercase()
                .ends_with(&format!("@{CODE_DOMAIN}"))
        });
        leaked
    }
}
