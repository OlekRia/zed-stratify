//! **Turning a violation into an edit.**
//!
//! Same edges the rules report on, so what moves is decided by what was
//! complained about — a fixer with its own opinion is a second rule.
//!
//! CALCULATION: text in, text out, no disk. That is what lets the editor offer
//! it as a code action on a buffer you have not saved, and `--fix` write it.
//!
//! # What it will and will not move
//!
//! It reorders **lists**: module declarations, workspace members, and the
//! `#[cfg(test)]` block that belongs at the end. Those are whole blocks with
//! unambiguous boundaries.
//!
//! It does NOT reorder items inside a file. An item's true extent is harder
//! than it looks — section comments between declarations, `impl` blocks that
//! belong with a type three items away — and a fixer that gets it wrong
//! produces a file that no longer parses. Writing this by hand for one real
//! file broke it twice before the boundary rule was right, which is evidence
//! rather than caution. [`Rule::Item`](super::Rule::Item) is reported and left
//! for a human.
//!
//! # The safety net
//!
//! Every fix is checked to have MOVED lines and not invented, dropped or
//! altered any: the multiset of non-blank lines before and after must match.
//! A fix that fails that check is discarded and the file is left alone.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use super::{
    Declaration, Member, declarations_of, member_edges, members_of, module_edges, reachable,
    tests_begin,
};

/// The order these nodes should appear in, keeping independents where they are.
///
/// A plain topological sort is free to shuffle unrelated entries, which would
/// make every run of `--fix` a different diff. This one places, at each step,
/// the EARLIEST-DECLARED group whose dependencies are all already placed — so
/// a list that already obeys the rule comes back unchanged, and one that does
/// not moves as little as it can.
///
/// Cycles are placed as a unit, in their original order. No arrangement
/// satisfies a cycle, so pretending to sort one would be inventing an answer.
fn stable_order(edges: &BTreeMap<usize, BTreeSet<usize>>, count: usize) -> Vec<usize> {
    // Mutually reachable nodes are one group.
    let mut group: Vec<usize> = (0..count).collect();
    for a in 0..count {
        for b in edges.get(&a).into_iter().flatten() {
            if reachable(edges, *b).contains(&a) {
                let merged = group[a].min(group[*b]);
                let (from, to) = (group[a].max(group[*b]), merged);
                for g in &mut group {
                    if *g == from {
                        *g = to;
                    }
                }
            }
        }
    }

    let mut members: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for (n, g) in group.iter().enumerate() {
        members.entry(*g).or_default().push(n);
    }

    let mut group_deps: BTreeMap<usize, BTreeSet<usize>> = members.keys().map(|g| (*g, BTreeSet::new())).collect();
    for a in 0..count {
        for b in edges.get(&a).into_iter().flatten() {
            if group[a] != group[*b] {
                group_deps.entry(group[a]).or_default().insert(group[*b]);
            }
        }
    }

    let mut placed: BTreeSet<usize> = BTreeSet::new();
    let mut out = Vec::with_capacity(count);
    while placed.len() < members.len() {
        let next = members
            .keys()
            .find(|g| !placed.contains(*g) && group_deps[*g].iter().all(|d| placed.contains(d)));
        match next {
            Some(g) => {
                out.extend(members[g].iter().copied());
                placed.insert(*g);
            }
            // Unreachable given the grouping above; refusing beats looping.
            None => return (0..count).collect(),
        }
    }
    out
}

/// One repair: text in, repaired text out, `None` when it has nothing to say.
type Step = fn(&Path, &str) -> Option<String>;

/// The lines a declaration owns: its doc comment above, its `pub use` below.
fn block_of(lines: &[&str], decl: &Declaration, next_head: Option<usize>) -> (usize, usize) {
    let head = decl.line - 1;
    let mut from = head;
    while from > 0 && {
        let prev = lines[from - 1].trim_start();
        prev.starts_with("///") || prev.starts_with("#[") || prev.starts_with("//!")
    } {
        from -= 1;
    }
    let mut to = head + 1;
    let limit = next_head.unwrap_or(lines.len());
    while to < limit
        && (lines[to].starts_with(&format!("pub use {}::", decl.name))
            || lines[to].starts_with(&format!("pub(crate) use {}::", decl.name))
            || lines[to].starts_with(&format!("use {}::", decl.name)))
    {
        to += 1;
    }
    (from, to)
}

/// Reorder the `mod` declarations of a `mod.rs` / `lib.rs`.
fn fix_modules(path: &Path, source: &str) -> Option<String> {
    if path
        .file_name()
        .is_none_or(|n| n != "mod.rs" && n != "lib.rs")
    {
        return None;
    }
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let declared = declarations_of(dir, source);
    if declared.len() < 2 {
        return None;
    }

    let lines: Vec<&str> = source.lines().collect();
    let heads: Vec<usize> = declared.iter().map(|d| d.line - 1).collect();
    let blocks: Vec<(usize, usize)> = declared
        .iter()
        .enumerate()
        .map(|(i, d)| block_of(&lines, d, heads.get(i + 1).copied()))
        .collect();

    // Only a contiguous run is safe to permute: anything else between the
    // declarations is text this fixer cannot account for.
    let (first, last) = (blocks[0].0, blocks[blocks.len() - 1].1);
    let covered: BTreeSet<usize> = blocks.iter().flat_map(|(a, b)| *a..*b).collect();
    if (first..last).any(|n| !covered.contains(&n) && !lines[n].trim().is_empty()) {
        return None;
    }

    let order = stable_order(&module_edges(&declared), declared.len());
    if order == (0..declared.len()).collect::<Vec<_>>() {
        return None;
    }

    let mut out: Vec<String> = lines[..first].iter().map(|l| (*l).to_owned()).collect();
    for (n, i) in order.iter().enumerate() {
        let (a, b) = blocks[*i];
        out.extend(lines[a..b].iter().map(|l| (*l).to_owned()));
        if n + 1 < order.len() {
            // A blank line between every declaration, so `cargo fmt` cannot
            // treat the list as one group and sort it back.
            out.push(String::new());
        }
    }
    out.extend(lines[last..].iter().map(|l| (*l).to_owned()));
    Some(out.join("\n") + "\n")
}

/// Reorder the `members = [...]` entries of a workspace manifest.
fn fix_members(path: &Path, source: &str) -> Option<String> {
    if path.file_name().is_none_or(|n| n != "Cargo.toml") {
        return None;
    }
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let members: Vec<Member> = members_of(dir, source);
    if members.len() < 2 {
        return None;
    }
    // One entry per line, or this cannot move them without rewriting TOML.
    let mut seen = BTreeSet::new();
    if !members.iter().all(|m| seen.insert(m.line)) {
        return None;
    }

    let order = stable_order(&member_edges(&members), members.len());
    if order == (0..members.len()).collect::<Vec<_>>() {
        return None;
    }

    let lines: Vec<&str> = source.lines().collect();
    let rows: Vec<usize> = members.iter().map(|m| m.line - 1).collect();
    let mut out: Vec<String> = lines.iter().map(|l| (*l).to_owned()).collect();
    for (slot, from) in order.iter().enumerate() {
        out[rows[slot]] = lines[rows[*from]].to_owned();
    }
    Some(out.join("\n") + "\n")
}

/// Move the file's `#[cfg(test)]` blocks to the end, in the order they appear.
fn fix_tests_last(path: &Path, source: &str) -> Option<String> {
    if path.extension().is_none_or(|e| e != "rs") || tests_begin(source) == usize::MAX {
        return None;
    }
    let lines: Vec<&str> = source.lines().collect();

    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut n = 0;
    while n < lines.len() {
        if !lines[n].starts_with("#[cfg(test)]") {
            n += 1;
            continue;
        }
        let mut from = n;
        while from > 0 && {
            let prev = lines[from - 1].trim_start();
            prev.starts_with("//") || prev.starts_with("#[")
        } {
            from -= 1;
        }
        // The item this attribute decorates, then its end. A TOP-LEVEL item's
        // body is indented and its closing brace sits at column zero; counting
        // braces instead is fooled by `&'a str`, which opens a char literal
        // that never closes.
        let mut head = n + 1;
        while head < lines.len() && (lines[head].starts_with("#[") || lines[head].starts_with("//")) {
            head += 1;
        }
        if head >= lines.len() {
            return None;
        }
        let to = if lines[head].trim_end().ends_with(';') {
            head + 1
        } else {
            (head + 1..lines.len()).find(|j| lines[*j].trim_end() == "}")? + 1
        };
        ranges.push((from, to));
        n = to;
    }

    // Already last, and only one of them: nothing to do.
    if ranges.is_empty()
        || (ranges.len() == 1
            && lines[ranges[0].1..].iter().all(|l| l.trim().is_empty()))
    {
        return None;
    }

    let inside: BTreeSet<usize> = ranges.iter().flat_map(|(a, b)| *a..*b).collect();
    let kept: Vec<String> = (0..lines.len())
        .filter(|n| !inside.contains(n))
        .map(|n| lines[n].to_owned())
        .collect();

    let mut out = kept.join("\n").trim_end().to_owned();
    for (a, b) in ranges {
        out.push_str("\n\n");
        out.push_str(lines[a..b].join("\n").trim_end());
    }
    Some(out + "\n")
}

/// Did this edit MOVE lines, rather than invent, drop or alter any?
///
/// The one invariant worth having. A reordering fixer that changes content is
/// not a reordering fixer, and the failure mode — a file that no longer parses
/// — is expensive to discover later.
fn only_moved(before: &str, after: &str) -> bool {
    fn bag(text: &str) -> Vec<&str> {
        let mut v: Vec<&str> = text
            .lines()
            .map(str::trim_end)
            .filter(|l| !l.is_empty())
            .collect();
        v.sort_unstable();
        v
    }
    bag(before) == bag(after)
}

/// **The whole fixer.** `None` when there is nothing to do, or when the result
/// failed the safety check — in which case the file is left exactly as it was.
#[must_use]
pub fn fix_source(path: &Path, source: &str) -> Option<String> {
    // A FILE WITH NOTHING WRONG IS NEVER TOUCHED. Without this the tests-last
    // step rewrote files whose several `#[cfg(test)]` blocks were already at
    // the end — a harmless edit that still turned `--fix` into a diff nobody
    // asked for, across three files on its first real run.
    let faults: BTreeSet<super::Rule> = super::check_source(path, source)
        .into_iter()
        .map(|v| v.rule)
        .collect();
    if faults.is_empty() {
        return None;
    }

    let mut text = source.to_owned();
    let mut touched = false;

    let steps: Vec<Step> = [
        (super::Rule::Tests, fix_tests_last as Step),
        (super::Rule::Module, fix_modules),
        (super::Rule::Workspace, fix_members),
    ]
    .into_iter()
    .filter(|(rule, _)| faults.contains(rule))
    .map(|(_, step)| step)
    .collect();

    for step in steps {
        if let Some(next) = step(path, &text)
            && only_moved(&text, &next)
        {
            text = next;
            touched = true;
        }
    }

    (touched && text != source).then_some(text)
}

/// **Numbering the files so a file explorer shows the layering.**
///
/// No editor can be made to sort a project panel by anything but the
/// alphabet — Zed's offers `directories_first` / `mixed` / `files_first` and
/// nothing else, and its extension API has no project-panel hook at any
/// version. The only lever left is the file NAME, and Rust hands it over:
/// `#[path = "01_web_product.rs"] mod web_product;` renames the file and
/// leaves the module path, the imports and `crate::WebProduct` untouched.
///
/// The numbers come from the declaration order, so this is re-runnable rather
/// than hand-maintained: reorder the list, run it again, the numbers follow.
///
/// Returns the rewritten `mod.rs`/`lib.rs` and the renames to perform. An
/// empty rename list means the numbering was already right.
#[must_use]
pub fn numbered_layout(path: &Path, source: &str) -> Option<(String, Vec<(PathBuf, PathBuf)>)> {
    if path
        .file_name()
        .is_none_or(|n| n != "mod.rs" && n != "lib.rs")
    {
        return None;
    }
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let declared = declarations_of(dir, source);
    if declared.len() < 2 {
        return None;
    }

    let lines: Vec<&str> = source.lines().collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len() + declared.len());
    let mut renames = Vec::new();
    let mut heads: BTreeMap<usize, (String, String)> = BTreeMap::new();

    for (slot, decl) in declared.iter().enumerate() {
        // The name the file should have, and what it has now. A directory
        // module is `NN_name/mod.rs`; a file module is `NN_name.rs`.
        let stem = format!("{:02}_{}", slot + 1, decl.name);
        let (current_file, current_dir) = (dir.join(format!("{}.rs", decl.name)), dir.join(&decl.name));
        let (from, to, attr) = if current_dir.is_dir() || dir.join(&stem).is_dir() {
            (current_dir, dir.join(&stem), format!("{stem}/mod.rs"))
        } else {
            (current_file, dir.join(format!("{stem}.rs")), format!("{stem}.rs"))
        };
        // Whatever it is called today, including an existing number.
        let existing = existing_path(&lines, decl.line - 1);
        let from = existing.map_or(from, |p| dir.join(p.trim_end_matches("/mod.rs")));
        if from != to && from.exists() {
            renames.push((from, to));
        }
        heads.insert(decl.line - 1, (attr, decl.name.clone()));
    }

    let mut n = 0;
    while n < lines.len() {
        // Drop any `#[path]` already there; it is about to be rewritten.
        if lines[n].trim_start().starts_with("#[path") && heads.contains_key(&(n + 1)) {
            n += 1;
            continue;
        }
        if let Some((attr, _)) = heads.get(&n) {
            out.push(format!("#[path = \"{attr}\"]"));
        }
        out.push(lines[n].to_owned());
        n += 1;
    }

    let text = out.join("\n") + "\n";
    (text != source || !renames.is_empty()).then_some((text, renames))
}

/// The `#[path = "..."]` already sitting above this declaration, if any.
fn existing_path(lines: &[&str], head: usize) -> Option<String> {
    lines[..head]
        .iter()
        .rev()
        .take_while(|l| l.trim_start().starts_with("#["))
        .find_map(|l| l.trim_start().strip_prefix("#[path"))
        .and_then(|rest| rest.split('"').nth(1))
        .map(str::to_owned)
}

#[cfg(test)]
mod given_a_file_to_repair {
    use super::*;

    #[test]
    fn when_code_follows_the_tests_then_the_tests_move_to_the_end() {
        let source = "fn a() {}\n#[cfg(test)]\nmod tests {\n    fn t() {}\n}\nfn stowaway() {}\n";
        let fixed = fix_source(Path::new("x.rs"), source).expect("a fix");
        assert!(
            fixed.trim_end().ends_with("}"),
            "the test module is last:\n{fixed}"
        );
        assert!(
            fixed.find("fn stowaway").unwrap() < fixed.find("#[cfg(test)]").unwrap(),
            "and the stowaway is above it:\n{fixed}"
        );
        assert!(super::super::check_source(Path::new("x.rs"), &fixed).is_empty());
    }

    #[test]
    fn when_a_file_already_obeys_the_rules_then_there_is_nothing_to_fix() {
        assert!(fix_source(Path::new("x.rs"), "fn helper() {}\nfn top() { helper(); }\n").is_none());
    }

    /// Several test modules, all already at the end, are not a fault — and a
    /// fixer that rewrites them anyway is a diff nobody asked for. It did
    /// exactly that to three real files before this check existed.
    #[test]
    fn when_several_test_modules_are_already_last_then_the_file_is_left_alone() {
        let source = "fn a() {}\n\n#[cfg(test)]\nmod one {\n    fn t() {}\n}\n\n#[cfg(test)]\nmod two {\n    fn t() {}\n}\n";
        assert!(super::super::check_source(Path::new("x.rs"), source).is_empty());
        assert!(fix_source(Path::new("x.rs"), source).is_none());
    }

    /// Items are reported, never moved — see the module docs.
    #[test]
    fn when_only_the_item_order_is_wrong_then_no_fix_is_offered() {
        let source = "fn top() { helper(); }\nfn helper() {}\n";
        assert!(!super::super::check_source(Path::new("x.rs"), source).is_empty());
        assert!(fix_source(Path::new("x.rs"), source).is_none());
    }

    #[test]
    fn when_a_module_is_declared_above_one_it_uses_then_the_pair_moves_together() {
        let dir = std::env::temp_dir().join("stratify-fix-modules");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(dir.join("high.rs"), "use crate::Low;\npub fn go(_: Low) {}\n").expect("w");
        std::fs::write(dir.join("low.rs"), "pub struct Low;\n").expect("w");

        let source = "mod high;\npub use high::*;\n\nmod low;\npub use low::*;\n";
        let path = dir.join("lib.rs");
        std::fs::write(&path, source).expect("w");

        let fixed = fix_source(&path, source).expect("a fix");
        assert!(
            fixed.find("mod low").unwrap() < fixed.find("mod high").unwrap(),
            "low comes first:\n{fixed}"
        );
        assert!(
            fixed.find("pub use low::*").unwrap() < fixed.find("mod high").unwrap(),
            "and its re-export travelled with it:\n{fixed}"
        );

        std::fs::write(&path, &fixed).expect("w");
        assert!(super::super::check_file(&path).is_empty(), "and it is clean");
        assert!(fix_source(&path, &fixed).is_none(), "and idempotent");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn when_a_workspace_is_upside_down_then_the_member_entries_swap() {
        let dir = std::env::temp_dir().join("stratify-fix-members");
        let _ = std::fs::remove_dir_all(&dir);
        for (folder, manifest) in [
            ("layers/top/app", "[package]\nname = \"app\"\n\n[dependencies]\nbase.workspace = true\n"),
            ("layers/bottom/base", "[package]\nname = \"base\"\n"),
        ] {
            std::fs::create_dir_all(dir.join(folder)).expect("dirs");
            std::fs::write(dir.join(folder).join("Cargo.toml"), manifest).expect("m");
        }
        let source = "[workspace]\nmembers = [\n  \"layers/top/*\",\n  \"layers/bottom/*\",\n]\n";
        let path = dir.join("Cargo.toml");
        std::fs::write(&path, source).expect("w");

        let fixed = fix_source(&path, source).expect("a fix");
        assert!(
            fixed.find("bottom").unwrap() < fixed.find("top").unwrap(),
            "the floor is listed first:\n{fixed}"
        );
        std::fs::write(&path, &fixed).expect("w");
        assert!(super::super::check_file(&path).is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The safety net, exercised directly: content may move and nothing else.
    #[test]
    fn when_an_edit_would_change_content_then_it_is_not_a_move() {
        assert!(only_moved("a\nb\n", "b\n\na\n"));
        assert!(!only_moved("a\nb\n", "a\n"), "a dropped line is not a move");
        assert!(!only_moved("a\nb\n", "a\nb\nc\n"), "nor an invented one");
        assert!(!only_moved("a\nb\n", "a\nB\n"), "nor an altered one");
    }
}
