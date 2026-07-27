//! Facts about a Rust source file, read off a real AST.
//!
//! This is the layer everything else in `fq-lint` is built on, and the reason
//! the tool exists at all. The first version of the file-size ratchet was a
//! hand-rolled Python line scanner; it was wrong on three of the tree's 140
//! files in three distinct ways, each of which is free here:
//!
//! * `#[cfg(any(test, feature = "test-support"))]` — a text rule matching the
//!   literal string `#[cfg(test)]` cannot see it (`fq-runtime/src/lib.rs`).
//! * `#[cfg(test)]` on an indented, nested item — a rule anchored to column 0
//!   cannot see it (`test_support/sim.rs`, two methods inside an `impl`).
//! * doc comments attached to a test-only item — they belong to the item, but
//!   a rule that starts counting at the `#[cfg(test)]` line misses them
//!   (`fq-store/src/grants.rs`).
//!
//! Parsing is delegated to [`syn`], the same crate every derive macro in the
//! tree already depends on. That buys a hard guarantee a heuristic cannot
//! offer: either the file parses as Rust and the facts below are exact, or
//! parsing fails loudly. There is no third outcome where it quietly guesses.
//!
//! Deliberately *not* here: anything needing name resolution or types. That is
//! clippy's job (it runs on HIR, after resolution) and clippy already runs in
//! this repo. See the module docs in `main.rs` for where the boundary sits.

use proc_macro2::TokenTree;
use quote::ToTokens;
use syn::spanned::Spanned;

/// Everything `fq-lint` knows about one source file.
#[derive(Debug, Clone)]
pub struct FileFacts {
    /// Physical lines in the file.
    pub total_lines: usize,
    /// Lines belonging to `#[cfg(test)]` items, at any nesting depth,
    /// including their attributes and doc comments.
    pub test_lines: usize,
    /// Every function-like item, test and production alike.
    pub functions: Vec<FnFacts>,
}

impl FileFacts {
    /// Lines that ship. This is what the ratchet budgets: Rust puts unit tests
    /// inline, so counting total lines would tax the test suite rather than
    /// the production surface.
    pub fn production_lines(&self) -> usize {
        self.total_lines.saturating_sub(self.test_lines)
    }
}

/// One function, method, or associated function.
#[derive(Debug, Clone)]
pub struct FnFacts {
    pub name: String,
    /// Enclosing scope — inline modules and the `impl` type, joined with `::`
    /// (`worker::Runner`, or `Runner as Reducer` for a trait impl). Empty for
    /// a free function at file scope.
    pub scope: String,
    /// Line of the `fn` keyword. Deliberately **not** the start of the item:
    /// an item's span includes its doc comments, and measuring from there
    /// would make documenting a function count against its budget. This repo
    /// puts incident and ADR rationale in doc comments; the gate must not
    /// discourage that.
    pub first_line: usize,
    pub last_line: usize,
    /// Parameter count, `self` included — the quantity behind the tree's 14
    /// `#[allow(clippy::too_many_arguments)]`.
    pub params: usize,
    /// Distinct lines carrying at least one token, across the signature and
    /// body. Comments and blank lines vanish because the lexer never emits
    /// tokens for them, so this is "lines of code" in the sense
    /// `clippy::too_many_lines` means — but computed from the AST, which
    /// matters: `cargo clippy -- --force-warn` does not bust cargo's
    /// fingerprint, so it silently replays diagnostics for only the units it
    /// happens to rebuild. Measured on this tree that reported 13 functions
    /// where a full rebuild found 35. An advisory that under-reports is worse
    /// than none, so the number is derived here instead.
    pub code_lines: usize,
    /// Whether this function sits under a `#[cfg(test)]` item.
    pub is_test: bool,
}

impl FnFacts {
    /// Signature-to-closing-brace span, doc comments and attributes excluded.
    pub fn lines(&self) -> usize {
        self.last_line.saturating_sub(self.first_line) + 1
    }

    /// Stable identity for a baseline entry.
    ///
    /// Keyed on name and scope rather than line number, because line numbers
    /// move on every edit above them and a baseline that churned on unrelated
    /// changes would be ignored within a week.
    pub fn key(&self, path: &str) -> String {
        if self.scope.is_empty() {
            format!("{path}::{}", self.name)
        } else {
            format!("{path}::{}::{}", self.scope, self.name)
        }
    }
}

/// Parse `src` and extract [`FileFacts`].
///
/// # Errors
/// Returns the [`syn::Error`] if `src` is not valid Rust. Callers should treat
/// that as fatal rather than falling back to an approximation.
pub fn analyze(src: &str) -> Result<FileFacts, syn::Error> {
    let file = syn::parse_file(src)?;
    let mut facts = FileFacts {
        total_lines: src.split('\n').count(),
        test_lines: 0,
        functions: Vec::new(),
    };
    walk_items(&file.items, false, "", &mut facts);
    Ok(facts)
}

/// Does this attribute list gate the item on `test`?
///
/// Operates on the tokenized `cfg(..)` predicate rather than on source text,
/// so nesting (`any`, `all`) and negation (`not`) are handled structurally.
fn is_test_gated(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|attr| {
        if !attr.path().is_ident("cfg") {
            return false;
        }
        match &attr.meta {
            syn::Meta::List(list) => mentions_test(list.tokens.clone(), false),
            _ => false,
        }
    })
}

/// Walk a `cfg` predicate's tokens looking for a positively-asserted `test`.
///
/// `negated` flips under `not(..)`, so `cfg(not(test))` — production-only code
/// — is correctly *not* treated as test code.
fn mentions_test(tokens: proc_macro2::TokenStream, negated: bool) -> bool {
    let mut iter = tokens.into_iter().peekable();
    while let Some(tree) = iter.next() {
        match tree {
            TokenTree::Ident(ident) => {
                let name = ident.to_string();
                // `any(..)` / `all(..)` / `not(..)` are followed by a group.
                if let Some(TokenTree::Group(group)) = iter.peek() {
                    let inner = group.stream();
                    let flip = if name == "not" { !negated } else { negated };
                    if mentions_test(inner, flip) {
                        return true;
                    }
                    iter.next();
                    continue;
                }
                if name == "test" && !negated {
                    return true;
                }
            }
            TokenTree::Group(group) => {
                if mentions_test(group.stream(), negated) {
                    return true;
                }
            }
            TokenTree::Punct(_) | TokenTree::Literal(_) => {}
        }
    }
    false
}

/// Line extent of an item, attributes and doc comments included.
///
/// `syn`'s `Spanned` is derived from the item's token stream, and `ToTokens`
/// emits attributes, so a doc comment sitting above `#[cfg(test)]` is part of
/// the span — which is what we want, since it documents test-only code.
fn span_lines(spanned: &impl Spanned) -> (usize, usize) {
    let span = spanned.span();
    (span.start().line, span.end().line)
}

fn record_test_item(spanned: &impl Spanned, facts: &mut FileFacts) {
    let (first, last) = span_lines(spanned);
    facts.test_lines += last.saturating_sub(first) + 1;
}

/// Join a parent scope with one more segment.
fn nest(scope: &str, segment: &str) -> String {
    if scope.is_empty() {
        segment.to_string()
    } else {
        format!("{scope}::{segment}")
    }
}

/// Last path segment of a type, e.g. `Runner` for `worker::Runner<'a>`.
/// `None` for shapes without a nameable head (tuples, references, `dyn`).
fn type_head(ty: &syn::Type) -> Option<String> {
    match ty {
        syn::Type::Path(p) => p.path.segments.last().map(|s| s.ident.to_string()),
        syn::Type::Reference(r) => type_head(&r.elem),
        syn::Type::Group(g) => type_head(&g.elem),
        syn::Type::Paren(p) => type_head(&p.elem),
        _ => None,
    }
}

/// Scope label for an `impl` block: `Runner`, or `Runner as Reducer` for a
/// trait impl. The trait matters — a type can implement an inherent `run` and
/// a trait `run`, and the two need distinct baseline keys.
fn impl_scope(i: &syn::ItemImpl) -> String {
    let ty = type_head(&i.self_ty).unwrap_or_else(|| "_".into());
    match &i.trait_ {
        Some((_, path, _)) => match path.segments.last() {
            Some(seg) => format!("{ty} as {}", seg.ident),
            None => ty,
        },
        None => ty,
    }
}

fn walk_items(items: &[syn::Item], in_test: bool, scope: &str, facts: &mut FileFacts) {
    for item in items {
        let gated = is_test_gated(item_attrs(item));
        // A test item's whole subtree is test code; count it once and do not
        // descend, so nested `#[cfg(test)]` cannot be double-counted.
        if gated && !in_test {
            record_test_item(item, facts);
        }
        let inside = in_test || gated;

        match item {
            syn::Item::Fn(f) => facts.functions.push(fn_facts(
                f.sig.ident.to_string(),
                scope,
                &f.sig,
                Some(&f.block),
                item,
                inside,
            )),
            syn::Item::Mod(m) => {
                if let Some((_, inner)) = &m.content {
                    walk_items(inner, inside, &nest(scope, &m.ident.to_string()), facts);
                }
            }
            syn::Item::Impl(i) => {
                let inner_scope = nest(scope, &impl_scope(i));
                for impl_item in &i.items {
                    walk_impl_item(impl_item, inside, &inner_scope, facts);
                }
            }
            syn::Item::Trait(t) => {
                let inner_scope = nest(scope, &t.ident.to_string());
                for trait_item in &t.items {
                    walk_trait_item(trait_item, inside, &inner_scope, facts);
                }
            }
            _ => {}
        }
    }
}

fn walk_impl_item(item: &syn::ImplItem, in_test: bool, scope: &str, facts: &mut FileFacts) {
    let gated = is_test_gated(impl_item_attrs(item));
    if gated && !in_test {
        record_test_item(item, facts);
    }
    let inside = in_test || gated;
    if let syn::ImplItem::Fn(f) = item {
        facts.functions.push(fn_facts(
            f.sig.ident.to_string(),
            scope,
            &f.sig,
            Some(&f.block),
            item,
            inside,
        ));
    }
}

fn walk_trait_item(item: &syn::TraitItem, in_test: bool, scope: &str, facts: &mut FileFacts) {
    let gated = is_test_gated(trait_item_attrs(item));
    if gated && !in_test {
        record_test_item(item, facts);
    }
    let inside = in_test || gated;
    if let syn::TraitItem::Fn(f) = item {
        facts.functions.push(fn_facts(
            f.sig.ident.to_string(),
            scope,
            &f.sig,
            f.default.as_ref(),
            item,
            inside,
        ));
    }
}

/// Collect every line touched by a token, recursing into groups.
fn token_lines(tokens: proc_macro2::TokenStream, lines: &mut std::collections::BTreeSet<usize>) {
    for tree in tokens {
        let span = tree.span();
        lines.insert(span.start().line);
        lines.insert(span.end().line);
        if let TokenTree::Group(group) = tree {
            token_lines(group.stream(), lines);
        }
    }
}

fn fn_facts(
    name: String,
    scope: &str,
    sig: &syn::Signature,
    body: Option<&syn::Block>,
    spanned: &impl Spanned,
    is_test: bool,
) -> FnFacts {
    let (_, last_line) = span_lines(spanned);
    // Signature plus body, never the attributes: a doc comment is tokens too,
    // and charging a function for documenting itself is the wrong incentive
    // here for the same reason it is in `first_line`.
    let mut code = std::collections::BTreeSet::new();
    token_lines(sig.to_token_stream(), &mut code);
    if let Some(block) = body {
        token_lines(block.to_token_stream(), &mut code);
    }
    // Measure from the `fn` keyword, not the start of the item: the item's
    // span includes doc comments, and charging a function for its own
    // documentation is exactly the wrong incentive in this codebase.
    let first_line = sig.fn_token.span.start().line;
    FnFacts {
        name,
        scope: scope.to_string(),
        first_line,
        last_line,
        code_lines: code.len(),
        params: sig.inputs.len(),
        is_test,
    }
}

fn item_attrs(item: &syn::Item) -> &[syn::Attribute] {
    use syn::Item::{
        Const, Enum, ExternCrate, Fn, ForeignMod, Impl, Macro, Mod, Static, Struct, Trait,
        TraitAlias, Type, Union, Use,
    };
    match item {
        Const(i) => &i.attrs,
        Enum(i) => &i.attrs,
        ExternCrate(i) => &i.attrs,
        Fn(i) => &i.attrs,
        ForeignMod(i) => &i.attrs,
        Impl(i) => &i.attrs,
        Macro(i) => &i.attrs,
        Mod(i) => &i.attrs,
        Static(i) => &i.attrs,
        Struct(i) => &i.attrs,
        Trait(i) => &i.attrs,
        TraitAlias(i) => &i.attrs,
        Type(i) => &i.attrs,
        Union(i) => &i.attrs,
        Use(i) => &i.attrs,
        _ => &[],
    }
}

fn impl_item_attrs(item: &syn::ImplItem) -> &[syn::Attribute] {
    use syn::ImplItem::{Const, Fn, Macro, Type};
    match item {
        Const(i) => &i.attrs,
        Fn(i) => &i.attrs,
        Type(i) => &i.attrs,
        Macro(i) => &i.attrs,
        _ => &[],
    }
}

fn trait_item_attrs(item: &syn::TraitItem) -> &[syn::Attribute] {
    use syn::TraitItem::{Const, Fn, Macro, Type};
    match item {
        Const(i) => &i.attrs,
        Fn(i) => &i.attrs,
        Type(i) => &i.attrs,
        Macro(i) => &i.attrs,
        _ => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prod(src: &str) -> usize {
        analyze(src).expect("valid Rust").production_lines()
    }

    #[test]
    fn counts_a_file_with_no_tests_whole() {
        assert_eq!(prod("fn a() {}\nfn b() {}\n"), 3);
    }

    #[test]
    fn excludes_a_plain_test_module() {
        assert_eq!(
            prod("fn a() {}\n#[cfg(test)]\nmod tests {\n    fn t() {}\n}\n"),
            2
        );
    }

    #[test]
    fn excludes_a_bodyless_test_module_declaration() {
        // The shape that sent the old text rule scanning to end-of-file.
        assert_eq!(
            prod("fn a() {}\n#[cfg(test)]\nmod support;\nfn b() {}\n"),
            3
        );
    }

    #[test]
    fn excludes_test_items_gated_on_any() {
        // `fq-runtime/src/lib.rs:57` — invisible to a literal `#[cfg(test)]` match.
        let src = "fn a() {}\n#[cfg(any(test, feature = \"x\"))]\nmod support;\nfn b() {}\n";
        assert_eq!(prod(src), 3);
    }

    #[test]
    fn excludes_nested_indented_test_items() {
        // `test_support/sim.rs:440` — invisible to a column-0 anchored rule.
        let src = "impl S {\n    fn a(&self) {}\n\n    #[cfg(test)]\n    fn t(&self) {\n    }\n}\n";
        assert_eq!(prod(src), 5);
    }

    #[test]
    fn test_item_span_includes_its_doc_comment() {
        // `fq-store/src/grants.rs:810` — the doc belongs to the test-only item.
        let src = "fn a() {}\n/// doc\n/// doc\n#[cfg(test)]\nmod tests {\n}\n";
        assert_eq!(prod(src), 2);
    }

    #[test]
    fn cfg_not_test_is_production_code() {
        let src = "fn a() {}\n#[cfg(not(test))]\nmod real {\n}\n";
        assert_eq!(prod(src), 5);
    }

    #[test]
    fn nested_test_modules_are_not_double_counted() {
        let src =
            "fn a() {}\n#[cfg(test)]\nmod outer {\n    #[cfg(test)]\n    mod inner {\n    }\n}\n";
        assert_eq!(prod(src), 2);
    }

    #[test]
    fn braces_in_string_literals_do_not_confuse_the_scan() {
        // The regression that rules out a brace-depth counter entirely.
        let src = "fn a() -> &'static str {\n    r#\"{\"k\": {\"n\": 1}}\"#\n}\n#[cfg(test)]\nmod tests {\n}\n";
        assert_eq!(prod(src), 4);
    }

    #[test]
    fn invalid_rust_is_an_error_not_a_guess() {
        assert!(analyze("fn a( {").is_err());
    }

    #[test]
    fn records_functions_with_line_extents_and_arity() {
        let facts = analyze("fn a(x: u8, y: u8) {\n    let _ = x + y;\n}\n").expect("valid Rust");
        assert_eq!(facts.functions.len(), 1);
        let f = &facts.functions[0];
        assert_eq!(f.name, "a");
        assert_eq!(f.params, 2);
        assert_eq!(f.lines(), 3);
        assert!(!f.is_test);
    }

    #[test]
    fn code_lines_skip_comments_and_blank_lines() {
        let src = "fn a() {\n    let x = 1;\n\n    // a comment\n\n    let y = 2;\n}\n";
        let f = &analyze(src).expect("valid Rust").functions[0];
        // fn/{, let x, let y, } — the blank and comment lines carry no tokens.
        assert_eq!(f.code_lines, 4);
        assert_eq!(f.lines(), 7, "physical span still counts them");
    }

    #[test]
    fn code_lines_exclude_the_doc_comment() {
        let src = "/// doc\n/// doc\n/// doc\nfn a() {\n    let x = 1;\n}\n";
        let f = &analyze(src).expect("valid Rust").functions[0];
        assert_eq!(f.code_lines, 3, "signature, body, closing brace only");
    }

    #[test]
    fn code_lines_cover_a_trait_method_without_a_body() {
        let src = "trait T {\n    fn a(&self);\n}\n";
        let f = &analyze(src).expect("valid Rust").functions[0];
        assert_eq!(f.code_lines, 1);
    }

    #[test]
    fn functions_inside_test_modules_are_marked_test() {
        let facts = analyze("#[cfg(test)]\nmod tests {\n    fn t() {}\n}\n").expect("valid Rust");
        assert!(facts.functions.iter().all(|f| f.is_test));
    }
}
