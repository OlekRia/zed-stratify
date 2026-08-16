# Stratify — a Zed extension

**Dependencies above dependents, shown as you type.**

F# lists its files in build order and forbids forward references, so the project
file *is* the architecture. Rust does not care, which leaves the order of a
`mod.rs` free to say something — and what it should say is the layering.

Three rules, one idea, at three scales:

| rule | code | what it says |
|---|---|---|
| item order | `stratify/item-order` | inside a file, an item is defined **below** everything it uses |
| module order | `stratify/module-order` | inside a `mod.rs`/`lib.rs`, a module is declared **below** every sibling it reaches for |
| member order | `stratify/member-order` | inside a `Cargo.toml`, a workspace member is listed **below** every member it depends on |

The third one orders **folders**: `crates/data-adapters/*` is one entry naming a
directory of crates, so a glob is expanded rather than rejected. A `members`
list is the first thing anyone reads about a repository, and in dependency order
it says which way the arrows point before a single file is opened:

```toml
members = [
    "crates/business-logic/*",   # depends on neither of the others
    "crates/data-adapters/*",    # reaches business-logic for domain types
    "crates/interfaces/*",       # sees everything
]
```

So a file reads downward — by the time you reach a function, everything it calls
is already behind you — and a handler file *ends* with its `handler` rather than
beginning with it. This is the opposite of "public entry point first", and it is
deliberate: the entry point is the conclusion, and a conclusion is easier to
read once you have the parts.

`routes/mod.rs` in the codebase this came from now reads `paths`, `primitives`,
`surfaces`, then `browse`, `editor`, `favourites`, then `router` — the whole
layering, in nine lines. It used to be alphabetical, which said nothing.

## What's in here

| | |
|---|---|
| `stratify/` | the three rules, as a dependency-free library |
| `stratify-lsp/` | a language server publishing them as diagnostics, plus `--check` for CI |
| `src/lib.rs` | the Zed extension, whose only job is to launch that server |

## What it does NOT flag

Swapping two modules that don't reference each other is fine. The order is
constrained only where a dependency exists; where two things are genuinely
independent, any order is honest.

Mutual dependencies are **vacuous, not violated** — `AppState` holding a
`StoreView` that takes an `AppState` cannot be ordered by any arrangement, so
the rule says nothing rather than nagging. That clause is what keeps this a rule
instead of a description with exceptions.

Three things are ignored outright: `#[cfg(test)]` modules, which stay last;
prose, because a doc comment saying "kept out of the handler" is not a call to
`handler`; and files declaring a `macro_rules!`, where a macro must precede its
uses and the compiler's requirement outranks the convention.

## Installing

The extension is a launcher. **Zed extensions cannot publish diagnostics** —
they can supply themes, grammars, slash commands, and language servers, and a
language server is the only thing allowed to mark up a buffer. So the rules live
in `stratify-lsp`, in this repo, and you install that first:

```sh
git clone https://github.com/OlekRia/zed-stratify
cd zed-stratify
cargo install --path stratify-lsp
```

Then in Zed: `cmd-shift-P` → **zed: extensions** → **Install Dev Extension** →
choose this directory.

It attaches to Rust buffers alongside rust-analyzer (Zed runs several language
servers per language) and reports as warnings, not errors — a convention that
blocks a build is a CI job, not an editor.

### Where it looks for the binary

1. `lsp.Stratify.binary.path` in your Zed settings — an explicit answer wins.
2. `stratify-lsp` on `PATH`, which is what `cargo install` gives you.
3. `target/release/stratify-lsp` in the worktree, so a clone works straight
   after `cargo build --release` with no install at all.

If none exist, it says so and names all three. A language server that silently
fails to start looks exactly like a codebase with nothing wrong in it.

## Without an editor

The same binary is a one-shot checker, which is what CI should call:

```sh
stratify-lsp --check .
# crates/…/mod.rs:8: `mod state` uses `semantic`, declared below it (line 12)
# 2 violation(s) — 235 files, 116 ordered, 27 module lists, 1 workspaces
```

Non-zero exit when anything is out of order. The counts are not decoration: a
rule that silently stopped matching reports zero violations and reads as
success. That has happened — a bug once treated `const` as a modifier rather
than a keyword, and every constant in the workspace became invisible to the
check. Watch the counts, not just the verdict.

## Gating a codebase on it

The same crate is what a test should assert on, so the editor and CI cannot
disagree:

```rust
let report = stratify::check_tree(Path::new("crates"));
assert!(report.files_checked > 60, "checked {}", report.files_checked);
assert!(report.violations.is_empty(), "{:#?}", report.violations);
```

Assert on the counts, not only the verdict — for the reason above.

## Licence

MIT.
