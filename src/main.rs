//! lsp-mcp-rs - Universal MCP bridge to any LSP server
//!
//! Uses Content-Length framed stdio (compatible with Claude Desktop)

mod client;
mod config;
mod protocol;

use anyhow::Result;
use lsp_types::{DocumentSymbolResponse, GotoDefinitionResponse, Hover, Location};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, Read, Write};
use std::path::Path;
use std::sync::Arc;
use tokio::sync::Mutex;

use client::LspClient;
use config::{Config, ServerConfig};

// ============================================================================
// MCP Protocol Types
// ============================================================================

#[derive(Debug, Deserialize)]
struct McpRequest {
    jsonrpc: String,
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Debug, Serialize)]
struct McpResponse {
    jsonrpc: String,
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<McpError>,
}

#[derive(Debug, Serialize)]
struct McpError {
    code: i32,
    message: String,
}

#[derive(Debug, Serialize)]
struct ToolDef {
    name: String,
    description: String,
    #[serde(rename = "inputSchema")]
    input_schema: Value,
}

// ============================================================================
// LSP Manager
// ============================================================================

struct LspManager {
    config: Config,
    clients: Mutex<HashMap<String, Arc<LspClient>>>,
}

impl LspManager {
    fn new(config: Config) -> Self {
        Self {
            config,
            clients: Mutex::new(HashMap::new()),
        }
    }

    fn server_for_file(&self, path: &Path) -> Option<(String, ServerConfig)> {
        let ext = path.extension()?.to_str()?;
        let ext = format!(".{}", ext);
        self.config
            .server_for_extension(&ext)
            .map(|(name, cfg)| (name.to_string(), cfg.clone()))
    }

    async fn get_client(&self, path: &Path) -> Result<Arc<LspClient>> {
        let (name, config) = self
            .server_for_file(path)
            .ok_or_else(|| anyhow::anyhow!("No LSP configured for: {}", path.display()))?;

        let mut clients = self.clients.lock().await;

        if let Some(client) = clients.get(&name) {
            return Ok(client.clone());
        }

        let client = Arc::new(LspClient::new(&name));
        client.start(&config).await?;
        clients.insert(name.clone(), client.clone());
        Ok(client)
    }
}

// ============================================================================
// MCP Server
// ============================================================================

struct McpServer {
    manager: Arc<LspManager>,
}

impl McpServer {
    fn new(config: Config) -> Self {
        Self {
            manager: Arc::new(LspManager::new(config)),
        }
    }

    fn get_tools() -> Vec<ToolDef> {
        vec![
            ToolDef {
                name: "lsp_hover".into(),
                description: "Get hover information (documentation, type) at a position".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "file": { "type": "string", "description": "Absolute path to the file" },
                        "line": { "type": "integer", "description": "Line number (0-indexed)" },
                        "column": { "type": "integer", "description": "Column number (0-indexed)" }
                    },
                    "required": ["file", "line", "column"]
                }),
            },
            ToolDef {
                name: "lsp_definition".into(),
                description: "Go to definition of symbol at position".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "file": { "type": "string", "description": "Absolute path to the file" },
                        "line": { "type": "integer", "description": "Line number (0-indexed)" },
                        "column": { "type": "integer", "description": "Column number (0-indexed)" }
                    },
                    "required": ["file", "line", "column"]
                }),
            },
            ToolDef {
                name: "lsp_references".into(),
                description: "Find all references to symbol at position".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "file": { "type": "string", "description": "Absolute path to the file" },
                        "line": { "type": "integer", "description": "Line number (0-indexed)" },
                        "column": { "type": "integer", "description": "Column number (0-indexed)" }
                    },
                    "required": ["file", "line", "column"]
                }),
            },
            ToolDef {
                name: "lsp_symbols".into(),
                description: "List all symbols in a file".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "file": { "type": "string", "description": "Absolute path to the file" }
                    },
                    "required": ["file"]
                }),
            },
            ToolDef {
                name: "lsp_diagnostics".into(),
                description: "Get errors and warnings for a file".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "file": { "type": "string", "description": "Absolute path to the file" }
                    },
                    "required": ["file"]
                }),
            },
            ToolDef {
                name: "lsp_servers".into(),
                description: "List configured LSP servers".into(),
                input_schema: json!({ "type": "object", "properties": {} }),
            },
        ]
    }

    async fn handle_request(&self, req: McpRequest) -> McpResponse {
        let id = req.id.unwrap_or(Value::Null);

        match req.method.as_str() {
            "initialize" => McpResponse {
                jsonrpc: "2.0".into(),
                id,
                result: Some(json!({
                    "protocolVersion": "2024-11-05",
                    "serverInfo": { "name": "lsp-mcp-rs", "version": env!("CARGO_PKG_VERSION") },
                    "capabilities": { "tools": {} }
                })),
                error: None,
            },

            "notifications/initialized" => McpResponse {
                jsonrpc: "2.0".into(),
                id,
                result: Some(json!({})),
                error: None,
            },

            "tools/list" => McpResponse {
                jsonrpc: "2.0".into(),
                id,
                result: Some(json!({ "tools": Self::get_tools() })),
                error: None,
            },

            "tools/call" => {
                let name = req.params["name"].as_str().unwrap_or_default();
                let args = &req.params["arguments"];
                let result = self.call_tool(name, args).await;

                McpResponse {
                    jsonrpc: "2.0".into(),
                    id,
                    result: Some(json!({
                        "content": [{ "type": "text", "text": result }],
                        "isError": result.starts_with("Error:")
                    })),
                    error: None,
                }
            }

            _ => McpResponse {
                jsonrpc: "2.0".into(),
                id,
                result: None,
                error: Some(McpError {
                    code: -32601,
                    message: format!("Method not found: {}", req.method),
                }),
            },
        }
    }

    async fn call_tool(&self, name: &str, args: &Value) -> String {
        match name {
            "lsp_hover" => self.tool_hover(args).await,
            "lsp_definition" => self.tool_definition(args).await,
            "lsp_references" => self.tool_references(args).await,
            "lsp_symbols" => self.tool_symbols(args).await,
            "lsp_diagnostics" => self.tool_diagnostics(args).await,
            "lsp_servers" => self.tool_servers(),
            _ => format!("Error: Unknown tool: {}", name),
        }
    }

    async fn tool_hover(&self, args: &Value) -> String {
        let file = args["file"].as_str().unwrap_or_default();
        let line = args["line"].as_u64().unwrap_or(0) as u32;
        let col = args["column"].as_u64().unwrap_or(0) as u32;

        let path = Path::new(file);
        match self.manager.get_client(path).await {
            Ok(client) => match client.hover(path, line, col).await {
                Ok(Some(h)) => format_hover(h),
                Ok(None) => "No hover information".into(),
                Err(e) => format!("Error: {}", e),
            },
            Err(e) => format!("Error: {}", e),
        }
    }

    async fn tool_definition(&self, args: &Value) -> String {
        let file = args["file"].as_str().unwrap_or_default();
        let line = args["line"].as_u64().unwrap_or(0) as u32;
        let col = args["column"].as_u64().unwrap_or(0) as u32;

        let path = Path::new(file);
        match self.manager.get_client(path).await {
            Ok(client) => match client.definition(path, line, col).await {
                Ok(Some(d)) => format_definition(d),
                Ok(None) => "No definition found".into(),
                Err(e) => format!("Error: {}", e),
            },
            Err(e) => format!("Error: {}", e),
        }
    }

    async fn tool_references(&self, args: &Value) -> String {
        let file = args["file"].as_str().unwrap_or_default();
        let line = args["line"].as_u64().unwrap_or(0) as u32;
        let col = args["column"].as_u64().unwrap_or(0) as u32;

        let path = Path::new(file);
        match self.manager.get_client(path).await {
            Ok(client) => match client.references(path, line, col).await {
                Ok(Some(refs)) => format_references(refs),
                Ok(None) => "No references found".into(),
                Err(e) => format!("Error: {}", e),
            },
            Err(e) => format!("Error: {}", e),
        }
    }

    async fn tool_symbols(&self, args: &Value) -> String {
        let file = args["file"].as_str().unwrap_or_default();

        let path = Path::new(file);
        match self.manager.get_client(path).await {
            Ok(client) => match client.document_symbols(path).await {
                Ok(Some(s)) => format_symbols(s),
                Ok(None) => "No symbols found".into(),
                Err(e) => format!("Error: {}", e),
            },
            Err(e) => format!("Error: {}", e),
        }
    }

    async fn tool_diagnostics(&self, args: &Value) -> String {
        let file = args["file"].as_str().unwrap_or_default();

        let path = Path::new(file);
        match self.manager.get_client(path).await {
            Ok(client) => match client.diagnostics(path).await {
                Ok(d) if d.is_empty() => "No diagnostics".into(),
                Ok(d) => serde_json::to_string_pretty(&d).unwrap_or_default(),
                Err(e) => format!("Error: {}", e),
            },
            Err(e) => format!("Error: {}", e),
        }
    }

    fn tool_servers(&self) -> String {
        let mut lines = vec!["Configured LSP servers:".to_string()];
        for (name, cfg) in &self.manager.config.servers {
            lines.push(format!("  {} -> {} ({})", name, cfg.command, cfg.extensions.join(", ")));
        }
        lines.join("\n")
    }
}

// ============================================================================
// Formatters
// ============================================================================

fn format_hover(h: Hover) -> String {
    match h.contents {
        lsp_types::HoverContents::Scalar(s) => match s {
            lsp_types::MarkedString::String(s) => s,
            lsp_types::MarkedString::LanguageString(ls) => format!("```{}\n{}\n```", ls.language, ls.value),
        },
        lsp_types::HoverContents::Array(arr) => arr
            .into_iter()
            .map(|s| match s {
                lsp_types::MarkedString::String(s) => s,
                lsp_types::MarkedString::LanguageString(ls) => format!("```{}\n{}\n```", ls.language, ls.value),
            })
            .collect::<Vec<_>>()
            .join("\n\n"),
        lsp_types::HoverContents::Markup(m) => m.value,
    }
}

fn format_definition(d: GotoDefinitionResponse) -> String {
    let locs = match d {
        GotoDefinitionResponse::Scalar(l) => vec![l],
        GotoDefinitionResponse::Array(a) => a,
        GotoDefinitionResponse::Link(links) => {
            return links
                .iter()
                .map(|l| format!("{}:{}:{}", l.target_uri.as_str(), l.target_selection_range.start.line + 1, l.target_selection_range.start.character + 1))
                .collect::<Vec<_>>()
                .join("\n");
        }
    };
    locs.iter()
        .map(|l| format!("{}:{}:{}", l.uri.as_str(), l.range.start.line + 1, l.range.start.character + 1))
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_references(refs: Vec<Location>) -> String {
    if refs.is_empty() {
        return "No references found".into();
    }
    refs.iter()
        .map(|l| format!("{}:{}:{}", l.uri.as_str(), l.range.start.line + 1, l.range.start.character + 1))
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_symbols(s: DocumentSymbolResponse) -> String {
    match s {
        DocumentSymbolResponse::Flat(syms) => syms
            .iter()
            .map(|s| format!("{} ({:?}) at {}:{}", s.name, s.kind, s.location.range.start.line + 1, s.location.range.start.character + 1))
            .collect::<Vec<_>>()
            .join("\n"),
        DocumentSymbolResponse::Nested(syms) => {
            fn fmt(syms: &[lsp_types::DocumentSymbol], prefix: &str) -> Vec<String> {
                syms.iter()
                    .flat_map(|s| {
                        let name = if prefix.is_empty() { s.name.clone() } else { format!("{}.{}", prefix, s.name) };
                        let mut r = vec![format!("{} ({:?}) {}:{}-{}:{}", name, s.kind, s.range.start.line + 1, s.range.start.character + 1, s.range.end.line + 1, s.range.end.character + 1)];
                        if let Some(c) = &s.children {
                            r.extend(fmt(c, &name));
                        }
                        r
                    })
                    .collect()
            }
            fmt(&syms, "").join("\n")
        }
    }
}

// ============================================================================
// Newline-delimited JSON I/O (Claude Desktop format)
// ============================================================================

/// Read a JSON line from stdin using spawn_blocking
async fn read_message_async() -> Option<String> {
    tokio::task::spawn_blocking(|| {
        let stdin = std::io::stdin();
        let mut stdin = stdin.lock();
        let mut line = String::new();

        match stdin.read_line(&mut line) {
            Ok(0) => {
                eprintln!("[lsp-mcp-rs] EOF");
                None
            }
            Ok(_) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    eprintln!("[lsp-mcp-rs] Empty line, skipping");
                    None
                } else {
                    eprintln!("[lsp-mcp-rs] Received: {}...", &trimmed[..trimmed.len().min(80)]);
                    Some(trimmed.to_string())
                }
            }
            Err(e) => {
                eprintln!("[lsp-mcp-rs] Read error: {}", e);
                None
            }
        }
    })
    .await
    .ok()
    .flatten()
}

/// Write a JSON line to stdout using spawn_blocking
async fn write_message_async(msg: String) -> std::io::Result<()> {
    tokio::task::spawn_blocking(move || {
        let stdout = std::io::stdout();
        let mut stdout = stdout.lock();
        eprintln!("[lsp-mcp-rs] Sending: {}...", &msg[..msg.len().min(80)]);
        writeln!(stdout, "{}", msg)?;
        stdout.flush()
    })
    .await
    .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?
}

// ============================================================================
// Main
// ============================================================================

#[tokio::main]
async fn main() -> Result<()> {
    eprintln!("[lsp-mcp-rs] Starting...");

    let config = Config::load_default().unwrap_or_else(|e| {
        eprintln!("Warning: Failed to load config: {}", e);
        Config { servers: HashMap::new() }
    });

    let server = McpServer::new(config);
    eprintln!("[lsp-mcp-rs] Ready for JSONL messages");

    loop {
        let msg = match read_message_async().await {
            Some(m) if !m.is_empty() => m,
            Some(_) => continue, // Empty line, keep reading
            None => {
                eprintln!("[lsp-mcp-rs] EOF, exiting");
                break;
            }
        };

        let Ok(req) = serde_json::from_str::<McpRequest>(&msg) else {
            eprintln!("[lsp-mcp-rs] Parse error, skipping");
            continue;
        };

        let response = server.handle_request(req).await;
        let response_str = serde_json::to_string(&response)?;

        if let Err(e) = write_message_async(response_str).await {
            eprintln!("[lsp-mcp-rs] Write error: {}", e);
            break;
        }
    }

    Ok(())
}
