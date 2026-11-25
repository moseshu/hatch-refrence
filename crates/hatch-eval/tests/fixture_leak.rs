//! Eval fixtures must not leak into the product.
//!
//! A harness fix that hard-codes an evaluation's data (a ticket id, a
//! contact address) passes that world without making the agent better
//! anywhere else. This test makes the "fix the capability, not the
//! scenario" rule mechanical for the clearest case:
//!
//! * markers are extracted automatically from every file under
//!   `eval/worlds/` with generic patterns (email addresses and
//!   ticket-style ids such as `AB-1234`), so a new world is covered the
//!   day it lands, with no per-scenario list to maintain;
//! * the scan covers everything that can reach the model or steer the
//!   agent at runtime: Rust sources of every product crate (all crates
//!   except `hatch-eval`) and the bundled prompts, skills, config
//!   example and verification rules;
//! * Rust syntax determines which items and modules are test-only.
//!   Comments are omitted and string literals remain part of the scan;
//! * literal Rust includes must not embed files from `eval/worlds/`.
//!
//! Markers from a private fixture set (hidden rule packs kept outside
//! the repository) can be added with `HATCH_LEAK_PRIVATE_FIXTURES=<dir>`;
//! their hits are reported by file only, never by the marker text.
//!
//! This catches hard-coded data. Overfitting to a task's *structure*
//! is out of its reach — that is what held-out tasks are for.

use proc_macro2::{TokenStream, TokenTree};
use quote::ToTokens;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use syn::parse::Parser;
use syn::punctuated::Punctuated;
use syn::visit_mut::{self, VisitMut};
use syn::{Attribute, Item, LitStr, Meta, Token};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("resolve repository root")
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            walk(&p, out);
        } else {
            out.push(p);
        }
    }
}

/// Markers come from `hatch_eval::markers`, shared with the held-out
/// independence check, so both agree on what counts as fixture data.
fn markers_under(dir: &Path) -> BTreeSet<String> {
    hatch_eval::markers::markers_under(dir)
}

// ---------------------------------------------------------- Rust sources

/// Only remove code that provably needs the built-in `test` cfg.
fn requires_test(meta: &Meta) -> bool {
    match meta {
        Meta::Path(path) => path.is_ident("test"),
        Meta::List(list) if list.path.is_ident("all") || list.path.is_ident("any") => {
            let Ok(predicates) =
                list.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)
            else {
                return false;
            };
            if list.path.is_ident("all") {
                predicates.iter().any(requires_test)
            } else {
                !predicates.is_empty() && predicates.iter().all(requires_test)
            }
        }
        _ => false,
    }
}

fn test_only(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|attr| {
        attr.path().is_ident("cfg")
            && attr
                .parse_args::<Meta>()
                .is_ok_and(|meta| requires_test(&meta))
    })
}

fn item_attrs(item: &Item) -> &[Attribute] {
    match item {
        Item::Const(i) => &i.attrs,
        Item::Enum(i) => &i.attrs,
        Item::ExternCrate(i) => &i.attrs,
        Item::Fn(i) => &i.attrs,
        Item::ForeignMod(i) => &i.attrs,
        Item::Impl(i) => &i.attrs,
        Item::Macro(i) => &i.attrs,
        Item::Mod(i) => &i.attrs,
        Item::Static(i) => &i.attrs,
        Item::Struct(i) => &i.attrs,
        Item::Trait(i) => &i.attrs,
        Item::TraitAlias(i) => &i.attrs,
        Item::Type(i) => &i.attrs,
        Item::Union(i) => &i.attrs,
        Item::Use(i) => &i.attrs,
        _ => &[],
    }
}

struct RuntimeRust {
    module_dir: PathBuf,
    path_attr_dir: PathBuf,
    test_modules: BTreeSet<PathBuf>,
    live_modules: BTreeSet<PathBuf>,
}

impl RuntimeRust {
    fn new(file: &Path) -> Self {
        let parent = file.parent().expect("source has a parent directory");
        let module_dir = match file.file_stem().and_then(|stem| stem.to_str()) {
            Some("lib" | "main" | "mod") | None => parent.to_path_buf(),
            Some(stem) => parent.join(stem),
        };
        Self {
            module_dir,
            path_attr_dir: parent.to_path_buf(),
            test_modules: BTreeSet::new(),
            live_modules: BTreeSet::new(),
        }
    }

    fn record_module(&mut self, item: &syn::ItemMod, gated: bool) {
        let name = item.ident.to_string();
        let explicit = item
            .attrs
            .iter()
            .find_map(|attr| self.explicit_path(&attr.meta));
        let mut paths = explicit.map_or_else(
            || {
                vec![
                    self.module_dir.join(format!("{name}.rs")),
                    self.module_dir.join(name).join("mod.rs"),
                ]
            },
            |path| vec![path],
        );
        let mut conditional_paths = Vec::new();
        for attr in &item.attrs {
            self.conditional_paths(&attr.meta, &mut conditional_paths);
        }
        // A conditional override does not prove that a file is test-only.
        if gated && !conditional_paths.is_empty() {
            return;
        }
        paths.extend(conditional_paths);
        let modules = if gated {
            &mut self.test_modules
        } else {
            &mut self.live_modules
        };
        modules.extend(
            paths
                .into_iter()
                .filter_map(|path| path.canonicalize().ok()),
        );
    }

    fn explicit_path(&self, meta: &Meta) -> Option<PathBuf> {
        match meta {
            Meta::NameValue(pair) if pair.path.is_ident("path") => match &pair.value {
                syn::Expr::Lit(expr) => match &expr.lit {
                    syn::Lit::Str(path) => Some(self.path_attr_dir.join(path.value())),
                    _ => None,
                },
                _ => None,
            },
            _ => None,
        }
    }

    fn conditional_paths(&self, meta: &Meta, paths: &mut Vec<PathBuf>) {
        let Meta::List(list) = meta else { return };
        if !list.path.is_ident("cfg_attr") {
            return;
        }
        if let Ok(args) = list.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated) {
            for attr in args.iter().skip(1) {
                paths.extend(self.explicit_path(attr));
                self.conditional_paths(attr, paths);
            }
        }
    }
}

impl VisitMut for RuntimeRust {
    fn visit_item_mut(&mut self, item: &mut Item) {
        let gated = test_only(item_attrs(item));
        if let Item::Mod(module) = item {
            if module.content.is_none() {
                self.record_module(module, gated);
            } else if !gated {
                let module_dir = module
                    .attrs
                    .iter()
                    .find_map(|attr| self.explicit_path(&attr.meta))
                    .unwrap_or_else(|| self.module_dir.join(module.ident.to_string()));
                let previous_attr_dir =
                    std::mem::replace(&mut self.path_attr_dir, module_dir.clone());
                let previous = std::mem::replace(&mut self.module_dir, module_dir);
                visit_mut::visit_item_mod_mut(self, module);
                self.module_dir = previous;
                self.path_attr_dir = previous_attr_dir;
                return;
            }
        }
        if gated {
            *item = Item::Verbatim(TokenStream::new());
        } else {
            visit_mut::visit_item_mut(self, item);
        }
    }

    fn visit_fields_named_mut(&mut self, fields: &mut syn::FieldsNamed) {
        fields.named = std::mem::take(&mut fields.named)
            .into_iter()
            .filter(|field| !test_only(&field.attrs))
            .collect();
        visit_mut::visit_fields_named_mut(self, fields);
    }

    fn visit_fields_unnamed_mut(&mut self, fields: &mut syn::FieldsUnnamed) {
        fields.unnamed = std::mem::take(&mut fields.unnamed)
            .into_iter()
            .filter(|field| !test_only(&field.attrs))
            .collect();
        visit_mut::visit_fields_unnamed_mut(self, fields);
    }

    fn visit_attributes_mut(&mut self, attrs: &mut Vec<Attribute>) {
        // rustdoc attributes include desugared doc comments.
        attrs.retain(|attr| !attr.path().is_ident("doc"));
        for attr in attrs {
            self.visit_attribute_mut(attr);
        }
    }
}

fn runtime_rust(file: &Path, src: &str) -> (Option<TokenStream>, RuntimeRust) {
    let mut runtime = RuntimeRust::new(file);
    let mut parsed = syn::parse_file(src)
        .unwrap_or_else(|error| panic!("cannot audit Rust source {}: {error}", file.display()));
    if test_only(&parsed.attrs) {
        return (None, runtime);
    }
    runtime.visit_file_mut(&mut parsed);
    (Some(parsed.into_token_stream()), runtime)
}

fn embeds_fixture(tokens: TokenStream, file: &Path, fixture_dir: &Path) -> bool {
    let tokens: Vec<_> = tokens.into_iter().collect();
    tokens.windows(3).any(|window| {
        let [TokenTree::Ident(name), TokenTree::Punct(bang), TokenTree::Group(args)] = window
        else {
            return false;
        };
        matches!(
            name.to_string().as_str(),
            "include_str" | "include_bytes" | "include"
        ) && bang.as_char() == '!'
            && Punctuated::<LitStr, Token![,]>::parse_terminated
                .parse2(args.stream())
                .ok()
                .filter(|args| args.len() == 1)
                .and_then(|args| file.parent()?.join(args[0].value()).canonicalize().ok())
                .is_some_and(|target| target.starts_with(fixture_dir))
    }) || tokens.into_iter().any(|token| match token {
        TokenTree::Group(group) => embeds_fixture(group.stream(), file, fixture_dir),
        _ => false,
    })
}

// --------------------------------------------------------------- the scan

/// Shipped text and source paths that directly embed a fixture.
fn shipped_files(root: &Path) -> (Vec<(String, String)>, Vec<String>) {
    let mut files = Vec::new();
    for dir in ["prompts", "skills", "config", "verification/rules"] {
        walk(&root.join(dir), &mut files);
    }
    if let Ok(rd) = std::fs::read_dir(root.join("crates")) {
        for entry in rd.flatten() {
            if entry.file_name() != "hatch-eval" {
                walk(&entry.path().join("src"), &mut files);
            }
        }
    }

    let relative = |p: &Path| p.strip_prefix(root).unwrap_or(p).display().to_string();
    let mut shipped = Vec::new();
    let mut rust = Vec::new();
    let mut test_modules = BTreeSet::new();
    let mut live_modules = BTreeSet::new();
    for file in files {
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        if file.extension().is_some_and(|ext| ext == "rs") {
            let (tokens, runtime) = runtime_rust(&file, &text);
            test_modules.extend(runtime.test_modules);
            live_modules.extend(runtime.live_modules);
            if let Some(tokens) = tokens {
                rust.push((file, tokens));
            }
        } else {
            shipped.push((relative(&file), text));
        }
    }

    let fixture_dir = root.join("eval/worlds");
    let mut includes = Vec::new();
    for (file, tokens) in rust {
        let canonical = file.canonicalize().expect("resolve scanned source");
        if test_modules.contains(&canonical) && !live_modules.contains(&canonical) {
            continue;
        }
        if embeds_fixture(tokens.clone(), &file, &fixture_dir) {
            includes.push(relative(&file));
        }
        shipped.push((relative(&file), tokens.to_string()));
    }
    includes.sort();
    (shipped, includes)
}

fn hits(markers: &BTreeSet<String>, files: &[(String, String)]) -> BTreeMap<String, Vec<String>> {
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (path, text) in files {
        for m in markers {
            if text.contains(m.as_str()) {
                out.entry(m.clone()).or_default().push(path.clone());
            }
        }
    }
    out
}

#[test]
fn product_code_contains_no_eval_fixture_identifiers() {
    let root = repo_root();
    let markers = markers_under(&root.join("eval/worlds"));
    assert!(
        markers.len() >= 20,
        "marker extraction looks broken: {markers:?}"
    );
    let (files, includes) = shipped_files(&root);
    assert!(
        includes.is_empty(),
        "runtime Rust directly embeds eval fixtures: {}",
        includes.join(", ")
    );
    assert!(
        files.len() >= 100,
        "scan found only {} shipped files",
        files.len()
    );
    assert!(
        files.iter().any(|(p, _)| p.starts_with("prompts/")),
        "bundled prompts must be scanned"
    );

    let found = hits(&markers, &files);
    assert!(
        found.is_empty(),
        "eval fixture identifiers appear in shipped code (fix the capability, not the scenario):\n{}",
        found
            .iter()
            .map(|(m, ps)| format!("  {m}: {}", ps.join(", ")))
            .collect::<Vec<_>>()
            .join("\n")
    );

    if let Ok(dir) = std::env::var("HATCH_LEAK_PRIVATE_FIXTURES") {
        let private = markers_under(Path::new(&dir));
        assert!(
            !private.is_empty(),
            "no markers found under HATCH_LEAK_PRIVATE_FIXTURES"
        );
        let found = hits(&private, &files);
        let places: BTreeSet<&String> = found.values().flatten().collect();
        assert!(
            found.is_empty(),
            "{} private fixture marker(s) appear in shipped code, in: {}",
            found.len(),
            places.into_iter().cloned().collect::<Vec<_>>().join(", ")
        );
    }
}

#[test]
fn markers_are_emails_and_ticket_ids() {
    let m = hatch_eval::markers::fixture_markers(
        "把HT-2291的修订稿发给 zhou.ning@corp.example。utf-8 与 v2 不算。",
    );
    assert_eq!(
        m.into_iter().collect::<Vec<_>>(),
        vec!["HT-2291".to_string(), "zhou.ning@corp.example".to_string()]
    );
}

#[test]
fn runtime_rust_drops_comments_and_test_items_but_keeps_literals() {
    let src = r##"
// HT-1001 in a comment
/* HT-1002 /* nested */ still comment */
const PROMPT: &str = "keep HT-1003";
const RAW: &str = r#"keep { HT-1004"#;
fn lifetimes<'a>(x: &'a str) -> char { let _ = x; '{' }
#[cfg(test)]
fn only_in_tests() { let s = "}"; let _ = "HT-1005"; }
#[cfg(all(test, unix))]
#[allow(dead_code)]
mod gated { const X: &str = "HT-1006"; }
#[cfg(test)]
mod tests;
fn after() -> &'static str { "keep HT-1007" }
#[cfg(test)]
mod tests {
    #[test]
    fn t() { let _ = ("HT-1008", '}'); }
}
"##;
    let out = test_rust(src).unwrap();
    for keep in ["HT-1003", "HT-1004", "HT-1007", "fn lifetimes", "fn after"] {
        assert!(out.contains(keep), "{keep} dropped:\n{out}");
    }
    for drop in [
        "HT-1001",
        "HT-1002",
        "HT-1005",
        "HT-1006",
        "HT-1008",
        "only_in_tests",
    ] {
        assert!(!out.contains(drop), "{drop} kept:\n{out}");
    }
    assert!(test_rust(
        "//! doc\n#![allow(dead_code)]\n#![cfg(test)]\nconst X: &str = \"HT-1009\";"
    )
    .is_none());
    // An inner gate inside a nested module does not hide the whole file.
    let nested = test_rust("mod m { #![cfg(test)] }\nconst Y: &str = \"HT-1010\";").unwrap();
    assert!(nested.contains("HT-1010"));
    // Escaped quote char literal does not swallow what follows.
    let quote = test_rust("const Q: char = '\\'';\nconst Z: &str = \"HT-1011\";").unwrap();
    assert!(quote.contains("HT-1011"), "{quote}");
}

fn test_rust(src: &str) -> Option<String> {
    runtime_rust(Path::new("src/lib.rs"), src)
        .0
        .map(|tokens| tokens.to_string())
}

#[test]
fn only_real_test_module_declarations_exclude_files() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let dir = root.join("crates/example/src");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("production.rs"), "const X: &str = \"XY-9371\";").unwrap();
    std::fs::write(dir.join("tests.rs"), "const X: &str = \"XY-9372\";").unwrap();
    std::fs::write(dir.join("shared.rs"), "const X: &str = \"XY-9373\";").unwrap();
    std::fs::write(dir.join("platform.rs"), "const X: &str = \"XY-9374\";").unwrap();
    std::fs::write(
        dir.join("parent.rs"),
        "#[cfg(test)] #[path = \"sibling.rs\"] mod sibling;",
    )
    .unwrap();
    std::fs::write(dir.join("sibling.rs"), "const X: &str = \"XY-9375\";").unwrap();
    std::fs::write(
        dir.join("lib.rs"),
        r##"
        mod production;
        mod shared;
        mod parent;
        #[cfg_attr(unix, path = "platform.rs")] mod platform_alias;
        #[cfg(test)] mod tests;
        #[cfg(test)] #[path = "shared.rs"] mod also_used_by_tests;
        #[cfg(test)] #[path = "platform.rs"] mod also_platform;
        const NOTE: &str = r#"#[cfg(test)] mod production;"#;
    "##,
    )
    .unwrap();

    let (files, _) = shipped_files(&root);
    let names: BTreeSet<_> = files.iter().map(|(name, _)| name.as_str()).collect();
    assert!(names.contains("crates/example/src/production.rs"));
    assert!(names.contains("crates/example/src/shared.rs"));
    assert!(names.contains("crates/example/src/platform.rs"));
    assert!(!names.contains("crates/example/src/tests.rs"));
    assert!(!names.contains("crates/example/src/sibling.rs"));
}

#[test]
fn test_gates_do_not_hide_similar_predicates_or_sibling_fields() {
    let out = test_rust(
        r#"
        #[cfg(all(testing_enabled, unix))]
        const LIVE: &str = "XY-9371";
        struct Example {
            #[cfg(test)] test: [u8; "XY-9372".len()],
            live: [u8; "XY-9373".len()],
        }
    "#,
    )
    .unwrap();
    assert!(out.contains("XY-9371"));
    assert!(!out.contains("XY-9372"));
    assert!(out.contains("XY-9373"));
}

#[test]
fn shipped_rust_cannot_embed_fixtures() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let dir = root.join("crates/example/src");
    let fixtures = root.join("eval/worlds/example");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::create_dir_all(&fixtures).unwrap();
    std::fs::write(fixtures.join("mail.txt"), "XY-9371").unwrap();
    let src = r##"
        const NOTE: &str = r#"include_str!("../../../eval/worlds/example/mail.txt")"#;
        #[cfg(test)]
        const FIXTURE: &str = include_str!("../../../eval/worlds/example/mail.txt");
    "##;
    let file = dir.join("lib.rs");
    std::fs::write(&file, src).unwrap();
    assert!(shipped_files(&root).1.is_empty());

    for include in ["include_str", "include_bytes"] {
        std::fs::write(
            &file,
            format!(r#"{src} fn live() {{ let _ = {include}!("../../../eval/worlds/example/mail.txt"); }}"#),
        ).unwrap();
        assert_eq!(shipped_files(&root).1, ["crates/example/src/lib.rs"]);
    }
}
