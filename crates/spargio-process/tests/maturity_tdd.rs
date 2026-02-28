use futures::executor::block_on;
use spargio_process::{CommandBuilder, CommandOptions};
use std::io;
use std::time::Duration;

fn slow_command_builder() -> CommandBuilder {
    if cfg!(windows) {
        CommandBuilder::new("cmd").args(["/C", "ping -n 2 127.0.0.1 > nul"])
    } else {
        CommandBuilder::new("sh").args(["-c", "sleep 0.1"])
    }
}

#[test]
fn command_builder_spawn_and_wait_lifecycle() {
    let rt = spargio::Runtime::builder()
        .shards(1)
        .build()
        .expect("runtime");

    let status = block_on(async {
        let child = CommandBuilder::new(if cfg!(windows) { "cmd" } else { "sh" })
            .args(if cfg!(windows) {
                vec!["/C", "exit", "0"]
            } else {
                vec!["-c", "exit 0"]
            })
            .spawn(&rt.handle())
            .await
            .expect("spawn");
        assert!(child.id().is_some(), "child should report pid");
        child.wait().await.expect("wait")
    });

    assert!(status.success());
}

#[test]
fn spawned_child_wait_timeout_is_enforced() {
    let rt = spargio::Runtime::builder()
        .shards(1)
        .build()
        .expect("runtime");

    let err = block_on(async {
        let child = slow_command_builder()
            .spawn(&rt.handle())
            .await
            .expect("spawn");
        child
            .wait_with_options(CommandOptions::default().with_timeout(Duration::from_millis(5)))
            .await
            .expect_err("timeout")
    });
    assert_eq!(err.kind(), io::ErrorKind::TimedOut);
}
