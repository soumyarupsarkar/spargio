use async_tungstenite::tungstenite::Message;
use futures::StreamExt;
use futures::executor::block_on;
use spargio::net::TcpListener;
use spargio_ws::{WsOptions, accept_with_options, connect_socket_addr_with_options};
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::Duration;

#[test]
fn ws_client_connect_timeout_is_enforced() {
    let rt = spargio::Runtime::builder()
        .shards(1)
        .build()
        .expect("runtime");
    let handle = rt.handle();

    block_on(async {
        let listener = TcpListener::bind_socket_addr(
            handle.clone(),
            SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)),
        )
        .await
        .expect("bind");
        let addr = listener.local_addr().expect("local addr");
        let server = handle
            .spawn_stealable({
                let listener = listener.clone();
                async move {
                    let (_stream, _) = listener.accept().await.expect("accept");
                    spargio::sleep(Duration::from_millis(100)).await;
                }
            })
            .expect("spawn");

        let err = connect_socket_addr_with_options(
            handle.clone(),
            addr,
            "/",
            WsOptions::default().with_timeout(Duration::from_millis(10)),
        )
        .await
        .expect_err("expected timeout");
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);

        server.await.expect("server");
    });
}

#[test]
fn ws_client_server_roundtrip_text_message() {
    let rt = spargio::Runtime::builder()
        .shards(1)
        .build()
        .expect("runtime");
    let handle = rt.handle();

    block_on(async {
        let listener = TcpListener::bind_socket_addr(
            handle.clone(),
            SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)),
        )
        .await
        .expect("bind");
        let addr = listener.local_addr().expect("local addr");

        let server = handle
            .spawn_stealable({
                let listener = listener.clone();
                async move {
                    let (stream, _) = listener.accept().await.expect("accept");
                    let mut ws = accept_with_options(
                        stream,
                        WsOptions::default().with_timeout(Duration::from_millis(250)),
                    )
                    .await
                    .expect("ws accept");
                    if let Some(Ok(Message::Text(text))) = ws.next().await {
                        ws.send(Message::Text(text)).await.expect("send");
                    } else {
                        panic!("expected text frame");
                    }
                    ws.close(None).await.expect("close");
                }
            })
            .expect("spawn");

        let (mut client, _response) = connect_socket_addr_with_options(
            handle.clone(),
            addr,
            "/chat",
            WsOptions::default().with_timeout(Duration::from_millis(250)),
        )
        .await
        .expect("connect");

        client
            .send(Message::Text("hello".into()))
            .await
            .expect("client send");
        match client.next().await {
            Some(Ok(Message::Text(text))) => assert_eq!(text, "hello"),
            other => panic!("unexpected frame: {other:?}"),
        }

        server.await.expect("server");
    });
}

#[test]
fn ws_client_connect_socket_addr_normalizes_path_without_slash() {
    let rt = spargio::Runtime::builder()
        .shards(1)
        .build()
        .expect("runtime");
    let handle = rt.handle();

    block_on(async {
        let listener = TcpListener::bind_socket_addr(
            handle.clone(),
            SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)),
        )
        .await
        .expect("bind");
        let addr = listener.local_addr().expect("local addr");

        let server = handle
            .spawn_stealable({
                let listener = listener.clone();
                async move {
                    let (stream, _) = listener.accept().await.expect("accept");
                    let mut ws = accept_with_options(
                        stream,
                        WsOptions::default().with_timeout(Duration::from_millis(250)),
                    )
                    .await
                    .expect("ws accept");
                    if let Some(Ok(Message::Text(text))) = ws.next().await {
                        ws.send(Message::Text(text)).await.expect("send");
                    } else {
                        panic!("expected text frame");
                    }
                }
            })
            .expect("spawn");

        let (mut client, _response) = connect_socket_addr_with_options(
            handle.clone(),
            addr,
            "chat",
            WsOptions::default().with_timeout(Duration::from_millis(250)),
        )
        .await
        .expect("connect");

        client
            .send(Message::Text("path-ok".into()))
            .await
            .expect("client send");
        match client.next().await {
            Some(Ok(Message::Text(text))) => assert_eq!(text, "path-ok"),
            other => panic!("unexpected frame: {other:?}"),
        }

        server.await.expect("server");
    });
}
