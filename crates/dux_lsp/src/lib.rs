//! The language server: a program that speaks the editor's protocol.
//!
//! `dux lsp` reads JSON-RPC messages from stdin and answers on stdout,
//! framed the way the Language Server Protocol asks for — `Content-Length`
//! headers over UTF-8 JSON. What the server knows about a program it borrows
//! from the compiler: diagnostics are the compiler's own, and hover and
//! go-to-definition read the same parsed program a build reads.

pub mod features;
pub mod jsonrpc;
pub mod server;

pub use server::{serve, stdio};
