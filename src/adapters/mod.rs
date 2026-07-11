//! Host config adapters for Claude Code and Codex.
//!
//! Copy-first extraction source: `origin/ishoo/src/model/adapters.rs`.
//! The retained behavior is intentionally conservative:
//! - explicit install only
//! - repo and user/global scopes
//! - no-clobber merges
//! - owned-entry detection before update/remove
//! - skipped file-level failures instead of whole-run clobbering
//! - readiness facts that explain effective host setup

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

const MANAGED_BEGIN: &str = "<!-- mcp-product-infra:begin -->";
const MANAGED_END: &str = "<!-- mcp-product-infra:end -->";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostServer {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub codex_approval_tools: Vec<String>,
}

impl HostServer {
    pub fn stdio(
        name: impl Into<String>,
        command: impl Into<String>,
        args: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            name: name.into(),
            command: command.into(),
            args: args.into_iter().map(Into::into).collect(),
            env: BTreeMap::new(),
            codex_approval_tools: Vec::new(),
        }
    }

    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }

    pub fn approve_tool(mut self, tool_name: impl Into<String>) -> Self {
        self.codex_approval_tools.push(tool_name.into());
        self
    }
}

#[derive(Clone, Debug)]
pub struct HostInstall {
    pub app_name: String,
    pub servers: Vec<HostServer>,
    pub managed_markdown_body: Option<String>,
    pub managed_markdown_markers: Option<(String, String)>,
    pub backup_existing_managed_markdown: bool,
    pub claude_allowed_commands: Vec<String>,
}

impl HostInstall {
    pub fn new(app_name: impl Into<String>) -> Self {
        Self {
            app_name: app_name.into(),
            servers: Vec::new(),
            managed_markdown_body: None,
            managed_markdown_markers: None,
            backup_existing_managed_markdown: false,
            claude_allowed_commands: Vec::new(),
        }
    }

    pub fn server(mut self, server: HostServer) -> Self {
        self.servers.push(server);
        self
    }

    pub fn managed_markdown_body(mut self, body: impl Into<String>) -> Self {
        self.managed_markdown_body = Some(body.into());
        self
    }

    /// Override the delimiters used for the managed CLAUDE.md / AGENTS.md block.
    ///
    /// The default product-owned markers remain `mcp-product-infra:begin/end`.
    /// Consumers with an established ownership contract can retain their own markers
    /// so an upgrade refreshes the existing block instead of adding a second one.
    pub fn managed_markdown_markers(
        mut self,
        begin: impl Into<String>,
        end: impl Into<String>,
    ) -> Self {
        self.managed_markdown_markers = Some((begin.into(), end.into()));
        self
    }

    /// Preserve a one-time `<file>.bak` before modifying an existing instruction file.
    pub fn backup_existing_managed_markdown(mut self) -> Self {
        self.backup_existing_managed_markdown = true;
        self
    }

    pub fn claude_allow(mut self, command: impl Into<String>) -> Self {
        self.claude_allowed_commands.push(command.into());
        self
    }

    pub fn install_repo(&self, repo_root: &Path) -> Result<InstallReport, String> {
        let repo_root =
            find_git_root(repo_root).ok_or_else(|| "not inside a git repository".to_string())?;
        let mut files = Vec::new();
        files.push((
            ".mcp.json".to_string(),
            with_action(&repo_root.join(".mcp.json"), || {
                self.ensure_mcp_json(&repo_root)
            })?,
        ));
        files.push((
            ".codex/config.toml".to_string(),
            with_action(&repo_root.join(".codex/config.toml"), || {
                self.ensure_codex_repo_config(&repo_root)
            })?,
        ));
        files.push((
            ".claude/settings.local.json".to_string(),
            with_action(&repo_root.join(".claude/settings.local.json"), || {
                self.ensure_claude_settings(&repo_root)
            })?,
        ));
        files.push((
            ".claude/.gitignore".to_string(),
            with_action(&repo_root.join(".claude/.gitignore"), || {
                ensure_claude_gitignore(&repo_root)
            })?,
        ));
        if self.managed_markdown_body.is_some() {
            files.push((
                "CLAUDE.md".to_string(),
                with_action(&repo_root.join("CLAUDE.md"), || {
                    self.ensure_managed_markdown(&repo_root.join("CLAUDE.md"))
                })?,
            ));
            files.push((
                "AGENTS.md".to_string(),
                with_action(&repo_root.join("AGENTS.md"), || {
                    self.ensure_managed_markdown(&repo_root.join("AGENTS.md"))
                })?,
            ));
        }
        Ok(InstallReport {
            root: repo_root,
            files,
        })
    }

    pub fn install_user(&self) -> Result<InstallReport, String> {
        let paths = default_user_config_paths();
        self.install_user_at(&paths.codex_config, &paths.claude_json)
            .map(|mut report| {
                report.root = paths.home;
                report
            })
    }

    /// Materialize user-scope registrations at explicit config paths.
    ///
    /// This supports host integrations that supply their own home/config roots in
    /// tests or embedded environments without mutating the process environment.
    pub fn install_user_at(
        &self,
        codex_config: &Path,
        claude_json: &Path,
    ) -> Result<InstallReport, String> {
        let mut files = Vec::new();
        files.push((
            codex_config.display().to_string(),
            with_action(codex_config, || self.ensure_codex_user_config(codex_config))?,
        ));
        files.push((
            claude_json.display().to_string(),
            with_action(claude_json, || self.ensure_claude_user_config(claude_json))?,
        ));
        Ok(InstallReport {
            root: codex_config
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_default(),
            files,
        })
    }

    pub fn remove_user(&self) -> Result<InstallReport, String> {
        let paths = default_user_config_paths();
        self.remove_user_at(&paths.codex_config, &paths.claude_json)
            .map(|mut report| {
                report.root = paths.home;
                report
            })
    }

    /// Remove only owned user-scope registrations at explicit config paths.
    pub fn remove_user_at(
        &self,
        codex_config: &Path,
        claude_json: &Path,
    ) -> Result<InstallReport, String> {
        let mut files = Vec::new();
        files.push((
            codex_config.display().to_string(),
            with_action(codex_config, || self.remove_codex_user_config(codex_config))?,
        ));
        files.push((
            claude_json.display().to_string(),
            with_action(claude_json, || self.remove_claude_user_config(claude_json))?,
        ));
        Ok(InstallReport {
            root: codex_config
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_default(),
            files,
        })
    }

    pub fn readiness(&self, repo_root: &Path) -> Vec<HostReadinessReport> {
        let paths = default_user_config_paths();
        self.readiness_at(repo_root, &paths.codex_config, &paths.claude_json)
    }

    /// Report host readiness using explicit user-scope config paths.
    pub fn readiness_at(
        &self,
        repo_root: &Path,
        codex_config: &Path,
        claude_json: &Path,
    ) -> Vec<HostReadinessReport> {
        vec![
            build_readiness(
                "Claude Code",
                inspect_claude_config(claude_json, &self.servers),
                inspect_claude_config(&repo_root.join(".mcp.json"), &self.servers),
            ),
            build_readiness(
                "Codex",
                inspect_codex_config(codex_config, &self.servers),
                inspect_codex_config(&repo_root.join(".codex/config.toml"), &self.servers),
            ),
        ]
    }

    fn ensure_mcp_json(&self, repo_root: &Path) -> Result<Materialized, String> {
        let path = repo_root.join(".mcp.json");
        let mut doc = match fs::read_to_string(&path) {
            Err(_) => json!({ "mcpServers": {} }),
            Ok(existing) => match serde_json::from_str::<Value>(&existing) {
                Ok(v) if v.is_object() => v,
                _ => {
                    return Ok(Materialized::Skipped(
                        ".mcp.json is not parseable JSON".to_string(),
                    ))
                }
            },
        };
        let Some(root) = doc.as_object_mut() else {
            return Ok(Materialized::Skipped(
                ".mcp.json is not a JSON object".to_string(),
            ));
        };
        let servers = root.entry("mcpServers").or_insert_with(|| json!({}));
        let Some(servers) = servers.as_object_mut() else {
            return Ok(Materialized::Skipped(
                ".mcp.json `mcpServers` is not an object".to_string(),
            ));
        };
        for server in &self.servers {
            servers
                .entry(server.name.clone())
                .or_insert_with(|| claude_server_json(server));
        }
        write_json(&path, &doc)?;
        Ok(Materialized::Wrote)
    }

    fn ensure_codex_repo_config(&self, repo_root: &Path) -> Result<Materialized, String> {
        let dir = repo_root.join(".codex");
        let path = dir.join("config.toml");
        let existing = fs::read_to_string(&path).unwrap_or_default();
        let table = match parse_toml_materialized(&existing, ".codex/config.toml") {
            Ok(table) => table,
            Err(skipped) => return Ok(skipped),
        };
        let mut additions = String::new();
        for server in &self.servers {
            append_codex_server(&table, &mut additions, server);
        }
        if additions.is_empty() {
            return Ok(Materialized::Wrote);
        }
        fs::create_dir_all(&dir).map_err(|e| format!("failed to create {}: {e}", dir.display()))?;
        let mut text = existing;
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&additions);
        while text.ends_with("\n\n") {
            text.pop();
        }
        fs::write(&path, text).map_err(|e| format!("failed to write {}: {e}", path.display()))?;
        Ok(Materialized::Wrote)
    }

    fn ensure_codex_user_config(&self, path: &Path) -> Result<Materialized, String> {
        let existing = fs::read_to_string(path).unwrap_or_default();
        let mut table = match parse_toml_materialized(&existing, &path.display().to_string()) {
            Ok(table) => table,
            Err(skipped) => return Ok(skipped),
        };
        let servers = table
            .entry("mcp_servers".to_string())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        let Some(servers) = servers.as_table_mut() else {
            return Ok(Materialized::Skipped(
                "Codex `mcp_servers` is not a table".to_string(),
            ));
        };
        for server in &self.servers {
            if let Some(existing) = servers.get(&server.name) {
                if !codex_server_is_owned(existing, server) {
                    return Ok(Materialized::Skipped(format!(
                        "Codex already has a non-owned `{}` MCP server; leaving it untouched",
                        server.name
                    )));
                }
            }
            servers.insert(server.name.clone(), codex_server_toml(server));
        }
        write_toml(path, &table)?;
        Ok(Materialized::Wrote)
    }

    fn remove_codex_user_config(&self, path: &Path) -> Result<Materialized, String> {
        let existing = match fs::read_to_string(path) {
            Ok(v) => v,
            Err(_) => return Ok(Materialized::Skipped("not present".to_string())),
        };
        let mut table = match parse_toml_materialized(&existing, &path.display().to_string()) {
            Ok(table) => table,
            Err(skipped) => return Ok(skipped),
        };
        let mut changed = false;
        if let Some(servers) = table.get_mut("mcp_servers").and_then(|v| v.as_table_mut()) {
            for server in &self.servers {
                if servers
                    .get(&server.name)
                    .is_some_and(|v| codex_server_is_owned(v, server))
                {
                    servers.remove(&server.name);
                    changed = true;
                }
            }
        }
        if changed {
            write_toml(path, &table)?;
        }
        Ok(Materialized::Wrote)
    }

    fn ensure_claude_user_config(&self, path: &Path) -> Result<Materialized, String> {
        let mut doc = match fs::read_to_string(path) {
            Err(_) => json!({ "mcpServers": {} }),
            Ok(existing) => match serde_json::from_str::<Value>(&existing) {
                Ok(v) if v.is_object() => v,
                _ => {
                    return Ok(Materialized::Skipped(format!(
                        "{} is not parseable JSON",
                        path.display()
                    )))
                }
            },
        };
        let Some(root) = doc.as_object_mut() else {
            return Ok(Materialized::Skipped(
                "Claude config is not an object".to_string(),
            ));
        };
        let servers = root.entry("mcpServers").or_insert_with(|| json!({}));
        let Some(servers) = servers.as_object_mut() else {
            return Ok(Materialized::Skipped(
                "Claude `mcpServers` is not an object".to_string(),
            ));
        };
        for server in &self.servers {
            if let Some(existing) = servers.get(&server.name) {
                if !claude_server_is_owned(existing, server) {
                    return Ok(Materialized::Skipped(format!(
                        "Claude already has a non-owned `{}` MCP server; leaving it untouched",
                        server.name
                    )));
                }
            }
            servers.insert(server.name.clone(), claude_server_json(server));
        }
        write_json(path, &doc)?;
        Ok(Materialized::Wrote)
    }

    fn remove_claude_user_config(&self, path: &Path) -> Result<Materialized, String> {
        let existing = match fs::read_to_string(path) {
            Ok(v) => v,
            Err(_) => return Ok(Materialized::Skipped("not present".to_string())),
        };
        let mut doc = match serde_json::from_str::<Value>(&existing) {
            Ok(v) if v.is_object() => v,
            _ => {
                return Ok(Materialized::Skipped(format!(
                    "{} is not parseable JSON",
                    path.display()
                )))
            }
        };
        let mut changed = false;
        if let Some(servers) = doc.get_mut("mcpServers").and_then(|v| v.as_object_mut()) {
            for server in &self.servers {
                if servers
                    .get(&server.name)
                    .is_some_and(|v| claude_server_is_owned(v, server))
                {
                    servers.remove(&server.name);
                    changed = true;
                }
            }
        }
        if changed {
            write_json(path, &doc)?;
        }
        Ok(Materialized::Wrote)
    }

    fn ensure_claude_settings(&self, repo_root: &Path) -> Result<Materialized, String> {
        let dir = repo_root.join(".claude");
        let path = dir.join("settings.local.json");
        let mut doc = match fs::read_to_string(&path) {
            Err(_) => json!({ "permissions": { "allow": [] } }),
            Ok(existing) => match serde_json::from_str::<Value>(&existing) {
                Ok(v) if v.is_object() => v,
                _ => {
                    return Ok(Materialized::Skipped(
                        ".claude/settings.local.json is not parseable JSON".to_string(),
                    ))
                }
            },
        };
        let Some(root) = doc.as_object_mut() else {
            return Ok(Materialized::Skipped(
                "Claude settings is not an object".to_string(),
            ));
        };
        let permissions = root.entry("permissions").or_insert_with(|| json!({}));
        let Some(permissions) = permissions.as_object_mut() else {
            return Ok(Materialized::Skipped(
                "Claude `permissions` is not an object".to_string(),
            ));
        };
        let allow = permissions.entry("allow").or_insert_with(|| json!([]));
        let Some(allow) = allow.as_array_mut() else {
            return Ok(Materialized::Skipped(
                "Claude `permissions.allow` is not an array".to_string(),
            ));
        };
        for command in &self.claude_allowed_commands {
            if !allow.iter().any(|v| v.as_str() == Some(command)) {
                allow.push(Value::String(command.clone()));
            }
        }
        fs::create_dir_all(&dir).map_err(|e| format!("failed to create {}: {e}", dir.display()))?;
        write_json(&path, &doc)?;
        Ok(Materialized::Wrote)
    }

    fn ensure_managed_markdown(&self, path: &Path) -> Result<Materialized, String> {
        let body = self.managed_markdown_body.clone().unwrap_or_default();
        let (begin, end) = self
            .managed_markdown_markers
            .as_ref()
            .map(|(begin, end)| (begin.as_str(), end.as_str()))
            .unwrap_or((MANAGED_BEGIN, MANAGED_END));
        let block = format!("{begin}\n{body}\n{end}\n");
        let existing = fs::read_to_string(path).ok();
        let new_text = match &existing {
            None => block,
            Some(existing) => match (existing.find(begin), existing.find(end)) {
                (Some(start), Some(end_marker_start)) if end_marker_start >= start => {
                    let end_marker_end = end_marker_start + end.len();
                    let mut out = String::with_capacity(existing.len());
                    out.push_str(&existing[..start]);
                    out.push_str(block.trim_end_matches('\n'));
                    out.push_str(&existing[end_marker_end..]);
                    out
                }
                _ => format!("{block}\n{existing}"),
            },
        };
        if existing.as_ref().is_some_and(|text| text == &new_text) {
            return Ok(Materialized::Wrote);
        }
        if self.backup_existing_managed_markdown {
            if let Some(existing) = &existing {
                let mut backup = path.as_os_str().to_os_string();
                backup.push(".bak");
                let backup = PathBuf::from(backup);
                if !backup.exists() {
                    fs::write(&backup, existing)
                        .map_err(|e| format!("failed to write {}: {e}", backup.display()))?;
                }
            }
        }
        fs::write(path, new_text)
            .map_err(|e| format!("failed to write {}: {e}", path.display()))?;
        Ok(Materialized::Wrote)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdapterAction {
    Created,
    Updated,
    Unchanged,
    Skipped(String),
}

impl AdapterAction {
    pub fn tag(&self) -> &'static str {
        match self {
            AdapterAction::Created => "created",
            AdapterAction::Updated => "updated",
            AdapterAction::Unchanged => "unchanged",
            AdapterAction::Skipped(_) => "skipped",
        }
    }
}

#[derive(Debug, Clone)]
pub struct InstallReport {
    pub root: PathBuf,
    pub files: Vec<(String, AdapterAction)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostConfigFact {
    /// `current` | `absent` | `drifted` | `unreadable` | `shadowed`.
    pub state: String,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostReadinessReport {
    pub host: String,
    pub user_registration: HostConfigFact,
    pub repository_adapter: HostConfigFact,
    /// `user` | `repository` | `both` | `none`.
    pub effective_source: String,
    /// `reachable` | `unreachable` | `unchecked`.
    pub connectivity: String,
    pub ready: bool,
    pub result: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub primary_action: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secondary_action: Option<String>,
}

enum Materialized {
    Wrote,
    Skipped(String),
}

fn with_action<F>(path: &Path, materialize: F) -> Result<AdapterAction, String>
where
    F: FnOnce() -> Result<Materialized, String>,
{
    let before = fs::read(path).ok();
    match materialize()? {
        Materialized::Skipped(reason) => Ok(AdapterAction::Skipped(reason)),
        Materialized::Wrote => {
            let after = fs::read(path).ok();
            Ok(match (before, after) {
                (None, Some(_)) => AdapterAction::Created,
                (Some(b), Some(a)) if b == a => AdapterAction::Unchanged,
                (Some(_), Some(_)) => AdapterAction::Updated,
                (_, None) => AdapterAction::Skipped("no file after write".to_string()),
            })
        }
    }
}

fn append_codex_server(table: &toml::Table, additions: &mut String, server: &HostServer) {
    if !toml_path_exists(table, &["mcp_servers", &server.name]) {
        additions.push_str(&format!(
            "[mcp_servers.{}]\ncommand = {:?}\nargs = [{}]\n\n",
            server.name,
            server.command,
            server
                .args
                .iter()
                .map(|arg| format!("{arg:?}"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    for tool in &server.codex_approval_tools {
        if !toml_path_exists(table, &["mcp_servers", &server.name, "tools", tool]) {
            additions.push_str(&format!(
                "[mcp_servers.{}.tools.{}]\napproval_mode = \"approve\"\n\n",
                server.name, tool
            ));
        }
    }
}

fn toml_path_exists(table: &toml::Table, path: &[&str]) -> bool {
    let mut value = match table.get(path[0]) {
        Some(value) => value,
        None => return false,
    };
    for key in &path[1..] {
        value = match value.as_table().and_then(|t| t.get(*key)) {
            Some(value) => value,
            None => return false,
        };
    }
    value.as_table().is_some()
}

fn parse_toml_materialized(existing: &str, label: &str) -> Result<toml::Table, Materialized> {
    if existing.trim().is_empty() {
        return Ok(toml::Table::new());
    }
    existing
        .parse::<toml::Table>()
        .map_err(|_| Materialized::Skipped(format!("{label} is not parseable TOML")))
}

fn codex_server_toml(server: &HostServer) -> toml::Value {
    let mut table = toml::Table::new();
    table.insert(
        "command".to_string(),
        toml::Value::String(server.command.clone()),
    );
    table.insert(
        "args".to_string(),
        toml::Value::Array(
            server
                .args
                .iter()
                .cloned()
                .map(toml::Value::String)
                .collect(),
        ),
    );
    if !server.codex_approval_tools.is_empty() {
        let mut tools = toml::Table::new();
        for tool in &server.codex_approval_tools {
            let mut mode = toml::Table::new();
            mode.insert(
                "approval_mode".to_string(),
                toml::Value::String("approve".to_string()),
            );
            tools.insert(tool.clone(), toml::Value::Table(mode));
        }
        table.insert("tools".to_string(), toml::Value::Table(tools));
    }
    toml::Value::Table(table)
}

fn claude_server_json(server: &HostServer) -> Value {
    // Emit the minimal `{command, args}` entry — the implicit stdio `type` and an
    // empty `env` are omitted so output stays byte-identical to a hand-authored
    // registration and re-running enable on an existing file is a true no-op.
    // `env` is included only when the server actually carries variables.
    // `claude_server_is_owned` treats an absent `type` as stdio, so ownership
    // detection is unaffected.
    let mut entry = serde_json::Map::new();
    entry.insert("command".into(), Value::String(server.command.clone()));
    entry.insert(
        "args".into(),
        Value::Array(server.args.iter().cloned().map(Value::String).collect()),
    );
    if !server.env.is_empty() {
        entry.insert(
            "env".into(),
            Value::Object(
                server
                    .env
                    .iter()
                    .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                    .collect(),
            ),
        );
    }
    Value::Object(entry)
}

fn codex_server_is_owned(value: &toml::Value, expected: &HostServer) -> bool {
    let Some(table) = value.as_table() else {
        return false;
    };
    let Some(command) = table.get("command").and_then(|v| v.as_str()) else {
        return false;
    };
    let args_match = table
        .get("args")
        .and_then(|v| v.as_array())
        .is_some_and(|args| {
            args.iter()
                .filter_map(|v| v.as_str())
                .eq(expected.args.iter().map(String::as_str))
        });
    command_name_matches(command, &expected.command) && args_match
}

fn claude_server_is_owned(value: &Value, expected: &HostServer) -> bool {
    let command = value.get("command").and_then(|v| v.as_str());
    let args_match = value
        .get("args")
        .and_then(|v| v.as_array())
        .is_some_and(|args| {
            args.iter()
                .filter_map(|v| v.as_str())
                .eq(expected.args.iter().map(String::as_str))
        });
    let stdio_or_absent = match value.get("type").and_then(|v| v.as_str()) {
        Some(t) => t == "stdio",
        None => true,
    };
    command.is_some_and(|c| command_name_matches(c, &expected.command))
        && args_match
        && stdio_or_absent
}

fn command_name_matches(actual: &str, expected: &str) -> bool {
    actual == expected
        || Path::new(actual).file_name().and_then(|name| name.to_str())
            == Path::new(expected)
                .file_name()
                .and_then(|name| name.to_str())
}

fn inspect_claude_config(path: &Path, expected: &[HostServer]) -> HostConfigFact {
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(_) => return fact("absent", path, None),
    };
    let doc: Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(_) => return fact("unreadable", path, Some("not parseable JSON".to_string())),
    };
    let Some(servers) = doc.get("mcpServers").and_then(|v| v.as_object()) else {
        return fact("absent", path, Some("missing mcpServers".to_string()));
    };
    for server in expected {
        match servers.get(&server.name) {
            Some(value) if claude_server_is_owned(value, server) => {}
            Some(_) => {
                return fact(
                    "drifted",
                    path,
                    Some(format!("{} exists but is not owned/current", server.name)),
                )
            }
            None => {
                return fact(
                    "drifted",
                    path,
                    Some(format!("missing {} MCP server", server.name)),
                )
            }
        }
    }
    fact("current", path, None)
}

fn inspect_codex_config(path: &Path, expected: &[HostServer]) -> HostConfigFact {
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(_) => return fact("absent", path, None),
    };
    let table: toml::Table = match raw.parse() {
        Ok(v) => v,
        Err(_) => return fact("unreadable", path, Some("not parseable TOML".to_string())),
    };
    let Some(servers) = table.get("mcp_servers").and_then(|v| v.as_table()) else {
        return fact("absent", path, Some("missing mcp_servers".to_string()));
    };
    for server in expected {
        match servers.get(&server.name) {
            Some(value) if codex_server_is_owned(value, server) => {}
            Some(_) => {
                return fact(
                    "drifted",
                    path,
                    Some(format!("{} exists but is not owned/current", server.name)),
                )
            }
            None => {
                return fact(
                    "drifted",
                    path,
                    Some(format!("missing {} MCP server", server.name)),
                )
            }
        }
    }
    fact("current", path, None)
}

fn build_readiness(
    host: &str,
    user_registration: HostConfigFact,
    repository_adapter: HostConfigFact,
) -> HostReadinessReport {
    let user_current = user_registration.state == "current";
    let repo_current = repository_adapter.state == "current";
    let repo_blocks_user = user_current
        && matches!(
            repository_adapter.state.as_str(),
            "drifted" | "unreadable" | "shadowed"
        );

    let mut repository_adapter = repository_adapter;
    let (effective_source, ready, result, primary_action, secondary_action) = if repo_blocks_user {
        repository_adapter.state = "shadowed".to_string();
        if repository_adapter.detail.is_none() {
            repository_adapter.detail = Some(
                "repository adapter overrides the current user/global registration".to_string(),
            );
        }
        (
            "repository",
            false,
            "repository_override_blocks_global",
            Some("Repair repository setup".to_string()),
            Some("Remove repository override or add shared repository setup".to_string()),
        )
    } else if user_current && repo_current {
        (
            "both",
            true,
            "ready_both",
            None,
            Some("Add shared repository setup".to_string()),
        )
    } else if user_current {
        (
            "user",
            true,
            "ready_globally",
            None,
            Some("Add shared repository setup".to_string()),
        )
    } else if repo_current {
        ("repository", true, "ready_repository", None, None)
    } else {
        (
            "none",
            false,
            "setup_required",
            Some("Set up this repo for agents".to_string()),
            Some("Register user-wide setup".to_string()),
        )
    };

    HostReadinessReport {
        host: host.to_string(),
        user_registration,
        repository_adapter,
        effective_source: effective_source.to_string(),
        connectivity: "unchecked".to_string(),
        ready,
        result: result.to_string(),
        primary_action,
        secondary_action,
    }
}

fn fact(state: &str, path: &Path, detail: Option<String>) -> HostConfigFact {
    HostConfigFact {
        state: state.to_string(),
        path: path.display().to_string(),
        detail,
    }
}

fn ensure_claude_gitignore(repo_root: &Path) -> Result<Materialized, String> {
    let dir = repo_root.join(".claude");
    let path = dir.join(".gitignore");
    let entry = "scheduled_tasks.lock";
    let existing = fs::read_to_string(&path).unwrap_or_default();
    if existing.lines().any(|line| line.trim() == entry) {
        return Ok(Materialized::Wrote);
    }
    fs::create_dir_all(&dir).map_err(|e| format!("failed to create {}: {e}", dir.display()))?;
    let mut text = existing;
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(entry);
    text.push('\n');
    fs::write(&path, text).map_err(|e| format!("failed to write {}: {e}", path.display()))?;
    Ok(Materialized::Wrote)
}

fn write_json(path: &Path, value: &Value) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
    }
    let mut text = serde_json::to_string_pretty(value).map_err(|e| e.to_string())?;
    text.push('\n');
    fs::write(path, text).map_err(|e| format!("failed to write {}: {e}", path.display()))
}

fn write_toml(path: &Path, table: &toml::Table) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
    }
    let mut text = toml::to_string_pretty(table).map_err(|e| e.to_string())?;
    if !text.ends_with('\n') {
        text.push('\n');
    }
    fs::write(path, text).map_err(|e| format!("failed to write {}: {e}", path.display()))
}

struct UserConfigPaths {
    home: PathBuf,
    codex_config: PathBuf,
    claude_json: PathBuf,
}

fn default_user_config_paths() -> UserConfigPaths {
    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("~"));
    let codex_home = env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".codex"));
    UserConfigPaths {
        codex_config: codex_home.join("config.toml"),
        claude_json: home.join(".claude.json"),
        home,
    }
}

fn find_git_root(start: &Path) -> Option<PathBuf> {
    let mut dir = start;
    loop {
        if dir.join(".git").exists() {
            return Some(dir.to_path_buf());
        }
        dir = dir.parent()?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_install_creates_expected_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".git")).unwrap();
        let install = HostInstall::new("todo")
            .server(HostServer::stdio("todo", "todo", ["mcp"]).approve_tool("todo_create"))
            .managed_markdown_body("Use todo_* MCP tools.")
            .claude_allow("Bash(todo *)");
        let report = install.install_repo(dir.path()).unwrap();
        assert_eq!(report.files.len(), 6);
        assert!(dir.path().join(".mcp.json").exists());
        assert!(dir.path().join(".codex/config.toml").exists());
        assert!(dir.path().join("CLAUDE.md").exists());
    }

    #[test]
    fn mcp_json_server_entry_is_minimal_and_env_only_when_set() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".git")).unwrap();
        HostInstall::new("todo")
            .server(HostServer::stdio("todo", "todo", ["mcp"]))
            .server(HostServer::stdio("keyed", "keyed", ["mcp"]).env("TOKEN", "abc"))
            .install_repo(dir.path())
            .unwrap();

        let doc: Value =
            serde_json::from_str(&fs::read_to_string(dir.path().join(".mcp.json")).unwrap())
                .unwrap();
        let plain = &doc["mcpServers"]["todo"];
        // Byte-parity with a hand-authored entry: no implicit `type`, no empty `env`.
        assert!(plain.get("type").is_none(), "no default type: {plain}");
        assert!(plain.get("env").is_none(), "no empty env: {plain}");
        assert_eq!(plain["command"], "todo");
        assert_eq!(plain["args"][0], "mcp");
        // A server that carries variables still emits its env.
        let keyed = &doc["mcpServers"]["keyed"];
        assert_eq!(keyed["env"]["TOKEN"], "abc");
        assert!(keyed.get("type").is_none());

        // Re-running enable is a true no-op on the minimal entry.
        let again = HostInstall::new("todo")
            .server(HostServer::stdio("todo", "todo", ["mcp"]))
            .server(HostServer::stdio("keyed", "keyed", ["mcp"]).env("TOKEN", "abc"))
            .install_repo(dir.path())
            .unwrap();
        let mcp = again.files.iter().find(|(f, _)| f == ".mcp.json").unwrap();
        assert_eq!(mcp.1, AdapterAction::Unchanged, "re-enable is a no-op");
    }

    #[test]
    fn configurable_markers_update_an_existing_block_in_place() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".git")).unwrap();
        let path = dir.path().join("CLAUDE.md");
        fs::write(
            &path,
            "user introduction\n<!-- ishoo:begin -->\nstale\n<!-- ishoo:end -->\nuser footer\n",
        )
        .unwrap();

        HostInstall::new("ishoo")
            .managed_markdown_body("fresh")
            .managed_markdown_markers("<!-- ishoo:begin -->", "<!-- ishoo:end -->")
            .install_repo(dir.path())
            .unwrap();

        assert_eq!(
            fs::read_to_string(path).unwrap(),
            "user introduction\n<!-- ishoo:begin -->\nfresh\n<!-- ishoo:end -->\nuser footer\n"
        );
    }

    #[test]
    fn unparseable_repo_codex_config_is_skipped_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".git")).unwrap();
        fs::create_dir_all(dir.path().join(".codex")).unwrap();
        fs::write(dir.path().join(".codex/config.toml"), "not = [toml").unwrap();
        let install = HostInstall::new("todo").server(HostServer::stdio("todo", "todo", ["mcp"]));
        let report = install.install_repo(dir.path()).unwrap();
        let codex = report
            .files
            .iter()
            .find(|(path, _)| path == ".codex/config.toml")
            .unwrap();
        assert!(matches!(codex.1, AdapterAction::Skipped(_)));
    }
}
