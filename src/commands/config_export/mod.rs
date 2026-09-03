// SPDX-FileCopyrightText: 2026 Mikko Parkkola
// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
//! Implementation of `mcp-gateway setup export`.
//!
//! Reads `gateway.yaml` and writes client-native MCP configuration entries
//! into every supported AI client config file. Supports proxy mode (HTTP URL)
//! and stdio mode (subprocess spawn), with auto-detection based on whether a
//! gateway daemon is currently running.
//!
//! # Merge strategy
//!
//! Existing client config content is preserved. The exporter reads the file,
//! upserts the gateway entry under its configured name, and writes back via an
//! atomic tempfile-rename to prevent partial-write corruption.
//!
//! # Clients
//!
//! | Client         | Config key        | File                                   |
//! |----------------|-------------------|----------------------------------------|
//! | Claude Code    | `mcpServers`      | `~/.claude.json`                       |
//! | Claude Desktop | `mcpServers`      | platform-specific                      |
//! | Cursor         | `mcpServers`      | `.cursor/mcp.json` (workspace-rel)     |
//! | VS Code Copilot| `servers`         | `.vscode/mcp.json` (workspace-rel)     |
//! | Windsurf       | `mcpServers`      | `~/.codeium/windsurf/mcp_config.json`  |
//! | Cline          | `mcpServers`      | `.cline/mcp_servers.json` (ws-rel)     |
//! | Zed            | `context_servers` | `~/.config/zed/settings.json`          |
//! | `OpenCode`     | `mcp`             | `.opencode/opencode.jsonc` (workspace-rel, comment-preserving) |
//! | Generic        | `mcpServers`      | stdout or `--output`                   |
//!
//! `OpenCode`'s entry shape and merge strategy differ from every other
//! client — see `build_opencode_entry` and `merge_into_opencode_config`.

mod watch;

#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use mcp_gateway::{
    cli::{ConnectionMode, ExportTarget},
    config::Config,
};

use crate::commands::paths::{claude_desktop_path, home_path, windsurf_path, zed_settings_path};

// ── Internal types ────────────────────────────────────────────────────────────

/// Resolved config spec for a single AI client.
pub(super) struct ClientSpec {
    /// Human-readable label used in CLI output.
    pub(super) label: &'static str,
    /// Resolved filesystem path to the client's config file.
    pub(super) path: PathBuf,
    /// JSON key that holds the server map (`"mcpServers"`, `"servers"`, etc.).
    pub(super) servers_key: &'static str,
}

/// Outcome of attempting to export to one client.
pub struct ExportResult {
    pub client: &'static str,
    pub path: PathBuf,
    pub action: ExportAction,
    pub safety: Option<ExportSafety>,
}

/// Safety metadata emitted for an applied client config mutation.
pub struct ExportSafety {
    pub backup_path: Option<PathBuf>,
    pub verified: bool,
}

/// What the exporter did (or failed to do) for a single client.
#[derive(Debug, Clone)]
pub enum ExportAction {
    /// A new config file was created.
    Created,
    /// An existing config file was updated (gateway entry upserted).
    Updated,
    /// Client config directory is absent — nothing to do.
    Skipped(String),
    /// An error occurred; the file was not modified.
    Failed(String),
}

struct SafeMergeResult {
    action: ExportAction,
    backup_path: Option<PathBuf>,
    verified: bool,
}

const BACKUP_MARKER: &str = ".mcp-gateway.bak.";

// ── Core logic ────────────────────────────────────────────────────────────────

/// Build the JSON entry to insert for this gateway instance.
///
/// Proxy mode produces `{"url": "http://host:port/mcp"}`.
/// Stdio mode produces `{"command": "mcp-gateway", "args": ["serve", "--stdio", ...]}`.
pub fn build_gateway_entry(
    config: &Config,
    config_path: Option<&Path>,
    mode: ConnectionMode,
) -> Value {
    match resolve_mode(mode, config) {
        ConnectionMode::Proxy | ConnectionMode::Auto => {
            json!({
                "url": format!("http://{}:{}/mcp", config.server.host, config.server.port)
            })
        }
        ConnectionMode::Stdio => {
            let mut args = vec!["serve".to_string(), "--stdio".to_string()];
            if let Some(p) = config_path {
                args.push("-c".to_string());
                args.push(p.display().to_string());
            }
            json!({
                "command": "mcp-gateway",
                "args": args
            })
        }
    }
}

/// Build the JSON entry to insert for this gateway instance, in `OpenCode`'s
/// own schema.
///
/// `OpenCode`'s `mcp.<name>` entries are shaped differently from every other
/// client this exporter targets: a `type` discriminator, and `command` as a
/// single array combining the binary and its arguments (not a separate
/// `command` string + `args` array).
///
/// Proxy mode produces `{"type": "remote", "url": "...", "enabled": true}`.
/// Stdio mode produces `{"type": "local", "command": [...], "enabled": true}`.
pub fn build_opencode_entry(
    config: &Config,
    config_path: Option<&Path>,
    mode: ConnectionMode,
) -> Value {
    match resolve_mode(mode, config) {
        ConnectionMode::Proxy | ConnectionMode::Auto => {
            json!({
                "type": "remote",
                "url": format!("http://{}:{}/mcp", config.server.host, config.server.port),
                "enabled": true
            })
        }
        ConnectionMode::Stdio => {
            let mut command = vec![
                "mcp-gateway".to_string(),
                "serve".to_string(),
                "--stdio".to_string(),
            ];
            if let Some(p) = config_path {
                command.push("-c".to_string());
                command.push(p.display().to_string());
            }
            json!({
                "type": "local",
                "command": command,
                "enabled": true
            })
        }
    }
}

/// Resolve `Auto` mode to a concrete `Proxy` or `Stdio` decision.
///
/// Auto resolution:
/// 1. If a gateway daemon is reachable at `host:port/health` → Proxy.
/// 2. If `mcp-gateway` is on `PATH` → Stdio.
/// 3. Otherwise → Proxy (user must start daemon manually).
pub fn resolve_mode(mode: ConnectionMode, config: &Config) -> ConnectionMode {
    if mode != ConnectionMode::Auto {
        return mode;
    }
    let health_url = format!(
        "http://{}:{}/health",
        config.server.host, config.server.port
    );
    if probe_health(&health_url) {
        return ConnectionMode::Proxy;
    }
    if which_mcp_gateway().is_some() {
        ConnectionMode::Stdio
    } else {
        ConnectionMode::Proxy
    }
}

/// Probe the gateway health endpoint with a 500 ms TCP-connect timeout.
///
/// Returns `true` if a TCP connection succeeds (daemon is reachable).
fn probe_health(url: &str) -> bool {
    use std::net::{TcpStream, ToSocketAddrs};
    use std::time::Duration;

    let host_port = url
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .split('/')
        .next()
        .unwrap_or("");

    let addr = host_port
        .to_socket_addrs()
        .ok()
        .and_then(|mut it| it.next());

    match addr {
        Some(a) => TcpStream::connect_timeout(&a, Duration::from_millis(500)).is_ok(),
        None => false,
    }
}

/// Return the path to `mcp-gateway` if it is on `PATH`, else `None`.
fn which_mcp_gateway() -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|p| p.join("mcp-gateway"))
            .find(|p| p.is_file())
    })
}

/// Merge (upsert) a gateway entry into a JSON config file.
///
/// If the file does not exist, it is created with minimal structure.
/// All existing content is preserved; only `servers_key[entry_name]` is set.
/// The write is atomic (tempfile + rename in the same directory).
///
/// Returns the action taken, or an error string if the operation failed.
pub fn merge_into_config(
    path: &Path,
    servers_key: &str,
    entry_name: &str,
    entry: &Value,
) -> Result<ExportAction, String> {
    let existed = path.exists();
    let mut doc: Value = if existed {
        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("Cannot read {}: {e}", path.display()))?;
        serde_json::from_str(&content)
            .map_err(|e| format!("Cannot parse {}: {e}", path.display()))?
    } else {
        json!({})
    };

    {
        let root = doc
            .as_object_mut()
            .ok_or_else(|| "Config root is not a JSON object".to_string())?;
        let servers = root
            .entry(servers_key)
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .ok_or_else(|| format!("'{servers_key}' is not a JSON object"))?;
        servers.insert(entry_name.to_string(), entry.clone());
    }

    let action = if existed {
        ExportAction::Updated
    } else {
        ExportAction::Created
    };

    let json_str = serde_json::to_string_pretty(&doc)
        .map_err(|e| format!("JSON serialization failed: {e}"))?;

    atomic_write(path, &json_str)?;

    Ok(action)
}

/// Write `content` to `path` atomically: write to a sibling tempfile in the
/// same directory, then rename over the target. Ensures the parent directory
/// exists first.
fn atomic_write(path: &Path, content: &str) -> Result<(), String> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Cannot create {}: {e}", parent.display()))?;
    }

    let parent = path.parent().unwrap_or(Path::new("."));
    let file_name = path.file_name().map_or_else(
        || "config.json".to_string(),
        |n| n.to_string_lossy().into_owned(),
    );
    let tmp = parent.join(format!(".{file_name}.tmp"));
    std::fs::write(&tmp, content)
        .map_err(|e| format!("Cannot write temp file {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .map_err(|e| format!("Cannot rename {} -> {}: {e}", tmp.display(), path.display()))?;

    Ok(())
}

fn merge_into_config_with_safety(
    path: &Path,
    servers_key: &str,
    entry_name: &str,
    entry: &Value,
) -> Result<SafeMergeResult, String> {
    let backup_path = create_backup(path)?;
    let action = merge_into_config(path, servers_key, entry_name, entry)?;
    verify_gateway_entry(path, servers_key, entry_name, entry)?;

    Ok(SafeMergeResult {
        action,
        backup_path,
        verified: true,
    })
}

fn create_backup(path: &Path) -> Result<Option<PathBuf>, String> {
    if !path.exists() {
        return Ok(None);
    }

    let parent = path.parent().unwrap_or(Path::new("."));
    let file_name = path
        .file_name()
        .ok_or_else(|| format!("Cannot determine file name for {}", path.display()))?
        .to_string_lossy();
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| format!("System clock error while creating backup: {e}"))?
        .as_nanos();
    let backup_path = parent.join(format!("{file_name}{BACKUP_MARKER}{timestamp}"));

    std::fs::copy(path, &backup_path).map_err(|e| {
        format!(
            "Cannot create backup {} from {}: {e}",
            backup_path.display(),
            path.display()
        )
    })?;

    Ok(Some(backup_path))
}

fn verify_gateway_entry(
    path: &Path,
    servers_key: &str,
    entry_name: &str,
    entry: &Value,
) -> Result<(), String> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("Cannot verify {}: {e}", path.display()))?;
    let doc: Value = serde_json::from_str(&content)
        .map_err(|e| format!("Cannot parse {} after write: {e}", path.display()))?;
    let actual = doc
        .get(servers_key)
        .and_then(|servers| servers.get(entry_name))
        .ok_or_else(|| {
            format!(
                "Verification failed: '{}.{}' missing from {}",
                servers_key,
                entry_name,
                path.display()
            )
        })?;

    if actual == entry {
        Ok(())
    } else {
        Err(format!(
            "Verification failed: '{}.{}' in {} does not match planned entry",
            servers_key,
            entry_name,
            path.display()
        ))
    }
}

// ── OpenCode-specific merge (comment-preserving) ────────────────────────────────

/// Merge (upsert) a gateway entry into `OpenCode`'s `.opencode/opencode.jsonc`.
///
/// Unlike `merge_into_config`, which reserializes the whole document through
/// `serde_json::Value` (destroying any comments), this uses `jsonc-parser`'s
/// CST editor: it mutates only the `mcp.<entry_name>` node in place and
/// renders the rest of the document — including comments elsewhere in the
/// file — byte-for-byte unchanged. Comments *inside* the existing
/// `mcp.<entry_name>` entry, if any, are not preserved, since that node is
/// replaced wholesale.
fn merge_into_opencode_config(
    path: &Path,
    entry_name: &str,
    entry: &Value,
) -> Result<ExportAction, String> {
    let existed = path.exists();
    let content = if existed {
        std::fs::read_to_string(path).map_err(|e| format!("Cannot read {}: {e}", path.display()))?
    } else {
        String::new()
    };

    let root =
        jsonc_parser::cst::CstRootNode::parse(&content, &jsonc_parser::ParseOptions::default())
            .map_err(|e| format!("Cannot parse {}: {e}", path.display()))?;
    let root_obj = root
        .object_value_or_create()
        .ok_or_else(|| format!("Config root of {} is not a JSON object", path.display()))?;
    let mcp_obj = root_obj
        .object_value_or_create("mcp")
        .ok_or_else(|| format!("'mcp' in {} is not a JSON object", path.display()))?;

    let input_value = to_cst_input(entry);
    if let Some(prop) = mcp_obj.get(entry_name) {
        prop.set_value(input_value);
    } else {
        mcp_obj.append(entry_name, input_value);
    }

    atomic_write(path, &root.to_string())?;

    Ok(if existed {
        ExportAction::Updated
    } else {
        ExportAction::Created
    })
}

/// Convert a `serde_json::Value` into `jsonc-parser`'s CST input-value type.
fn to_cst_input(value: &Value) -> jsonc_parser::cst::CstInputValue {
    use jsonc_parser::cst::CstInputValue;
    match value {
        Value::Null => CstInputValue::Null,
        Value::Bool(b) => CstInputValue::Bool(*b),
        Value::Number(n) => CstInputValue::Number(n.to_string()),
        Value::String(s) => CstInputValue::String(s.clone()),
        Value::Array(arr) => CstInputValue::Array(arr.iter().map(to_cst_input).collect()),
        Value::Object(map) => CstInputValue::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), to_cst_input(v)))
                .collect(),
        ),
    }
}

fn merge_into_opencode_config_with_safety(
    path: &Path,
    entry_name: &str,
    entry: &Value,
) -> Result<SafeMergeResult, String> {
    let backup_path = create_backup(path)?;
    let action = merge_into_opencode_config(path, entry_name, entry)?;
    verify_opencode_entry(path, entry_name, entry)?;

    Ok(SafeMergeResult {
        action,
        backup_path,
        verified: true,
    })
}

fn verify_opencode_entry(path: &Path, entry_name: &str, entry: &Value) -> Result<(), String> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("Cannot verify {}: {e}", path.display()))?;
    let root =
        jsonc_parser::cst::CstRootNode::parse(&content, &jsonc_parser::ParseOptions::default())
            .map_err(|e| format!("Cannot parse {} after write: {e}", path.display()))?;
    let actual = root
        .object_value()
        .and_then(|obj| obj.object_value("mcp"))
        .and_then(|mcp| mcp.get(entry_name))
        .and_then(|prop| prop.value())
        .and_then(|node| node.to_serde_value())
        .ok_or_else(|| {
            format!(
                "Verification failed: 'mcp.{entry_name}' missing from {}",
                path.display()
            )
        })?;

    if &actual == entry {
        Ok(())
    } else {
        Err(format!(
            "Verification failed: 'mcp.{entry_name}' in {} does not match planned entry",
            path.display()
        ))
    }
}

pub fn rollback_client_config(backup_path: &Path) -> Result<PathBuf, String> {
    let original_path = original_path_from_backup(backup_path)?;
    let bytes = std::fs::read(backup_path)
        .map_err(|e| format!("Cannot read backup {}: {e}", backup_path.display()))?;

    if let Some(parent) = original_path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Cannot create {}: {e}", parent.display()))?;
    }

    let parent = original_path.parent().unwrap_or(Path::new("."));
    let file_name = original_path.file_name().map_or_else(
        || "config.json".to_string(),
        |name| name.to_string_lossy().into_owned(),
    );
    let tmp = parent.join(format!(".{file_name}.rollback.tmp"));
    std::fs::write(&tmp, &bytes)
        .map_err(|e| format!("Cannot write rollback temp file {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &original_path).map_err(|e| {
        format!(
            "Cannot rename {} -> {}: {e}",
            tmp.display(),
            original_path.display()
        )
    })?;

    Ok(original_path)
}

fn original_path_from_backup(backup_path: &Path) -> Result<PathBuf, String> {
    let file_name = backup_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("Invalid backup file name: {}", backup_path.display()))?;
    let (original_name, _) = file_name.rsplit_once(BACKUP_MARKER).ok_or_else(|| {
        format!(
            "Backup file name must contain '{BACKUP_MARKER}': {}",
            backup_path.display()
        )
    })?;

    if original_name.is_empty() {
        return Err(format!(
            "Backup file name has empty original path: {}",
            backup_path.display()
        ));
    }

    Ok(backup_path.with_file_name(original_name))
}

// ── Client specs ──────────────────────────────────────────────────────────────

/// Build the list of `ClientSpec`s for the given target.
///
/// For workspace-relative paths (Cursor, VS Code, Cline), the path is resolved
/// relative to the current working directory — matching how those tools look
/// for their per-project configs.
pub(super) fn client_specs(target: ExportTarget) -> Vec<ClientSpec> {
    let cwd = std::env::current_dir().unwrap_or_default();

    let all_specs = vec![
        ClientSpec {
            label: "Claude Code",
            path: home_path(".claude.json"),
            servers_key: "mcpServers",
        },
        ClientSpec {
            label: "Claude Desktop",
            path: claude_desktop_path(),
            servers_key: "mcpServers",
        },
        ClientSpec {
            label: "Cursor",
            path: cwd.join(".cursor/mcp.json"),
            servers_key: "mcpServers",
        },
        ClientSpec {
            label: "VS Code Copilot",
            path: cwd.join(".vscode/mcp.json"),
            servers_key: "servers",
        },
        ClientSpec {
            label: "Windsurf",
            path: windsurf_path(),
            servers_key: "mcpServers",
        },
        ClientSpec {
            label: "Cline",
            path: cwd.join(".cline/mcp_servers.json"),
            servers_key: "mcpServers",
        },
        ClientSpec {
            label: "Zed",
            path: zed_settings_path(),
            servers_key: "context_servers",
        },
        ClientSpec {
            label: "OpenCode",
            path: cwd.join(".opencode/opencode.jsonc"),
            servers_key: "mcp",
        },
    ];

    match target {
        ExportTarget::All => all_specs,
        ExportTarget::ClaudeCode => all_specs
            .into_iter()
            .filter(|s| s.label == "Claude Code")
            .collect(),
        ExportTarget::ClaudeDesktop => all_specs
            .into_iter()
            .filter(|s| s.label == "Claude Desktop")
            .collect(),
        ExportTarget::Cursor => all_specs
            .into_iter()
            .filter(|s| s.label == "Cursor")
            .collect(),
        ExportTarget::VsCodeCopilot => all_specs
            .into_iter()
            .filter(|s| s.label == "VS Code Copilot")
            .collect(),
        ExportTarget::Windsurf => all_specs
            .into_iter()
            .filter(|s| s.label == "Windsurf")
            .collect(),
        ExportTarget::Cline => all_specs
            .into_iter()
            .filter(|s| s.label == "Cline")
            .collect(),
        ExportTarget::Zed => all_specs.into_iter().filter(|s| s.label == "Zed").collect(),
        ExportTarget::OpenCode => all_specs
            .into_iter()
            .filter(|s| s.label == "OpenCode")
            .collect(),
        ExportTarget::Generic => vec![], // handled separately in run_config_export
    }
}

// ── Command entry point ───────────────────────────────────────────────────────

/// Run `mcp-gateway setup export`.
///
/// Loads `gateway.yaml` from `config_path`, resolves the connection mode, then
/// writes (or prints, for dry-run) a gateway entry into every selected client's
/// config file.
#[allow(clippy::too_many_lines)]
pub async fn run_config_export(
    target: ExportTarget,
    mode: ConnectionMode,
    name: &str,
    watch: bool,
    dry_run: bool,
    rollback: Option<PathBuf>,
    config_path: &Path,
) -> ExitCode {
    if let Some(backup_path) = rollback {
        match rollback_client_config(&backup_path) {
            Ok(original_path) => {
                println!(
                    "Restored {} from {}",
                    original_path.display(),
                    backup_path.display()
                );
                println!("Run 'mcp-gateway doctor' to verify the restored client config.");
                return ExitCode::SUCCESS;
            }
            Err(e) => {
                eprintln!("Error: Rollback failed: {e}");
                return ExitCode::FAILURE;
            }
        }
    }

    let config = match Config::load(Some(config_path)) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Error: Cannot load {}: {e}", config_path.display());
            return ExitCode::FAILURE;
        }
    };

    if dry_run {
        println!("Dry-run mode — no files will be written.");
        println!();
    }

    let resolved = resolve_mode(mode, &config);
    let mode_label = match resolved {
        ConnectionMode::Proxy | ConnectionMode::Auto => "proxy",
        ConnectionMode::Stdio => "stdio",
    };
    let entry = build_gateway_entry(&config, Some(config_path), resolved);

    if target == ExportTarget::Generic {
        // Generic: print JSON to stdout.
        let wrapper = json!({ "mcpServers": { name: entry } });
        println!(
            "{}",
            serde_json::to_string_pretty(&wrapper).unwrap_or_default()
        );
        return ExitCode::SUCCESS;
    }

    // OpenCode's entry shape differs from every other client (see
    // `build_opencode_entry`) — show the shape that will actually be
    // written when it's the only target selected.
    let display_entry = if target == ExportTarget::OpenCode {
        build_opencode_entry(&config, Some(config_path), resolved)
    } else {
        entry
    };

    println!("Exporting gateway config to AI clients...");
    println!();
    println!("Planned gateway entry ({mode_label} mode):");
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({ name: display_entry })).unwrap_or_default()
    );
    println!();

    let results = do_export(target, name, dry_run, &config, Some(config_path), resolved);

    let mut written = 0usize;
    let mut failed = false;

    for r in &results {
        let path = r.path.display();
        let status = match &r.action {
            ExportAction::Created => {
                written += 1;
                let suffix = format_safety_suffix(r.safety.as_ref());
                format!("Created  {path}{suffix}")
            }
            ExportAction::Updated => {
                written += 1;
                let suffix = format_safety_suffix(r.safety.as_ref());
                format!("Updated  {path}{suffix}")
            }
            ExportAction::Skipped(reason) => format!("Skipped  ({reason})"),
            ExportAction::Failed(err) => {
                failed = true;
                format!("FAILED   {err}")
            }
        };
        let client = r.client;
        println!("  {client:16} {status}");
    }

    println!();
    if dry_run {
        println!("Would export to {written} client(s) ({mode_label} mode).");
    } else {
        println!("Exported to {written} client(s) ({mode_label} mode).");
    }
    println!();
    println!("Entry name: \"{name}\"");
    println!("Gateway endpoint: configured from server.host/server.port");

    if watch {
        watch::run_watch_loop(target, mode, name, config_path).await;
    }

    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// Perform the actual export for all specs; returns a result per client.
///
/// Builds the gateway entry per spec rather than once up front: `OpenCode`
/// needs its own entry shape (see `build_opencode_entry`), so the same
/// pre-built `Value` can't be reused for every client the way it can for
/// everyone else.
fn do_export(
    target: ExportTarget,
    name: &str,
    dry_run: bool,
    config: &Config,
    config_path: Option<&Path>,
    mode: ConnectionMode,
) -> Vec<ExportResult> {
    let specs = client_specs(target);

    specs
        .into_iter()
        .map(|spec| {
            let entry = if spec.label == "OpenCode" {
                build_opencode_entry(config, config_path, mode)
            } else {
                build_gateway_entry(config, config_path, mode)
            };
            let (action, safety) = export_one_detailed(&spec, name, &entry, dry_run);
            ExportResult {
                client: spec.label,
                path: spec.path,
                action,
                safety,
            }
        })
        .collect()
}

fn format_safety_suffix(safety: Option<&ExportSafety>) -> String {
    let Some(safety) = safety else {
        return String::new();
    };

    let mut parts = Vec::new();
    if let Some(backup_path) = &safety.backup_path {
        parts.push(format!("backup: {}", backup_path.display()));
        parts.push(format!(
            "rollback: mcp-gateway setup export --rollback {}",
            backup_path.display()
        ));
    }
    if safety.verified {
        parts.push("verified".to_string());
    }

    if parts.is_empty() {
        String::new()
    } else {
        format!(" ({})", parts.join("; "))
    }
}

/// Export (or dry-run) a single client spec.
pub(super) fn export_one(
    spec: &ClientSpec,
    name: &str,
    entry: &Value,
    dry_run: bool,
) -> ExportAction {
    export_one_detailed(spec, name, entry, dry_run).0
}

fn export_one_detailed(
    spec: &ClientSpec,
    name: &str,
    entry: &Value,
    dry_run: bool,
) -> (ExportAction, Option<ExportSafety>) {
    // For workspace-relative paths (Cursor, VS Code, Cline): skip if the
    // parent directory does not exist, since there is no project open.
    if (!spec.path.is_absolute()
        || spec.label == "Cursor"
        || spec.label == "VS Code Copilot"
        || spec.label == "Cline")
        && let Some(parent) = spec.path.parent()
        && !parent.as_os_str().is_empty()
        && !parent.exists()
    {
        return (
            ExportAction::Skipped(format!("no {} directory", parent.display())),
            None,
        );
    }

    // For global paths (Claude Desktop, Windsurf, Zed): skip if the parent
    // directory doesn't exist (client not installed).
    if spec.label != "Claude Code"
        && let Some(parent) = spec.path.parent()
        && !parent.as_os_str().is_empty()
        && !parent.exists()
    {
        return (
            ExportAction::Skipped(format!("{} not installed", spec.label)),
            None,
        );
    }

    if dry_run {
        // Dry-run: report what would happen without touching the filesystem.
        let action = if spec.path.exists() {
            ExportAction::Updated
        } else {
            ExportAction::Created
        };
        (action, None)
    } else {
        // OpenCode's `.opencode/opencode.jsonc` may contain comments, which
        // the generic parse-mutate-reserialize merge would destroy — it gets
        // its own comment-preserving writer instead.
        let result = if spec.label == "OpenCode" {
            merge_into_opencode_config_with_safety(&spec.path, name, entry)
        } else {
            merge_into_config_with_safety(&spec.path, spec.servers_key, name, entry)
        };
        match result {
            Ok(result) => {
                let safety = ExportSafety {
                    backup_path: result.backup_path,
                    verified: result.verified,
                };
                (result.action, Some(safety))
            }
            Err(e) => (ExportAction::Failed(e), None),
        }
    }
}
