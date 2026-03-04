//! WebSocket companion APIs for spargio runtimes.
//!
//! This crate provides a thin adapter over `async-tungstenite` using
//! `spargio::net::TcpStream` via `spargio-protocols::io_compat`.
#![deny(missing_docs)]

use async_tungstenite::tungstenite::client::IntoClientRequest;
use async_tungstenite::tungstenite::error::Error as WsError;
use async_tungstenite::tungstenite::handshake::client::Response;
use async_tungstenite::tungstenite::protocol::WebSocketConfig;
use async_tungstenite::{WebSocketStream, accept_async_with_config, client_async_with_config};
use spargio::RuntimeHandle;
use spargio::net::TcpStream;
use spargio_protocols::io_compat::FuturesTcpStream;
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::time::Duration;

/// WebSocket stream type over Spargio-compatible TCP transport.
pub type WsStream = WebSocketStream<FuturesTcpStream>;
/// HTTP upgrade response returned from client connect.
pub type WsResponse = Response;

#[derive(Debug, Clone, Copy)]
/// WebSocket handshake/configuration options.
pub struct WsOptions {
    timeout: Option<Duration>,
    config: WebSocketConfig,
}

impl Default for WsOptions {
    fn default() -> Self {
        Self {
            timeout: None,
            config: WebSocketConfig::default(),
        }
    }
}

impl WsOptions {
    /// Sets handshake timeout.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Sets max WebSocket message size.
    pub fn with_max_message_size(mut self, max_message_size: Option<usize>) -> Self {
        self.config.max_message_size = max_message_size;
        self
    }

    /// Sets max WebSocket frame size.
    pub fn with_max_frame_size(mut self, max_frame_size: Option<usize>) -> Self {
        self.config.max_frame_size = max_frame_size;
        self
    }

    /// Sets acceptance policy for unmasked frames.
    pub fn with_accept_unmasked_frames(mut self, accept_unmasked_frames: bool) -> Self {
        self.config.accept_unmasked_frames = accept_unmasked_frames;
        self
    }

    /// Returns configured timeout.
    pub fn timeout(self) -> Option<Duration> {
        self.timeout
    }

    /// Returns configured websocket config.
    pub fn config(self) -> WebSocketConfig {
        self.config
    }
}

#[derive(Debug, Clone, Copy, Default)]
/// Reusable WebSocket client connector.
pub struct WsConnector {
    options: WsOptions,
}

impl WsConnector {
    /// Creates a connector with default options.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns connector with custom options.
    pub fn with_options(mut self, options: WsOptions) -> Self {
        self.options = options;
        self
    }

    /// Returns current connector options.
    pub fn options(self) -> WsOptions {
        self.options
    }

    /// Performs client websocket handshake on an existing TCP stream.
    pub async fn connect<R>(
        &self,
        stream: TcpStream,
        request: R,
    ) -> io::Result<(WsStream, WsResponse)>
    where
        R: IntoClientRequest + Unpin,
    {
        connect_with_options(stream, request, self.options).await
    }

    /// Connects TCP to `addr` and performs client websocket handshake.
    pub async fn connect_socket_addr(
        &self,
        handle: RuntimeHandle,
        addr: SocketAddr,
        path: &str,
    ) -> io::Result<(WsStream, WsResponse)> {
        connect_socket_addr_with_options(handle, addr, path, self.options).await
    }
}

#[derive(Debug, Clone, Copy, Default)]
/// Reusable WebSocket server acceptor.
pub struct WsAcceptor {
    options: WsOptions,
}

impl WsAcceptor {
    /// Creates an acceptor with default options.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns acceptor with custom options.
    pub fn with_options(mut self, options: WsOptions) -> Self {
        self.options = options;
        self
    }

    /// Returns current acceptor options.
    pub fn options(self) -> WsOptions {
        self.options
    }

    /// Performs server websocket handshake.
    pub async fn accept(&self, stream: TcpStream) -> io::Result<WsStream> {
        accept_with_options(stream, self.options).await
    }
}

/// Performs client websocket handshake on an existing TCP stream.
pub async fn connect<R>(stream: TcpStream, request: R) -> io::Result<(WsStream, WsResponse)>
where
    R: IntoClientRequest + Unpin,
{
    connect_with_options(stream, request, WsOptions::default()).await
}

/// Performs client websocket handshake with explicit options.
pub async fn connect_with_options<R>(
    stream: TcpStream,
    request: R,
    options: WsOptions,
) -> io::Result<(WsStream, WsResponse)>
where
    R: IntoClientRequest + Unpin,
{
    let fut = client_async_with_config(
        request,
        FuturesTcpStream::new(stream),
        Some(options.config()),
    );
    run_handshake(options.timeout(), fut, "ws client handshake timed out").await
}

/// Connects TCP to `addr` and performs client websocket handshake.
pub async fn connect_socket_addr(
    handle: RuntimeHandle,
    addr: SocketAddr,
    path: &str,
) -> io::Result<(WsStream, WsResponse)> {
    connect_socket_addr_with_options(handle, addr, path, WsOptions::default()).await
}

/// Connects TCP to `addr` and performs client websocket handshake with options.
pub async fn connect_socket_addr_with_options(
    handle: RuntimeHandle,
    addr: SocketAddr,
    path: &str,
    options: WsOptions,
) -> io::Result<(WsStream, WsResponse)> {
    let stream = TcpStream::connect_socket_addr(handle, addr).await?;
    let normalized_path = normalize_path(path);
    let request = format!("ws://{addr}{normalized_path}");
    connect_with_options(stream, request, options).await
}

/// Performs server websocket handshake.
pub async fn accept(stream: TcpStream) -> io::Result<WsStream> {
    accept_with_options(stream, WsOptions::default()).await
}

/// Performs server websocket handshake with explicit options.
pub async fn accept_with_options(stream: TcpStream, options: WsOptions) -> io::Result<WsStream> {
    let fut = accept_async_with_config(FuturesTcpStream::new(stream), Some(options.config()));
    run_handshake(options.timeout(), fut, "ws server handshake timed out").await
}

fn normalize_path(path: &str) -> String {
    if path.is_empty() {
        return "/".to_owned();
    }
    if path.starts_with('/') {
        return path.to_owned();
    }
    format!("/{path}")
}

async fn run_handshake<T, F>(
    timeout: Option<Duration>,
    fut: F,
    timeout_msg: &'static str,
) -> io::Result<T>
where
    F: Future<Output = Result<T, WsError>>,
{
    let result = match timeout {
        Some(limit) => match spargio::timeout(limit, fut).await {
            Ok(result) => result,
            Err(_) => return Err(io::Error::new(io::ErrorKind::TimedOut, timeout_msg)),
        },
        None => fut.await,
    };
    result.map_err(ws_error_to_io)
}

fn ws_error_to_io(err: WsError) -> io::Error {
    match err {
        WsError::Io(io) => io,
        other => io::Error::other(other.to_string()),
    }
}
