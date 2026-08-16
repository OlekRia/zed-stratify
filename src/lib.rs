//! **Stratification, shown as you type.**
//!
//! F# lists its files in build order and forbids forward references, so the
//! project file *is* the architecture. Rust does not care, which leaves the
//! order of a `mod.rs` free to say something — and what it should say is the
//! layering: the floor first, then what stands on it, then whatever sees
//! everything. The same idea inside a file: an item is defined below whatever
//! it uses, so a file reads downward and a handler is the last thing in it.
//!
//! This extension does not check any of that itself. **A Zed extension cannot
//! publish diagnostics** — it can supply themes, grammars, slash commands, and
//! LANGUAGE SERVERS, and a language server is the only thing here allowed to
//! mark up a buffer. So the whole job of this file is to find `stratify-lsp`
//! and start it; the rules live in that binary, next to the test that gates
//! them, because a rule with two implementations is two rules.
//!
//! Where it looks, in order:
//!
//! 1. `lsp.Stratify.binary.path` in your Zed settings — an explicit answer wins.
//! 2. `stratify-lsp` on `PATH`, which is what `cargo install` gives you.
//! 3. `target/release/stratify-lsp` in the worktree, so this repo works
//!    straight after `cargo build --release` with no install at all.
//!
//! If none of those exist it says so, naming the three places, because a
//! language server that silently fails to start looks exactly like a codebase
//! with nothing wrong in it.

use zed_extension_api::{self as zed, Result, serde_json};

/// The binary this extension exists to launch.
const SERVER: &str = "stratify-lsp";

/// Built by `cargo build --release` inside the checkout that carries the tool.
const IN_TREE: &str = "target/release/stratify-lsp";

/// What to try when nothing is configured and nothing is on `PATH`.
fn in_worktree(worktree: &zed::Worktree) -> Option<String> {
    let candidate = format!("{}/{IN_TREE}", worktree.root_path());
    std::fs::metadata(&candidate).is_ok().then_some(candidate)
}

/// An explicit `binary.path` in the user's settings, which outranks discovery.
fn configured(id: &zed::LanguageServerId, worktree: &zed::Worktree) -> Option<zed::Command> {
    let settings = zed::settings::LspSettings::for_worktree(id.as_ref(), worktree).ok()?;
    let binary = settings.binary?;
    Some(zed::Command {
        command: binary.path?,
        args: binary.arguments.unwrap_or_default(),
        env: Default::default(),
    })
}

struct StratifyExtension;

impl zed::Extension for StratifyExtension {
    fn new() -> Self {
        Self
    }

    fn language_server_command(
        &mut self,
        id: &zed::LanguageServerId,
        worktree: &zed::Worktree,
    ) -> Result<zed::Command> {
        if let Some(command) = configured(id, worktree) {
            return Ok(command);
        }

        let path = worktree
            .which(SERVER)
            .or_else(|| in_worktree(worktree))
            .ok_or_else(|| {
                format!(
                    "{SERVER} not found. Set `lsp.Stratify.binary.path` in your Zed \
                     settings, put {SERVER} on PATH (`cargo install --path stratify-lsp`), \
                     or build it in this worktree at {IN_TREE}."
                )
            })?;

        Ok(zed::Command {
            command: path,
            // No arguments: bare means "speak LSP on stdio". `--check <dir>` is
            // the same binary's one-shot mode, which CI uses and Zed must not.
            args: Vec::new(),
            env: Default::default(),
        })
    }

    /// Nothing to configure. Returning `null` rather than an empty object is
    /// deliberate — some servers treat `{}` as "the user set options" and
    /// override their own defaults with it.
    fn language_server_initialization_options(
        &mut self,
        _id: &zed::LanguageServerId,
        _worktree: &zed::Worktree,
    ) -> Result<Option<serde_json::Value>> {
        Ok(None)
    }
}

zed::register_extension!(StratifyExtension);
