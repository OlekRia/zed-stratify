//! **The stratification rules, as squiggles in an editor.**
//!
//! A language server speaking LSP over stdin/stdout. Zed extensions cannot
//! publish diagnostics themselves — an extension may launch a language server,
//! and the server is what gets to mark up a buffer — so this binary is what the
//! `zed-stratify` extension exists to start.
//!
//! It checks the BUFFER, not the file, so a violation appears while you are
//! typing rather than after you save. [`stratify::check_source`] is pure for
//! exactly that reason; only the sibling-module lookup touches the disk.
//!
//! Also runs standalone. `stratify-lsp --check <dir>` prints `file:line:` lines
//! and exits non-zero if any — what CI calls, and what proves the binary works
//! without an editor in the loop. `stratify-lsp --fix <dir>` repairs what can
//! be repaired and reports what it left, which is item order: see the fixer's
//! module docs for why that one is reported and not moved.
//!
//! **ACTION** (Normand's sense) — it owns stdio and a document map. Every
//! decision it makes lives in `stratify`, which is a calculation.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

/// LSP's own numbering: a warning, not an error. These are conventions, and a
/// convention that blocks the build is `just verify`'s job, not the editor's.
const WARNING: i64 = 2;

/// `file:///a/b%20c.rs` -> `/a/b c.rs`. Percent-decoding is done by hand
/// because pulling a URL crate in for one function is not worth the tree.
fn path_of(uri: &str) -> Option<PathBuf> {
    let raw = uri.strip_prefix("file://")?;
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?;
            if let Ok(b) = u8::from_str_radix(hex, 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).ok().map(PathBuf::from)
}

/// The whole line the offending declaration sits on, so the squiggle lands on
/// something you can see rather than a zero-width point.
fn range_of(line_1based: usize, source: &str) -> Value {
    let line = line_1based.saturating_sub(1);
    let width = source.lines().nth(line).map_or(0, str::len);
    json!({
        "start": { "line": line, "character": 0 },
        "end":   { "line": line, "character": width },
    })
}

/// Every violation in one buffer, as LSP diagnostics.
fn diagnostics(path: &Path, source: &str) -> Vec<Value> {
    stratify::check_source(path, source)
        .into_iter()
        .map(|v| {
            json!({
                "range": range_of(v.line, source),
                "severity": WARNING,
                "code": v.rule.code(),
                "source": "stratify",
                "message": v.message(),
                // The thing it should be below, so the editor can offer a jump.
                "relatedInformation": [{
                    "location": {
                        "uri": format!("file://{}", path.display()),
                        "range": range_of(v.uses_line, source),
                    },
                    "message": format!("`{}` is declared here", v.uses),
                }],
            })
        })
        .collect()
}

/// Replace the buffer wholesale.
///
/// The alternative — a minimal diff of moved blocks — is more code for a worse
/// result: the editor renders one edit as one undo step either way, and a
/// partial application of a reordering is a file that does not compile.
fn whole_document(before: &str, after: &str) -> Value {
    json!({
        "range": {
            "start": { "line": 0, "character": 0 },
            "end": { "line": before.lines().count(), "character": 0 },
        },
        "newText": after,
    })
}

/// One LSP message, framed. Reads exactly the declared body and no further —
/// over-reading here desynchronises the stream and the server goes silent.
fn read_message(input: &mut impl Read) -> Option<Value> {
    let mut header = Vec::new();
    let mut byte = [0u8; 1];
    while !header.ends_with(b"\r\n\r\n") {
        if input.read(&mut byte).ok()? == 0 {
            return None;
        }
        header.push(byte[0]);
    }
    let header = String::from_utf8(header).ok()?;
    let len: usize = header
        .lines()
        .find_map(|l| l.strip_prefix("Content-Length: "))?
        .trim()
        .parse()
        .ok()?;

    let mut body = vec![0u8; len];
    input.read_exact(&mut body).ok()?;
    serde_json::from_slice(&body).ok()
}

fn write_message(out: &mut impl Write, message: &Value) {
    let body = message.to_string();
    let _ = write!(out, "Content-Length: {}\r\n\r\n{body}", body.len());
    let _ = out.flush();
}

/// What every document handler does: check the buffer, tell the editor.
fn publish(out: &mut impl Write, uri: &str, source: &str) {
    let Some(path) = path_of(uri) else {
        return;
    };
    write_message(
        out,
        &json!({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
            "params": { "uri": uri, "diagnostics": diagnostics(&path, source) },
        }),
    );
}

/// The text a document notification carries, whichever shape it arrives in.
///
/// `didOpen` puts it under `textDocument.text`, a full-sync `didChange` puts it
/// in the first content change, and `didSave` carries it only when the client
/// asked to include it — Zed does not, so a save falls back to the last text
/// this server was given.
fn text_of(params: &Value) -> Option<String> {
    params
        .pointer("/textDocument/text")
        .or_else(|| params.pointer("/contentChanges/0/text"))
        .or_else(|| params.get("text"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// The LSP conversation. Full text sync, because these rules read whole files
/// and reassembling incremental edits to do that would be work for nothing.
fn serve() {
    let mut input = std::io::stdin().lock();
    let mut output = std::io::stdout().lock();
    let mut open: HashMap<String, String> = HashMap::new();

    while let Some(message) = read_message(&mut input) {
        let method = message.get("method").and_then(Value::as_str).unwrap_or("");
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        let id = message.get("id").cloned();

        match method {
            "initialize" => write_message(
                &mut output,
                &json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "capabilities": {
                            "textDocumentSync": { "openClose": true, "change": 1, "save": true },
                            // Offered on the whole document rather than per
                            // diagnostic: reordering a list fixes every
                            // violation in it at once, and one action that
                            // leaves the file clean beats six that each move
                            // one line.
                            "codeActionProvider": { "codeActionKinds": ["quickfix"] },
                        },
                        "serverInfo": { "name": "stratify", "version": env!("CARGO_PKG_VERSION") },
                    },
                }),
            ),
            "textDocument/codeAction" => {
                let uri = params
                    .pointer("/textDocument/uri")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let actions = path_of(uri)
                    .zip(open.get(uri))
                    .and_then(|(path, text)| {
                        stratify::fix_source(&path, text).map(|fixed| (path, text, fixed))
                    })
                    .map_or_else(Vec::new, |(path, text, fixed)| {
                        vec![json!({
                            "title": "Stratify: order declarations by dependency",
                            "kind": "quickfix",
                            "isPreferred": true,
                            "edit": { "changes": { uri: [whole_document(text, &fixed)] } },
                            "diagnostics": diagnostics(&path, text),
                        })]
                    });
                write_message(
                    &mut output,
                    &json!({ "jsonrpc": "2.0", "id": id, "result": actions }),
                );
            }
            "shutdown" => write_message(
                &mut output,
                &json!({ "jsonrpc": "2.0", "id": id, "result": Value::Null }),
            ),
            "exit" => return,
            "textDocument/didOpen" | "textDocument/didChange" | "textDocument/didSave" => {
                let Some(uri) = params
                    .pointer("/textDocument/uri")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                else {
                    continue;
                };
                // A save without text is not a reason to forget the buffer.
                if let Some(text) = text_of(&params) {
                    open.insert(uri.clone(), text);
                }
                if let Some(text) = open.get(&uri) {
                    publish(&mut output, &uri, &text.clone());
                }
            }
            "textDocument/didClose" => {
                if let Some(uri) = params.pointer("/textDocument/uri").and_then(Value::as_str) {
                    open.remove(uri);
                    // Clear the squiggles; a closed file's warnings are noise.
                    publish(&mut output, uri, "");
                }
            }
            // An unknown REQUEST still needs an answer or the client waits.
            _ if id.is_some() => write_message(
                &mut output,
                &json!({ "jsonrpc": "2.0", "id": id, "result": Value::Null }),
            ),
            _ => {}
        }
    }
}

/// Repair every file that can be repaired, and say what was left behind.
fn fix(root: &Path) -> std::process::ExitCode {
    let mut files = Vec::new();
    stratify::source_files(root, &mut files);

    let mut fixed = 0usize;
    for path in &files {
        let Ok(source) = std::fs::read_to_string(path) else {
            continue;
        };
        if let Some(next) = stratify::fix_source(path, &source)
            && std::fs::write(path, &next).is_ok()
        {
            println!("fixed {}", path.display());
            fixed += 1;
        }
    }

    // Fixing a module list can reveal an item fault that was hiding below a
    // test module, so the report comes from a FRESH read rather than from
    // what was known before the edits.
    let report = stratify::check_tree(root);
    println!(
        "{fixed} file(s) fixed, {} left for a human",
        report.violations.len()
    );
    for v in &report.violations {
        println!("{}:{}: {}", v.file.display(), v.line, v.message());
    }
    if report.violations.is_empty() {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::FAILURE
    }
}

/// The standalone check, so this binary is testable without an editor.
fn check(root: &Path) -> std::process::ExitCode {
    let report = stratify::check_tree(root);
    for v in &report.violations {
        println!("{}:{}: {}", v.file.display(), v.line, v.message());
    }
    println!(
        "{} violation(s) — {} files, {} ordered, {} module lists, {} workspaces",
        report.violations.len(),
        report.files_seen,
        report.files_checked,
        report.lists_checked,
        report.workspaces_checked,
    );
    if report.violations.is_empty() {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::FAILURE
    }
}

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.split_first() {
        Some((flag, rest)) if flag == "--check" => {
            let root = rest.first().map_or(".", String::as_str);
            check(Path::new(root))
        }
        Some((flag, rest)) if flag == "--fix" => {
            let root = rest.first().map_or(".", String::as_str);
            fix(Path::new(root))
        }
        _ => {
            serve();
            std::process::ExitCode::SUCCESS
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn when_a_uri_is_percent_encoded_then_the_path_comes_back_whole() {
        assert_eq!(
            path_of("file:///a/b%20c/mod.rs"),
            Some(PathBuf::from("/a/b c/mod.rs"))
        );
        assert_eq!(path_of("untitled:Untitled-1"), None);
    }

    #[test]
    fn when_a_buffer_is_out_of_order_then_a_diagnostic_lands_on_that_line() {
        let source = "fn top() { helper(); }\nfn helper() {}\n";
        let found = diagnostics(Path::new("/x/x.rs"), source);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0]["range"]["start"]["line"], json!(0));
        assert_eq!(found[0]["range"]["end"]["character"], json!(22));
        assert_eq!(found[0]["severity"], json!(WARNING));
        assert_eq!(found[0]["code"], json!("stratify/item-order"));
        assert_eq!(
            found[0]["relatedInformation"][0]["location"]["range"]["start"]["line"],
            json!(1),
            "the related location points at what should come first"
        );
    }

    #[test]
    fn when_a_buffer_is_ordered_then_there_are_no_diagnostics() {
        assert!(diagnostics(Path::new("/x/x.rs"), "fn helper() {}\nfn top() { helper(); }\n").is_empty());
    }

    /// The framing is the part that silently breaks: read one byte too many and
    /// the server answers nothing forever.
    #[test]
    fn when_two_messages_arrive_together_then_both_are_read() {
        let frame = |body: &str| format!("Content-Length: {}\r\n\r\n{body}", body.len());
        let stream = format!("{}{}", frame(r#"{"id":1,"method":"initialize"}"#), frame(r#"{"method":"exit"}"#));
        let mut input = stream.as_bytes();

        let first = read_message(&mut input).expect("first message");
        assert_eq!(first["method"], json!("initialize"));
        let second = read_message(&mut input).expect("second message");
        assert_eq!(second["method"], json!("exit"));
        assert!(read_message(&mut input).is_none(), "and then the stream ends");
    }

    /// A save arrives with no text; the server must remember the buffer.
    #[test]
    fn when_a_save_carries_no_text_then_the_open_buffer_is_still_found() {
        let open = json!({ "textDocument": { "uri": "file:///x.rs", "text": "fn a() {}\n" } });
        assert_eq!(text_of(&open).as_deref(), Some("fn a() {}\n"));

        let change = json!({ "contentChanges": [{ "text": "fn b() {}\n" }] });
        assert_eq!(text_of(&change).as_deref(), Some("fn b() {}\n"));

        let save = json!({ "textDocument": { "uri": "file:///x.rs" } });
        assert_eq!(text_of(&save), None);
    }
}
