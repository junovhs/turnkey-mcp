//! Agent-guard: programmatic protection against shell footguns that corrupt
//! what an agent *reads back*, installed as a host PreToolUse hook.
//!
//! The first guarded footgun is ripgrep's `-r`: in ripgrep `-r` is `--replace`
//! (rg recurses by default), so grep muscle-memory `rg -rn PAT` runs
//! `rg --replace n PAT` — every match in the output is silently rewritten to
//! the literal letter `n`. The mangled output reads exactly like data
//! corruption, and agents have burned whole sessions misdiagnosing it as a
//! store/harness bug. Prose warnings don't survive contact with muscle memory;
//! this guard blocks the command before it runs.
//!
//! Apps expose [`run_stdin`] behind a hidden CLI subcommand (e.g.
//! `ishoo agent-guard`) and register it via
//! [`crate::HostInstall::claude_hook`], so the guard ships inside the product
//! binary and versions with it — no scripts materialized on user machines.
//!
//! Fail-open by construction: anything unparseable or non-Bash exits 0 (allow).
//! A guard that can wedge every shell command would be worse than the footgun.

use std::io::Read;

/// Exit code that tells the Claude Code hook runner to block the tool call
/// and surface stderr to the model.
pub const BLOCK_EXIT_CODE: i32 = 2;

/// The explanation surfaced to the agent when a command is blocked. Written to
/// teach the correction, not just refuse: the agent's very next attempt should
/// be the right command.
pub const RG_REPLACE_BLOCK_MESSAGE: &str = "BLOCKED by agent-guard: `rg` called with a short \
`-r` flag (e.g. -rn/-rln). In ripgrep, -r means --replace, NOT recursive — rg recurses by \
default. `rg -rn PAT` runs `rg --replace n PAT`: every match in the output is silently \
rewritten to the letter 'n', which reads exactly like corrupted data. Re-run as \
`rg -n PAT [PATH]`. If you genuinely want replacement output, spell out `--replace <text>`.";

/// ripgrep short flags that consume a value: inside a bundled token the rest of
/// the token is that flag's value, not further flags (e.g. `-trust` = `--type
/// rust`), so scanning for `r` must stop there.
const RG_VALUE_FLAGS: &[char] = &[
    'A', 'B', 'C', 'd', 'E', 'e', 'f', 'g', 'j', 'M', 'm', 't', 'T',
];

/// Read a Claude Code hook payload from `input` and evaluate it. Returns the
/// process exit code; when blocking, the explanation has been written to
/// stderr. Never errors: any failure allows (exit 0).
pub fn run_stdin(input: &mut impl Read) -> i32 {
    let mut raw = String::new();
    if input.read_to_string(&mut raw).is_err() {
        return 0;
    }
    match evaluate_hook_payload(&raw) {
        Some(message) => {
            eprintln!("{message}");
            BLOCK_EXIT_CODE
        }
        None => 0,
    }
}

/// Evaluate a raw PreToolUse hook JSON payload. `Some(message)` means block.
pub fn evaluate_hook_payload(raw: &str) -> Option<&'static str> {
    let payload: serde_json::Value = serde_json::from_str(raw).ok()?;
    if payload.get("tool_name").and_then(|v| v.as_str()) != Some("Bash") {
        return None;
    }
    let command = payload
        .get("tool_input")
        .and_then(|v| v.get("command"))
        .and_then(|v| v.as_str())?;
    evaluate_bash_command(command)
}

/// Evaluate one shell command string. `Some(message)` means block.
pub fn evaluate_bash_command(command: &str) -> Option<&'static str> {
    if bash_command_misuses_rg_replace(command) {
        Some(RG_REPLACE_BLOCK_MESSAGE)
    } else {
        None
    }
}

/// True when any `rg` invocation in the command engages short `-r` (--replace).
/// Long-form `--replace` is allowed: it can only be deliberate.
fn bash_command_misuses_rg_replace(command: &str) -> bool {
    if !command.contains("rg") {
        return false;
    }
    // Split pipelines/lists so `sort -r | rg -n x` never false-positives; each
    // segment is scanned from every `rg` word (handles `xargs rg ...`, env
    // prefixes, absolute paths).
    for segment in split_shell_segments(command) {
        let tokens = tokenize(&segment);
        for (i, token) in tokens.iter().enumerate() {
            if base_name(token) != "rg" {
                continue;
            }
            for arg in &tokens[i + 1..] {
                if arg == "--" {
                    break;
                }
                if arg.starts_with("--") {
                    continue;
                }
                // A flag bundle never contains whitespace; a quoted multi-word
                // pattern like "-r is replace" is an argument, not flags.
                if arg.len() > 1
                    && arg.starts_with('-')
                    && !arg.contains(char::is_whitespace)
                    && bundle_engages_replace(arg)
                {
                    return true;
                }
            }
        }
    }
    false
}

/// Split on unquoted `|`, `||`, `&&`, `;` so flags of one pipeline stage are
/// never attributed to another.
fn split_shell_segments(command: &str) -> Vec<String> {
    let mut segments = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut chars = command.chars().peekable();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                }
                current.push(c);
            }
            None => match c {
                '\'' | '"' => {
                    quote = Some(c);
                    current.push(c);
                }
                '|' | ';' => {
                    if c == '|' && chars.peek() == Some(&'|') {
                        chars.next();
                    }
                    segments.push(std::mem::take(&mut current));
                }
                '&' if chars.peek() == Some(&'&') => {
                    chars.next();
                    segments.push(std::mem::take(&mut current));
                }
                _ => current.push(c),
            },
        }
    }
    segments.push(current);
    segments
}

/// Whitespace tokenization with simple single/double-quote awareness — enough
/// to keep quoted patterns as one token so a pattern like "-r foo" in quotes
/// is not read as a flag bundle plus argument.
fn tokenize(segment: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    for c in segment.chars() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                } else {
                    current.push(c);
                }
            }
            None => match c {
                '\'' | '"' => quote = Some(c),
                c if c.is_whitespace() => {
                    if !current.is_empty() {
                        tokens.push(std::mem::take(&mut current));
                    }
                }
                _ => current.push(c),
            },
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn base_name(token: &str) -> &str {
    token.rsplit(['/', '\\']).next().unwrap_or(token)
}

/// True when a short-flag bundle like `-rn`, `-nr`, or `-rln` reaches `r` as a
/// flag (not as some value-taking flag's bundled value).
fn bundle_engages_replace(token: &str) -> bool {
    for c in token.chars().skip(1) {
        if c == 'r' {
            return true;
        }
        if RG_VALUE_FLAGS.contains(&c) || !c.is_ascii_alphabetic() {
            return false;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bash_payload(command: &str) -> String {
        serde_json::json!({
            "tool_name": "Bash",
            "tool_input": { "command": command }
        })
        .to_string()
    }

    #[test]
    fn blocks_the_grep_muscle_memory_forms() {
        for cmd in [
            "rg -rn \"foo\" src/",
            "rg -rln reinstall src/",
            "rg -nr foo .",
            "rg -r n foo file.txt",
            "cd /x && rg -rn foo | head -5",
            "xargs rg -rn foo",
            "/usr/bin/rg -rn foo",
            // Shell quotes don't stop rg from parsing this as flags.
            "rg \"-rn\" foo src/",
        ] {
            assert!(
                evaluate_hook_payload(&bash_payload(cmd)).is_some(),
                "must block: {cmd}"
            );
        }
    }

    #[test]
    fn allows_correct_and_unrelated_usage() {
        for cmd in [
            "rg -n \"foo\" src/",
            "rg -in --replace X foo file",
            "rg --replace=X foo file",
            "rg -t rust foo",
            "rg -trust foo",
            "sort -r file.txt",
            "grep -rn foo src/",
            "rg -e foo -- -rfile",
            "ls | sort -r; rg -n foo",
            "echo 'rg -rn inside a string'",
            "rg -n \"-r is replace\" doc.md",
        ] {
            assert!(
                evaluate_hook_payload(&bash_payload(cmd)).is_none(),
                "must allow: {cmd}"
            );
        }
    }

    #[test]
    fn non_bash_tools_and_garbage_input_are_allowed() {
        assert!(evaluate_hook_payload("not json").is_none());
        assert!(evaluate_hook_payload("{}").is_none());
        let read_tool = serde_json::json!({
            "tool_name": "Read",
            "tool_input": { "file_path": "/tmp/x" }
        })
        .to_string();
        assert!(evaluate_hook_payload(&read_tool).is_none());
    }

    #[test]
    fn run_stdin_blocks_with_exit_2_and_allows_with_0() {
        let blocked = bash_payload("rg -rn foo").into_bytes();
        assert_eq!(run_stdin(&mut blocked.as_slice()), BLOCK_EXIT_CODE);
        let allowed = bash_payload("rg -n foo").into_bytes();
        assert_eq!(run_stdin(&mut allowed.as_slice()), 0);
    }
}
