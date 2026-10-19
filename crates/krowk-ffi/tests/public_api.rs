//! R-FFI-2, R-PROTO-2: krowk-ffi's public API is frozen in
//! `public-api.txt`, rendered from the source, so a change to what Swift
//! compiles against shows in the Rust PR's diff. A change fails this until
//! `make ffi-api` (UPDATE_PUBLIC_API=1) rewrites the file, and the rewrite
//! is refused when an existing item was removed or changed and
//! `FFI_API_VERSION` was not bumped; an addition alone needs no bump. The
//! same idea as krowk-harness's `tests/schema.rs`.
//!
//! The rendering: every item in `src/lib.rs` and the modules it declares,
//! in source order, that is `pub` or carries a `uniffi` attribute (UniFFI
//! exports from a private module too, so `mod error;` is followed like a
//! `pub mod`); each printed by prettyplease with docs, bodies, private
//! fields and every attribute but `derive`, `non_exhaustive` and the
//! `uniffi` ones stripped.

use std::path::{Path, PathBuf};

use syn::{Attribute, Fields, Item, Visibility};

const UPDATE: &str = "run `UPDATE_PUBLIC_API=1 cargo test -p krowk-ffi --test public_api` and bump FFI_API_VERSION if anything existing changed";

/// The private fields of a struct are rendered as this field, then the
/// line is replaced with the marker: prettyplease drops comments, so the
/// marker is put in as text after rendering.
const OPAQUE_FIELD: &str = "_opaque";
const OPAQUE_MARKER: &str = "// opaque: private fields";

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Whether the attribute is one that decides what Swift sees.
fn kept(attr: &Attribute) -> bool {
    let path = attr.path();
    path.is_ident("derive")
        || path.is_ident("non_exhaustive")
        || path.segments.first().is_some_and(|s| s.ident == "uniffi")
}

fn strip(attrs: &mut Vec<Attribute>) {
    attrs.retain(kept);
}

/// Whether UniFFI exports the item: a `uniffi::` attribute or derive.
fn has_uniffi(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|a| {
        let path = a.path();
        path.segments.first().is_some_and(|s| s.ident == "uniffi")
            || (path.is_ident("derive")
                && a.meta.require_list().is_ok_and(|l| l.tokens.to_string().contains("uniffi")))
    })
}

/// Whether UniFFI writes every field of the struct, private ones too:
/// a Record, derived or `uniffi::remote`.
fn is_record(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|a| {
        let path = a.path();
        (path.is_ident("derive") || path.segments.first().is_some_and(|s| s.ident == "uniffi"))
            && a.meta.require_list().is_ok_and(|l| l.tokens.to_string().contains("Record"))
    })
}

/// `#[cfg(test)]`, or `#[cfg(all(test, …))]`: compiled only for tests.
fn is_cfg_test(attrs: &[Attribute]) -> bool {
    fn test_in(meta: &syn::meta::ParseNestedMeta) -> bool {
        if meta.path.is_ident("test") {
            return true;
        }
        let mut test = false;
        if meta.path.is_ident("all") {
            let _ = meta.parse_nested_meta(|m| {
                test |= test_in(&m);
                Ok(())
            });
        }
        test
    }
    attrs.iter().any(|a| {
        a.path().is_ident("cfg") && {
            let mut test = false;
            let _ = a.parse_nested_meta(|m| {
                test |= test_in(&m);
                Ok(())
            });
            test
        }
    })
}

fn attrs_of(item: &Item) -> &[Attribute] {
    match item {
        Item::Const(i) => &i.attrs,
        Item::Enum(i) => &i.attrs,
        Item::Fn(i) => &i.attrs,
        Item::Impl(i) => &i.attrs,
        Item::Macro(i) => &i.attrs,
        Item::Mod(i) => &i.attrs,
        Item::Static(i) => &i.attrs,
        Item::Struct(i) => &i.attrs,
        Item::Trait(i) => &i.attrs,
        Item::Type(i) => &i.attrs,
        Item::Use(i) => &i.attrs,
        _ => &[],
    }
}

fn is_pub(vis: &Visibility) -> bool {
    matches!(vis, Visibility::Public(_))
}

fn empty_block() -> syn::Block {
    syn::parse_quote!({})
}

/// The `#[path = "…"]` of a `mod` declaration, if it has one.
fn path_attr(attrs: &[Attribute]) -> Option<String> {
    attrs.iter().find(|a| a.path().is_ident("path")).and_then(|a| match &a.meta {
        syn::Meta::NameValue(nv) => match &nv.value {
            syn::Expr::Lit(syn::ExprLit { lit: syn::Lit::Str(s), .. }) => Some(s.value()),
            _ => None,
        },
        _ => None,
    })
}

fn read_module(file: &Path) -> syn::File {
    let text = std::fs::read_to_string(file).unwrap_or_else(|e| panic!("{}: {e}", file.display()));
    syn::parse_file(&text).unwrap_or_else(|e| panic!("{}: {e}", file.display()))
}

/// The directory a source file's own `mod x;` declarations resolve
/// against: `src/` for `lib.rs` and `mod.rs`, `src/x/` for `src/x.rs`.
fn child_dir(file: &Path) -> PathBuf {
    let parent = file.parent().unwrap().to_path_buf();
    match file.file_name().and_then(|n| n.to_str()) {
        Some("lib.rs" | "mod.rs" | "main.rs") => parent,
        _ => parent.join(file.file_stem().unwrap()),
    }
}

/// The public items of `items`, rendered, in source order, following
/// each `mod` where it is declared: out of line from `dir`, or with
/// `#[path]` from `file_dir`, the directory of the file the items are in.
fn collect(items: &[Item], dir: &Path, file_dir: &Path, out: &mut Vec<String>) {
    for item in items {
        if is_cfg_test(attrs_of(item)) {
            continue;
        }
        match item {
            Item::Mod(m) => match &m.content {
                Some((_, inner)) => {
                    let inner_dir = dir.join(m.ident.to_string());
                    collect(inner, &inner_dir, &inner_dir, out)
                }
                None => {
                    let file = match path_attr(&m.attrs) {
                        Some(p) => file_dir.join(p),
                        None => {
                            let flat = dir.join(format!("{}.rs", m.ident));
                            if flat.exists() { flat } else { dir.join(m.ident.to_string()).join("mod.rs") }
                        }
                    };
                    let parsed = read_module(&file);
                    collect(&parsed.items, &child_dir(&file), file.parent().unwrap(), out);
                }
            },
            // A macro call is not an item: `uniffi::setup_scaffolding!()`
            // expands to UniFFI's own plumbing, and other macros to
            // nothing Swift sees — save UniFFI's type macros
            // (`uniffi::custom_type!`, rule 5), which decide how a type
            // crosses and are listed as written.
            Item::Macro(m) => {
                let path = &m.mac.path;
                if path.segments.first().is_some_and(|s| s.ident == "uniffi")
                    && !path.segments.last().is_some_and(|s| s.ident == "setup_scaffolding")
                {
                    out.push(print(Item::Macro(m.clone()), false));
                }
            }
            item => out.extend(render(item)),
        }
    }
}

/// An item as it goes in the file, none when it is not public, or one
/// per method for an inherent `impl`, so that a method added to an
/// exported object is an addition, not a change.
fn render(item: &Item) -> Vec<String> {
    let mut item = item.clone();
    let mut opaque = false;
    let public = match &mut item {
        Item::Fn(f) => {
            let public = is_pub(&f.vis) || has_uniffi(&f.attrs);
            strip(&mut f.attrs);
            *f.block = empty_block();
            strip_args(&mut f.sig);
            public
        }
        Item::Struct(s) => {
            let public = is_pub(&s.vis) || has_uniffi(&s.attrs);
            // UniFFI writes a Record's every field, private or not.
            if !is_record(&s.attrs) {
                opaque = hide_private_fields(&mut s.fields);
            }
            s.fields.iter_mut().for_each(|f| strip(&mut f.attrs));
            strip(&mut s.attrs);
            public
        }
        Item::Enum(e) => {
            let public = is_pub(&e.vis) || has_uniffi(&e.attrs);
            strip(&mut e.attrs);
            for v in &mut e.variants {
                strip(&mut v.attrs);
                v.fields.iter_mut().for_each(|f| strip(&mut f.attrs));
            }
            public
        }
        Item::Trait(t) => {
            let public = is_pub(&t.vis) || has_uniffi(&t.attrs);
            strip(&mut t.attrs);
            for ti in &mut t.items {
                match ti {
                    syn::TraitItem::Fn(f) => {
                        strip(&mut f.attrs);
                        strip_args(&mut f.sig);
                        if f.default.is_some() {
                            f.default = Some(empty_block());
                        }
                    }
                    syn::TraitItem::Const(c) => strip(&mut c.attrs),
                    syn::TraitItem::Type(t) => strip(&mut t.attrs),
                    _ => {}
                }
            }
            public
        }
        Item::Type(t) => {
            let public = is_pub(&t.vis);
            strip(&mut t.attrs);
            public
        }
        // UniFFI cannot export a const or a static, so Swift never sees
        // the value: the type is frozen, the value (`_`) is not, which
        // also keeps a bump of FFI_API_VERSION itself from counting as a
        // change.
        Item::Const(c) => {
            let public = is_pub(&c.vis);
            strip(&mut c.attrs);
            *c.expr = syn::parse_quote!(_);
            public
        }
        Item::Static(s) => {
            let public = is_pub(&s.vis);
            strip(&mut s.attrs);
            *s.expr = syn::parse_quote!(_);
            public
        }
        Item::Use(u) => {
            let public = is_pub(&u.vis);
            strip(&mut u.attrs);
            public
        }
        // An `impl` shows its `pub fn`s, or every method when it is
        // `#[uniffi::export]`ed (UniFFI exports them all); a trait impl
        // only when UniFFI exports it, and whole, since a method added
        // to a trait changes what Swift implements.
        Item::Impl(i) => {
            let exported = has_uniffi(&i.attrs);
            if i.trait_.is_some() && !exported {
                return Vec::new();
            }
            strip(&mut i.attrs);
            i.items.retain_mut(|ii| match ii {
                syn::ImplItem::Fn(f) => {
                    let keep = exported || is_pub(&f.vis);
                    strip(&mut f.attrs);
                    strip_args(&mut f.sig);
                    f.block = empty_block();
                    keep
                }
                syn::ImplItem::Const(c) => {
                    let keep = is_pub(&c.vis);
                    strip(&mut c.attrs);
                    c.expr = syn::parse_quote!(_);
                    keep
                }
                _ => false,
            });
            if i.trait_.is_none() {
                let methods = std::mem::take(&mut i.items);
                return methods
                    .into_iter()
                    .map(|m| {
                        let mut one = i.clone();
                        one.items = vec![m];
                        print(Item::Impl(one), false)
                    })
                    .collect();
            }
            !i.items.is_empty()
        }
        _ => false,
    };
    if !public {
        return Vec::new();
    }
    vec![print(item, opaque)]
}

/// prettyplease's text of one stripped item, the opaque marker put in.
fn print(item: Item, opaque: bool) -> String {
    let file = syn::File { shebang: None, attrs: Vec::new(), items: vec![item] };
    let mut text = prettyplease::unparse(&file);
    if opaque {
        text = text
            .lines()
            .map(|l| {
                let t = l.trim_start();
                let indent = &l[..l.len() - t.len()];
                if t == format!("{OPAQUE_FIELD}: (),") {
                    format!("{indent}{OPAQUE_MARKER}")
                } else {
                    l.replace(&format!("{OPAQUE_FIELD})"), "/* opaque: private fields */)")
                        .replace(&format!("{OPAQUE_FIELD},"), "/* opaque: private fields */,")
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
    }
    // No blank line inside an item, so the file splits back into items
    // on blank lines.
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    lines.join("\n")
}

fn strip_args(sig: &mut syn::Signature) {
    for arg in &mut sig.inputs {
        match arg {
            syn::FnArg::Receiver(r) => strip(&mut r.attrs),
            syn::FnArg::Typed(t) => strip(&mut t.attrs),
        }
    }
}

/// Keeps the `pub` fields; when any field is not `pub`, adds the
/// placeholder field `render` swaps for the marker, and says so.
fn hide_private_fields(fields: &mut Fields) -> bool {
    let mut opaque = false;
    match fields {
        Fields::Named(named) => {
            let before = named.named.len();
            named.named = std::mem::take(&mut named.named).into_iter().filter(|f| is_pub(&f.vis)).collect();
            if named.named.len() != before {
                opaque = true;
                named.named.push(syn::parse_quote!(_opaque: ()));
            }
        }
        Fields::Unnamed(unnamed) => {
            let before = unnamed.unnamed.len();
            unnamed.unnamed = std::mem::take(&mut unnamed.unnamed).into_iter().filter(|f| is_pub(&f.vis)).collect();
            if unnamed.unnamed.len() != before {
                opaque = true;
                unnamed.unnamed.push(syn::parse_quote!(_opaque));
            }
        }
        Fields::Unit => {}
    }
    opaque
}

/// The crate's API, item by item, from `src/lib.rs` down.
fn render_crate() -> Vec<String> {
    let lib = manifest_dir().join("src/lib.rs");
    let mut out = Vec::new();
    collect(&read_module(&lib).items, &child_dir(&lib), lib.parent().unwrap(), &mut out);
    out
}

/// The file: the version line as found, a blank line, then the items,
/// one blank line between each.
fn render_file(version_line: &str, items: &[String]) -> String {
    let mut text = format!("{version_line}\n");
    for item in items {
        text.push('\n');
        text.push_str(item);
        text.push('\n');
    }
    text
}

/// The number on a file's first line, `FFI_API_VERSION = N`.
fn version_of(text: &str) -> Option<u32> {
    text.lines().next()?.strip_prefix("FFI_API_VERSION = ")?.trim().parse().ok()
}

/// A file's items: what follows the version line, split on blank lines
/// (an item never holds one; `render` sees to that).
fn items_of(text: &str) -> Vec<String> {
    let body = text.split_once('\n').map_or("", |(_, rest)| rest);
    body.split("\n\n").map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned).collect()
}

#[derive(Debug, PartialEq)]
enum Decision {
    /// Write this text to the file.
    Write(String),
    /// An existing item was removed or changed without a version bump:
    /// these, and nothing written.
    Refuse(Vec<String>),
}

/// Whether `UPDATE_PUBLIC_API=1` may replace `old` (the file as found)
/// with `new` (the items rendered now). Every old item must still be
/// rendered, word for word, unless the version on `old`'s first line is
/// above `committed`, the version in the last commit (`None` when that
/// cannot be read, which counts as not bumped). The first line is kept
/// as found: the version is bumped by hand.
fn decide(committed: Option<u32>, old: &str, new: &[String]) -> Decision {
    // As a multiset: of two identical items, one going is a removal.
    let mut removed = items_of(old);
    for item in new {
        if let Some(k) = removed.iter().position(|r| r == item) {
            removed.remove(k);
        }
    }
    let bumped = matches!((committed, version_of(old)), (Some(c), Some(v)) if v > c);
    if !removed.is_empty() && !bumped {
        return Decision::Refuse(removed);
    }
    let version_line = old.lines().next().filter(|l| version_of(l).is_some()).unwrap_or("FFI_API_VERSION = 1");
    Decision::Write(render_file(version_line, new))
}

/// The version of public-api.txt in HEAD, what "bumped" is measured
/// against.
fn committed_version() -> Option<u32> {
    let out = std::process::Command::new("git")
        .args(["show", "HEAD:./public-api.txt"])
        .current_dir(manifest_dir())
        .output()
        .ok()?;
    out.status.success().then(|| version_of(&String::from_utf8_lossy(&out.stdout))).flatten()
}

/// A line diff of `old` against `new` (longest common subsequence), with
/// two lines of context around each change.
fn diff(old: &str, new: &str) -> String {
    let (a, b): (Vec<&str>, Vec<&str>) = (old.lines().collect(), new.lines().collect());
    let mut lcs = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            lcs[i][j] = if a[i] == b[j] { lcs[i + 1][j + 1] + 1 } else { lcs[i + 1][j].max(lcs[i][j + 1]) };
        }
    }
    let mut lines = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < a.len() || j < b.len() {
        if i < a.len() && j < b.len() && a[i] == b[j] {
            lines.push((' ', a[i]));
            (i, j) = (i + 1, j + 1);
        } else if i < a.len() && (j == b.len() || lcs[i + 1][j] >= lcs[i][j + 1]) {
            lines.push(('-', a[i]));
            i += 1;
        } else {
            lines.push(('+', b[j]));
            j += 1;
        }
    }
    let near = |k: usize| lines[k.saturating_sub(2)..(k + 3).min(lines.len())].iter().any(|(c, _)| *c != ' ');
    if !lines.iter().any(|(c, _)| *c != ' ') {
        return "(the lines are the same; the file's whitespace at the end is not)\n".into();
    }
    let mut text = String::from("--- public-api.txt\n+++ rendered from src/\n");
    let mut skipped = false;
    for (k, (c, line)) in lines.iter().enumerate() {
        if near(k) {
            if skipped {
                text.push_str("@@\n");
            }
            skipped = false;
            text.push_str(&format!("{c}{line}\n"));
        } else {
            skipped = true;
        }
    }
    text
}

fn api_file() -> PathBuf {
    manifest_dir().join("public-api.txt")
}

#[test]
fn r_ffi_2_r_proto_2_the_checked_in_public_api_is_rendered_from_the_source_and_current() {
    let items = render_crate();
    let old = std::fs::read_to_string(api_file()).unwrap_or_default();
    let version_line = old.lines().next().filter(|l| version_of(l).is_some()).unwrap_or("FFI_API_VERSION = 1");
    let want = render_file(version_line, &items);
    if want == old {
        return;
    }
    if !std::env::var("UPDATE_PUBLIC_API").is_ok_and(|v| !v.is_empty()) {
        panic!("public-api.txt no longer matches krowk-ffi's source:\n\n{}\n{UPDATE}", diff(&old, &want));
    }
    match decide(committed_version(), &old, &items) {
        // Written beside and renamed over, so the version test running
        // alongside never reads a half-written file.
        Decision::Write(text) => {
            let tmp = api_file().with_extension("txt.new");
            std::fs::write(&tmp, text).unwrap();
            std::fs::rename(&tmp, api_file()).unwrap();
        }
        Decision::Refuse(removed) => panic!(
            "public-api.txt not written: these existing items were removed or changed, and FFI_API_VERSION on its \
             first line is not above the committed one — bump it there and in src/lib.rs, then rerun:\n\n{}",
            removed.join("\n\n")
        ),
    }
}

#[test]
fn r_ffi_2_ffi_api_version_is_the_files_first_line() {
    let text = std::fs::read_to_string(api_file()).expect("crates/krowk-ffi/public-api.txt is checked in");
    assert_eq!(
        Some(krowk_ffi::FFI_API_VERSION),
        version_of(&text),
        "FFI_API_VERSION in src/lib.rs and the first line of public-api.txt differ — make them the same"
    );
    assert_eq!(krowk_ffi::ffi_api_version(), krowk_ffi::FFI_API_VERSION);
}

/// The fixture's items, rendered as the crate's are.
fn render_source(src: &str) -> Vec<String> {
    let mut out = Vec::new();
    collect(&syn::parse_file(src).unwrap().items, Path::new("."), Path::new("."), &mut out);
    out
}

#[test]
fn r_ffi_2_an_item_renders_without_docs_bodies_or_private_fields() {
    let items = render_source(
        r#"
        /// Docs go.
        #[inline]
        pub fn a() { let _ = 1; }

        /// So do these.
        #[derive(Debug)]
        pub struct S {
            /// And field docs.
            pub x: u32,
            y: u32,
        }

        fn private() {}

        uniffi::setup_scaffolding!();
        "#,
    );
    assert_eq!(
        items,
        ["pub fn a() {}", "#[derive(Debug)]\npub struct S {\n    pub x: u32,\n    // opaque: private fields\n}"]
    );
}

#[test]
fn r_ffi_2_removing_or_changing_an_item_needs_a_version_bump() {
    let (a, b) = ("pub fn a() {}".to_string(), "pub fn b() {}".to_string());
    let old = render_file("FFI_API_VERSION = 1", &[a.clone(), b.clone()]);
    assert_eq!(decide(Some(1), &old, std::slice::from_ref(&a)), Decision::Refuse(vec![b.clone()]));
    // A changed signature is the old item gone and a new one there.
    let b2 = "pub fn b(x: u32) {}".to_string();
    assert_eq!(decide(Some(1), &old, &[a.clone(), b2]), Decision::Refuse(vec![b.clone()]));
    let bumped = old.replacen("FFI_API_VERSION = 1", "FFI_API_VERSION = 2", 1);
    assert_eq!(decide(Some(1), &bumped, std::slice::from_ref(&a)), Decision::Write(render_file("FFI_API_VERSION = 2", &[a])));
}

#[test]
fn r_proto_2_r_ffi_2_adding_an_export_needs_no_version_bump() {
    let (a, b, c) = ("pub fn a() {}".to_string(), "pub fn b() {}".to_string(), "pub fn c() {}".to_string());
    let old = render_file("FFI_API_VERSION = 1", &[a.clone(), b.clone()]);
    let new = [a, b, c];
    assert_eq!(decide(Some(1), &old, &new), Decision::Write(render_file("FFI_API_VERSION = 1", &new)));
}

#[test]
fn r_ffi_2_a_records_private_fields_are_listed_since_swift_sees_them() {
    let items = render_source(
        r#"
        #[derive(uniffi::Record)]
        pub struct R { pub a: u32, b: u32 }

        #[uniffi::remote(Record)]
        pub struct Q { a: u32 }
        "#,
    );
    assert_eq!(
        items,
        [
            "#[derive(uniffi::Record)]\npub struct R {\n    pub a: u32,\n    b: u32,\n}",
            "#[uniffi::remote(Record)]\npub struct Q {\n    a: u32,\n}",
        ]
    );
}

#[test]
fn r_ffi_2_a_method_added_to_an_exported_object_needs_no_version_bump() {
    let before = render_source("#[uniffi::export] impl O { pub fn a(&self) {} }");
    let after = render_source("#[uniffi::export] impl O { pub fn a(&self) {} pub fn b(&self) -> u32 { 1 } }");
    assert_eq!(before, ["#[uniffi::export]\nimpl O {\n    pub fn a(&self) {}\n}"]);
    assert_eq!(after.len(), 2);
    let old = render_file("FFI_API_VERSION = 1", &before);
    assert_eq!(decide(Some(1), &old, &after), Decision::Write(render_file("FFI_API_VERSION = 1", &after)));
}

#[test]
fn r_ffi_2_test_only_items_and_private_ones_are_not_listed() {
    let items = render_source(
        r#"
        #[cfg(test)]
        pub fn t() {}
        #[cfg(all(test, unix))]
        mod tests { pub fn u() {} }
        #[cfg(not(test))]
        pub fn shipped() {}
        pub(crate) fn inner() {}
        impl S { fn private(&self) {} }
        impl Clone for S { fn clone(&self) -> Self { S } }
        "#,
    );
    assert_eq!(items, ["pub fn shipped() {}"]);
}

#[test]
fn r_ffi_2_one_of_two_identical_items_going_is_a_removal() {
    let a = "pub fn new() {}".to_string();
    let old = render_file("FFI_API_VERSION = 1", &[a.clone(), a.clone()]);
    assert_eq!(decide(Some(1), &old, std::slice::from_ref(&a)), Decision::Refuse(vec![a]));
}
