//! **A FILE READS DOWNWARD: WHAT YOU DEPEND ON COMES FIRST.**
//!
//! Two rules, one idea, at two scales:
//!
//! * [`Rule::Item`] — inside a file, an item is defined below everything it
//!   uses. So a handler file ENDS with its `handler`; constants and small
//!   helpers come first.
//! * [`Rule::Module`] — inside a `mod.rs` or `lib.rs`, a module is declared
//!   below every sibling it reaches for. F# makes this the compiler's business;
//!   Rust does not, which leaves the list free to say something, and what it
//!   should say is the stratification.
//!
//! # What the rules actually say
//!
//! Where `a` uses `b` and `b` does NOT use `a`, directly or through anything
//! else, `b` comes first.
//!
//! THE ONE-WAY CLAUSE IS NOT AN ESCAPE HATCH. Mutually dependent items —
//! `AppState` holding a `StoreView` that takes an `AppState` — cannot be
//! ordered by any arrangement, so the rule is vacuous there rather than
//! violated. Saying so precisely is what keeps this a rule instead of a
//! description with exceptions, which is how a previous folder rule drifted.
//!
//! # What they do not look at
//!
//! * `#[cfg(test)]` modules, which stay last and reference everything above.
//! * PROSE. A doc comment saying "kept out of the handler" is not a call to
//!   `handler`. Counting it invented a cycle that stopped one file being
//!   ordered at all — so comments are stripped before dependencies are read.
//! * Files declaring a `macro_rules!`, because a macro is only visible after
//!   its definition, including to the `mod` lines below it. There, order is a
//!   compiler requirement rather than a convention, and it wins.
//!
//! **CALCULATION** (Normand's sense) at its core — [`check_source`] is a pure
//! function of the text. Only [`check_file`] and [`check_tree`] read the disk,
//! which is what lets the editor check a buffer you have not saved.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// Which scale a violation is at. The editor shows this; the test prints it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rule {
    /// An item defined above something it uses.
    Item,
    /// A module declared above a sibling it uses.
    Module,
}

impl Rule {
    /// The stable identifier a diagnostic carries, so it can be filtered.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Item => "stratify/item-order",
            Self::Module => "stratify/module-order",
        }
    }
}

/// One thing out of order: where it is, and what it should be below.
#[derive(Debug, Clone)]
pub struct Violation {
    pub rule: Rule,
    pub file: PathBuf,
    /// 1-based, so it can be printed as `file:line` and clicked.
    pub line: usize,
    /// The name that is too early.
    pub name: String,
    /// What it uses, and where that is declared.
    pub uses: String,
    pub uses_line: usize,
}

impl Violation {
    /// One line, `file:line: what`, which is what both consumers want.
    #[must_use]
    pub fn message(&self) -> String {
        match self.rule {
            Rule::Item => format!(
                "`{}` uses `{}`, which is defined below it (line {})",
                self.name, self.uses, self.uses_line
            ),
            Rule::Module => format!(
                "`mod {}` uses `{}`, declared below it (line {})",
                self.name, self.uses, self.uses_line
            ),
        }
    }
}

/// Does `body` name `ident` as a whole word?
fn names(body: &str, ident: &str) -> bool {
    body.match_indices(ident).any(|(at, _)| {
        let before = body[..at].chars().next_back();
        let after = body[at + ident.len()..].chars().next();
        let boundary = |c: Option<char>| c.is_none_or(|c| !c.is_alphanumeric() && c != '_');
        boundary(before) && boundary(after)
    })
}

/// Everything reachable from `start` by following uses.
fn reachable(uses: &BTreeMap<usize, BTreeSet<usize>>, start: usize) -> BTreeSet<usize> {
    let mut seen = BTreeSet::new();
    let mut stack = vec![start];
    while let Some(n) = stack.pop() {
        for m in uses.get(&n).into_iter().flatten() {
            if seen.insert(*m) {
                stack.push(*m);
            }
        }
    }
    seen
}

/// The pairs that are genuinely out of order: `a` uses `b`, `b` is later, and
/// `b` does not use `a` back. Shared by both rules, which are the same rule.
fn out_of_order(uses: &BTreeMap<usize, BTreeSet<usize>>) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    for (a, deps) in uses {
        for b in deps {
            if b > a && !reachable(uses, *b).contains(a) {
                out.push((*a, *b));
            }
        }
    }
    out
}

/// Where a `#[cfg(test)]` module starts, which is where both rules stop.
fn tests_begin(source: &str) -> usize {
    source
        .lines()
        .position(|l| l.trim_start().starts_with("#[cfg(test)]"))
        .unwrap_or(usize::MAX)
}

/// Dependencies come from CODE. See the note about prose above.
fn code_only(lines: &[&str]) -> String {
    let mut out = String::new();
    for line in lines {
        if line.trim_start().starts_with("//") {
            continue;
        }
        // A trailing comment, but only when no string opens first: a URL in a
        // literal contains `//`, and cutting there can only LOSE a dependency,
        // never invent one.
        let code = match line.find("//") {
            Some(at) if !line[..at].contains('"') => &line[..at],
            _ => line,
        };
        out.push_str(code);
        out.push('\n');
    }
    out
}

/// The item a line starts, if it starts one. Deliberately line-based: a real
/// parser would be better and is not worth a build-dependency for a layout
/// convention.
fn item_name(line: &str) -> Option<(&'static str, String)> {
    if line.starts_with(char::is_whitespace) || line.starts_with(['/', '#', '}']) {
        return None;
    }
    let mut rest = line;
    // `const` is NOT stripped here. It is a modifier in `const fn` and the
    // KEYWORD in `const NAME: T`, and treating it as a modifier made every
    // constant invisible to this rule — which is what deliberately breaking the
    // rule revealed, and what it would otherwise have shipped silently.
    for lead in [
        "pub(crate) ",
        "pub(super) ",
        "pub(in crate) ",
        "pub ",
        "async ",
        "unsafe ",
    ] {
        while let Some(r) = rest.strip_prefix(lead) {
            rest = r;
        }
    }
    // `const fn` before `const`, so the longer form wins.
    for kind in [
        "const fn ",
        "fn ",
        "const ",
        "static ",
        "struct ",
        "enum ",
        "trait ",
        "type ",
        "macro_rules! ",
    ] {
        if let Some(after) = rest.strip_prefix(kind) {
            let name: String = after
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            if !name.is_empty() {
                let k: &'static str = if kind == "macro_rules! " { "macro" } else { "item" };
                return Some((k, name));
            }
        }
    }
    None
}

/// A top-level item: its name, the line it is announced on, and what it spans.
struct Item {
    name: String,
    head: usize,
    from: usize,
    to: usize,
}

/// The file's top-level items, or `None` when the rule does not apply to it.
fn items_of(source: &str) -> Option<Vec<Item>> {
    let all: Vec<&str> = source.lines().collect();
    let cut = tests_begin(source).min(all.len());

    let mut heads: Vec<(String, usize)> = Vec::new();
    for (n, line) in all[..cut].iter().enumerate() {
        match item_name(line) {
            Some(("macro", _)) => return None, // see the note about macros
            Some((_, name)) => heads.push((name, n)),
            None => {}
        }
    }
    if heads.len() < 2 {
        return None;
    }
    // Two items sharing a name cannot be told apart by a textual scan.
    let unique: BTreeSet<&str> = heads.iter().map(|(n, _)| n.as_str()).collect();
    if unique.len() != heads.len() {
        return None;
    }

    // An item owns the doc comment and attributes above it.
    let start_of = |mut n: usize| {
        while n > 0 {
            let prev = all[n - 1].trim_start();
            if prev.starts_with("///") || prev.starts_with("#[") || prev.starts_with("//!") {
                n -= 1;
            } else {
                break;
            }
        }
        n
    };

    let starts: Vec<usize> = heads.iter().map(|(_, n)| start_of(*n)).collect();
    let mut out = Vec::new();
    for (i, (name, head)) in heads.iter().enumerate() {
        out.push(Item {
            name: name.clone(),
            head: *head,
            from: starts[i],
            to: starts.get(i + 1).copied().unwrap_or(cut),
        });
    }
    Some(out)
}

/// Items defined above something they use. Pure: the whole rule is in the text.
fn item_violations(path: &Path, source: &str) -> Vec<Violation> {
    let Some(items) = items_of(source) else {
        return Vec::new();
    };
    let lines: Vec<&str> = source.lines().collect();

    let mut uses: BTreeMap<usize, BTreeSet<usize>> = BTreeMap::new();
    for (n, item) in items.iter().enumerate() {
        let body = code_only(&lines[item.from..item.to]);
        let mut deps = BTreeSet::new();
        for (m, other) in items.iter().enumerate() {
            if m != n && names(&body, &other.name) {
                deps.insert(m);
            }
        }
        uses.insert(n, deps);
    }

    out_of_order(&uses)
        .into_iter()
        .map(|(a, b)| Violation {
            rule: Rule::Item,
            file: path.to_path_buf(),
            line: items[a].head + 1,
            name: items[a].name.clone(),
            uses: items[b].name.clone(),
            uses_line: items[b].head + 1,
        })
        .collect()
}

/// A `mod X;` declaration: the name, where it is declared, and the source that
/// module owns — a file beside this one, a directory, or both.
struct Declaration {
    name: String,
    line: usize,
    owned: String,
}

/// Every `mod X;` in a `mod.rs` or `lib.rs`, with the source behind it.
///
/// The only part of either rule that READS THE DISK, because a sibling's body
/// is not in the buffer being checked.
fn declarations_of(dir: &Path, source: &str) -> Vec<Declaration> {
    let cut = tests_begin(source);
    let mut out = Vec::new();

    for (n, line) in source.lines().enumerate() {
        if n >= cut {
            break;
        }
        let mut rest = line;
        for lead in ["pub(crate) ", "pub(super) ", "pub "] {
            if let Some(r) = rest.strip_prefix(lead) {
                rest = r;
            }
        }
        let Some(after) = rest.strip_prefix("mod ") else {
            continue;
        };
        let name: String = after
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        if name.is_empty() || !after[name.len()..].trim_start().starts_with(';') {
            continue;
        }

        let mut owned = String::new();
        if let Ok(text) = std::fs::read_to_string(dir.join(format!("{name}.rs"))) {
            owned.push_str(&text);
        }
        let sub = dir.join(&name);
        if sub.is_dir() {
            let mut files = Vec::new();
            rust_files(&sub, &mut files);
            for f in files {
                if let Ok(text) = std::fs::read_to_string(f) {
                    owned.push('\n');
                    owned.push_str(&text);
                }
            }
        }
        out.push(Declaration {
            name,
            line: n + 1,
            owned,
        });
    }
    out
}

/// Does `body` name `ident` as a whole word with a path continuing from it?
///
/// NOT `names(body, "low::")`. That asks for a word boundary AFTER the colons,
/// and what follows `::` is a path segment — a letter, never a boundary — so
/// the check could only ever match `low::{a, b}` and quietly missed every
/// ordinary `super::low::help()`. It made the module rule depend almost
/// entirely on its brace-import fallback, which is why so few violations
/// surfaced the first time it ran.
fn names_path(body: &str, ident: &str) -> bool {
    body.match_indices(ident).any(|(at, _)| {
        let before = body[..at].chars().next_back();
        let boundary = before.is_none_or(|c| !c.is_alphanumeric() && c != '_');
        boundary && body[at + ident.len()..].starts_with("::")
    })
}

/// Does this module's source reach for that sibling?
fn reaches_for(source: &str, sibling: &str) -> bool {
    if names_path(source, sibling) {
        return true;
    }
    // `use super::{a, b};` names a sibling without a following `::`.
    source
        .lines()
        .filter(|l| l.trim_start().starts_with("use ") && l.contains('{'))
        .any(|l| names(l, sibling))
}

/// Modules declared above a sibling they use.
fn module_violations(path: &Path, source: &str) -> Vec<Violation> {
    let is_list = path
        .file_name()
        .is_some_and(|n| n == "mod.rs" || n == "lib.rs");
    if !is_list {
        return Vec::new();
    }
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let declared = declarations_of(dir, source);
    if declared.len() < 2 {
        return Vec::new();
    }

    let mut uses: BTreeMap<usize, BTreeSet<usize>> = BTreeMap::new();
    for (n, decl) in declared.iter().enumerate() {
        let mut deps = BTreeSet::new();
        for (m, other) in declared.iter().enumerate() {
            if m != n && reaches_for(&decl.owned, &other.name) {
                deps.insert(m);
            }
        }
        uses.insert(n, deps);
    }

    out_of_order(&uses)
        .into_iter()
        .map(|(a, b)| Violation {
            rule: Rule::Module,
            file: path.to_path_buf(),
            line: declared[a].line,
            name: declared[a].name.clone(),
            uses: declared[b].name.clone(),
            uses_line: declared[b].line,
        })
        .collect()
}

/// **Both rules against one file's text.** The editor calls this on the buffer
/// as you type, so it must not assume the text has been saved.
#[must_use]
pub fn check_source(path: &Path, source: &str) -> Vec<Violation> {
    let mut out = item_violations(path, source);
    out.extend(module_violations(path, source));
    out
}

/// Both rules against a file on disk. Unreadable is not a violation.
#[must_use]
pub fn check_file(path: &Path) -> Vec<Violation> {
    std::fs::read_to_string(path).map_or_else(|_| Vec::new(), |s| check_source(path, &s))
}

/// Every `.rs` file under a directory.
pub fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut found: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    found.sort(); // so a report reads the same twice
    for path in found {
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// What a whole-tree run found, and HOW MUCH IT LOOKED AT.
///
/// The counts are not statistics. A rule that silently stopped matching would
/// report zero violations and look like success — the test asserts on these.
#[derive(Debug, Default)]
pub struct Report {
    pub violations: Vec<Violation>,
    /// Files where the item rule had two or more items to order.
    pub files_checked: usize,
    /// `mod.rs`/`lib.rs` files with two or more declarations.
    pub lists_checked: usize,
    pub files_seen: usize,
}

/// Both rules across a tree — what the test and a whole-project run use.
#[must_use]
pub fn check_tree(root: &Path) -> Report {
    let mut files = Vec::new();
    rust_files(root, &mut files);

    let mut report = Report {
        files_seen: files.len(),
        ..Report::default()
    };
    for path in &files {
        let Ok(source) = std::fs::read_to_string(path) else {
            continue;
        };
        if items_of(&source).is_some() {
            report.files_checked += 1;
            report.violations.extend(item_violations(path, &source));
        }
        let dir = path.parent().unwrap_or_else(|| Path::new("."));
        if path
            .file_name()
            .is_some_and(|n| n == "mod.rs" || n == "lib.rs")
            && declarations_of(dir, &source).len() >= 2
        {
            report.lists_checked += 1;
            report.violations.extend(module_violations(path, &source));
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(source: &str) -> Vec<Violation> {
        check_source(Path::new("x.rs"), source)
    }

    #[test]
    fn when_an_item_uses_one_below_it_then_that_is_a_violation() {
        let found = at("fn top() { helper(); }\nfn helper() {}\n");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].rule, Rule::Item);
        assert_eq!(found[0].name, "top");
        assert_eq!(found[0].uses, "helper");
        assert_eq!(found[0].line, 1);
        assert_eq!(found[0].uses_line, 2);
    }

    #[test]
    fn when_the_helper_comes_first_then_nothing_is_reported() {
        assert!(at("fn helper() {}\nfn top() { helper(); }\n").is_empty());
    }

    /// The one-way clause: no arrangement satisfies a cycle, so the rule is
    /// vacuous rather than violated. This is the clause that keeps it a rule.
    #[test]
    fn when_two_items_use_each_other_then_neither_is_a_violation() {
        assert!(at("fn a() { b(); }\nfn b() { a(); }\n").is_empty());
    }

    /// The blind spot that shipped once: `const` was stripped as a modifier,
    /// which made every constant invisible.
    #[test]
    fn when_a_function_uses_a_constant_below_it_then_that_is_a_violation() {
        let found = at("fn f() -> u8 { LIMIT }\nconst LIMIT: u8 = 3;\n");
        assert_eq!(found.len(), 1, "constants must be visible to the rule");
        assert_eq!(found[0].uses, "LIMIT");
    }

    /// Prose is not a call. Counting it once invented a cycle.
    #[test]
    fn when_a_comment_names_an_item_then_it_is_not_a_dependency() {
        assert!(at("/// Calls helper eventually.\nfn top() {}\nfn helper() {}\n").is_empty());
    }

    #[test]
    fn when_a_file_declares_a_macro_then_the_rule_does_not_apply() {
        assert!(at("macro_rules! m { () => {} }\nfn top() { helper(); }\nfn helper() {}\n").is_empty());
    }

    #[test]
    fn when_the_test_module_uses_everything_above_then_it_is_not_a_violation() {
        assert!(
            at("fn helper() {}\nfn top() { helper(); }\n#[cfg(test)]\nmod tests { use super::*; fn t() { top(); } }\n")
                .is_empty()
        );
    }

    /// Module order needs sibling sources from disk, so this one writes a tree.
    #[test]
    fn when_a_module_is_declared_above_a_sibling_it_uses_then_that_is_a_violation() {
        let dir = std::env::temp_dir().join("stratify-module-order-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(dir.join("high.rs"), "pub fn go() { super::low::help(); }\n").expect("write");
        std::fs::write(dir.join("low.rs"), "pub fn help() {}\n").expect("write");

        let source = "mod high;\nmod low;\n";
        std::fs::write(dir.join("mod.rs"), source).expect("write");
        let found = check_file(&dir.join("mod.rs"));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].rule, Rule::Module);
        assert_eq!(found[0].name, "high");
        assert_eq!(found[0].uses, "low");

        let fixed = "mod low;\nmod high;\n";
        std::fs::write(dir.join("mod.rs"), fixed).expect("write");
        assert!(check_file(&dir.join("mod.rs")).is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Independent siblings may sit in any order — the discovery that came from
    /// trying to break the rule with `browse` and `editor` and failing.
    #[test]
    fn when_two_modules_do_not_use_each_other_then_any_order_is_allowed() {
        let dir = std::env::temp_dir().join("stratify-independent-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(dir.join("a.rs"), "pub fn one() {}\n").expect("write");
        std::fs::write(dir.join("b.rs"), "pub fn two() {}\n").expect("write");

        for source in ["mod a;\nmod b;\n", "mod b;\nmod a;\n"] {
            std::fs::write(dir.join("mod.rs"), source).expect("write");
            assert!(check_file(&dir.join("mod.rs")).is_empty(), "{source}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
