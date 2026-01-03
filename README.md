# lsp-mcp-rs

MCP server that bridges to any LSP. One binary, multiple language servers.

## Build

```
cargo build --release
```

Binary: `target/release/lsp-mcp-rs.exe`

## Configure

Create `config.toml` next to the binary:

```toml
[servers.lua]
command = "C:/path/to/lua-language-server.exe"
args = ["--stdio"]
extensions = [".lua"]

[servers.rust]
command = "rust-analyzer"
args = []
extensions = [".rs"]

[servers.python]
command = "pyright-langserver"
args = ["--stdio"]
extensions = [".py"]
```

Add to Claude Desktop config (`claude_desktop_config.json`):

```json
{
  "mcpServers": {
    "lsp": {
      "command": "X:\\path\\to\\lsp-mcp-rs.exe",
      "args": []
    }
  }
}
```

## Tools

| Tool | Description |
|------|-------------|
| `lsp_hover` | Get docs/type at position |
| `lsp_definition` | Go to definition |
| `lsp_references` | Find all references |
| `lsp_symbols` | List symbols in file |
| `lsp_diagnostics` | Get errors/warnings |
| `lsp_servers` | List configured servers |

All position arguments are 0-indexed.

## How it works

1. MCP request comes in with a file path
2. File extension maps to configured LSP server
3. LSP spawns on first use, stays running
4. Request forwarded to LSP, response returned via MCP

## License

MIT

## Authors

DC, KALIC, Stryk9190
