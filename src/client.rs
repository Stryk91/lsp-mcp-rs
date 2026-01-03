use anyhow::{Context, Result};
use lsp_types::*;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{oneshot, Mutex};

use crate::config::ServerConfig;
use crate::protocol::{encode_message, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse};

pub struct LspClient {
    name: String,
    process: Mutex<Option<Child>>,
    stdin: Mutex<Option<tokio::process::ChildStdin>>,
    pending: Arc<Mutex<HashMap<i64, oneshot::Sender<JsonRpcResponse>>>>,
    next_id: AtomicI64,
    initialized: Mutex<bool>,
    root_uri: Mutex<Option<Uri>>,
}

impl LspClient {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            process: Mutex::new(None),
            stdin: Mutex::new(None),
            pending: Arc::new(Mutex::new(HashMap::new())),
            next_id: AtomicI64::new(1),
            initialized: Mutex::new(false),
            root_uri: Mutex::new(None),
        }
    }

    pub async fn start(&self, config: &ServerConfig) -> Result<()> {
        let mut cmd = Command::new(&config.command);
        cmd.args(&config.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());

        let mut child = cmd.spawn().context("Failed to spawn LSP process")?;

        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();

        *self.process.lock().await = Some(child);
        *self.stdin.lock().await = Some(stdin);

        let pending = self.pending.clone();

        // Spawn reader task
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);

            loop {
                let mut content_length: Option<usize> = None;

                // Read headers
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                        return;
                    }
                    if line == "\r\n" || line == "\n" {
                        break;
                    }
                    if line.to_lowercase().starts_with("content-length:") {
                        content_length = line
                            .split(':')
                            .nth(1)
                            .and_then(|s| s.trim().parse().ok());
                    }
                }

                let Some(len) = content_length else {
                    continue;
                };

                // Read body
                let mut body = vec![0u8; len];
                if reader.read_exact(&mut body).await.is_err() {
                    return;
                }

                let Ok(response) = serde_json::from_slice::<JsonRpcResponse>(&body) else {
                    continue;
                };

                if let Some(id) = response.id {
                    let mut pending = pending.lock().await;
                    if let Some(tx) = pending.remove(&id) {
                        let _ = tx.send(response);
                    }
                }
            }
        });

        Ok(())
    }

    pub async fn is_running(&self) -> bool {
        self.process.lock().await.is_some()
    }

    async fn send_request(&self, method: &str, params: Option<Value>) -> Result<JsonRpcResponse> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let request = JsonRpcRequest::new(id, method, params);
        let msg = encode_message(&request);

        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        {
            let mut stdin = self.stdin.lock().await;
            if let Some(ref mut stdin) = *stdin {
                stdin.write_all(msg.as_bytes()).await?;
                stdin.flush().await?;
            } else {
                anyhow::bail!("LSP not running");
            }
        }

        tokio::time::timeout(std::time::Duration::from_secs(30), rx)
            .await
            .context("LSP request timed out")?
            .context("LSP response channel closed")
    }

    async fn send_notification(&self, method: &str, params: Option<Value>) -> Result<()> {
        let notification = JsonRpcNotification::new(method, params);
        let msg = encode_message(&notification);

        let mut stdin = self.stdin.lock().await;
        if let Some(ref mut stdin) = *stdin {
            stdin.write_all(msg.as_bytes()).await?;
            stdin.flush().await?;
        }
        Ok(())
    }

    pub async fn initialize(&self, root_path: &Path) -> Result<()> {
        let root_uri = path_to_uri(root_path)?;

        *self.root_uri.lock().await = Some(root_uri.clone());

        let params = InitializeParams {
            process_id: Some(std::process::id()),
            #[allow(deprecated)]
            root_uri: Some(root_uri.clone()),
            capabilities: ClientCapabilities {
                text_document: Some(TextDocumentClientCapabilities {
                    hover: Some(HoverClientCapabilities::default()),
                    definition: Some(GotoCapability::default()),
                    references: Some(ReferenceClientCapabilities::default()),
                    document_symbol: Some(DocumentSymbolClientCapabilities::default()),
                    completion: Some(CompletionClientCapabilities::default()),
                    rename: Some(RenameClientCapabilities::default()),
                    formatting: Some(DocumentFormattingClientCapabilities::default()),
                    publish_diagnostics: Some(PublishDiagnosticsClientCapabilities::default()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            ..Default::default()
        };

        let response = self
            .send_request("initialize", Some(serde_json::to_value(params)?))
            .await?;

        if response.error.is_some() {
            anyhow::bail!("Initialize failed: {:?}", response.error);
        }

        self.send_notification("initialized", Some(json!({})))
            .await?;

        *self.initialized.lock().await = true;
        Ok(())
    }

    pub async fn ensure_initialized(&self, file_path: &Path) -> Result<()> {
        if *self.initialized.lock().await {
            return Ok(());
        }

        // Find project root
        let root = find_project_root(file_path).unwrap_or_else(|| {
            file_path
                .parent()
                .unwrap_or(Path::new("/"))
                .to_path_buf()
        });

        self.initialize(&root).await
    }

    pub async fn open_file(&self, path: &Path) -> Result<()> {
        let content = tokio::fs::read_to_string(path).await?;
        let uri = path_to_uri(path)?;

        let lang_id = path
            .extension()
            .and_then(|e| e.to_str())
            .map(ext_to_language_id)
            .unwrap_or("plaintext");

        let params = DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri,
                language_id: lang_id.to_string(),
                version: 1,
                text: content,
            },
        };

        self.send_notification("textDocument/didOpen", Some(serde_json::to_value(params)?))
            .await
    }

    pub async fn hover(&self, path: &Path, line: u32, character: u32) -> Result<Option<Hover>> {
        self.ensure_initialized(path).await?;
        self.open_file(path).await?;

        let uri = path_to_uri(path)?;

        let params = HoverParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position: Position { line, character },
            },
            work_done_progress_params: Default::default(),
        };

        let response = self
            .send_request("textDocument/hover", Some(serde_json::to_value(params)?))
            .await?;

        if let Some(result) = response.result {
            Ok(serde_json::from_value(result)?)
        } else {
            Ok(None)
        }
    }

    pub async fn definition(
        &self,
        path: &Path,
        line: u32,
        character: u32,
    ) -> Result<Option<GotoDefinitionResponse>> {
        self.ensure_initialized(path).await?;
        self.open_file(path).await?;

        let uri = path_to_uri(path)?;

        let params = GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position: Position { line, character },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        };

        let response = self
            .send_request(
                "textDocument/definition",
                Some(serde_json::to_value(params)?),
            )
            .await?;

        if let Some(result) = response.result {
            Ok(serde_json::from_value(result)?)
        } else {
            Ok(None)
        }
    }

    pub async fn references(
        &self,
        path: &Path,
        line: u32,
        character: u32,
    ) -> Result<Option<Vec<Location>>> {
        self.ensure_initialized(path).await?;
        self.open_file(path).await?;

        let uri = path_to_uri(path)?;

        let params = ReferenceParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position: Position { line, character },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
            context: ReferenceContext {
                include_declaration: true,
            },
        };

        let response = self
            .send_request(
                "textDocument/references",
                Some(serde_json::to_value(params)?),
            )
            .await?;

        if let Some(result) = response.result {
            Ok(serde_json::from_value(result)?)
        } else {
            Ok(None)
        }
    }

    pub async fn document_symbols(&self, path: &Path) -> Result<Option<DocumentSymbolResponse>> {
        self.ensure_initialized(path).await?;
        self.open_file(path).await?;

        let uri = path_to_uri(path)?;

        let params = DocumentSymbolParams {
            text_document: TextDocumentIdentifier { uri },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        };

        let response = self
            .send_request(
                "textDocument/documentSymbol",
                Some(serde_json::to_value(params)?),
            )
            .await?;

        if let Some(result) = response.result {
            Ok(serde_json::from_value(result)?)
        } else {
            Ok(None)
        }
    }

    pub async fn diagnostics(&self, path: &Path) -> Result<Vec<Diagnostic>> {
        self.ensure_initialized(path).await?;
        self.open_file(path).await?;

        // LSP diagnostics are push-based, but we can trigger them by saving
        // For now, return empty - proper implementation needs notification handling
        Ok(vec![])
    }

    pub async fn shutdown(&self) -> Result<()> {
        let _ = self.send_request("shutdown", None).await;
        let _ = self.send_notification("exit", None).await;

        if let Some(mut child) = self.process.lock().await.take() {
            let _ = child.kill().await;
        }

        *self.initialized.lock().await = false;
        Ok(())
    }
}

fn path_to_uri(path: &Path) -> Result<Uri> {
    // Canonicalize path and convert to file URI
    let abs_path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };

    let path_str = abs_path.to_string_lossy();

    // Handle Windows paths
    let path_part = if cfg!(windows) || path_str.starts_with("/mnt/") {
        // Convert /mnt/c/... to C:/...
        if path_str.starts_with("/mnt/") {
            let drive = path_str.chars().nth(5).unwrap_or('c');
            let rest = &path_str[7..];
            format!("{}:{}", drive.to_ascii_uppercase(), rest)
        } else {
            path_str.replace('\\', "/")
        }
    } else {
        path_str.to_string()
    };

    // URL-encode special characters (spaces, etc)
    let encoded: String = path_part
        .chars()
        .map(|c| match c {
            ' ' => "%20".to_string(),
            '#' => "%23".to_string(),
            '?' => "%3F".to_string(),
            _ => c.to_string(),
        })
        .collect();

    let uri_str = if encoded.chars().nth(1) == Some(':') {
        // Windows absolute path
        format!("file:///{}", encoded)
    } else {
        format!("file://{}", encoded)
    };

    uri_str
        .parse()
        .map_err(|e| anyhow::anyhow!("Invalid URI: {}", e))
}

pub fn uri_to_path_string(uri: &Uri) -> String {
    let s = uri.as_str();
    if s.starts_with("file:///") {
        let path = &s[7..]; // Keep one slash
        // Handle Windows paths like file:///C:/...
        if path.len() > 2 && path.chars().nth(1) == Some(':') {
            path.to_string()
        } else {
            path.to_string()
        }
    } else {
        s.to_string()
    }
}

fn find_project_root(path: &Path) -> Option<std::path::PathBuf> {
    let markers = [
        ".git",
        "Cargo.toml",
        "package.json",
        "pyproject.toml",
        "go.mod",
        ".luarc.json",
        "compile_commands.json",
    ];

    let mut current = path.parent()?;
    loop {
        for marker in &markers {
            if current.join(marker).exists() {
                return Some(current.to_path_buf());
            }
        }
        current = current.parent()?;
    }
}

fn ext_to_language_id(ext: &str) -> &str {
    match ext.to_lowercase().as_str() {
        "lua" => "lua",
        "rs" => "rust",
        "py" | "pyi" => "python",
        "js" => "javascript",
        "jsx" => "javascriptreact",
        "ts" => "typescript",
        "tsx" => "typescriptreact",
        "go" => "go",
        "c" | "h" => "c",
        "cpp" | "hpp" | "cc" | "cxx" => "cpp",
        "json" => "json",
        "yaml" | "yml" => "yaml",
        "md" => "markdown",
        _ => "plaintext",
    }
}
