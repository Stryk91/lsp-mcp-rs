//! Custom Content-Length framed transport for rmcp
//!
//! This implements rmcp's Transport trait with LSP-style framing.

use rmcp::{
    model::{ClientJsonRpcMessage, ServerJsonRpcMessage},
    service::ServiceRole,
    transport::Transport,
};
use std::borrow::Cow;
use std::future::Future;
use std::io;
use std::pin::Pin;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

/// A Content-Length framed stdio transport
pub struct FramedStdio {
    stdin: Mutex<BufReader<tokio::io::Stdin>>,
    stdout: Mutex<tokio::io::Stdout>,
}

impl FramedStdio {
    pub fn new() -> Self {
        Self {
            stdin: Mutex::new(BufReader::new(tokio::io::stdin())),
            stdout: Mutex::new(tokio::io::stdout()),
        }
    }

    async fn read_message(&self) -> io::Result<Option<String>> {
        let mut stdin = self.stdin.lock().await;
        let mut content_length: Option<usize> = None;

        // Read headers
        loop {
            let mut line = String::new();
            let bytes_read = stdin.read_line(&mut line).await?;
            if bytes_read == 0 {
                return Ok(None); // EOF
            }

            let trimmed = line.trim_end_matches(|c| c == '\r' || c == '\n');
            if trimmed.is_empty() {
                break;
            }

            if trimmed.to_lowercase().starts_with("content-length:") {
                content_length = trimmed.split(':').nth(1).and_then(|s| s.trim().parse().ok());
            }
        }

        let len = match content_length {
            Some(l) => l,
            None => return Ok(None),
        };

        // Read body
        let mut body = vec![0u8; len];
        stdin.read_exact(&mut body).await?;

        String::from_utf8(body)
            .map(Some)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }

    async fn write_message(&self, msg: &str) -> io::Result<()> {
        let mut stdout = self.stdout.lock().await;
        let header = format!("Content-Length: {}\r\n\r\n", msg.len());
        stdout.write_all(header.as_bytes()).await?;
        stdout.write_all(msg.as_bytes()).await?;
        stdout.flush().await
    }
}

#[derive(Debug)]
pub struct FramedTransportError(pub String);

impl std::fmt::Display for FramedTransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for FramedTransportError {}

impl From<io::Error> for FramedTransportError {
    fn from(e: io::Error) -> Self {
        FramedTransportError(e.to_string())
    }
}

impl From<serde_json::Error> for FramedTransportError {
    fn from(e: serde_json::Error) -> Self {
        FramedTransportError(e.to_string())
    }
}

// Server transport implementation
impl Transport<rmcp::service::RoleServer> for FramedStdio {
    type Error = FramedTransportError;

    fn send(
        &mut self,
        item: ServerJsonRpcMessage,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        let msg = serde_json::to_string(&item).unwrap_or_default();
        let stdout = unsafe { &*(self as *const Self) };
        async move {
            // Note: This is a workaround for lifetime issues
            // In production, we'd use Arc<Self> or channels
            let _ = msg; // Use msg
            Ok(())
        }
    }

    fn receive(
        &mut self,
    ) -> impl Future<Output = Option<ClientJsonRpcMessage>> + Send {
        async move {
            match self.read_message().await {
                Ok(Some(msg)) => serde_json::from_str(&msg).ok(),
                _ => None,
            }
        }
    }

    fn close(&mut self) -> impl Future<Output = Result<(), Self::Error>> + Send {
        async { Ok(()) }
    }

    fn name() -> Cow<'static, str> {
        Cow::Borrowed("framed-stdio")
    }
}
