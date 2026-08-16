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
    /// A workspace member listed above one it depends on.
    Workspace,
    /// Code after the `#[cfg(test)]` module instead of before it.
    Tests,
}

impl Rule {
    /// The stable identifier a diagnostic carries, so it can be filtered.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Item => "stratify/item-order",
            Self::Module => "stratify/module-order",
            Self::Workspace => "stratify/member-order",
            Self::Tests => "stratify/tests-last",
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
            Rule::Workspace => format!(
                "member `{}` depends on `{}`, listed below it (line {})",
                self.name, self.uses, self.uses_line
            ),
            Rule::Tests => format!(
                "`{}` is declared after the `#[cfg(test)]` module (line {}) — \
                 tests come last",
                self.name, self.uses_line
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

/// Where the file's OWN `#[cfg(test)]` region starts — where the rules stop.
///
/// COLUMN ZERO ONLY. An indented `#[cfg(test)]` is a test module nested inside
/// some other item, and reading it as the file's boundary had two costs: the
/// item rule stopped checking two thirds of the file, and everything below it
/// was reported as a stowaway. Both were found in one real file.
fn tests_begin(source: &str) -> usize {
    source
        .lines()
        .position(|l| l.starts_with("#[cfg(test)]"))
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

/// The name in `mod X {`, if this line opens an inline module at column zero.
fn inline_module_name(line: &str) -> Option<String> {
    let mut rest = line;
    for lead in ["pub(crate) ", "pub(super) ", "pub "] {
        if let Some(r) = rest.strip_prefix(lead) {
            rest = r;
        }
    }
    let after = rest.strip_prefix("mod ")?;
    let name: String = after
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    if name.is_empty() || !after[name.len()..].trim_start().starts_with('{') {
        return None;
    }
    Some(name)
}

/// The name in `mod X;`, if this line is one. At column zero only.
fn declared_module_name(line: &str) -> Option<String> {
    let mut rest = line;
    for lead in ["pub(crate) ", "pub(super) ", "pub "] {
        if let Some(r) = rest.strip_prefix(lead) {
            rest = r;
        }
    }
    let after = rest.strip_prefix("mod ")?;
    let name: String = after
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    if name.is_empty() || !after[name.len()..].trim_start().starts_with(';') {
        return None;
    }
    Some(name)
}

/// A top-level item: its name, the line it is announced on, and what it spans.
///
/// NOT `Item`: that reads as `Rule::Item` to anyone skimming, and to the rule
/// this crate enforces — which is how the name got changed.
struct TopLevelItem {
    name: String,
    head: usize,
    from: usize,
    to: usize,
}

/// The file's top-level items, or `None` when the rule does not apply to it.
fn items_of(source: &str) -> Option<Vec<TopLevelItem>> {
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
        out.push(TopLevelItem {
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

/// Every `.rs` file and every `Cargo.toml` under a directory.
///
/// `target/` and dotted directories are skipped: build output contains
/// generated Rust nobody wrote and vendored manifests nobody owns, and
/// reporting a layout convention against either is noise.
pub fn source_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut found: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    found.sort(); // so a report reads the same twice
    for path in found {
        let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
        if path.is_dir() {
            if name != "target" && !name.starts_with('.') {
                source_files(&path, out);
            }
        } else if path.extension().is_some_and(|e| e == "rs") || name == "Cargo.toml" {
            out.push(path);
        }
    }
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
        let Some(name) = declared_module_name(line) else {
            continue;
        };

        let mut owned = String::new();
        if let Ok(text) = std::fs::read_to_string(dir.join(format!("{name}.rs"))) {
            owned.push_str(&text);
        }
        let sub = dir.join(&name);
        if sub.is_dir() {
            let mut files = Vec::new();
            source_files(&sub, &mut files);
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

/// A workspace member entry: the text as written, where, and the crates it
/// resolves to. `crates/data-adapters/*` is ONE entry naming a FOLDER, which is
/// why a glob is expanded rather than rejected — ordering the folders is the
/// point at this scale.
struct Member {
    entry: String,
    line: usize,
    crates: Vec<PathBuf>,
}

/// The quoted entries of a `members = [...]` list, with their line numbers.
///
/// Line-based, like the rest of this crate. A TOML parser would be better and
/// is not worth a dependency for a layout convention — and this crate having no
/// dependencies is what lets a test and an editor share it without argument.
fn members_of(dir: &Path, source: &str) -> Vec<Member> {
    let mut inside = false;
    let mut out = Vec::new();

    for (n, line) in source.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with('#') {
            continue;
        }
        // `default-members` is a different list with a different job.
        if !inside {
            let head = trimmed.replace(' ', "");
            if head.starts_with("members=[") {
                inside = true;
            } else {
                continue;
            }
        }
        for chunk in trimmed.split('"').skip(1).step_by(2) {
            let mut crates = Vec::new();
            match chunk.strip_suffix("/*") {
                Some(folder) => {
                    let mut found: Vec<PathBuf> = std::fs::read_dir(dir.join(folder))
                        .into_iter()
                        .flatten()
                        .flatten()
                        .map(|e| e.path())
                        .filter(|p| p.join("Cargo.toml").is_file())
                        .collect();
                    found.sort();
                    crates.extend(found);
                }
                None => {
                    let one = dir.join(chunk);
                    if one.join("Cargo.toml").is_file() {
                        crates.push(one);
                    }
                }
            }
            out.push(Member {
                entry: chunk.to_owned(),
                line: n + 1,
                crates,
            });
        }
        if trimmed.contains(']') {
            break;
        }
    }
    out
}

/// A manifest's `name`, and the packages it depends on in production.
///
/// `[dev-dependencies]` are NOT read. A test reaching sideways for a fixture
/// crate says nothing about which layer stands on which, and treating it as
/// architecture is how a layering rule starts reporting cycles that are not
/// there.
fn package_and_dependencies(manifest: &str) -> (String, BTreeSet<String>) {
    let mut name = String::new();
    let mut deps = BTreeSet::new();
    let mut section = "";

    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('#') {
            continue;
        }
        if trimmed.starts_with('[') {
            section = match trimmed {
                "[package]" => "package",
                "[dependencies]" => "dependencies",
                _ => {
                    // `[dependencies.serde]` is the long form of one entry.
                    if let Some(rest) = trimmed.strip_prefix("[dependencies.") {
                        if let Some(key) = rest.strip_suffix(']') {
                            deps.insert(key.to_owned());
                        }
                    }
                    ""
                }
            };
            continue;
        }
        let Some((key, value)) = trimmed.split_once('=') else {
            continue;
        };
        let key = key.trim();
        match section {
            "package" if key == "name" => name = value.trim().trim_matches('"').to_owned(),
            "dependencies" => {
                // `mcg-search-index.workspace = true` is ONE dependency whose
                // key carries a sub-field. Keeping the whole key made every
                // workspace-inherited dependency invisible, and the rule
                // reported a confident zero on a workspace listed upside down.
                deps.insert(key.split('.').next().unwrap_or(key).to_owned());
            }
            _ => {}
        }
    }
    (name, deps)
}

/// **THE SAME RULE AT THE WIDEST SCALE: workspace members are LISTED in
/// dependency order**, folders included.
///
/// A `members` list is the first thing anyone reads about a repository, and
/// alphabetical order tells them nothing. In dependency order it says which
/// way the arrows point before a single file is opened: in the codebase this
/// came from, `data-adapters` then `business-logic` then `interfaces` — the
/// three strata, in the order they stand on each other.
fn workspace_violations(path: &Path, source: &str) -> Vec<Violation> {
    if path.file_name().is_none_or(|n| n != "Cargo.toml") {
        return Vec::new();
    }
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let members = members_of(dir, source);
    if members.len() < 2 {
        return Vec::new();
    }

    // What each entry PROVIDES, and what it ASKS FOR, pooled per entry — a
    // folder of crates is one node here, which is what makes folders orderable.
    let mut provides: Vec<BTreeSet<String>> = Vec::new();
    let mut wants: Vec<BTreeSet<String>> = Vec::new();
    for member in &members {
        let (mut mine, mut theirs) = (BTreeSet::new(), BTreeSet::new());
        for krate in &member.crates {
            if let Ok(manifest) = std::fs::read_to_string(krate.join("Cargo.toml")) {
                let (name, deps) = package_and_dependencies(&manifest);
                mine.insert(name);
                theirs.extend(deps);
            }
        }
        provides.push(mine);
        wants.push(theirs);
    }

    let mut uses: BTreeMap<usize, BTreeSet<usize>> = BTreeMap::new();
    for n in 0..members.len() {
        let deps = (0..members.len())
            .filter(|m| *m != n && provides[*m].iter().any(|name| wants[n].contains(name)))
            .collect();
        uses.insert(n, deps);
    }

    out_of_order(&uses)
        .into_iter()
        .map(|(a, b)| Violation {
            rule: Rule::Workspace,
            file: path.to_path_buf(),
            line: members[a].line,
            name: members[a].entry.clone(),
            uses: members[b].entry.clone(),
            uses_line: members[b].line,
        })
        .collect()
}

/// **TESTS COME LAST, AND NOTHING FOLLOWS THEM.**
///
/// The three rules above all stop at `#[cfg(test)]`, because a test module
/// legitimately reaches for everything above it and ordering it against the
/// code would make every file a violation. That exemption is only honest if
/// the module really is the end of the file — otherwise code hides below it,
/// unordered and unchecked, in the one region nothing looks at.
///
/// So this is the rule that makes the other three mean what they say.
fn tests_last_violations(path: &Path, source: &str) -> Vec<Violation> {
    if path.extension().is_none_or(|e| e != "rs") {
        return Vec::new();
    }
    let begins = tests_begin(source);
    if begins == usize::MAX {
        return Vec::new();
    }

    let mut out = Vec::new();
    // TRUE, not false: the scan starts one line PAST the `#[cfg(test)]` that
    // `tests_begin` found, so the item directly beneath it would otherwise be
    // read as the first stowaway — which is exactly what it is not.
    let mut gated = true;
    for (n, line) in source.lines().enumerate().skip(begins + 1) {
        // What the rule actually forbids is PRODUCTION code below the tests.
        // A second `#[cfg(test)]` item — another test module, or a helper that
        // only exists for tests — is still the test region, and moving it
        // above the first one would be worse rather than better.
        if line.trim_start().starts_with("#[cfg(test)]") {
            gated = true;
            continue;
        }
        // Only column zero: everything INSIDE the test module is indented, and
        // a nested item there is the test module's business, not this rule's.
        //
        // `mod tests {` counts here even though the other rules ignore inline
        // modules. It has to: it is what CONSUMES the `#[cfg(test)]` above it,
        // and missing it let the gate fall through onto the next item — which
        // is how a stowaway two lines below the tests read as legitimate.
        let name = match item_name(line) {
            Some((_, name)) => name,
            None => declared_module_name(line)
                .or_else(|| inline_module_name(line))
                .unwrap_or_default(),
        };
        if name.is_empty() {
            continue;
        }
        if gated {
            gated = false;
            continue;
        }
        out.push(Violation {
            rule: Rule::Tests,
            file: path.to_path_buf(),
            line: n + 1,
            name,
            uses: "#[cfg(test)]".to_owned(),
            uses_line: begins + 1,
        });
    }
    out
}

/// **Every rule against one file's text.** The editor calls this on the buffer
/// as you type, so it must not assume the text has been saved.
#[must_use]
pub fn check_source(path: &Path, source: &str) -> Vec<Violation> {
    let mut out = item_violations(path, source);
    out.extend(module_violations(path, source));
    out.extend(workspace_violations(path, source));
    out.extend(tests_last_violations(path, source));
    out
}

/// Both rules against a file on disk. Unreadable is not a violation.
#[must_use]
pub fn check_file(path: &Path) -> Vec<Violation> {
    std::fs::read_to_string(path).map_or_else(|_| Vec::new(), |s| check_source(path, &s))
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
    /// Manifests with a `members` list of two or more entries.
    pub workspaces_checked: usize,
    pub files_seen: usize,
}

/// Every rule across a tree — what a test and a whole-project run use.
///
/// It calls [`check_source`] rather than the rules directly. It used to name
/// them one by one, and a fourth rule added to `check_source` alone was then
/// live in the editor and silent in CI for exactly as long as it took to
/// notice — the two paths must not be able to drift.
#[must_use]
pub fn check_tree(root: &Path) -> Report {
    let mut files = Vec::new();
    source_files(root, &mut files);

    let mut report = Report {
        files_seen: files.len(),
        ..Report::default()
    };
    for path in &files {
        let Ok(source) = std::fs::read_to_string(path) else {
            continue;
        };
        report.violations.extend(check_source(path, &source));

        // The counts say how much was actually looked at, per scale.
        let dir = path.parent().unwrap_or_else(|| Path::new("."));
        let rust = path.extension().is_some_and(|e| e == "rs");
        if rust && items_of(&source).is_some() {
            report.files_checked += 1;
        }
        if rust
            && path
                .file_name()
                .is_some_and(|n| n == "mod.rs" || n == "lib.rs")
            && declarations_of(dir, &source).len() >= 2
        {
            report.lists_checked += 1;
        }
        if path.file_name().is_some_and(|n| n == "Cargo.toml") && members_of(dir, &source).len() >= 2
        {
            report.workspaces_checked += 1;
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

    /// The widest scale, and the one the user asked for by name: FOLDERS of a
    /// workspace, ordered by which stands on which.
    #[test]
    fn when_a_workspace_member_is_listed_above_one_it_depends_on_then_that_is_a_violation() {
        let dir = std::env::temp_dir().join("stratify-workspace-test");
        let _ = std::fs::remove_dir_all(&dir);
        for (folder, manifest) in [
            ("layers/top/app", "[package]\nname = \"app\"\n\n[dependencies]\nbase = { path = \"../../bottom/base\" }\n"),
            ("layers/bottom/base", "[package]\nname = \"base\"\n"),
        ] {
            std::fs::create_dir_all(dir.join(folder)).expect("dirs");
            std::fs::write(dir.join(folder).join("Cargo.toml"), manifest).expect("manifest");
        }

        let bad = "[workspace]\nmembers = [\n  \"layers/top/*\",\n  \"layers/bottom/*\",\n]\n";
        // NB the fixture above writes `base = { path = ... }`; the inherited
        // spelling is covered by its own test below.
        std::fs::write(dir.join("Cargo.toml"), bad).expect("write");
        let found = check_file(&dir.join("Cargo.toml"));
        assert_eq!(found.len(), 1, "{found:#?}");
        assert_eq!(found[0].rule, Rule::Workspace);
        assert_eq!(found[0].name, "layers/top/*");
        assert_eq!(found[0].uses, "layers/bottom/*");
        assert_eq!(found[0].line, 3, "the offending entry's own line");

        let good = "[workspace]\nmembers = [\n  \"layers/bottom/*\",\n  \"layers/top/*\",\n]\n";
        std::fs::write(dir.join("Cargo.toml"), good).expect("write");
        assert!(check_file(&dir.join("Cargo.toml")).is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `dep.workspace = true` is the spelling this whole workspace uses, and
    /// reading it as a key called `dep.workspace` hid every edge there was.
    #[test]
    fn when_a_dependency_is_inherited_from_the_workspace_then_it_is_still_a_dependency() {
        let (name, deps) = package_and_dependencies(
            "[package]\nname = \"app\"\n\n[dependencies]\nmcg-search-index.workspace = true\nreqwest = { version = \"0.13\" }\n",
        );
        assert_eq!(name, "app");
        assert!(deps.contains("mcg-search-index"), "{deps:?}");
        assert!(deps.contains("reqwest"), "{deps:?}");
    }

    /// The rule that makes the other three honest: they all stop at
    /// `#[cfg(test)]`, so anything below it is in a region nothing checks.
    #[test]
    fn when_code_follows_the_test_module_then_that_is_a_violation() {
        let found = at("fn a() {}\n#[cfg(test)]\nmod tests {\n    fn t() {}\n}\nfn stowaway() {}\n");
        assert_eq!(found.len(), 1, "{found:#?}");
        assert_eq!(found[0].rule, Rule::Tests);
        assert_eq!(found[0].name, "stowaway");
        assert_eq!(found[0].line, 6);
        assert_eq!(found[0].uses_line, 2, "and it points at the test module");
    }

    /// A second test module below the first is still the test region.
    #[test]
    fn when_what_follows_the_tests_is_itself_cfg_test_then_it_is_not_a_stowaway() {
        assert!(
            at("fn a() {}\n#[cfg(test)]\nmod tests {}\n\n#[cfg(test)]\nmod more_tests;\n\n#[cfg(test)]\nfn helper() {}\n")
                .is_empty()
        );
    }

    /// A test module nested inside another item is not the file's boundary.
    #[test]
    fn when_a_nested_test_module_appears_then_the_file_keeps_being_checked() {
        let source = concat!(
            "mod inner {\n",
            "    #[cfg(test)]\n",
            "    mod tests {}\n",
            "}\n",
            "fn top() { helper(); }\n",
            "fn helper() {}\n",
        );
        let found = at(source);
        assert_eq!(found.len(), 1, "{found:#?}");
        assert_eq!(found[0].rule, Rule::Item, "an ordering fault, not a stowaway");
        assert_eq!(found[0].name, "top");
    }

    /// The off-by-one: the item directly under the first `#[cfg(test)]`.
    #[test]
    fn when_a_test_only_function_sits_directly_below_the_marker_then_it_is_fine() {
        assert!(at("fn a() {}\n#[cfg(test)]\npub fn only_for_tests() {}\n").is_empty());
    }

    #[test]
    fn when_the_test_module_really_is_last_then_nothing_is_reported() {
        assert!(at("fn a() {}\n#[cfg(test)]\nmod tests {\n    fn t() { a(); }\n}\n").is_empty());
    }

    /// Items INSIDE the test module are indented, and are its own business.
    #[test]
    fn when_the_test_module_declares_its_own_items_then_they_are_not_stowaways() {
        assert!(
            at("fn a() {}\n#[cfg(test)]\nmod tests {\n    use super::*;\n    struct Fixture;\n    fn t() {}\n}\n")
                .is_empty()
        );
    }

    #[test]
    fn when_a_module_is_declared_after_the_tests_then_that_is_a_violation_too() {
        let found = at("fn a() {}\n#[cfg(test)]\nmod tests {}\nmod helpers;\n");
        assert_eq!(found.len(), 1, "{found:#?}");
        assert_eq!(found[0].name, "helpers");
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
