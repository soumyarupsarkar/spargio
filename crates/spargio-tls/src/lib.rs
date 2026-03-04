//! TLS companion APIs for spargio runtimes.
//!
//! This crate provides a thin adapter over `rustls` + `futures-rustls` using
//! `spargio::net::TcpStream` via `spargio-protocols::io_compat`.
#![deny(missing_docs)]

use futures_rustls::{TlsAcceptor as RustlsAcceptor, TlsConnector as RustlsConnector};
use rustls::pki_types::ServerName;
use rustls::{ClientConfig, ServerConfig};
use spargio::RuntimeHandle;
use spargio::net::TcpStream;
use spargio_protocols::io_compat::FuturesTcpStream;
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

/// Client-side TLS stream type.
pub type ClientTlsStream = futures_rustls::client::TlsStream<FuturesTcpStream>;
/// Server-side TLS stream type.
pub type ServerTlsStream = futures_rustls::server::TlsStream<FuturesTcpStream>;

#[derive(Debug, Clone, Copy, Default)]
/// Handshake options used by TLS connect/accept helpers.
pub struct HandshakeOptions {
    timeout: Option<Duration>,
}

impl HandshakeOptions {
    /// Sets handshake timeout.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Returns configured handshake timeout.
    pub fn timeout(self) -> Option<Duration> {
        self.timeout
    }
}

#[derive(Clone)]
/// Reusable TLS client connector.
pub struct TlsConnector {
    config: Arc<ClientConfig>,
    options: HandshakeOptions,
}

impl TlsConnector {
    /// Creates a connector from a rustls client config.
    pub fn new(config: Arc<ClientConfig>) -> Self {
        Self {
            config,
            options: HandshakeOptions::default(),
        }
    }

    /// Returns a connector with custom handshake options.
    pub fn with_options(mut self, options: HandshakeOptions) -> Self {
        self.options = options;
        self
    }

    /// Returns current connector options.
    pub fn options(&self) -> HandshakeOptions {
        self.options
    }

    /// Performs client TLS handshake over an existing Spargio TCP stream.
    pub async fn connect(
        &self,
        stream: TcpStream,
        server_name: ServerName<'static>,
    ) -> io::Result<ClientTlsStream> {
        connect_with_options(stream, server_name, self.config.clone(), self.options).await
    }

    /// Connects TCP to `addr` then performs client TLS handshake.
    pub async fn connect_socket_addr(
        &self,
        handle: RuntimeHandle,
        addr: SocketAddr,
        server_name: ServerName<'static>,
    ) -> io::Result<ClientTlsStream> {
        connect_socket_addr_with_options(
            handle,
            addr,
            server_name,
            self.config.clone(),
            self.options,
        )
        .await
    }
}

#[derive(Clone)]
/// Reusable TLS server acceptor.
pub struct TlsAcceptor {
    config: Arc<ServerConfig>,
    options: HandshakeOptions,
}

impl TlsAcceptor {
    /// Creates an acceptor from a rustls server config.
    pub fn new(config: Arc<ServerConfig>) -> Self {
        Self {
            config,
            options: HandshakeOptions::default(),
        }
    }

    /// Returns an acceptor with custom handshake options.
    pub fn with_options(mut self, options: HandshakeOptions) -> Self {
        self.options = options;
        self
    }

    /// Returns current acceptor options.
    pub fn options(&self) -> HandshakeOptions {
        self.options
    }

    /// Performs server TLS handshake over an accepted Spargio TCP stream.
    pub async fn accept(&self, stream: TcpStream) -> io::Result<ServerTlsStream> {
        accept_with_options(stream, self.config.clone(), self.options).await
    }
}

/// Performs client TLS handshake over an existing Spargio TCP stream.
pub async fn connect(
    stream: TcpStream,
    server_name: ServerName<'static>,
    config: Arc<ClientConfig>,
) -> io::Result<ClientTlsStream> {
    connect_with_options(stream, server_name, config, HandshakeOptions::default()).await
}

/// Performs client TLS handshake with explicit handshake options.
pub async fn connect_with_options(
    stream: TcpStream,
    server_name: ServerName<'static>,
    config: Arc<ClientConfig>,
    options: HandshakeOptions,
) -> io::Result<ClientTlsStream> {
    let connector = RustlsConnector::from(config);
    let fut = connector.connect(server_name, FuturesTcpStream::new(stream));
    run_handshake(options.timeout(), fut, "tls client handshake timed out").await
}

/// Connects TCP to `addr` and performs client TLS handshake.
pub async fn connect_socket_addr(
    handle: RuntimeHandle,
    addr: SocketAddr,
    server_name: ServerName<'static>,
    config: Arc<ClientConfig>,
) -> io::Result<ClientTlsStream> {
    connect_socket_addr_with_options(
        handle,
        addr,
        server_name,
        config,
        HandshakeOptions::default(),
    )
    .await
}

/// Connects TCP to `addr` and performs client TLS handshake with options.
pub async fn connect_socket_addr_with_options(
    handle: RuntimeHandle,
    addr: SocketAddr,
    server_name: ServerName<'static>,
    config: Arc<ClientConfig>,
    options: HandshakeOptions,
) -> io::Result<ClientTlsStream> {
    let stream = TcpStream::connect_socket_addr(handle, addr).await?;
    connect_with_options(stream, server_name, config, options).await
}

/// Performs server TLS handshake over an accepted Spargio TCP stream.
pub async fn accept(stream: TcpStream, config: Arc<ServerConfig>) -> io::Result<ServerTlsStream> {
    accept_with_options(stream, config, HandshakeOptions::default()).await
}

/// Performs server TLS handshake with explicit handshake options.
pub async fn accept_with_options(
    stream: TcpStream,
    config: Arc<ServerConfig>,
    options: HandshakeOptions,
) -> io::Result<ServerTlsStream> {
    let acceptor = RustlsAcceptor::from(config);
    let fut = acceptor.accept(FuturesTcpStream::new(stream));
    run_handshake(options.timeout(), fut, "tls server handshake timed out").await
}

async fn run_handshake<T, F>(
    timeout: Option<Duration>,
    fut: F,
    timeout_msg: &'static str,
) -> io::Result<T>
where
    F: Future<Output = io::Result<T>>,
{
    match timeout {
        Some(limit) => match spargio::timeout(limit, fut).await {
            Ok(result) => result,
            Err(_) => Err(io::Error::new(io::ErrorKind::TimedOut, timeout_msg)),
        },
        None => fut.await,
    }
}
