#![cfg(feature = "macros")]

fn io_uring_available() -> bool {
    match spargio::Runtime::builder()
        .shards(1)
        .backend(spargio::BackendKind::IoUring)
        .build()
    {
        Ok(_) => true,
        Err(spargio::RuntimeError::IoUringInit(_))
        | Err(spargio::RuntimeError::UnsupportedBackend(_)) => false,
        Err(err) => panic!("unexpected runtime init error: {err:?}"),
    }
}

#[spargio::main]
async fn macro_entry_returns_value() -> usize {
    40 + 2
}

#[spargio::main(shards = 1, backend = "io_uring")]
async fn macro_entry_single_shard() -> spargio::ShardId {
    spargio::ShardCtx::current()
        .expect("running on shard")
        .shard_id()
}

#[spargio::main(shards = 1, backend = "io_uring")]
async fn macro_entry_receives_handle(handle: spargio::RuntimeHandle) -> usize {
    handle.shard_count()
}

#[spargio::main(shards = 0)]
async fn macro_entry_invalid_builder() {}

#[test]
fn main_macro_executes_async_body() {
    if !io_uring_available() {
        return;
    }
    assert_eq!(macro_entry_returns_value(), 42);
}

#[test]
fn main_macro_applies_builder_overrides() {
    if !io_uring_available() {
        return;
    }
    assert_eq!(macro_entry_single_shard(), 0);
}

#[test]
fn main_macro_can_inject_runtime_handle_argument() {
    if !io_uring_available() {
        return;
    }
    assert_eq!(macro_entry_receives_handle(), 1);
}

#[test]
fn main_macro_panics_on_runtime_build_failure() {
    let panicked = std::panic::catch_unwind(macro_entry_invalid_builder).is_err();
    assert!(panicked, "expected runtime build failure panic");
}
