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

#[test]
fn boundary_wait_timeout_reports_timeout() {
    let (client, _server) = boundary::channel::<u64, u64>(1);

    let ticket = client.call(7).expect("queued");
    let err = ticket
        .wait_timeout_blocking(Duration::from_millis(10))
        .expect_err("must time out");
    assert_eq!(err, BoundaryError::Timeout);
}

#[test]
fn boundary_server_observes_canceled_client() {
    let (client, server) = boundary::channel::<u64, u64>(1);

    let ticket = client.call(11).expect("queued");
    drop(ticket);

    let req = server.recv().expect("request");
    let err = req.respond(22).expect_err("client canceled");
    assert_eq!(err, BoundaryError::Canceled);
}

#[test]
fn boundary_deadline_is_propagated_to_server() {
    let (client, server) = boundary::channel::<u64, u64>(1);

    let _ticket = client
        .call_with_timeout(5, Duration::from_millis(250))
        .expect("queued");

    let req = server.recv().expect("request");
    assert!(req.deadline().is_some());
}
