use spargio::boundary::{self, BoundaryError};
use std::time::Duration;

#[test]
fn boundary_try_call_returns_overloaded_when_queue_is_full() {
    let (client, _server) = boundary::channel::<u64, u64>(1);

    let _first = client.try_call(1).expect("first queued");
    match client.try_call(2) {
        Ok(_) => panic!("second call should overflow"),
        Err(err) => assert_eq!(err, BoundaryError::Overloaded),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn boundary_wait_timeout_reports_timeout() {
    let (client, _server) = boundary::channel::<u64, u64>(1);

    let ticket = client.call(7).await.expect("queued");
    let err = ticket
        .wait_timeout(Duration::from_millis(10))
        .await
        .expect_err("must time out");
    assert_eq!(err, BoundaryError::Timeout);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn boundary_server_observes_canceled_client() {
    let (client, server) = boundary::channel::<u64, u64>(1);

    let ticket = client.call(11).await.expect("queued");
    drop(ticket);

    let req = server.recv().await.expect("request");
    let err = req.respond(22).expect_err("client canceled");
    assert_eq!(err, BoundaryError::Canceled);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn boundary_deadline_is_propagated_to_server() {
    let (client, server) = boundary::channel::<u64, u64>(1);

    let _ticket = client
        .call_with_timeout(5, Duration::from_millis(250))
        .await
        .expect("queued");

    let req = server.recv().await.expect("request");
    assert!(req.deadline().is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn boundary_call_and_recv_round_trip() {
    let (client, server) = boundary::channel::<u64, u64>(1);

    let caller = tokio::spawn(async move {
        let ticket = client.call(7).await.expect("queued");
        ticket.await.expect("response")
    });

    let req = server.recv().await.expect("request");
    assert_eq!(*req.request(), 7);
    req.respond(9).expect("respond");

    let out = caller.await.expect("join");
    assert_eq!(out, 9);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn boundary_recv_timeout_reports_timeout() {
    let (_client, server) = boundary::channel::<u64, u64>(1);
    let err = match server.recv_timeout(Duration::from_millis(10)).await {
        Ok(_) => panic!("must time out"),
        Err(err) => err,
    };
    assert_eq!(err, BoundaryError::Timeout);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn boundary_ticket_wait_timeout_reports_timeout() {
    let (client, _server) = boundary::channel::<u64, u64>(1);
    let ticket = client.call(7).await.expect("queued");
    let err = ticket
        .wait_timeout(Duration::from_millis(10))
        .await
        .expect_err("must time out");
    assert_eq!(err, BoundaryError::Timeout);
}
