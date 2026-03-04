#[cfg(unix)]
use criterion::{Criterion, Throughput, black_box, criterion_group, criterion_main};
#[cfg(unix)]
use futures::future::{Either, select};
#[cfg(all(feature = "uring-native", target_os = "linux"))]
use futures::{StreamExt, channel::mpsc, executor::block_on, future::join_all};
#[cfg(all(feature = "uring-native", target_os = "linux"))]
use libc;
#[cfg(all(feature = "uring-native", target_os = "linux"))]
use spargio::{
    BackendKind, Runtime, RuntimeBuilder, RuntimeError, RuntimeHandle, StealableQueueBackend,
};
#[cfg(unix)]
use std::io::{Read, Write};
#[cfg(unix)]
use std::net::{SocketAddr, TcpListener};
#[cfg(unix)]
use std::os::unix::fs::FileExt;
#[cfg(all(feature = "uring-native", target_os = "linux"))]
use std::str::FromStr;
#[cfg(unix)]
use std::sync::mpsc as std_mpsc;
#[cfg(unix)]
use std::thread;
#[cfg(unix)]
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[cfg(unix)]
const RTT_ROUNDS: usize = 512;
#[cfg(unix)]
const RTT_PAYLOAD: usize = 256;
#[cfg(unix)]
const THROUGHPUT_FRAMES: usize = 2048;
#[cfg(unix)]
const THROUGHPUT_FRAME_BYTES: usize = 4096;
#[cfg(unix)]
const THROUGHPUT_WINDOW: usize = 32;
#[cfg(unix)]
const IMBALANCED_STREAMS: usize = 8;
#[cfg(unix)]
const IMBALANCED_HEAVY_FRAMES: usize = 2048;
#[cfg(unix)]
const IMBALANCED_LIGHT_FRAMES: usize = 128;
#[cfg(unix)]
const IMBALANCED_FRAME_BYTES: usize = 4096;
#[cfg(unix)]
const IMBALANCED_WINDOW: usize = 32;
#[cfg(unix)]
const IMBALANCED_TOTAL_FRAMES: usize =
    IMBALANCED_HEAVY_FRAMES + ((IMBALANCED_STREAMS - 1) * IMBALANCED_LIGHT_FRAMES);
#[cfg(unix)]
const HOTSPOT_ROTATION_STEPS: usize = 64;
#[cfg(unix)]
const HOTSPOT_ROTATION_HEAVY_FRAMES: usize = 32;
#[cfg(unix)]
const HOTSPOT_ROTATION_LIGHT_FRAMES: usize = 2;
#[cfg(unix)]
const HOTSPOT_ROTATION_FRAME_BYTES: usize = 4096;
#[cfg(unix)]
const HOTSPOT_ROTATION_WINDOW: usize = 32;
#[cfg(unix)]
const HOTSPOT_ROTATION_TOTAL_FRAMES: usize = HOTSPOT_ROTATION_STEPS
    * (HOTSPOT_ROTATION_HEAVY_FRAMES + ((IMBALANCED_STREAMS - 1) * HOTSPOT_ROTATION_LIGHT_FRAMES));
#[cfg(unix)]
const PIPELINE_STREAMS: usize = IMBALANCED_STREAMS;
#[cfg(unix)]
const PIPELINE_FRAMES_PER_STREAM: usize = 1024;
#[cfg(unix)]
const PIPELINE_FRAME_BYTES: usize = 4096;
#[cfg(unix)]
const PIPELINE_WINDOW: usize = 32;
#[cfg(unix)]
const PIPELINE_ROTATE_EVERY: usize = 64;
#[cfg(unix)]
const PIPELINE_HEAVY_CPU_ITERS: usize = 4000;
#[cfg(unix)]
const PIPELINE_LIGHT_CPU_ITERS: usize = 150;
#[cfg(unix)]
const PIPELINE_TOTAL_FRAMES: usize = PIPELINE_STREAMS * PIPELINE_FRAMES_PER_STREAM;
#[cfg(unix)]
const KEYED_HOTSPOT_STEPS: usize = HOTSPOT_ROTATION_STEPS;
#[cfg(unix)]
const KEYED_HOTSPOT_HEAVY_FRAMES: usize = HOTSPOT_ROTATION_HEAVY_FRAMES;
#[cfg(unix)]
const KEYED_HOTSPOT_LIGHT_FRAMES: usize = HOTSPOT_ROTATION_LIGHT_FRAMES;
#[cfg(unix)]
const KEYED_HOTSPOT_FRAME_BYTES: usize = HOTSPOT_ROTATION_FRAME_BYTES;
#[cfg(unix)]
const KEYED_HOTSPOT_WINDOW: usize = HOTSPOT_ROTATION_WINDOW;
#[cfg(unix)]
const KEYED_HOTSPOT_OWNER_SHARDS: usize = 2;
#[cfg(unix)]
const KEYED_DISPATCHES_PER_FRAME: usize = 16;
#[cfg(all(feature = "uring-native", target_os = "linux"))]
const KEYED_DISPATCH_TAG: u16 = 0x4B44;
#[cfg(all(feature = "uring-native", target_os = "linux"))]
const KEYED_STOP_TAG: u16 = 0x4B45;
#[cfg(unix)]
const KEYED_HOTSPOT_TOTAL_FRAMES: usize = KEYED_HOTSPOT_STEPS
    * (KEYED_HOTSPOT_HEAVY_FRAMES + ((IMBALANCED_STREAMS - 1) * KEYED_HOTSPOT_LIGHT_FRAMES));
#[cfg(unix)]
const KEYED_CPU_HOTSPOT_STEPS: usize = 96;
#[cfg(unix)]
const KEYED_CPU_HOTSPOT_HEAVY_FRAMES: usize = 24;
#[cfg(unix)]
const KEYED_CPU_HOTSPOT_LIGHT_FRAMES: usize = 2;
#[cfg(unix)]
const KEYED_CPU_HOTSPOT_FRAME_BYTES: usize = 4096;
#[cfg(unix)]
const KEYED_CPU_HOTSPOT_WINDOW: usize = 64;
#[cfg(unix)]
const KEYED_CPU_HOTSPOT_OWNER_SHARDS: usize = 2;
#[cfg(unix)]
const KEYED_CPU_HOTSPOT_HEAVY_ITERS: usize = 6_000;
#[cfg(unix)]
const KEYED_CPU_HOTSPOT_LIGHT_ITERS: usize = 200;
#[cfg(unix)]
const KEYED_CPU_HOTSPOT_TOTAL_FRAMES: usize = KEYED_CPU_HOTSPOT_STEPS
    * (KEYED_CPU_HOTSPOT_HEAVY_FRAMES
        + ((IMBALANCED_STREAMS - 1) * KEYED_CPU_HOTSPOT_LIGHT_FRAMES));
#[cfg(unix)]
const INGRESS_RR_STEPS: usize = 16_384;
#[cfg(unix)]
const INGRESS_RR_PAYLOAD_BYTES: usize = 256;
#[cfg(unix)]
const INGRESS_RR_WINDOW: usize = 1;
#[cfg(unix)]
const INGRESS_RR_TOTAL_FRAMES: usize = INGRESS_RR_STEPS;
#[cfg(unix)]
const FS_NET_MICRO_ROUNDS: usize = 1_024;
#[cfg(unix)]
const FS_NET_MICRO_READ_BYTES: usize = 4096;
#[cfg(unix)]
const FS_NET_MICRO_REPLY_BYTES: usize = 256;
#[cfg(unix)]
const FS_NET_MICRO_FILE_BLOCKS: usize = 128;
#[cfg(unix)]
const FS_NET_DEADLINE_EPOCHS: usize = 16;
#[cfg(unix)]
const FS_NET_DEADLINE_READS_PER_EPOCH: usize = 64;
#[cfg(unix)]
const FS_NET_DEADLINE_REPLY_BYTES: usize = 256;
#[cfg(unix)]
const FS_NET_DEADLINE_DISPATCH_STEPS_PER_EPOCH: usize = 1_024;
#[cfg(unix)]
const FS_NET_DEADLINE_WINDOW: usize = 1;
#[cfg(unix)]
const FS_NET_DEADLINE_TIMER_ROUNDS: usize = 32;
#[cfg(unix)]
const FS_NET_DEADLINE_TIMER_BATCH: usize = 4;
#[cfg(unix)]
const FS_NET_DEADLINE_TIMER_SLEEP_US: u64 = 200;
#[cfg(unix)]
const FS_NET_DEADLINE_FILE_BLOCKS: usize = 256;
#[cfg(unix)]
const FS_NET_DEADLINE_TOTAL_BYTES: usize = FS_NET_DEADLINE_EPOCHS
    * ((FS_NET_DEADLINE_READS_PER_EPOCH * FS_NET_MICRO_READ_BYTES)
        + (FS_NET_DEADLINE_DISPATCH_STEPS_PER_EPOCH * FS_NET_DEADLINE_REPLY_BYTES));
#[cfg(unix)]
const ECHO_DEADLINE_EPOCHS: usize = 24;
#[cfg(unix)]
const ECHO_DEADLINE_ROUTE_STEPS: usize = 512;
#[cfg(unix)]
const ECHO_DEADLINE_RTT_ROUNDS: usize = 64;
#[cfg(unix)]
const ECHO_DEADLINE_PAYLOAD: usize = 256;
#[cfg(unix)]
const ECHO_DEADLINE_WINDOW: usize = 1;
#[cfg(unix)]
const ECHO_DEADLINE_TIMER_ROUNDS: usize = 16;
#[cfg(unix)]
const ECHO_DEADLINE_TIMER_BATCH: usize = 2;
#[cfg(unix)]
const ECHO_DEADLINE_TIMER_SLEEP_US: u64 = 200;
#[cfg(unix)]
const ECHO_DEADLINE_TOTAL_BYTES: usize = ECHO_DEADLINE_EPOCHS
    * ((ECHO_DEADLINE_ROUTE_STEPS * ECHO_DEADLINE_PAYLOAD)
        + (ECHO_DEADLINE_RTT_ROUNDS * ECHO_DEADLINE_PAYLOAD));
#[cfg(unix)]
const MULTITENANT_STEPS: usize = 192;
#[cfg(unix)]
const MULTITENANT_HEAVY_FRAMES: usize = 24;
#[cfg(unix)]
const MULTITENANT_LIGHT_FRAMES: usize = 3;
#[cfg(unix)]
const MULTITENANT_PAYLOAD: usize = 4096;
#[cfg(unix)]
const MULTITENANT_WINDOW: usize = 8;
#[cfg(unix)]
const MULTITENANT_OWNER_SHARDS: usize = 2;
#[cfg(unix)]
const MULTITENANT_TOTAL_FRAMES: usize = MULTITENANT_STEPS
    * (MULTITENANT_HEAVY_FRAMES + ((IMBALANCED_STREAMS - 1) * MULTITENANT_LIGHT_FRAMES));
#[cfg(unix)]
const HOTFLIP_PHASES: usize = 256;
#[cfg(unix)]
const HOTFLIP_FLIP_EVERY_PHASES: usize = 4;
#[cfg(unix)]
const HOTFLIP_HOT_FRAMES: usize = 64;
#[cfg(unix)]
const HOTFLIP_COLD_FRAMES: usize = 1;
#[cfg(unix)]
const HOTFLIP_PAYLOAD: usize = 4096;
#[cfg(unix)]
const HOTFLIP_WINDOW: usize = 16;
#[cfg(unix)]
const HOTFLIP_TOTAL_FRAMES: usize =
    HOTFLIP_PHASES * (HOTFLIP_HOT_FRAMES + ((IMBALANCED_STREAMS - 1) * HOTFLIP_COLD_FRAMES));
#[cfg(unix)]
const PIPELINE_BARRIER_PHASES: usize = 320;
#[cfg(unix)]
const PIPELINE_BARRIER_FRAMES_PER_STREAM: usize = 4;
#[cfg(unix)]
const PIPELINE_BARRIER_PAYLOAD: usize = 4096;
#[cfg(unix)]
const PIPELINE_BARRIER_WINDOW: usize = 4;
#[cfg(unix)]
const PIPELINE_BARRIER_TOTAL_FRAMES: usize =
    PIPELINE_BARRIER_PHASES * IMBALANCED_STREAMS * PIPELINE_BARRIER_FRAMES_PER_STREAM;
#[cfg(unix)]
const KEYED_SPILLOVER_STEPS: usize = 160;
#[cfg(unix)]
const KEYED_SPILLOVER_HEAVY_FRAMES: usize = 80;
#[cfg(unix)]
const KEYED_SPILLOVER_LIGHT_FRAMES: usize = 3;
#[cfg(unix)]
const KEYED_SPILLOVER_PAYLOAD: usize = 4096;
#[cfg(unix)]
const KEYED_SPILLOVER_WINDOW: usize = 32;
#[cfg(unix)]
const KEYED_SPILLOVER_OWNER_SHARDS: usize = 2;
#[cfg(unix)]
const KEYED_SPILLOVER_TOTAL_FRAMES: usize = KEYED_SPILLOVER_STEPS
    * (KEYED_SPILLOVER_HEAVY_FRAMES + ((IMBALANCED_STREAMS - 1) * KEYED_SPILLOVER_LIGHT_FRAMES));
#[cfg(unix)]
const FS_META_REPLY_EPOCHS: usize = 24;
#[cfg(unix)]
const FS_META_REPLY_OPS_PER_EPOCH: usize = 64;
#[cfg(unix)]
const FS_META_REPLY_REPLY_BYTES: usize = 256;
#[cfg(unix)]
const FS_META_REPLY_FILE_BLOCKS: usize = 256;
#[cfg(unix)]
const FS_META_REPLY_TIMER_ROUNDS: usize = 8;
#[cfg(unix)]
const FS_META_REPLY_TIMER_BATCH: usize = 2;
#[cfg(unix)]
const FS_META_REPLY_TIMER_SLEEP_US: u64 = 200;
#[cfg(unix)]
const FS_META_REPLY_TOTAL_BYTES: usize = FS_META_REPLY_EPOCHS
    * (FS_META_REPLY_OPS_PER_EPOCH * (FS_NET_MICRO_READ_BYTES + FS_META_REPLY_REPLY_BYTES));
#[cfg(unix)]
const HD_FANOUT_CANCEL_EPOCHS: usize = 8;
#[cfg(unix)]
const HD_FANOUT_CANCEL_STEPS: usize = 512;
#[cfg(unix)]
const HD_FANOUT_CANCEL_HEAVY_FRAMES: usize = 1;
#[cfg(unix)]
const HD_FANOUT_CANCEL_LIGHT_FRAMES: usize = 1;
#[cfg(unix)]
const HD_FANOUT_CANCEL_PAYLOAD: usize = 256;
#[cfg(unix)]
const HD_FANOUT_CANCEL_WINDOW: usize = 64;
#[cfg(unix)]
const HD_FANOUT_CANCEL_TIMER_ROUNDS: usize = 12;
#[cfg(unix)]
const HD_FANOUT_CANCEL_TIMER_BATCH: usize = 4;
#[cfg(unix)]
const HD_FANOUT_CANCEL_TIMER_SLEEP_US: u64 = 200;
#[cfg(unix)]
const HD_FANOUT_CANCEL_TOTAL_FRAMES: usize = HD_FANOUT_CANCEL_EPOCHS
    * HD_FANOUT_CANCEL_STEPS
    * (HD_FANOUT_CANCEL_HEAVY_FRAMES + ((IMBALANCED_STREAMS - 1) * HD_FANOUT_CANCEL_LIGHT_FRAMES));
#[cfg(unix)]
const HD_MULTITENANT_STEPS: usize = 320;
#[cfg(unix)]
const HD_MULTITENANT_HEAVY_FRAMES: usize = 40;
#[cfg(unix)]
const HD_MULTITENANT_LIGHT_FRAMES: usize = 8;
#[cfg(unix)]
const HD_MULTITENANT_PAYLOAD: usize = 4096;
#[cfg(unix)]
const HD_MULTITENANT_WINDOW: usize = 64;
#[cfg(unix)]
const HD_MULTITENANT_OWNER_SHARDS: usize = 2;
#[cfg(unix)]
const HD_MULTITENANT_TOTAL_FRAMES: usize = HD_MULTITENANT_STEPS
    * (HD_MULTITENANT_HEAVY_FRAMES + ((IMBALANCED_STREAMS - 1) * HD_MULTITENANT_LIGHT_FRAMES));
#[cfg(unix)]
const HD_BARRIER_PIPELINE_PHASES: usize = 320;
#[cfg(unix)]
const HD_BARRIER_PIPELINE_FRAMES_PER_STREAM: usize = 8;
#[cfg(unix)]
const HD_BARRIER_PIPELINE_PAYLOAD: usize = 4096;
#[cfg(unix)]
const HD_BARRIER_PIPELINE_WINDOW: usize = 64;
#[cfg(unix)]
const HD_BARRIER_PIPELINE_TOTAL_FRAMES: usize =
    HD_BARRIER_PIPELINE_PHASES * IMBALANCED_STREAMS * HD_BARRIER_PIPELINE_FRAMES_PER_STREAM;
#[cfg(unix)]
const HD_DEADLINE_GATEWAY_EPOCHS: usize = 10;
#[cfg(unix)]
const HD_DEADLINE_GATEWAY_STREAM_FRAMES: usize = 768;
#[cfg(unix)]
const HD_DEADLINE_GATEWAY_ROUTE_STEPS: usize = 512;
#[cfg(unix)]
const HD_DEADLINE_GATEWAY_RTT_ROUNDS: usize = 128;
#[cfg(unix)]
const HD_DEADLINE_GATEWAY_PAYLOAD: usize = 256;
#[cfg(unix)]
const HD_DEADLINE_GATEWAY_WINDOW: usize = 64;
#[cfg(unix)]
const HD_DEADLINE_GATEWAY_TIMER_ROUNDS: usize = 16;
#[cfg(unix)]
const HD_DEADLINE_GATEWAY_TIMER_BATCH: usize = 4;
#[cfg(unix)]
const HD_DEADLINE_GATEWAY_TIMER_SLEEP_US: u64 = 200;
#[cfg(unix)]
const HD_DEADLINE_GATEWAY_TOTAL_BYTES: usize = HD_DEADLINE_GATEWAY_EPOCHS
    * ((HD_DEADLINE_GATEWAY_STREAM_FRAMES * HD_DEADLINE_GATEWAY_PAYLOAD)
        + (HD_DEADLINE_GATEWAY_ROUTE_STEPS * HD_DEADLINE_GATEWAY_PAYLOAD)
        + (HD_DEADLINE_GATEWAY_RTT_ROUNDS * HD_DEADLINE_GATEWAY_PAYLOAD));
#[cfg(unix)]
const HD_FS_ADMISSION_EPOCHS: usize = 8;
#[cfg(unix)]
const HD_FS_ADMISSION_READS_PER_EPOCH: usize = 128;
#[cfg(unix)]
const HD_FS_ADMISSION_META_PER_EPOCH: usize = 128;
#[cfg(unix)]
const HD_FS_ADMISSION_DISPATCH_STEPS: usize = 1_024;
#[cfg(unix)]
const HD_FS_ADMISSION_REPLY_BYTES: usize = 256;
#[cfg(unix)]
const HD_FS_ADMISSION_WINDOW: usize = 64;
#[cfg(unix)]
const HD_FS_ADMISSION_TIMER_ROUNDS: usize = 12;
#[cfg(unix)]
const HD_FS_ADMISSION_TIMER_BATCH: usize = 4;
#[cfg(unix)]
const HD_FS_ADMISSION_TIMER_SLEEP_US: u64 = 200;
#[cfg(unix)]
const HD_FS_ADMISSION_FILE_BLOCKS: usize = 512;
#[cfg(unix)]
const HD_FS_ADMISSION_TOTAL_BYTES: usize = HD_FS_ADMISSION_EPOCHS
    * ((HD_FS_ADMISSION_READS_PER_EPOCH * FS_NET_MICRO_READ_BYTES)
        + (HD_FS_ADMISSION_DISPATCH_STEPS * HD_FS_ADMISSION_REPLY_BYTES));
#[cfg(unix)]
const FANOUT_ROTATING_FRAMES_PER_STREAM: usize = 1_024;
#[cfg(unix)]
const FANOUT_ROTATING_FRAME_BYTES: usize = 4096;
#[cfg(unix)]
const FANOUT_ROTATING_WINDOW: usize = 32;
#[cfg(unix)]
const FANOUT_ROTATING_ROTATE_EVERY: usize = 16;
#[cfg(unix)]
const FANOUT_ROTATING_HEAVY_CPU_ITERS: usize = 8_000;
#[cfg(unix)]
const FANOUT_ROTATING_LIGHT_CPU_ITERS: usize = 160;
#[cfg(unix)]
const FANOUT_ROTATING_TOTAL_FRAMES: usize = PIPELINE_STREAMS * FANOUT_ROTATING_FRAMES_PER_STREAM;
#[cfg(unix)]
const SESSION_OWNER_SPILLOVER_STEPS: usize = 128;
#[cfg(unix)]
const SESSION_OWNER_SPILLOVER_HEAVY_FRAMES: usize = 96;
#[cfg(unix)]
const SESSION_OWNER_SPILLOVER_LIGHT_FRAMES: usize = 2;
#[cfg(unix)]
const SESSION_OWNER_SPILLOVER_FRAME_BYTES: usize = 4096;
#[cfg(unix)]
const SESSION_OWNER_SPILLOVER_WINDOW: usize = 32;
#[cfg(unix)]
const SESSION_OWNER_SPILLOVER_TOTAL_FRAMES: usize = SESSION_OWNER_SPILLOVER_STEPS
    * (SESSION_OWNER_SPILLOVER_HEAVY_FRAMES
        + ((IMBALANCED_STREAMS - 1) * SESSION_OWNER_SPILLOVER_LIGHT_FRAMES));
#[cfg(unix)]
const BURST_FLIP_PHASES: usize = 160;
#[cfg(unix)]
const BURST_FLIP_FLIP_EVERY_PHASES: usize = 10;
#[cfg(unix)]
const BURST_FLIP_HOT_FRAMES: usize = 96;
#[cfg(unix)]
const BURST_FLIP_COLD_FRAMES: usize = 2;
#[cfg(unix)]
const BURST_FLIP_FRAME_BYTES: usize = 4096;
#[cfg(unix)]
const BURST_FLIP_WINDOW: usize = 32;
#[cfg(unix)]
const BURST_FLIP_TOTAL_FRAMES: usize = BURST_FLIP_PHASES
    * (BURST_FLIP_HOT_FRAMES + ((IMBALANCED_STREAMS - 1) * BURST_FLIP_COLD_FRAMES));
#[cfg(unix)]
const FANIN_BARRIER_PHASES: usize = 384;
#[cfg(unix)]
const FANIN_BARRIER_FRAMES_PER_STREAM: usize = 6;
#[cfg(unix)]
const FANIN_BARRIER_PAYLOAD_BYTES: usize = 1024;
#[cfg(unix)]
const FANIN_BARRIER_WINDOW: usize = 6;
#[cfg(unix)]
const FANIN_BARRIER_TOTAL_FRAMES: usize =
    FANIN_BARRIER_PHASES * IMBALANCED_STREAMS * FANIN_BARRIER_FRAMES_PER_STREAM;
#[cfg(unix)]
const SERIAL_DEP_CHAIN_ROUNDS: usize = 2048;
#[cfg(unix)]
const SERIAL_DEP_CHAIN_PAYLOAD: usize = 256;
#[cfg(unix)]
const KEYED_FLIP_STEPS: usize = 192;
#[cfg(unix)]
const KEYED_FLIP_HOT_FRAMES: usize = 64;
#[cfg(unix)]
const KEYED_FLIP_COLD_FRAMES: usize = 1;
#[cfg(unix)]
const KEYED_FLIP_FRAME_BYTES: usize = 4096;
#[cfg(unix)]
const KEYED_FLIP_WINDOW: usize = 32;
#[cfg(unix)]
const KEYED_FLIP_TOTAL_FRAMES: usize = KEYED_FLIP_STEPS
    * (KEYED_FLIP_HOT_FRAMES + ((IMBALANCED_STREAMS - 1) * KEYED_FLIP_COLD_FRAMES));
#[cfg(unix)]
const FANIN_ROUNDS_PHASES: usize = 512;
#[cfg(unix)]
const FANIN_ROUNDS_FRAMES_PER_STREAM: usize = 4;
#[cfg(unix)]
const FANIN_ROUNDS_PAYLOAD: usize = 1024;
#[cfg(unix)]
const FANIN_ROUNDS_WINDOW: usize = 4;
#[cfg(unix)]
const FANIN_ROUNDS_TOTAL_FRAMES: usize =
    FANIN_ROUNDS_PHASES * IMBALANCED_STREAMS * FANIN_ROUNDS_FRAMES_PER_STREAM;
#[cfg(unix)]
const WAKEUP_SPARSE_EVENTS: usize = 128;
#[cfg(unix)]
const WAKEUP_SPARSE_IDLE_US: u64 = 50;
#[cfg(unix)]
const WAKEUP_SPARSE_PAYLOAD: usize = 64;
#[cfg(unix)]
const TIMER_STORM_ROUNDS: usize = 1024;
#[cfg(unix)]
const TIMER_STORM_BATCH: usize = 8;
#[cfg(unix)]
const TIMER_STORM_SLEEP_US: u64 = 200;
#[cfg(unix)]
const MIXED_CONTROL_EPOCHS: usize = 8;
#[cfg(unix)]
const MIXED_CONTROL_DATA_FRAMES: usize = 512;
#[cfg(unix)]
const MIXED_CONTROL_DATA_PAYLOAD: usize = 4096;
#[cfg(unix)]
const MIXED_CONTROL_DATA_WINDOW: usize = 32;
#[cfg(unix)]
const MIXED_CONTROL_CTRL_ROUNDS: usize = 32;
#[cfg(unix)]
const MIXED_CONTROL_CTRL_PAYLOAD: usize = 64;
#[cfg(unix)]
const MIXED_CONTROL_TOTAL_BYTES: usize = MIXED_CONTROL_EPOCHS
    * ((MIXED_CONTROL_DATA_FRAMES * MIXED_CONTROL_DATA_PAYLOAD)
        + (MIXED_CONTROL_CTRL_ROUNDS * MIXED_CONTROL_CTRL_PAYLOAD));
#[cfg(unix)]
const BOUNDED_BP_FRAMES_PER_STREAM: usize = 768;
#[cfg(unix)]
const BOUNDED_BP_PAYLOAD: usize = 4096;
#[cfg(unix)]
const BOUNDED_BP_WINDOW: usize = 2;
#[cfg(unix)]
const BOUNDED_BP_ROTATE_EVERY: usize = 8;
#[cfg(unix)]
const BOUNDED_BP_HEAVY_ITERS: usize = 1_200;
#[cfg(unix)]
const BOUNDED_BP_LIGHT_ITERS: usize = 80;
#[cfg(unix)]
const BOUNDED_BP_TOTAL_FRAMES: usize = PIPELINE_STREAMS * BOUNDED_BP_FRAMES_PER_STREAM;
#[cfg(unix)]
const POST_IO_LOCALITY_FRAMES_PER_STREAM: usize = 512;
#[cfg(unix)]
const POST_IO_LOCALITY_PAYLOAD: usize = 4096;
#[cfg(unix)]
const POST_IO_LOCALITY_WINDOW: usize = 1;
#[cfg(unix)]
const POST_IO_LOCALITY_ROTATE_EVERY: usize = 1;
#[cfg(unix)]
const POST_IO_LOCALITY_HEAVY_ITERS: usize = 9_000;
#[cfg(unix)]
const POST_IO_LOCALITY_LIGHT_ITERS: usize = 180;
#[cfg(unix)]
const POST_IO_LOCALITY_TOTAL_FRAMES: usize = PIPELINE_STREAMS * POST_IO_LOCALITY_FRAMES_PER_STREAM;

#[cfg(unix)]
fn imbalanced_frames_for_stream(idx: usize, heavy_frames: usize, light_frames: usize) -> usize {
    if idx == 0 { heavy_frames } else { light_frames }
}

#[cfg(unix)]
fn hotspot_rotation_frames_for_step(
    stream_idx: usize,
    step_idx: usize,
    stream_count: usize,
    heavy_frames: usize,
    light_frames: usize,
) -> usize {
    let stream_count = stream_count.max(1);
    let hotspot_stream = step_idx % stream_count;
    if stream_idx == hotspot_stream {
        heavy_frames
    } else {
        light_frames
    }
}

#[cfg(unix)]
fn burst_flip_frames_for_phase(
    stream_idx: usize,
    phase_idx: usize,
    stream_count: usize,
    flip_every_phases: usize,
    hot_frames: usize,
    cold_frames: usize,
) -> usize {
    let stream_count = stream_count.max(1);
    let flip_every_phases = flip_every_phases.max(1);
    let hot_stream = (phase_idx / flip_every_phases) % stream_count;
    if stream_idx == hot_stream {
        hot_frames
    } else {
        cold_frames
    }
}

#[cfg(unix)]
fn run_sparse_event_loop<F>(events: usize, idle_us: u64, mut fire_once: F) -> u64
where
    F: FnMut() -> u64,
{
    let mut checksum = 0u64;
    for _ in 0..events {
        checksum = checksum.wrapping_add(fire_once());
        std::thread::sleep(std::time::Duration::from_micros(idle_us.max(1)));
    }
    checksum
}

#[cfg(unix)]
fn run_mixed_control_data_loop<H, FData, FCtrl>(
    harness: &mut H,
    epochs: usize,
    mut run_data: FData,
    mut run_ctrl: FCtrl,
) -> u64
where
    FData: FnMut(&mut H) -> u64,
    FCtrl: FnMut(&mut H) -> u64,
{
    let mut checksum = 0u64;
    for _ in 0..epochs {
        checksum = checksum.wrapping_add(run_data(harness));
        checksum = checksum.wrapping_add(run_ctrl(harness));
    }
    checksum
}

#[cfg(unix)]
fn run_fs_net_deadline_loop<H, FTimer, FNet>(
    harness: &mut H,
    fixture: &FsBenchFixture,
    epochs: usize,
    reads_per_epoch: usize,
    mut run_timer: FTimer,
    mut run_net: FNet,
) -> u64
where
    FTimer: FnMut(&mut H) -> u64,
    FNet: FnMut(&mut H) -> u64,
{
    let mut checksum = 0u64;
    for _ in 0..epochs {
        checksum = checksum.wrapping_add(run_timer(harness));
        checksum = checksum.wrapping_add(fixture.read_qd1(reads_per_epoch));
        checksum = checksum.wrapping_add(run_net(harness));
    }
    checksum
}

#[cfg(unix)]
fn hotspot_iters_for_frame(
    stream_idx: usize,
    frame_idx: usize,
    stream_count: usize,
    rotate_every: usize,
    heavy_iters: usize,
    light_iters: usize,
) -> usize {
    let stream_count = stream_count.max(1);
    let rotate_every = rotate_every.max(1);
    let hotspot_stream = (frame_idx / rotate_every) % stream_count;
    if stream_idx == hotspot_stream {
        heavy_iters
    } else {
        light_iters
    }
}

#[cfg(unix)]
fn pipeline_cpu_stage(seed: u8, stream_idx: usize, frame_idx: usize, iters: usize) -> u64 {
    let mut x = ((seed as u64) << 40)
        ^ ((stream_idx as u64) << 17)
        ^ ((frame_idx as u64) << 3)
        ^ 0x9E37_79B9_7F4A_7C15;
    for i in 0..iters {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x = x.wrapping_add((i as u64).wrapping_mul(0x27D4_EB2D));
    }
    std::hint::black_box(x)
}

#[cfg(unix)]
fn keyed_hotspot_cpu_tail(
    steps: usize,
    stream_count: usize,
    heavy_iters: usize,
    light_iters: usize,
) -> u64 {
    let stream_count = stream_count.max(1);
    let mut checksum = 0u64;
    for step in 0..steps {
        let hot_stream = step % stream_count;
        for stream_idx in 0..stream_count {
            let iters = if stream_idx == hot_stream {
                heavy_iters
            } else {
                light_iters
            };
            checksum = checksum.wrapping_add(pipeline_cpu_stage(
                (step & 0xFF) as u8,
                stream_idx,
                step,
                iters,
            ));
        }
    }
    checksum
}

#[cfg(unix)]
struct FsBenchFixture {
    path: std::path::PathBuf,
    file: std::fs::File,
    block_size: usize,
    blocks: usize,
}

#[cfg(unix)]
impl FsBenchFixture {
    fn new(block_size: usize, blocks: usize) -> Self {
        use std::time::{SystemTime, UNIX_EPOCH};

        let mut path = std::env::temp_dir();
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        path.push(format!(
            "spargio_net_api_fs_micro_{}_{}.dat",
            std::process::id(),
            stamp
        ));

        let mut seed = 0u8;
        {
            let mut writer = std::fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(&path)
                .expect("create fs micro fixture");
            let mut block = vec![0u8; block_size.max(1)];
            for _ in 0..blocks.max(1) {
                block[0] = seed;
                seed = seed.wrapping_add(1);
                writer.write_all(&block).expect("write fs micro fixture");
            }
            writer.sync_all().expect("sync fs micro fixture");
        }

        let file = std::fs::OpenOptions::new()
            .read(true)
            .open(&path)
            .expect("open fs micro fixture");

        Self {
            path,
            file,
            block_size: block_size.max(1),
            blocks: blocks.max(1),
        }
    }

    fn read_qd1(&self, rounds: usize) -> u64 {
        let mut buf = vec![0u8; self.block_size];
        let mut checksum = 0u64;
        for i in 0..rounds {
            let block_idx = i % self.blocks;
            let offset = (block_idx * self.block_size) as u64;
            let n = self
                .file
                .read_at(&mut buf, offset)
                .expect("fs micro fixture read_at");
            assert_eq!(n, self.block_size, "short fs micro fixture read");
            checksum = checksum.wrapping_add(u64::from(buf[0]));
        }
        checksum
    }

    fn metadata_qd1(&self, rounds: usize) -> u64 {
        let mut checksum = 0u64;
        for _ in 0..rounds {
            let meta = self.file.metadata().expect("fs micro fixture metadata");
            checksum = checksum.wrapping_add(meta.len());
        }
        checksum
    }
}

#[cfg(unix)]
impl Drop for FsBenchFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(unix)]
fn spawn_echo_server_with_clients(
    name: &str,
    client_count: usize,
) -> (SocketAddr, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind listener");
    let addr = listener.local_addr().expect("listener addr");
    let client_count = client_count.max(1);
    let thread_name = name.to_owned();

    let join = thread::Builder::new()
        .name(thread_name.clone())
        .spawn(move || {
            let mut handlers = Vec::with_capacity(client_count);
            for idx in 0..client_count {
                let (mut server, _) = listener.accept().expect("accept");
                server.set_nodelay(true).expect("nodelay server");
                let conn_name = format!("{thread_name}-conn-{idx}");
                let handler = thread::Builder::new()
                    .name(conn_name)
                    .spawn(move || {
                        let mut buf = [0u8; 64 * 1024];
                        loop {
                            match server.read(&mut buf) {
                                Ok(0) => break,
                                Ok(n) => {
                                    if server.write_all(&buf[..n]).is_err() {
                                        break;
                                    }
                                }
                                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {
                                    continue;
                                }
                                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                                    thread::yield_now();
                                }
                                Err(_) => break,
                            }
                        }
                    })
                    .expect("spawn echo conn thread");
                handlers.push(handler);
            }

            for join in handlers {
                if join.join().is_err() {
                    break;
                }
            }
        })
        .expect("spawn echo thread");

    (addr, join)
}

#[cfg(unix)]
enum TokioNetCmd {
    EchoRtt {
        rounds: usize,
        payload: usize,
        reply: std_mpsc::Sender<u64>,
    },
    EchoWindowed {
        frames: usize,
        payload: usize,
        window: usize,
        reply: std_mpsc::Sender<u64>,
    },
    EchoImbalanced {
        heavy_frames: usize,
        light_frames: usize,
        payload: usize,
        window: usize,
        reply: std_mpsc::Sender<u64>,
    },
    EchoHotspotRotation {
        steps: usize,
        heavy_frames: usize,
        light_frames: usize,
        payload: usize,
        window: usize,
        reply: std_mpsc::Sender<u64>,
    },
    EchoPipelineHotspot {
        frames_per_stream: usize,
        payload: usize,
        window: usize,
        rotate_every: usize,
        heavy_iters: usize,
        light_iters: usize,
        reply: std_mpsc::Sender<u64>,
    },
    EchoKeyedHotspot {
        steps: usize,
        heavy_frames: usize,
        light_frames: usize,
        payload: usize,
        window: usize,
        owner_shards: usize,
        reply: std_mpsc::Sender<u64>,
    },
    EchoBurstFlipImbalance {
        phases: usize,
        flip_every_phases: usize,
        hot_frames: usize,
        cold_frames: usize,
        payload: usize,
        window: usize,
        reply: std_mpsc::Sender<u64>,
    },
    EchoFaninBarrierMicroBatches {
        phases: usize,
        frames_per_stream: usize,
        payload: usize,
        window: usize,
        reply: std_mpsc::Sender<u64>,
    },
    TimerCancelRescheduleStorm {
        rounds: usize,
        batch: usize,
        sleep_us: u64,
        reply: std_mpsc::Sender<u64>,
    },
    Shutdown {
        reply: std_mpsc::Sender<()>,
    },
}

#[cfg(unix)]
struct TokioNetHarness {
    cmd_tx: tokio::sync::mpsc::UnboundedSender<TokioNetCmd>,
    thread: Option<thread::JoinHandle<()>>,
    echo_thread: Option<thread::JoinHandle<()>>,
}

#[cfg(unix)]
impl TokioNetHarness {
    fn new() -> Self {
        let (addr, echo_thread) =
            spawn_echo_server_with_clients("bench-net-echo-tokio", IMBALANCED_STREAMS);
        let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel::<TokioNetCmd>();

        let thread = thread::Builder::new()
            .name("bench-net-tokio".to_owned())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                    .expect("tokio runtime");

                rt.block_on(async move {
                    let mut streams = Vec::with_capacity(IMBALANCED_STREAMS);
                    for _ in 0..IMBALANCED_STREAMS {
                        let stream = tokio::net::TcpStream::connect(addr)
                            .await
                            .expect("tokio connect");
                        stream.set_nodelay(true).expect("tokio nodelay");
                        streams.push(stream);
                    }

                    while let Some(cmd) = cmd_rx.recv().await {
                        match cmd {
                            TokioNetCmd::EchoRtt {
                                rounds,
                                payload,
                                reply,
                            } => {
                                let stream = streams.first_mut().expect("tokio primary stream");
                                let value = tokio_echo_rtt(stream, rounds, payload).await;
                                let _ = reply.send(value);
                            }
                            TokioNetCmd::EchoWindowed {
                                frames,
                                payload,
                                window,
                                reply,
                            } => {
                                let stream = streams.first_mut().expect("tokio primary stream");
                                let value =
                                    tokio_echo_windowed(stream, frames, payload, window).await;
                                let _ = reply.send(value);
                            }
                            TokioNetCmd::EchoImbalanced {
                                heavy_frames,
                                light_frames,
                                payload,
                                window,
                                reply,
                            } => {
                                let value = tokio_echo_imbalanced(
                                    &mut streams,
                                    heavy_frames,
                                    light_frames,
                                    payload,
                                    window,
                                )
                                .await;
                                let _ = reply.send(value);
                            }
                            TokioNetCmd::EchoHotspotRotation {
                                steps,
                                heavy_frames,
                                light_frames,
                                payload,
                                window,
                                reply,
                            } => {
                                let value = tokio_echo_hotspot_rotation(
                                    &mut streams,
                                    steps,
                                    heavy_frames,
                                    light_frames,
                                    payload,
                                    window,
                                )
                                .await;
                                let _ = reply.send(value);
                            }
                            TokioNetCmd::EchoPipelineHotspot {
                                frames_per_stream,
                                payload,
                                window,
                                rotate_every,
                                heavy_iters,
                                light_iters,
                                reply,
                            } => {
                                let value = tokio_echo_pipeline_hotspot(
                                    &mut streams,
                                    frames_per_stream,
                                    payload,
                                    window,
                                    rotate_every,
                                    heavy_iters,
                                    light_iters,
                                )
                                .await;
                                let _ = reply.send(value);
                            }
                            TokioNetCmd::EchoKeyedHotspot {
                                steps,
                                heavy_frames,
                                light_frames,
                                payload,
                                window,
                                owner_shards,
                                reply,
                            } => {
                                let value = tokio_echo_keyed_hotspot_rotation(
                                    &mut streams,
                                    steps,
                                    heavy_frames,
                                    light_frames,
                                    payload,
                                    window,
                                    owner_shards,
                                )
                                .await;
                                let _ = reply.send(value);
                            }
                            TokioNetCmd::EchoBurstFlipImbalance {
                                phases,
                                flip_every_phases,
                                hot_frames,
                                cold_frames,
                                payload,
                                window,
                                reply,
                            } => {
                                let value = tokio_echo_burst_flip_imbalance(
                                    &mut streams,
                                    phases,
                                    flip_every_phases,
                                    hot_frames,
                                    cold_frames,
                                    payload,
                                    window,
                                )
                                .await;
                                let _ = reply.send(value);
                            }
                            TokioNetCmd::EchoFaninBarrierMicroBatches {
                                phases,
                                frames_per_stream,
                                payload,
                                window,
                                reply,
                            } => {
                                let value = tokio_echo_fanin_barrier_micro_batches(
                                    &mut streams,
                                    phases,
                                    frames_per_stream,
                                    payload,
                                    window,
                                )
                                .await;
                                let _ = reply.send(value);
                            }
                            TokioNetCmd::TimerCancelRescheduleStorm {
                                rounds,
                                batch,
                                sleep_us,
                                reply,
                            } => {
                                let value =
                                    tokio_timer_cancel_reschedule_storm(rounds, batch, sleep_us)
                                        .await;
                                let _ = reply.send(value);
                            }
                            TokioNetCmd::Shutdown { reply } => {
                                let _ = reply.send(());
                                break;
                            }
                        }
                    }
                });
            })
            .expect("spawn tokio net bench thread");

        Self {
            cmd_tx,
            thread: Some(thread),
            echo_thread: Some(echo_thread),
        }
    }

    fn echo_rtt(&mut self, rounds: usize, payload: usize) -> u64 {
        let (tx, rx) = std_mpsc::channel();
        self.cmd_tx
            .send(TokioNetCmd::EchoRtt {
                rounds,
                payload,
                reply: tx,
            })
            .expect("send echo rtt cmd");
        rx.recv().expect("echo rtt reply")
    }

    fn echo_windowed(&mut self, frames: usize, payload: usize, window: usize) -> u64 {
        let (tx, rx) = std_mpsc::channel();
        self.cmd_tx
            .send(TokioNetCmd::EchoWindowed {
                frames,
                payload,
                window,
                reply: tx,
            })
            .expect("send echo windowed cmd");
        rx.recv().expect("echo windowed reply")
    }

    fn echo_imbalanced(
        &mut self,
        heavy_frames: usize,
        light_frames: usize,
        payload: usize,
        window: usize,
    ) -> u64 {
        let (tx, rx) = std_mpsc::channel();
        self.cmd_tx
            .send(TokioNetCmd::EchoImbalanced {
                heavy_frames,
                light_frames,
                payload,
                window,
                reply: tx,
            })
            .expect("send echo imbalanced cmd");
        rx.recv().expect("echo imbalanced reply")
    }

    fn echo_hotspot_rotation(
        &mut self,
        steps: usize,
        heavy_frames: usize,
        light_frames: usize,
        payload: usize,
        window: usize,
    ) -> u64 {
        let (tx, rx) = std_mpsc::channel();
        self.cmd_tx
            .send(TokioNetCmd::EchoHotspotRotation {
                steps,
                heavy_frames,
                light_frames,
                payload,
                window,
                reply: tx,
            })
            .expect("send echo hotspot rotation cmd");
        rx.recv().expect("echo hotspot rotation reply")
    }

    fn echo_pipeline_hotspot(
        &mut self,
        frames_per_stream: usize,
        payload: usize,
        window: usize,
        rotate_every: usize,
        heavy_iters: usize,
        light_iters: usize,
    ) -> u64 {
        let (tx, rx) = std_mpsc::channel();
        self.cmd_tx
            .send(TokioNetCmd::EchoPipelineHotspot {
                frames_per_stream,
                payload,
                window,
                rotate_every,
                heavy_iters,
                light_iters,
                reply: tx,
            })
            .expect("send echo pipeline hotspot cmd");
        rx.recv().expect("echo pipeline hotspot reply")
    }

    fn echo_keyed_hotspot_rotation(
        &mut self,
        steps: usize,
        heavy_frames: usize,
        light_frames: usize,
        payload: usize,
        window: usize,
        owner_shards: usize,
    ) -> u64 {
        let (tx, rx) = std_mpsc::channel();
        self.cmd_tx
            .send(TokioNetCmd::EchoKeyedHotspot {
                steps,
                heavy_frames,
                light_frames,
                payload,
                window,
                owner_shards,
                reply: tx,
            })
            .expect("send echo keyed hotspot cmd");
        rx.recv().expect("echo keyed hotspot reply")
    }

    fn echo_burst_flip_imbalance(
        &mut self,
        phases: usize,
        flip_every_phases: usize,
        hot_frames: usize,
        cold_frames: usize,
        payload: usize,
        window: usize,
    ) -> u64 {
        let (tx, rx) = std_mpsc::channel();
        self.cmd_tx
            .send(TokioNetCmd::EchoBurstFlipImbalance {
                phases,
                flip_every_phases,
                hot_frames,
                cold_frames,
                payload,
                window,
                reply: tx,
            })
            .expect("send echo burst-flip cmd");
        rx.recv().expect("echo burst-flip reply")
    }

    fn echo_fanin_barrier_micro_batches(
        &mut self,
        phases: usize,
        frames_per_stream: usize,
        payload: usize,
        window: usize,
    ) -> u64 {
        let (tx, rx) = std_mpsc::channel();
        self.cmd_tx
            .send(TokioNetCmd::EchoFaninBarrierMicroBatches {
                phases,
                frames_per_stream,
                payload,
                window,
                reply: tx,
            })
            .expect("send echo fanin-barrier cmd");
        rx.recv().expect("echo fanin-barrier reply")
    }

    fn timer_cancel_reschedule_storm(&mut self, rounds: usize, batch: usize, sleep_us: u64) -> u64 {
        let (tx, rx) = std_mpsc::channel();
        self.cmd_tx
            .send(TokioNetCmd::TimerCancelRescheduleStorm {
                rounds,
                batch,
                sleep_us,
                reply: tx,
            })
            .expect("send tokio timer-storm cmd");
        rx.recv().expect("tokio timer-storm reply")
    }

    fn shutdown(&mut self) {
        let (tx, rx) = std_mpsc::channel();
        let _ = self.cmd_tx.send(TokioNetCmd::Shutdown { reply: tx });
        let _ = rx.recv();

        if let Some(join) = self.thread.take() {
            let _ = join.join();
        }
        if let Some(join) = self.echo_thread.take() {
            let _ = join.join();
        }
    }
}

#[cfg(unix)]
impl Drop for TokioNetHarness {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(unix)]
async fn tokio_echo_rtt(
    stream: &mut tokio::net::TcpStream,
    rounds: usize,
    payload_len: usize,
) -> u64 {
    let mut payload = vec![0u8; payload_len.max(1)];
    let mut recv = vec![0u8; payload_len.max(1)];
    let mut checksum = 0u64;

    for i in 0..rounds {
        payload[0] = i as u8;
        stream.write_all(&payload).await.expect("tokio write_all");
        stream
            .read_exact(&mut recv)
            .await
            .expect("tokio read_exact");
        checksum = checksum.wrapping_add(u64::from(recv[0]));
    }

    checksum
}

#[cfg(unix)]
async fn tokio_echo_windowed(
    stream: &mut tokio::net::TcpStream,
    frames: usize,
    payload_len: usize,
    window: usize,
) -> u64 {
    let mut payload = vec![0u8; payload_len.max(1)];
    let mut recv = vec![0u8; payload_len.max(1)];
    let mut checksum = 0u64;
    let mut next = 0usize;
    let window = window.max(1);

    while next < frames {
        let batch = (frames - next).min(window);
        for idx in 0..batch {
            payload[0] = (next + idx) as u8;
            stream.write_all(&payload).await.expect("tokio write_all");
        }
        for _ in 0..batch {
            stream
                .read_exact(&mut recv)
                .await
                .expect("tokio read_exact");
            checksum = checksum.wrapping_add(u64::from(recv[0]));
        }
        next += batch;
    }

    checksum
}

#[cfg(unix)]
async fn tokio_echo_imbalanced(
    streams: &mut Vec<tokio::net::TcpStream>,
    heavy_frames: usize,
    light_frames: usize,
    payload_len: usize,
    window: usize,
) -> u64 {
    let mut joins = tokio::task::JoinSet::new();
    let moved = std::mem::take(streams);
    let stream_count = moved.len();
    for (idx, mut stream) in moved.into_iter().enumerate() {
        let frames = imbalanced_frames_for_stream(idx, heavy_frames, light_frames);
        joins.spawn(async move {
            let value = tokio_echo_windowed(&mut stream, frames, payload_len, window).await;
            (stream, value)
        });
    }

    let mut checksum = 0u64;
    let mut restored = Vec::with_capacity(stream_count);
    while let Some(outcome) = joins.join_next().await {
        let (stream, value) = outcome.expect("tokio imbalanced stream join");
        restored.push(stream);
        checksum = checksum.wrapping_add(value);
    }
    *streams = restored;
    checksum
}

#[cfg(unix)]
async fn tokio_echo_hotspot_rotation_stream(
    stream: &mut tokio::net::TcpStream,
    stream_idx: usize,
    stream_count: usize,
    steps: usize,
    heavy_frames: usize,
    light_frames: usize,
    payload_len: usize,
    window: usize,
) -> u64 {
    let mut checksum = 0u64;
    for step in 0..steps {
        let frames = hotspot_rotation_frames_for_step(
            stream_idx,
            step,
            stream_count,
            heavy_frames,
            light_frames,
        );
        if frames == 0 {
            continue;
        }
        checksum =
            checksum.wrapping_add(tokio_echo_windowed(stream, frames, payload_len, window).await);
    }
    checksum
}

#[cfg(unix)]
async fn tokio_echo_hotspot_rotation(
    streams: &mut Vec<tokio::net::TcpStream>,
    steps: usize,
    heavy_frames: usize,
    light_frames: usize,
    payload_len: usize,
    window: usize,
) -> u64 {
    let mut joins = tokio::task::JoinSet::new();
    let moved = std::mem::take(streams);
    let stream_count = moved.len();
    for (idx, mut stream) in moved.into_iter().enumerate() {
        joins.spawn(async move {
            let value = tokio_echo_hotspot_rotation_stream(
                &mut stream,
                idx,
                stream_count,
                steps,
                heavy_frames,
                light_frames,
                payload_len,
                window,
            )
            .await;
            (stream, value)
        });
    }

    let mut checksum = 0u64;
    let mut restored = Vec::with_capacity(stream_count);
    while let Some(outcome) = joins.join_next().await {
        let (stream, value) = outcome.expect("tokio hotspot rotation stream join");
        restored.push(stream);
        checksum = checksum.wrapping_add(value);
    }
    *streams = restored;
    checksum
}

#[cfg(unix)]
async fn tokio_echo_pipeline_stream(
    stream: &mut tokio::net::TcpStream,
    stream_idx: usize,
    stream_count: usize,
    frames: usize,
    payload_len: usize,
    window: usize,
    rotate_every: usize,
    heavy_iters: usize,
    light_iters: usize,
) -> u64 {
    let mut payload = vec![0u8; payload_len.max(1)];
    let mut recv = vec![0u8; payload_len.max(1)];
    let mut checksum = 0u64;
    let mut next = 0usize;
    let window = window.max(1);

    while next < frames {
        let batch = (frames - next).min(window);
        for idx in 0..batch {
            payload[0] = (next + idx) as u8;
            stream.write_all(&payload).await.expect("tokio write_all");
        }
        for idx in 0..batch {
            stream
                .read_exact(&mut recv)
                .await
                .expect("tokio read_exact");
            let frame_idx = next + idx;
            let cpu_iters = hotspot_iters_for_frame(
                stream_idx,
                frame_idx,
                stream_count,
                rotate_every,
                heavy_iters,
                light_iters,
            );
            checksum = checksum.wrapping_add(pipeline_cpu_stage(
                recv[0], stream_idx, frame_idx, cpu_iters,
            ));
        }
        next += batch;
    }

    checksum
}

#[cfg(unix)]
async fn tokio_echo_pipeline_hotspot(
    streams: &mut Vec<tokio::net::TcpStream>,
    frames_per_stream: usize,
    payload_len: usize,
    window: usize,
    rotate_every: usize,
    heavy_iters: usize,
    light_iters: usize,
) -> u64 {
    let mut joins = tokio::task::JoinSet::new();
    let moved = std::mem::take(streams);
    let stream_count = moved.len();
    for (idx, mut stream) in moved.into_iter().enumerate() {
        joins.spawn(async move {
            let value = tokio_echo_pipeline_stream(
                &mut stream,
                idx,
                stream_count,
                frames_per_stream,
                payload_len,
                window,
                rotate_every,
                heavy_iters,
                light_iters,
            )
            .await;
            (stream, value)
        });
    }

    let mut checksum = 0u64;
    let mut restored = Vec::with_capacity(stream_count);
    while let Some(outcome) = joins.join_next().await {
        let (stream, value) = outcome.expect("tokio pipeline stream join");
        restored.push(stream);
        checksum = checksum.wrapping_add(value);
    }
    *streams = restored;
    checksum
}

#[cfg(unix)]
async fn tokio_echo_keyed_hotspot_rotation(
    streams: &mut Vec<tokio::net::TcpStream>,
    steps: usize,
    heavy_frames: usize,
    light_frames: usize,
    payload_len: usize,
    window: usize,
    owner_shards: usize,
) -> u64 {
    let owner_count = owner_shards.max(1);
    let mut owner_txs = Vec::with_capacity(owner_count);
    let mut owner_joins = tokio::task::JoinSet::new();
    for _ in 0..owner_count {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<u32>();
        owner_txs.push(tx);
        owner_joins.spawn(async move {
            let mut sum = 0u64;
            while let Some(v) = rx.recv().await {
                sum = sum.wrapping_add(v as u64);
            }
            sum
        });
    }
    let owner_txs = std::sync::Arc::new(owner_txs);

    let stream_count = streams.len();
    let moved = std::mem::take(streams);
    let mut joins = tokio::task::JoinSet::new();
    for (stream_idx, mut stream) in moved.into_iter().enumerate() {
        let owner_txs = owner_txs.clone();
        joins.spawn(async move {
            let mut sum = 0u64;
            for step in 0..steps {
                let frames = hotspot_rotation_frames_for_step(
                    stream_idx,
                    step,
                    stream_count,
                    heavy_frames,
                    light_frames,
                );
                if frames == 0 {
                    continue;
                }
                sum = sum.wrapping_add(
                    tokio_echo_windowed(&mut stream, frames, payload_len, window).await,
                );
                let owner = step % owner_txs.len().max(1);
                let tx = &owner_txs[owner];
                for _ in 0..frames {
                    for _ in 0..KEYED_DISPATCHES_PER_FRAME {
                        tx.send(1).expect("tokio keyed owner channel closed");
                    }
                }
            }
            (stream, sum)
        });
    }

    let mut checksum = 0u64;
    let mut restored = Vec::with_capacity(stream_count);
    while let Some(outcome) = joins.join_next().await {
        let (stream, value) = outcome.expect("tokio keyed hotspot stream join");
        restored.push(stream);
        checksum = checksum.wrapping_add(value);
    }
    *streams = restored;

    drop(owner_txs);
    while let Some(outcome) = owner_joins.join_next().await {
        checksum = checksum.wrapping_add(outcome.expect("tokio keyed owner join"));
    }
    checksum
}

#[cfg(unix)]
async fn tokio_echo_burst_flip_imbalance(
    streams: &mut Vec<tokio::net::TcpStream>,
    phases: usize,
    flip_every_phases: usize,
    hot_frames: usize,
    cold_frames: usize,
    payload_len: usize,
    window: usize,
) -> u64 {
    let stream_count = streams.len();
    let mut checksum = 0u64;
    for phase in 0..phases {
        let mut joins = tokio::task::JoinSet::new();
        let moved = std::mem::take(streams);
        for (idx, mut stream) in moved.into_iter().enumerate() {
            let frames = burst_flip_frames_for_phase(
                idx,
                phase,
                stream_count,
                flip_every_phases,
                hot_frames,
                cold_frames,
            );
            joins.spawn(async move {
                let value = tokio_echo_windowed(&mut stream, frames, payload_len, window).await;
                (stream, value)
            });
        }

        let mut restored = Vec::with_capacity(stream_count);
        while let Some(outcome) = joins.join_next().await {
            let (stream, value) = outcome.expect("tokio burst-flip stream join");
            restored.push(stream);
            checksum = checksum.wrapping_add(value);
        }
        *streams = restored;
    }
    checksum
}

#[cfg(unix)]
async fn tokio_echo_fanin_barrier_micro_batches(
    streams: &mut Vec<tokio::net::TcpStream>,
    phases: usize,
    frames_per_stream: usize,
    payload_len: usize,
    window: usize,
) -> u64 {
    let stream_count = streams.len();
    let mut checksum = 0u64;
    for phase in 0..phases {
        let mut joins = tokio::task::JoinSet::new();
        let moved = std::mem::take(streams);
        for (idx, mut stream) in moved.into_iter().enumerate() {
            joins.spawn(async move {
                let value =
                    tokio_echo_windowed(&mut stream, frames_per_stream, payload_len, window).await;
                (idx, stream, value)
            });
        }

        let mut restored = Vec::with_capacity(stream_count);
        while let Some(outcome) = joins.join_next().await {
            let (idx, stream, value) = outcome.expect("tokio fanin barrier stream join");
            restored.push(stream);
            checksum = checksum.wrapping_add(value);
            checksum =
                checksum.wrapping_add(pipeline_cpu_stage((phase & 0xFF) as u8, idx, phase, 64));
        }
        *streams = restored;
    }
    checksum
}

#[cfg(unix)]
async fn tokio_timer_cancel_reschedule_storm(rounds: usize, batch: usize, sleep_us: u64) -> u64 {
    let mut checksum = 0u64;
    let batch = batch.max(1);
    let sleep_for = std::time::Duration::from_micros(sleep_us.max(1));
    let tick = std::time::Duration::from_micros(1);

    for round in 0..rounds {
        for lane in 0..batch {
            let sleep_fut = tokio::time::sleep(sleep_for);
            futures::pin_mut!(sleep_fut);
            let immediate = futures::future::ready(());
            futures::pin_mut!(immediate);
            match select(sleep_fut, immediate).await {
                Either::Left((_, _)) => {
                    checksum = checksum.wrapping_add(1);
                }
                Either::Right((_, _)) => {
                    checksum = checksum.wrapping_add(3);
                }
            }
            checksum = checksum.wrapping_add((lane as u64) ^ (round as u64));
        }
        tokio::time::sleep(tick).await;
    }

    checksum
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
#[derive(Clone, Copy)]
enum SpargioStreamInitMode {
    SingleContext,
    DistributedConnect,
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
#[derive(Clone, Copy)]
enum SpargioWorkerPlacement {
    StealablePreferred,
    Pinned,
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
fn bench_env_parse<T: FromStr>(name: &str) -> Option<T> {
    std::env::var(name).ok()?.parse().ok()
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
fn bench_env_parse_affinity(name: &str) -> Option<Vec<usize>> {
    let raw = std::env::var(name).ok()?;
    let mut cpus = Vec::new();
    for part in raw.split(',') {
        let cpu = part.trim().parse::<usize>().ok()?;
        cpus.push(cpu);
    }
    if cpus.is_empty() {
        return None;
    }
    Some(cpus)
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
fn apply_spargio_bench_overrides(mut builder: RuntimeBuilder) -> RuntimeBuilder {
    if let Some(v) = bench_env_parse::<usize>("SPARGIO_BENCH_STEAL_VICTIM_PROBE_COUNT") {
        builder = builder.steal_victim_probe_count(v);
    }
    if let Some(v) = bench_env_parse::<usize>("SPARGIO_BENCH_STEAL_BATCH_SIZE") {
        builder = builder.steal_batch_size(v);
    }
    if let Some(v) = bench_env_parse::<usize>("SPARGIO_BENCH_STEAL_LOCALITY_MARGIN") {
        builder = builder.steal_locality_margin(v);
    }
    if let Some(v) = bench_env_parse::<usize>("SPARGIO_BENCH_STEAL_FAIL_COST") {
        builder = builder.steal_fail_cost(v);
    }
    if let Some(v) = bench_env_parse::<usize>("SPARGIO_BENCH_STEAL_BACKOFF_MIN") {
        builder = builder.steal_backoff_min(v);
    }
    if let Some(v) = bench_env_parse::<usize>("SPARGIO_BENCH_STEAL_BACKOFF_MAX") {
        builder = builder.steal_backoff_max(v);
    }
    if let Some(v) = bench_env_parse::<usize>("SPARGIO_BENCH_STEAL_VICTIM_STRIDE") {
        builder = builder.steal_victim_stride(v);
    }
    if let Some(v) = bench_env_parse::<usize>("SPARGIO_BENCH_STEAL_BUDGET") {
        builder = builder.steal_budget(v);
    }
    if let Some(v) = bench_env_parse::<usize>("SPARGIO_BENCH_STEALABLE_QUEUE_CAPACITY") {
        builder = builder.stealable_queue_capacity(v);
    }
    if let Some(cpus) = bench_env_parse_affinity("SPARGIO_BENCH_THREAD_AFFINITY") {
        builder = builder.thread_affinity(cpus);
    }
    if let Ok(v) = std::env::var("SPARGIO_BENCH_STEALABLE_QUEUE_BACKEND") {
        let backend = match v.to_ascii_lowercase().as_str() {
            "segqueue" | "segqueueexperimental" | "seg_queue" => {
                StealableQueueBackend::SegQueueExperimental
            }
            _ => StealableQueueBackend::Mutex,
        };
        builder = builder.stealable_queue_backend(backend);
    }
    builder
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
#[derive(Clone)]
struct SpargioBenchStream {
    stream: spargio::net::TcpStream,
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
#[derive(Clone, Copy)]
enum SpargioRecvMode {
    Multishot,
    ReadExact,
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
enum SpargioNetCmd {
    EchoRtt {
        rounds: usize,
        payload: usize,
        reply: std_mpsc::Sender<u64>,
    },
    EchoWindowed {
        frames: usize,
        payload: usize,
        window: usize,
        reply: std_mpsc::Sender<u64>,
    },
    EchoImbalanced {
        heavy_frames: usize,
        light_frames: usize,
        payload: usize,
        window: usize,
        reply: std_mpsc::Sender<u64>,
    },
    EchoHotspotRotation {
        steps: usize,
        heavy_frames: usize,
        light_frames: usize,
        payload: usize,
        window: usize,
        reply: std_mpsc::Sender<u64>,
    },
    EchoPipelineHotspot {
        frames_per_stream: usize,
        payload: usize,
        window: usize,
        rotate_every: usize,
        heavy_iters: usize,
        light_iters: usize,
        reply: std_mpsc::Sender<u64>,
    },
    EchoKeyedHotspot {
        steps: usize,
        heavy_frames: usize,
        light_frames: usize,
        payload: usize,
        window: usize,
        owner_shards: usize,
        reply: std_mpsc::Sender<u64>,
    },
    EchoBurstFlipImbalance {
        phases: usize,
        flip_every_phases: usize,
        hot_frames: usize,
        cold_frames: usize,
        payload: usize,
        window: usize,
        reply: std_mpsc::Sender<u64>,
    },
    EchoFaninBarrierMicroBatches {
        phases: usize,
        frames_per_stream: usize,
        payload: usize,
        window: usize,
        reply: std_mpsc::Sender<u64>,
    },
    TimerCancelRescheduleStorm {
        rounds: usize,
        batch: usize,
        sleep_us: u64,
        reply: std_mpsc::Sender<u64>,
    },
    Shutdown {
        reply: std_mpsc::Sender<()>,
    },
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
struct SpargioNetHarness {
    runtime: Runtime,
    cmd_tx: mpsc::UnboundedSender<SpargioNetCmd>,
    worker_join: Option<spargio::JoinHandle<()>>,
    echo_thread: Option<thread::JoinHandle<()>>,
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
impl SpargioNetHarness {
    fn new() -> Option<Self> {
        Self::new_with_stream_init_mode(
            SpargioStreamInitMode::SingleContext,
            SpargioWorkerPlacement::StealablePreferred,
        )
    }

    fn new_pinned() -> Option<Self> {
        Self::new_with_stream_init_mode(
            SpargioStreamInitMode::SingleContext,
            SpargioWorkerPlacement::Pinned,
        )
    }

    fn new_distributed() -> Option<Self> {
        Self::new_with_stream_init_mode(
            SpargioStreamInitMode::DistributedConnect,
            SpargioWorkerPlacement::StealablePreferred,
        )
    }

    fn new_with_stream_init_mode(
        stream_init: SpargioStreamInitMode,
        worker_placement: SpargioWorkerPlacement,
    ) -> Option<Self> {
        let base_builder = apply_spargio_bench_overrides(
            Runtime::builder()
                .backend(BackendKind::IoUring)
                .shards(2)
                .hot_msg_tags([KEYED_DISPATCH_TAG, KEYED_STOP_TAG])
                .coalesced_hot_msg_tag(KEYED_DISPATCH_TAG),
        );
        let runtime = match base_builder.clone().io_uring_throughput_mode(None).build() {
            Ok(rt) => rt,
            Err(RuntimeError::IoUringInit(_)) => match base_builder.build() {
                Ok(rt) => rt,
                Err(RuntimeError::IoUringInit(_)) => return None,
                Err(err) => panic!("unexpected runtime init error: {err:?}"),
            },
            Err(err) => panic!("unexpected runtime init error: {err:?}"),
        };

        let (addr, echo_thread) =
            spawn_echo_server_with_clients("bench-net-echo-spargio", IMBALANCED_STREAMS);
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded::<SpargioNetCmd>();
        let handle = runtime.handle();
        let worker_handle = handle.clone();
        let worker_fut = async move {
            let mut streams = Vec::with_capacity(IMBALANCED_STREAMS);
            match stream_init {
                SpargioStreamInitMode::SingleContext => {
                    for _ in 0..IMBALANCED_STREAMS {
                        let stream = spargio::net::TcpStream::connect(worker_handle.clone(), addr)
                            .await
                            .expect("spargio connect");
                        streams.push(SpargioBenchStream { stream });
                    }
                }
                SpargioStreamInitMode::DistributedConnect => {
                    let distributed = spargio::net::TcpStream::connect_many_round_robin(
                        worker_handle.clone(),
                        addr,
                        IMBALANCED_STREAMS,
                    )
                    .await
                    .expect("distributed stream connect");
                    for stream in distributed {
                        streams.push(SpargioBenchStream { stream });
                    }
                }
            }
            while let Some(cmd) = cmd_rx.next().await {
                match cmd {
                    SpargioNetCmd::EchoRtt {
                        rounds,
                        payload,
                        reply,
                    } => {
                        let stream = &streams.first().expect("spargio primary stream").stream;
                        let value = spargio_echo_rtt(stream, rounds, payload).await;
                        let _ = reply.send(value);
                    }
                    SpargioNetCmd::EchoWindowed {
                        frames,
                        payload,
                        window,
                        reply,
                    } => {
                        let stream = &streams.first().expect("spargio primary stream").stream;
                        let value = spargio_echo_windowed(
                            stream,
                            frames,
                            payload,
                            window,
                            SpargioRecvMode::Multishot,
                        )
                        .await;
                        let _ = reply.send(value);
                    }
                    SpargioNetCmd::EchoImbalanced {
                        heavy_frames,
                        light_frames,
                        payload,
                        window,
                        reply,
                    } => {
                        let value = spargio_echo_imbalanced(
                            worker_handle.clone(),
                            &streams,
                            heavy_frames,
                            light_frames,
                            payload,
                            window,
                        )
                        .await;
                        let _ = reply.send(value);
                    }
                    SpargioNetCmd::EchoHotspotRotation {
                        steps,
                        heavy_frames,
                        light_frames,
                        payload,
                        window,
                        reply,
                    } => {
                        let value = spargio_echo_hotspot_rotation(
                            worker_handle.clone(),
                            &streams,
                            steps,
                            heavy_frames,
                            light_frames,
                            payload,
                            window,
                        )
                        .await;
                        let _ = reply.send(value);
                    }
                    SpargioNetCmd::EchoPipelineHotspot {
                        frames_per_stream,
                        payload,
                        window,
                        rotate_every,
                        heavy_iters,
                        light_iters,
                        reply,
                    } => {
                        let value = spargio_echo_pipeline_hotspot(
                            worker_handle.clone(),
                            &streams,
                            frames_per_stream,
                            payload,
                            window,
                            rotate_every,
                            heavy_iters,
                            light_iters,
                        )
                        .await;
                        let _ = reply.send(value);
                    }
                    SpargioNetCmd::EchoKeyedHotspot {
                        steps,
                        heavy_frames,
                        light_frames,
                        payload,
                        window,
                        owner_shards,
                        reply,
                    } => {
                        let value = spargio_echo_keyed_hotspot_rotation(
                            worker_handle.clone(),
                            &streams,
                            steps,
                            heavy_frames,
                            light_frames,
                            payload,
                            window,
                            owner_shards,
                        )
                        .await;
                        let _ = reply.send(value);
                    }
                    SpargioNetCmd::EchoBurstFlipImbalance {
                        phases,
                        flip_every_phases,
                        hot_frames,
                        cold_frames,
                        payload,
                        window,
                        reply,
                    } => {
                        let value = spargio_echo_burst_flip_imbalance(
                            worker_handle.clone(),
                            &streams,
                            phases,
                            flip_every_phases,
                            hot_frames,
                            cold_frames,
                            payload,
                            window,
                        )
                        .await;
                        let _ = reply.send(value);
                    }
                    SpargioNetCmd::EchoFaninBarrierMicroBatches {
                        phases,
                        frames_per_stream,
                        payload,
                        window,
                        reply,
                    } => {
                        let value = spargio_echo_fanin_barrier_micro_batches(
                            worker_handle.clone(),
                            &streams,
                            phases,
                            frames_per_stream,
                            payload,
                            window,
                        )
                        .await;
                        let _ = reply.send(value);
                    }
                    SpargioNetCmd::TimerCancelRescheduleStorm {
                        rounds,
                        batch,
                        sleep_us,
                        reply,
                    } => {
                        let value =
                            spargio_timer_cancel_reschedule_storm(rounds, batch, sleep_us).await;
                        let _ = reply.send(value);
                    }
                    SpargioNetCmd::Shutdown { reply } => {
                        let _ = reply.send(());
                        break;
                    }
                }
            }
        };
        let worker_join = match worker_placement {
            SpargioWorkerPlacement::StealablePreferred => handle
                .spawn_with_placement(spargio::TaskPlacement::StealablePreferred(1), worker_fut),
            SpargioWorkerPlacement::Pinned => {
                handle.spawn_with_placement(spargio::TaskPlacement::Pinned(1), worker_fut)
            }
        }
        .ok()?;

        Some(Self {
            runtime,
            cmd_tx,
            worker_join: Some(worker_join),
            echo_thread: Some(echo_thread),
        })
    }

    fn echo_rtt(&mut self, rounds: usize, payload_len: usize) -> u64 {
        let (tx, rx) = std_mpsc::channel();
        self.cmd_tx
            .unbounded_send(SpargioNetCmd::EchoRtt {
                rounds,
                payload: payload_len,
                reply: tx,
            })
            .expect("send spargio rtt cmd");
        rx.recv().expect("spargio rtt reply")
    }

    fn echo_windowed(&mut self, frames: usize, payload_len: usize, window: usize) -> u64 {
        let (tx, rx) = std_mpsc::channel();
        self.cmd_tx
            .unbounded_send(SpargioNetCmd::EchoWindowed {
                frames,
                payload: payload_len,
                window,
                reply: tx,
            })
            .expect("send spargio windowed cmd");
        rx.recv().expect("spargio windowed reply")
    }

    fn echo_imbalanced(
        &mut self,
        heavy_frames: usize,
        light_frames: usize,
        payload_len: usize,
        window: usize,
    ) -> u64 {
        let (tx, rx) = std_mpsc::channel();
        self.cmd_tx
            .unbounded_send(SpargioNetCmd::EchoImbalanced {
                heavy_frames,
                light_frames,
                payload: payload_len,
                window,
                reply: tx,
            })
            .expect("send spargio imbalanced cmd");
        rx.recv().expect("spargio imbalanced reply")
    }

    fn echo_hotspot_rotation(
        &mut self,
        steps: usize,
        heavy_frames: usize,
        light_frames: usize,
        payload_len: usize,
        window: usize,
    ) -> u64 {
        let (tx, rx) = std_mpsc::channel();
        self.cmd_tx
            .unbounded_send(SpargioNetCmd::EchoHotspotRotation {
                steps,
                heavy_frames,
                light_frames,
                payload: payload_len,
                window,
                reply: tx,
            })
            .expect("send spargio hotspot rotation cmd");
        rx.recv().expect("spargio hotspot rotation reply")
    }

    fn echo_pipeline_hotspot(
        &mut self,
        frames_per_stream: usize,
        payload_len: usize,
        window: usize,
        rotate_every: usize,
        heavy_iters: usize,
        light_iters: usize,
    ) -> u64 {
        let (tx, rx) = std_mpsc::channel();
        self.cmd_tx
            .unbounded_send(SpargioNetCmd::EchoPipelineHotspot {
                frames_per_stream,
                payload: payload_len,
                window,
                rotate_every,
                heavy_iters,
                light_iters,
                reply: tx,
            })
            .expect("send spargio pipeline hotspot cmd");
        rx.recv().expect("spargio pipeline hotspot reply")
    }

    fn echo_keyed_hotspot_rotation(
        &mut self,
        steps: usize,
        heavy_frames: usize,
        light_frames: usize,
        payload_len: usize,
        window: usize,
        owner_shards: usize,
    ) -> u64 {
        let (tx, rx) = std_mpsc::channel();
        self.cmd_tx
            .unbounded_send(SpargioNetCmd::EchoKeyedHotspot {
                steps,
                heavy_frames,
                light_frames,
                payload: payload_len,
                window,
                owner_shards,
                reply: tx,
            })
            .expect("send spargio keyed hotspot cmd");
        rx.recv().expect("spargio keyed hotspot reply")
    }

    fn echo_burst_flip_imbalance(
        &mut self,
        phases: usize,
        flip_every_phases: usize,
        hot_frames: usize,
        cold_frames: usize,
        payload_len: usize,
        window: usize,
    ) -> u64 {
        let (tx, rx) = std_mpsc::channel();
        self.cmd_tx
            .unbounded_send(SpargioNetCmd::EchoBurstFlipImbalance {
                phases,
                flip_every_phases,
                hot_frames,
                cold_frames,
                payload: payload_len,
                window,
                reply: tx,
            })
            .expect("send spargio burst-flip cmd");
        rx.recv().expect("spargio burst-flip reply")
    }

    fn echo_fanin_barrier_micro_batches(
        &mut self,
        phases: usize,
        frames_per_stream: usize,
        payload_len: usize,
        window: usize,
    ) -> u64 {
        let (tx, rx) = std_mpsc::channel();
        self.cmd_tx
            .unbounded_send(SpargioNetCmd::EchoFaninBarrierMicroBatches {
                phases,
                frames_per_stream,
                payload: payload_len,
                window,
                reply: tx,
            })
            .expect("send spargio fanin-barrier cmd");
        rx.recv().expect("spargio fanin-barrier reply")
    }

    fn timer_cancel_reschedule_storm(&mut self, rounds: usize, batch: usize, sleep_us: u64) -> u64 {
        let (tx, rx) = std_mpsc::channel();
        self.cmd_tx
            .unbounded_send(SpargioNetCmd::TimerCancelRescheduleStorm {
                rounds,
                batch,
                sleep_us,
                reply: tx,
            })
            .expect("send spargio timer-storm cmd");
        rx.recv().expect("spargio timer-storm reply")
    }

    fn shutdown(&mut self) {
        let (tx, rx) = std_mpsc::channel();
        let _ = self
            .cmd_tx
            .unbounded_send(SpargioNetCmd::Shutdown { reply: tx });
        let _ = rx.recv();
        if let Some(join) = self.worker_join.take() {
            let _ = block_on(join);
        }
        if let Some(join) = self.echo_thread.take() {
            let _ = join.join();
        }
        let _ = &self.runtime;
    }
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
impl Drop for SpargioNetHarness {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
async fn spargio_echo_rtt(
    stream: &spargio::net::TcpStream,
    rounds: usize,
    payload_len: usize,
) -> u64 {
    let mut payload = vec![0u8; payload_len.max(1)];
    let mut recv = vec![0u8; payload_len.max(1)];
    let mut checksum = 0u64;

    for i in 0..rounds {
        payload[0] = i as u8;
        payload = stream
            .write_all_owned(payload)
            .await
            .expect("write_all_owned");
        recv = stream
            .read_exact_owned(recv)
            .await
            .expect("read_exact_owned");
        checksum = checksum.wrapping_add(u64::from(recv[0]));
    }

    checksum
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
async fn spargio_echo_windowed(
    stream: &spargio::net::TcpStream,
    frames: usize,
    payload_len: usize,
    window: usize,
    recv_mode: SpargioRecvMode,
) -> u64 {
    let payload_len = payload_len.max(1);
    let mut recv = vec![0u8; payload_len];
    let mut tx_pool: Vec<Vec<u8>> = (0..window.max(1)).map(|_| vec![0u8; payload_len]).collect();
    let mut multishot_supported = matches!(recv_mode, SpargioRecvMode::Multishot);
    let mut checksum = 0u64;
    let mut next = 0usize;
    let window = window.max(1);

    while next < frames {
        let batch = (frames - next).min(window);
        let mut send_batch = Vec::with_capacity(batch);
        for idx in 0..batch {
            let mut payload = tx_pool.pop().unwrap_or_else(|| vec![0u8; payload_len]);
            payload[0] = (next + idx) as u8;
            send_batch.push(payload);
        }
        let (_sent, mut returned) = stream
            .send_all_batch(send_batch, batch)
            .await
            .expect("send_all_batch");
        tx_pool.append(&mut returned);

        let mut remaining = batch * payload_len;
        if multishot_supported {
            let buffer_count = ((batch * 2).max(4)).min(u16::MAX as usize) as u16;
            match stream
                .recv_multishot_segments(payload_len, buffer_count, remaining)
                .await
            {
                Ok(multishot) => {
                    for seg in multishot.segments {
                        let end = seg
                            .offset
                            .saturating_add(seg.len)
                            .min(multishot.buffer.len());
                        if seg.offset < end {
                            checksum =
                                checksum.wrapping_add(u64::from(multishot.buffer[seg.offset]));
                            remaining = remaining.saturating_sub(end - seg.offset);
                            if remaining == 0 {
                                break;
                            }
                        }
                    }
                }
                Err(err) => {
                    let raw = err.raw_os_error().unwrap_or_default();
                    if raw == libc::EINVAL || raw == libc::ENOSYS || raw == libc::EOPNOTSUPP {
                        multishot_supported = false;
                    } else {
                        panic!("recv_multishot_segments failed unexpectedly: {err}");
                    }
                }
            }
        }

        while remaining > 0 {
            recv = stream
                .read_exact_owned(recv)
                .await
                .expect("read_exact_owned");
            checksum = checksum.wrapping_add(u64::from(recv[0]));
            remaining = remaining.saturating_sub(payload_len);
        }
        next += batch;
    }

    checksum
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
async fn spargio_echo_imbalanced(
    handle: RuntimeHandle,
    streams: &[SpargioBenchStream],
    heavy_frames: usize,
    light_frames: usize,
    payload_len: usize,
    window: usize,
) -> u64 {
    let mut joins = Vec::with_capacity(streams.len());
    for (idx, bench_stream) in streams.iter().cloned().enumerate() {
        let stream = bench_stream.stream;
        let frames = imbalanced_frames_for_stream(idx, heavy_frames, light_frames);
        let fut = async move {
            spargio_echo_windowed(
                &stream,
                frames,
                payload_len,
                window,
                SpargioRecvMode::Multishot,
            )
            .await
        };
        let join = handle
            .spawn_stealable(fut)
            .expect("spawn spargio imbalanced stream (stealable)");
        joins.push(join);
    }

    let mut checksum = 0u64;
    for join in joins {
        let value = join.await.expect("spargio imbalanced stream join");
        checksum = checksum.wrapping_add(value);
    }
    checksum
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
async fn spargio_echo_hotspot_rotation_stream(
    stream: &spargio::net::TcpStream,
    stream_idx: usize,
    stream_count: usize,
    steps: usize,
    heavy_frames: usize,
    light_frames: usize,
    payload_len: usize,
    window: usize,
    recv_mode: SpargioRecvMode,
) -> u64 {
    let mut checksum = 0u64;
    for step in 0..steps {
        let frames = hotspot_rotation_frames_for_step(
            stream_idx,
            step,
            stream_count,
            heavy_frames,
            light_frames,
        );
        if frames == 0 {
            continue;
        }
        checksum = checksum.wrapping_add(
            spargio_echo_windowed(stream, frames, payload_len, window, recv_mode).await,
        );
    }
    checksum
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
async fn spargio_echo_hotspot_rotation(
    handle: RuntimeHandle,
    streams: &[SpargioBenchStream],
    steps: usize,
    heavy_frames: usize,
    light_frames: usize,
    payload_len: usize,
    window: usize,
) -> u64 {
    let stream_count = streams.len();
    let mut joins = Vec::with_capacity(stream_count);
    for (idx, bench_stream) in streams.iter().cloned().enumerate() {
        let stream = bench_stream.stream;
        let stream_for_spawn = stream.clone();
        let fut = async move {
            spargio_echo_hotspot_rotation_stream(
                &stream,
                idx,
                stream_count,
                steps,
                heavy_frames,
                light_frames,
                payload_len,
                window,
                SpargioRecvMode::ReadExact,
            )
            .await
        };
        let join = stream_for_spawn
            .spawn_on_session(&handle, fut)
            .expect("spawn spargio hotspot rotation stream");
        joins.push(join);
    }

    let mut checksum = 0u64;
    for join in joins {
        let value = join.await.expect("spargio hotspot rotation stream join");
        checksum = checksum.wrapping_add(value);
    }
    checksum
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
async fn spargio_echo_pipeline_stream(
    stream: &spargio::net::TcpStream,
    stream_idx: usize,
    stream_count: usize,
    frames: usize,
    payload_len: usize,
    window: usize,
    rotate_every: usize,
    heavy_iters: usize,
    light_iters: usize,
) -> u64 {
    let mut payload = vec![0u8; payload_len.max(1)];
    let mut recv = vec![0u8; payload_len.max(1)];
    let mut checksum = 0u64;
    let mut next = 0usize;
    let window = window.max(1);

    while next < frames {
        let batch = (frames - next).min(window);
        for idx in 0..batch {
            payload[0] = (next + idx) as u8;
            payload = stream
                .write_all_owned(payload)
                .await
                .expect("write_all_owned");
        }
        for idx in 0..batch {
            recv = stream
                .read_exact_owned(recv)
                .await
                .expect("read_exact_owned");
            let frame_idx = next + idx;
            let cpu_iters = hotspot_iters_for_frame(
                stream_idx,
                frame_idx,
                stream_count,
                rotate_every,
                heavy_iters,
                light_iters,
            );
            checksum = checksum.wrapping_add(pipeline_cpu_stage(
                recv[0], stream_idx, frame_idx, cpu_iters,
            ));
        }
        next += batch;
    }

    checksum
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
async fn spargio_echo_pipeline_hotspot(
    handle: RuntimeHandle,
    streams: &[SpargioBenchStream],
    frames_per_stream: usize,
    payload_len: usize,
    window: usize,
    rotate_every: usize,
    heavy_iters: usize,
    light_iters: usize,
) -> u64 {
    let stream_count = streams.len();
    let mut joins = Vec::with_capacity(stream_count);
    for (idx, bench_stream) in streams.iter().cloned().enumerate() {
        let stream = bench_stream.stream;
        let stream_for_spawn = stream.clone();
        let fut = async move {
            spargio_echo_pipeline_stream(
                &stream,
                idx,
                stream_count,
                frames_per_stream,
                payload_len,
                window,
                rotate_every,
                heavy_iters,
                light_iters,
            )
            .await
        };
        let join = stream_for_spawn
            .spawn_on_session(&handle, fut)
            .expect("spawn spargio pipeline hotspot stream");
        joins.push(join);
    }

    let mut checksum = 0u64;
    for join in joins {
        let value = join.await.expect("spargio pipeline stream join");
        checksum = checksum.wrapping_add(value);
    }
    checksum
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
async fn spargio_send_keyed_dispatch_messages(
    remote: &spargio::RemoteShard,
    mut count: usize,
    batch_size: usize,
) {
    let batch_size = batch_size.max(1);
    while count > 0 {
        let chunk = count.min(batch_size);
        let msgs = std::iter::repeat((KEYED_DISPATCH_TAG, 1u32)).take(chunk);
        match remote.send_many_raw_nowait(msgs) {
            Ok(()) => {
                count -= chunk;
            }
            Err(spargio::SendError::Backpressure) => {
                let ticket = remote
                    .send_raw(KEYED_DISPATCH_TAG, 1)
                    .expect("send keyed dispatch backpressure fallback");
                ticket
                    .await
                    .expect("await keyed dispatch backpressure fallback");
                count -= 1;
            }
            Err(spargio::SendError::Closed) => {
                panic!("send keyed dispatch failed: shard closed");
            }
        }
    }
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
async fn spargio_echo_keyed_hotspot_rotation(
    handle: RuntimeHandle,
    streams: &[SpargioBenchStream],
    steps: usize,
    heavy_frames: usize,
    light_frames: usize,
    payload_len: usize,
    window: usize,
    owner_shards: usize,
) -> u64 {
    let owner_count = owner_shards.max(1).min(handle.shard_count().max(1));
    let stream_count = streams.len();

    let mut owner_joins = Vec::with_capacity(owner_count);
    for owner_idx in 0..owner_count {
        let join = handle
            .spawn_pinned(owner_idx as spargio::ShardId, async move {
                if stream_count == 0 {
                    return 0u64;
                }
                let mut stop_seen = 0usize;
                let mut sum = 0u64;
                while stop_seen < stream_count {
                    let hot_count = {
                        spargio::ShardCtx::current()
                            .expect("spargio keyed owner shard context")
                            .next_hot_count(KEYED_DISPATCH_TAG)
                    };
                    let next = {
                        spargio::ShardCtx::current()
                            .expect("spargio keyed owner shard context")
                            .next_hot_event()
                    };
                    match select(Box::pin(hot_count), Box::pin(next)).await {
                        Either::Left((count, _)) => {
                            sum = sum.wrapping_add(count);
                        }
                        Either::Right((spargio::Event::RingMsg { tag, val, .. }, _)) => {
                            if tag == KEYED_STOP_TAG {
                                stop_seen += 1;
                            } else if tag == KEYED_DISPATCH_TAG {
                                sum = sum.wrapping_add(val as u64);
                            }
                        }
                    }
                }
                while let Some(count) = spargio::ShardCtx::current()
                    .expect("spargio keyed owner shard context")
                    .try_take_hot_count(KEYED_DISPATCH_TAG)
                {
                    sum = sum.wrapping_add(count);
                }
                sum
            })
            .expect("spawn spargio keyed owner");
        owner_joins.push(join);
    }

    let mut stream_joins = Vec::with_capacity(stream_count);
    for (stream_idx, bench_stream) in streams.iter().cloned().enumerate() {
        let stream = bench_stream.stream;
        let stream_for_spawn = stream.clone();
        let keyed_remotes = (0..owner_count)
            .map(|owner| {
                handle
                    .remote(owner as spargio::ShardId)
                    .expect("spargio keyed owner remote")
            })
            .collect::<Vec<_>>();
        let fut = async move {
            let mut sum = 0u64;
            for step in 0..steps {
                let frames = hotspot_rotation_frames_for_step(
                    stream_idx,
                    step,
                    stream_count,
                    heavy_frames,
                    light_frames,
                );
                if frames == 0 {
                    continue;
                }
                sum = sum.wrapping_add(
                    spargio_echo_windowed(
                        &stream,
                        frames,
                        payload_len,
                        window,
                        SpargioRecvMode::ReadExact,
                    )
                    .await,
                );
                let owner_idx = step % keyed_remotes.len().max(1);
                spargio_send_keyed_dispatch_messages(
                    &keyed_remotes[owner_idx],
                    frames.saturating_mul(KEYED_DISPATCHES_PER_FRAME),
                    window.saturating_mul(2),
                )
                .await;
            }
            for remote in &keyed_remotes {
                let ticket = remote
                    .send_raw(KEYED_STOP_TAG, 1)
                    .expect("send spargio keyed owner stop");
                ticket.await.expect("await spargio keyed owner stop");
            }
            sum
        };
        let join = stream_for_spawn
            .spawn_on_session(&handle, fut)
            .expect("spawn spargio keyed hotspot stream");
        stream_joins.push(join);
    }

    let mut checksum = 0u64;
    for join in stream_joins {
        checksum = checksum.wrapping_add(join.await.expect("spargio keyed hotspot stream join"));
    }
    for join in owner_joins {
        checksum = checksum.wrapping_add(join.await.expect("spargio keyed owner join"));
    }
    checksum
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
async fn spargio_echo_burst_flip_imbalance(
    handle: RuntimeHandle,
    streams: &[SpargioBenchStream],
    phases: usize,
    flip_every_phases: usize,
    hot_frames: usize,
    cold_frames: usize,
    payload_len: usize,
    window: usize,
) -> u64 {
    let stream_count = streams.len().max(1);
    let mut checksum = 0u64;
    for phase in 0..phases {
        let mut joins = Vec::with_capacity(streams.len());
        for (idx, bench_stream) in streams.iter().cloned().enumerate() {
            let stream = bench_stream.stream;
            let frames = burst_flip_frames_for_phase(
                idx,
                phase,
                stream_count,
                flip_every_phases,
                hot_frames,
                cold_frames,
            );
            let fut = async move {
                spargio_echo_windowed(
                    &stream,
                    frames,
                    payload_len,
                    window,
                    SpargioRecvMode::Multishot,
                )
                .await
            };
            let join = handle
                .spawn_stealable(fut)
                .expect("spawn spargio burst-flip stream");
            joins.push(join);
        }

        for join in joins {
            checksum = checksum.wrapping_add(join.await.expect("spargio burst-flip stream join"));
        }
    }
    checksum
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
async fn spargio_echo_fanin_barrier_micro_batches(
    handle: RuntimeHandle,
    streams: &[SpargioBenchStream],
    phases: usize,
    frames_per_stream: usize,
    payload_len: usize,
    window: usize,
) -> u64 {
    let stream_count = streams.len().max(1);
    let mut checksum = 0u64;
    for phase in 0..phases {
        let mut joins = Vec::with_capacity(stream_count);
        for (idx, bench_stream) in streams.iter().cloned().enumerate() {
            let stream = bench_stream.stream;
            let stream_for_spawn = stream.clone();
            let fut = async move {
                let value = spargio_echo_windowed(
                    &stream,
                    frames_per_stream,
                    payload_len,
                    window,
                    SpargioRecvMode::ReadExact,
                )
                .await;
                (idx, value)
            };
            let join = stream_for_spawn
                .spawn_on_session(&handle, fut)
                .expect("spawn spargio fanin-barrier stream");
            joins.push(join);
        }

        for join in joins {
            let (idx, value) = join.await.expect("spargio fanin-barrier stream join");
            checksum = checksum.wrapping_add(value);
            checksum =
                checksum.wrapping_add(pipeline_cpu_stage((phase & 0xFF) as u8, idx, phase, 64));
        }
    }
    checksum
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
async fn spargio_timer_cancel_reschedule_storm(rounds: usize, batch: usize, sleep_us: u64) -> u64 {
    let mut checksum = 0u64;
    let batch = batch.max(1);
    let sleep_for = std::time::Duration::from_micros(sleep_us.max(1));
    let tick = std::time::Duration::from_micros(1);

    for round in 0..rounds {
        for lane in 0..batch {
            let sleep_fut = spargio::sleep(sleep_for);
            futures::pin_mut!(sleep_fut);
            let immediate = futures::future::ready(());
            futures::pin_mut!(immediate);
            match select(sleep_fut, immediate).await {
                Either::Left((_, _)) => {
                    checksum = checksum.wrapping_add(1);
                }
                Either::Right((_, _)) => {
                    checksum = checksum.wrapping_add(3);
                }
            }
            checksum = checksum.wrapping_add((lane as u64) ^ (round as u64));
        }
        spargio::sleep(tick).await;
    }

    checksum
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
enum CompioNetCmd {
    EchoRtt {
        rounds: usize,
        payload: usize,
        reply: std_mpsc::Sender<u64>,
    },
    EchoWindowed {
        frames: usize,
        payload: usize,
        window: usize,
        reply: std_mpsc::Sender<u64>,
    },
    EchoImbalanced {
        heavy_frames: usize,
        light_frames: usize,
        payload: usize,
        window: usize,
        reply: std_mpsc::Sender<u64>,
    },
    EchoHotspotRotation {
        steps: usize,
        heavy_frames: usize,
        light_frames: usize,
        payload: usize,
        window: usize,
        reply: std_mpsc::Sender<u64>,
    },
    EchoPipelineHotspot {
        frames_per_stream: usize,
        payload: usize,
        window: usize,
        rotate_every: usize,
        heavy_iters: usize,
        light_iters: usize,
        reply: std_mpsc::Sender<u64>,
    },
    EchoKeyedHotspot {
        steps: usize,
        heavy_frames: usize,
        light_frames: usize,
        payload: usize,
        window: usize,
        owner_shards: usize,
        reply: std_mpsc::Sender<u64>,
    },
    EchoBurstFlipImbalance {
        phases: usize,
        flip_every_phases: usize,
        hot_frames: usize,
        cold_frames: usize,
        payload: usize,
        window: usize,
        reply: std_mpsc::Sender<u64>,
    },
    EchoFaninBarrierMicroBatches {
        phases: usize,
        frames_per_stream: usize,
        payload: usize,
        window: usize,
        reply: std_mpsc::Sender<u64>,
    },
    TimerCancelRescheduleStorm {
        rounds: usize,
        batch: usize,
        sleep_us: u64,
        reply: std_mpsc::Sender<u64>,
    },
    Shutdown {
        reply: std_mpsc::Sender<()>,
    },
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
struct CompioNetHarness {
    cmd_tx: std_mpsc::Sender<CompioNetCmd>,
    thread: Option<thread::JoinHandle<()>>,
    echo_thread: Option<thread::JoinHandle<()>>,
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
impl CompioNetHarness {
    fn new() -> Option<Self> {
        let (addr, echo_thread) =
            spawn_echo_server_with_clients("bench-net-echo-compio", IMBALANCED_STREAMS);
        let (cmd_tx, cmd_rx) = std_mpsc::channel::<CompioNetCmd>();
        let (ready_tx, ready_rx) = std_mpsc::channel::<bool>();

        let thread = thread::Builder::new()
            .name("bench-net-compio".to_owned())
            .spawn(move || {
                let runtime = match compio::runtime::Runtime::new() {
                    Ok(rt) => rt,
                    Err(_) => {
                        let _ = ready_tx.send(false);
                        return;
                    }
                };
                let _ = ready_tx.send(true);

                runtime.block_on(async move {
                    let mut streams = Vec::with_capacity(IMBALANCED_STREAMS);
                    for _ in 0..IMBALANCED_STREAMS {
                        let stream = compio::net::TcpStream::connect(addr)
                            .await
                            .expect("compio connect");
                        streams.push(stream);
                    }
                    while let Ok(cmd) = cmd_rx.recv() {
                        match cmd {
                            CompioNetCmd::EchoRtt {
                                rounds,
                                payload,
                                reply,
                            } => {
                                let stream = streams.first_mut().expect("compio primary stream");
                                let value = compio_echo_rtt(stream, rounds, payload).await;
                                let _ = reply.send(value);
                            }
                            CompioNetCmd::EchoWindowed {
                                frames,
                                payload,
                                window,
                                reply,
                            } => {
                                let stream = streams.first_mut().expect("compio primary stream");
                                let value =
                                    compio_echo_windowed(stream, frames, payload, window).await;
                                let _ = reply.send(value);
                            }
                            CompioNetCmd::EchoImbalanced {
                                heavy_frames,
                                light_frames,
                                payload,
                                window,
                                reply,
                            } => {
                                let value = compio_echo_imbalanced(
                                    &mut streams,
                                    heavy_frames,
                                    light_frames,
                                    payload,
                                    window,
                                )
                                .await;
                                let _ = reply.send(value);
                            }
                            CompioNetCmd::EchoHotspotRotation {
                                steps,
                                heavy_frames,
                                light_frames,
                                payload,
                                window,
                                reply,
                            } => {
                                let value = compio_echo_hotspot_rotation(
                                    &mut streams,
                                    steps,
                                    heavy_frames,
                                    light_frames,
                                    payload,
                                    window,
                                )
                                .await;
                                let _ = reply.send(value);
                            }
                            CompioNetCmd::EchoPipelineHotspot {
                                frames_per_stream,
                                payload,
                                window,
                                rotate_every,
                                heavy_iters,
                                light_iters,
                                reply,
                            } => {
                                let value = compio_echo_pipeline_hotspot(
                                    &mut streams,
                                    frames_per_stream,
                                    payload,
                                    window,
                                    rotate_every,
                                    heavy_iters,
                                    light_iters,
                                )
                                .await;
                                let _ = reply.send(value);
                            }
                            CompioNetCmd::EchoKeyedHotspot {
                                steps,
                                heavy_frames,
                                light_frames,
                                payload,
                                window,
                                owner_shards,
                                reply,
                            } => {
                                let value = compio_echo_keyed_hotspot_rotation(
                                    &mut streams,
                                    steps,
                                    heavy_frames,
                                    light_frames,
                                    payload,
                                    window,
                                    owner_shards,
                                )
                                .await;
                                let _ = reply.send(value);
                            }
                            CompioNetCmd::EchoBurstFlipImbalance {
                                phases,
                                flip_every_phases,
                                hot_frames,
                                cold_frames,
                                payload,
                                window,
                                reply,
                            } => {
                                let value = compio_echo_burst_flip_imbalance(
                                    &mut streams,
                                    phases,
                                    flip_every_phases,
                                    hot_frames,
                                    cold_frames,
                                    payload,
                                    window,
                                )
                                .await;
                                let _ = reply.send(value);
                            }
                            CompioNetCmd::EchoFaninBarrierMicroBatches {
                                phases,
                                frames_per_stream,
                                payload,
                                window,
                                reply,
                            } => {
                                let value = compio_echo_fanin_barrier_micro_batches(
                                    &mut streams,
                                    phases,
                                    frames_per_stream,
                                    payload,
                                    window,
                                )
                                .await;
                                let _ = reply.send(value);
                            }
                            CompioNetCmd::TimerCancelRescheduleStorm {
                                rounds,
                                batch,
                                sleep_us,
                                reply,
                            } => {
                                let value =
                                    compio_timer_cancel_reschedule_storm(rounds, batch, sleep_us)
                                        .await;
                                let _ = reply.send(value);
                            }
                            CompioNetCmd::Shutdown { reply } => {
                                let _ = reply.send(());
                                break;
                            }
                        }
                    }
                });
            })
            .expect("spawn compio net bench thread");

        if !ready_rx.recv().ok().unwrap_or(false) {
            let _ = thread.join();
            let _ = echo_thread.join();
            return None;
        }

        Some(Self {
            cmd_tx,
            thread: Some(thread),
            echo_thread: Some(echo_thread),
        })
    }

    fn echo_rtt(&mut self, rounds: usize, payload: usize) -> u64 {
        let (tx, rx) = std_mpsc::channel();
        self.cmd_tx
            .send(CompioNetCmd::EchoRtt {
                rounds,
                payload,
                reply: tx,
            })
            .expect("send compio rtt cmd");
        rx.recv().expect("compio rtt reply")
    }

    fn echo_windowed(&mut self, frames: usize, payload: usize, window: usize) -> u64 {
        let (tx, rx) = std_mpsc::channel();
        self.cmd_tx
            .send(CompioNetCmd::EchoWindowed {
                frames,
                payload,
                window,
                reply: tx,
            })
            .expect("send compio windowed cmd");
        rx.recv().expect("compio windowed reply")
    }

    fn echo_imbalanced(
        &mut self,
        heavy_frames: usize,
        light_frames: usize,
        payload: usize,
        window: usize,
    ) -> u64 {
        let (tx, rx) = std_mpsc::channel();
        self.cmd_tx
            .send(CompioNetCmd::EchoImbalanced {
                heavy_frames,
                light_frames,
                payload,
                window,
                reply: tx,
            })
            .expect("send compio imbalanced cmd");
        rx.recv().expect("compio imbalanced reply")
    }

    fn echo_hotspot_rotation(
        &mut self,
        steps: usize,
        heavy_frames: usize,
        light_frames: usize,
        payload: usize,
        window: usize,
    ) -> u64 {
        let (tx, rx) = std_mpsc::channel();
        self.cmd_tx
            .send(CompioNetCmd::EchoHotspotRotation {
                steps,
                heavy_frames,
                light_frames,
                payload,
                window,
                reply: tx,
            })
            .expect("send compio hotspot rotation cmd");
        rx.recv().expect("compio hotspot rotation reply")
    }

    fn echo_pipeline_hotspot(
        &mut self,
        frames_per_stream: usize,
        payload: usize,
        window: usize,
        rotate_every: usize,
        heavy_iters: usize,
        light_iters: usize,
    ) -> u64 {
        let (tx, rx) = std_mpsc::channel();
        self.cmd_tx
            .send(CompioNetCmd::EchoPipelineHotspot {
                frames_per_stream,
                payload,
                window,
                rotate_every,
                heavy_iters,
                light_iters,
                reply: tx,
            })
            .expect("send compio pipeline hotspot cmd");
        rx.recv().expect("compio pipeline hotspot reply")
    }

    fn echo_keyed_hotspot_rotation(
        &mut self,
        steps: usize,
        heavy_frames: usize,
        light_frames: usize,
        payload: usize,
        window: usize,
        owner_shards: usize,
    ) -> u64 {
        let (tx, rx) = std_mpsc::channel();
        self.cmd_tx
            .send(CompioNetCmd::EchoKeyedHotspot {
                steps,
                heavy_frames,
                light_frames,
                payload,
                window,
                owner_shards,
                reply: tx,
            })
            .expect("send compio keyed hotspot cmd");
        rx.recv().expect("compio keyed hotspot reply")
    }

    fn echo_burst_flip_imbalance(
        &mut self,
        phases: usize,
        flip_every_phases: usize,
        hot_frames: usize,
        cold_frames: usize,
        payload: usize,
        window: usize,
    ) -> u64 {
        let (tx, rx) = std_mpsc::channel();
        self.cmd_tx
            .send(CompioNetCmd::EchoBurstFlipImbalance {
                phases,
                flip_every_phases,
                hot_frames,
                cold_frames,
                payload,
                window,
                reply: tx,
            })
            .expect("send compio burst-flip cmd");
        rx.recv().expect("compio burst-flip reply")
    }

    fn echo_fanin_barrier_micro_batches(
        &mut self,
        phases: usize,
        frames_per_stream: usize,
        payload: usize,
        window: usize,
    ) -> u64 {
        let (tx, rx) = std_mpsc::channel();
        self.cmd_tx
            .send(CompioNetCmd::EchoFaninBarrierMicroBatches {
                phases,
                frames_per_stream,
                payload,
                window,
                reply: tx,
            })
            .expect("send compio fanin-barrier cmd");
        rx.recv().expect("compio fanin-barrier reply")
    }

    fn timer_cancel_reschedule_storm(&mut self, rounds: usize, batch: usize, sleep_us: u64) -> u64 {
        let (tx, rx) = std_mpsc::channel();
        self.cmd_tx
            .send(CompioNetCmd::TimerCancelRescheduleStorm {
                rounds,
                batch,
                sleep_us,
                reply: tx,
            })
            .expect("send compio timer-storm cmd");
        rx.recv().expect("compio timer-storm reply")
    }

    fn shutdown(&mut self) {
        let (tx, rx) = std_mpsc::channel();
        let _ = self.cmd_tx.send(CompioNetCmd::Shutdown { reply: tx });
        let _ = rx.recv();
        if let Some(join) = self.thread.take() {
            let _ = join.join();
        }
        if let Some(join) = self.echo_thread.take() {
            let _ = join.join();
        }
    }
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
impl Drop for CompioNetHarness {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
async fn compio_echo_rtt(
    stream: &mut compio::net::TcpStream,
    rounds: usize,
    payload_len: usize,
) -> u64 {
    use compio::io::{AsyncReadExt, AsyncWriteExt};

    let mut payload = vec![0u8; payload_len.max(1)];
    let mut recv = vec![0u8; payload_len.max(1)];
    let mut checksum = 0u64;

    for i in 0..rounds {
        payload[0] = i as u8;
        let out = stream.write_all(payload).await;
        out.0.expect("compio write_all");
        payload = out.1;

        let ((), returned) = stream.read_exact(recv).await.expect("compio read_exact");
        recv = returned;
        checksum = checksum.wrapping_add(u64::from(recv[0]));
    }

    checksum
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
async fn compio_echo_windowed(
    stream: &mut compio::net::TcpStream,
    frames: usize,
    payload_len: usize,
    window: usize,
) -> u64 {
    use compio::io::{AsyncReadExt, AsyncWriteExt};

    let window = window.max(1);
    let payload_len = payload_len.max(1);
    let mut recv = vec![0u8; payload_len];
    let mut tx_pool: Vec<Vec<u8>> = (0..window).map(|_| vec![0u8; payload_len]).collect();
    let mut checksum = 0u64;
    let mut next = 0usize;

    while next < frames {
        let batch = (frames - next).min(window);
        for idx in 0..batch {
            let mut payload = tx_pool.pop().unwrap_or_else(|| vec![0u8; payload_len]);
            payload[0] = (next + idx) as u8;
            let out = stream.write_all(payload).await;
            out.0.expect("compio write_all");
            tx_pool.push(out.1);
        }

        for _ in 0..batch {
            let ((), returned) = stream.read_exact(recv).await.expect("compio read_exact");
            recv = returned;
            checksum = checksum.wrapping_add(u64::from(recv[0]));
        }

        next += batch;
    }

    checksum
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
async fn compio_echo_imbalanced(
    streams: &mut Vec<compio::net::TcpStream>,
    heavy_frames: usize,
    light_frames: usize,
    payload_len: usize,
    window: usize,
) -> u64 {
    let moved = std::mem::take(streams);
    let stream_count = moved.len();
    let mut work = Vec::with_capacity(stream_count);
    for (idx, mut stream) in moved.into_iter().enumerate() {
        let frames = imbalanced_frames_for_stream(idx, heavy_frames, light_frames);
        work.push(async move {
            let value = compio_echo_windowed(&mut stream, frames, payload_len, window).await;
            (stream, value)
        });
    }

    let mut checksum = 0u64;
    let mut restored = Vec::with_capacity(stream_count);
    for (stream, value) in join_all(work).await {
        restored.push(stream);
        checksum = checksum.wrapping_add(value);
    }
    *streams = restored;
    checksum
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
async fn compio_echo_hotspot_rotation_stream(
    stream: &mut compio::net::TcpStream,
    stream_idx: usize,
    stream_count: usize,
    steps: usize,
    heavy_frames: usize,
    light_frames: usize,
    payload_len: usize,
    window: usize,
) -> u64 {
    let mut checksum = 0u64;
    for step in 0..steps {
        let frames = hotspot_rotation_frames_for_step(
            stream_idx,
            step,
            stream_count,
            heavy_frames,
            light_frames,
        );
        if frames == 0 {
            continue;
        }
        checksum =
            checksum.wrapping_add(compio_echo_windowed(stream, frames, payload_len, window).await);
    }
    checksum
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
async fn compio_echo_hotspot_rotation(
    streams: &mut Vec<compio::net::TcpStream>,
    steps: usize,
    heavy_frames: usize,
    light_frames: usize,
    payload_len: usize,
    window: usize,
) -> u64 {
    let moved = std::mem::take(streams);
    let stream_count = moved.len();
    let mut work = Vec::with_capacity(stream_count);
    for (idx, mut stream) in moved.into_iter().enumerate() {
        work.push(async move {
            let value = compio_echo_hotspot_rotation_stream(
                &mut stream,
                idx,
                stream_count,
                steps,
                heavy_frames,
                light_frames,
                payload_len,
                window,
            )
            .await;
            (stream, value)
        });
    }

    let mut checksum = 0u64;
    let mut restored = Vec::with_capacity(stream_count);
    for (stream, value) in join_all(work).await {
        restored.push(stream);
        checksum = checksum.wrapping_add(value);
    }
    *streams = restored;
    checksum
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
async fn compio_echo_pipeline_stream(
    stream: &mut compio::net::TcpStream,
    stream_idx: usize,
    stream_count: usize,
    frames: usize,
    payload_len: usize,
    window: usize,
    rotate_every: usize,
    heavy_iters: usize,
    light_iters: usize,
) -> u64 {
    use compio::io::{AsyncReadExt, AsyncWriteExt};

    let window = window.max(1);
    let payload_len = payload_len.max(1);
    let mut recv = vec![0u8; payload_len];
    let mut tx_pool: Vec<Vec<u8>> = (0..window).map(|_| vec![0u8; payload_len]).collect();
    let mut checksum = 0u64;
    let mut next = 0usize;

    while next < frames {
        let batch = (frames - next).min(window);
        for idx in 0..batch {
            let mut payload = tx_pool.pop().unwrap_or_else(|| vec![0u8; payload_len]);
            payload[0] = (next + idx) as u8;
            let out = stream.write_all(payload).await;
            out.0.expect("compio write_all");
            tx_pool.push(out.1);
        }

        for idx in 0..batch {
            let ((), returned) = stream.read_exact(recv).await.expect("compio read_exact");
            recv = returned;
            let frame_idx = next + idx;
            let cpu_iters = hotspot_iters_for_frame(
                stream_idx,
                frame_idx,
                stream_count,
                rotate_every,
                heavy_iters,
                light_iters,
            );
            checksum = checksum.wrapping_add(pipeline_cpu_stage(
                recv[0], stream_idx, frame_idx, cpu_iters,
            ));
        }

        next += batch;
    }

    checksum
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
async fn compio_echo_pipeline_hotspot(
    streams: &mut Vec<compio::net::TcpStream>,
    frames_per_stream: usize,
    payload_len: usize,
    window: usize,
    rotate_every: usize,
    heavy_iters: usize,
    light_iters: usize,
) -> u64 {
    let moved = std::mem::take(streams);
    let stream_count = moved.len();
    let mut work = Vec::with_capacity(stream_count);
    for (idx, mut stream) in moved.into_iter().enumerate() {
        work.push(async move {
            let value = compio_echo_pipeline_stream(
                &mut stream,
                idx,
                stream_count,
                frames_per_stream,
                payload_len,
                window,
                rotate_every,
                heavy_iters,
                light_iters,
            )
            .await;
            (stream, value)
        });
    }

    let mut checksum = 0u64;
    let mut restored = Vec::with_capacity(stream_count);
    for (stream, value) in join_all(work).await {
        restored.push(stream);
        checksum = checksum.wrapping_add(value);
    }
    *streams = restored;
    checksum
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
async fn compio_echo_keyed_hotspot_rotation(
    streams: &mut Vec<compio::net::TcpStream>,
    steps: usize,
    heavy_frames: usize,
    light_frames: usize,
    payload_len: usize,
    window: usize,
    owner_shards: usize,
) -> u64 {
    let owner_count = owner_shards.max(1);
    let moved = std::mem::take(streams);
    let stream_count = moved.len();
    let mut work = Vec::with_capacity(stream_count);
    for (stream_idx, mut stream) in moved.into_iter().enumerate() {
        work.push(async move {
            let mut sum = 0u64;
            let mut owner_sums = vec![0u64; owner_count];
            for step in 0..steps {
                let frames = hotspot_rotation_frames_for_step(
                    stream_idx,
                    step,
                    stream_count,
                    heavy_frames,
                    light_frames,
                );
                if frames == 0 {
                    continue;
                }
                sum = sum.wrapping_add(
                    compio_echo_windowed(&mut stream, frames, payload_len, window).await,
                );
                let owner = step % owner_count;
                owner_sums[owner] = owner_sums[owner]
                    .wrapping_add((frames.saturating_mul(KEYED_DISPATCHES_PER_FRAME)) as u64);
            }
            (stream, sum, owner_sums)
        });
    }

    let mut checksum = 0u64;
    let mut restored = Vec::with_capacity(stream_count);
    let mut owner_totals = vec![0u64; owner_count];
    for (stream, value, owner_sums) in join_all(work).await {
        restored.push(stream);
        checksum = checksum.wrapping_add(value);
        for (idx, owner_sum) in owner_sums.into_iter().enumerate() {
            owner_totals[idx] = owner_totals[idx].wrapping_add(owner_sum);
        }
    }
    *streams = restored;

    for owner_sum in owner_totals {
        checksum = checksum.wrapping_add(owner_sum);
    }
    checksum
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
async fn compio_echo_burst_flip_imbalance(
    streams: &mut Vec<compio::net::TcpStream>,
    phases: usize,
    flip_every_phases: usize,
    hot_frames: usize,
    cold_frames: usize,
    payload_len: usize,
    window: usize,
) -> u64 {
    let stream_count = streams.len();
    let mut checksum = 0u64;
    for phase in 0..phases {
        let moved = std::mem::take(streams);
        let mut work = Vec::with_capacity(stream_count);
        for (idx, mut stream) in moved.into_iter().enumerate() {
            let frames = burst_flip_frames_for_phase(
                idx,
                phase,
                stream_count,
                flip_every_phases,
                hot_frames,
                cold_frames,
            );
            work.push(async move {
                let value = compio_echo_windowed(&mut stream, frames, payload_len, window).await;
                (stream, value)
            });
        }

        let mut restored = Vec::with_capacity(stream_count);
        for (stream, value) in join_all(work).await {
            restored.push(stream);
            checksum = checksum.wrapping_add(value);
        }
        *streams = restored;
    }
    checksum
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
async fn compio_echo_fanin_barrier_micro_batches(
    streams: &mut Vec<compio::net::TcpStream>,
    phases: usize,
    frames_per_stream: usize,
    payload_len: usize,
    window: usize,
) -> u64 {
    let stream_count = streams.len();
    let mut checksum = 0u64;
    for phase in 0..phases {
        let moved = std::mem::take(streams);
        let mut work = Vec::with_capacity(stream_count);
        for (idx, mut stream) in moved.into_iter().enumerate() {
            work.push(async move {
                let value =
                    compio_echo_windowed(&mut stream, frames_per_stream, payload_len, window).await;
                (idx, stream, value)
            });
        }

        let mut restored = Vec::with_capacity(stream_count);
        for (idx, stream, value) in join_all(work).await {
            restored.push(stream);
            checksum = checksum.wrapping_add(value);
            checksum =
                checksum.wrapping_add(pipeline_cpu_stage((phase & 0xFF) as u8, idx, phase, 64));
        }
        *streams = restored;
    }
    checksum
}

#[cfg(all(feature = "uring-native", target_os = "linux"))]
async fn compio_timer_cancel_reschedule_storm(rounds: usize, batch: usize, sleep_us: u64) -> u64 {
    let mut checksum = 0u64;
    let batch = batch.max(1);
    let sleep_for = std::time::Duration::from_micros(sleep_us.max(1));
    let tick = std::time::Duration::from_micros(1);

    for round in 0..rounds {
        for lane in 0..batch {
            let sleep_fut = compio::time::sleep(sleep_for);
            futures::pin_mut!(sleep_fut);
            let immediate = futures::future::ready(());
            futures::pin_mut!(immediate);
            match select(sleep_fut, immediate).await {
                Either::Left((_, _)) => {
                    checksum = checksum.wrapping_add(1);
                }
                Either::Right((_, _)) => {
                    checksum = checksum.wrapping_add(3);
                }
            }
            checksum = checksum.wrapping_add((lane as u64) ^ (round as u64));
        }
        compio::time::sleep(tick).await;
    }

    checksum
}

#[cfg(unix)]
fn bench_net_echo_rtt(c: &mut Criterion) {
    let mut group = c.benchmark_group("net_echo_rtt_256b");
    group.throughput(Throughput::Bytes((RTT_ROUNDS * RTT_PAYLOAD) as u64));

    let mut tokio = TokioNetHarness::new();
    black_box(tokio.echo_rtt(32, RTT_PAYLOAD));
    group.bench_function("tokio_tcp_echo_qd1", |b| {
        b.iter(|| black_box(tokio.echo_rtt(RTT_ROUNDS, RTT_PAYLOAD)))
    });

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(mut spargio) = SpargioNetHarness::new() {
        black_box(spargio.echo_rtt(32, RTT_PAYLOAD));
        group.bench_function("spargio_tcp_echo_qd1", |b| {
            b.iter(|| black_box(spargio.echo_rtt(RTT_ROUNDS, RTT_PAYLOAD)))
        });
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(mut spargio) = SpargioNetHarness::new_pinned() {
        black_box(spargio.echo_rtt(32, RTT_PAYLOAD));
        group.bench_function("spargio_pinned_tcp_echo_qd1", |b| {
            b.iter(|| black_box(spargio.echo_rtt(RTT_ROUNDS, RTT_PAYLOAD)))
        });
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(mut compio) = CompioNetHarness::new() {
        black_box(compio.echo_rtt(32, RTT_PAYLOAD));
        group.bench_function("compio_tcp_echo_qd1", |b| {
            b.iter(|| black_box(compio.echo_rtt(RTT_ROUNDS, RTT_PAYLOAD)))
        });
    }

    group.finish();
}

#[cfg(unix)]
fn bench_net_stream_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("net_stream_throughput_4k_window32");
    group.throughput(Throughput::Bytes(
        (THROUGHPUT_FRAMES * THROUGHPUT_FRAME_BYTES) as u64,
    ));

    let mut tokio = TokioNetHarness::new();
    black_box(tokio.echo_windowed(128, THROUGHPUT_FRAME_BYTES, THROUGHPUT_WINDOW));
    group.bench_function("tokio_tcp_echo_window32", |b| {
        b.iter(|| {
            black_box(tokio.echo_windowed(
                THROUGHPUT_FRAMES,
                THROUGHPUT_FRAME_BYTES,
                THROUGHPUT_WINDOW,
            ))
        })
    });

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(mut spargio) = SpargioNetHarness::new() {
        black_box(spargio.echo_windowed(128, THROUGHPUT_FRAME_BYTES, THROUGHPUT_WINDOW));
        group.bench_function("spargio_tcp_echo_window32", |b| {
            b.iter(|| {
                black_box(spargio.echo_windowed(
                    THROUGHPUT_FRAMES,
                    THROUGHPUT_FRAME_BYTES,
                    THROUGHPUT_WINDOW,
                ))
            })
        });
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(mut spargio) = SpargioNetHarness::new_pinned() {
        black_box(spargio.echo_windowed(128, THROUGHPUT_FRAME_BYTES, THROUGHPUT_WINDOW));
        group.bench_function("spargio_pinned_tcp_echo_window32", |b| {
            b.iter(|| {
                black_box(spargio.echo_windowed(
                    THROUGHPUT_FRAMES,
                    THROUGHPUT_FRAME_BYTES,
                    THROUGHPUT_WINDOW,
                ))
            })
        });
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(mut compio) = CompioNetHarness::new() {
        black_box(compio.echo_windowed(128, THROUGHPUT_FRAME_BYTES, THROUGHPUT_WINDOW));
        group.bench_function("compio_tcp_echo_window32", |b| {
            b.iter(|| {
                black_box(compio.echo_windowed(
                    THROUGHPUT_FRAMES,
                    THROUGHPUT_FRAME_BYTES,
                    THROUGHPUT_WINDOW,
                ))
            })
        });
    }

    group.finish();
}

#[cfg(unix)]
fn bench_net_stream_imbalanced(c: &mut Criterion) {
    let mut group = c.benchmark_group("net_stream_imbalanced_4k_hot1_light7");
    group.throughput(Throughput::Bytes(
        (IMBALANCED_TOTAL_FRAMES * IMBALANCED_FRAME_BYTES) as u64,
    ));

    let warmup_heavy = (IMBALANCED_HEAVY_FRAMES / 8).max(1);
    let warmup_light = (IMBALANCED_LIGHT_FRAMES / 4).max(1);

    let mut tokio = TokioNetHarness::new();
    black_box(tokio.echo_imbalanced(
        warmup_heavy,
        warmup_light,
        IMBALANCED_FRAME_BYTES,
        IMBALANCED_WINDOW,
    ));
    group.bench_function("tokio_tcp_8streams_hotcold", |b| {
        b.iter(|| {
            black_box(tokio.echo_imbalanced(
                IMBALANCED_HEAVY_FRAMES,
                IMBALANCED_LIGHT_FRAMES,
                IMBALANCED_FRAME_BYTES,
                IMBALANCED_WINDOW,
            ))
        })
    });

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(mut spargio) = SpargioNetHarness::new_distributed() {
        black_box(spargio.echo_imbalanced(
            warmup_heavy,
            warmup_light,
            IMBALANCED_FRAME_BYTES,
            IMBALANCED_WINDOW,
        ));
        group.bench_function("spargio_tcp_8streams_hotcold", |b| {
            b.iter(|| {
                black_box(spargio.echo_imbalanced(
                    IMBALANCED_HEAVY_FRAMES,
                    IMBALANCED_LIGHT_FRAMES,
                    IMBALANCED_FRAME_BYTES,
                    IMBALANCED_WINDOW,
                ))
            })
        });
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(mut compio) = CompioNetHarness::new() {
        black_box(compio.echo_imbalanced(
            warmup_heavy,
            warmup_light,
            IMBALANCED_FRAME_BYTES,
            IMBALANCED_WINDOW,
        ));
        group.bench_function("compio_tcp_8streams_hotcold", |b| {
            b.iter(|| {
                black_box(compio.echo_imbalanced(
                    IMBALANCED_HEAVY_FRAMES,
                    IMBALANCED_LIGHT_FRAMES,
                    IMBALANCED_FRAME_BYTES,
                    IMBALANCED_WINDOW,
                ))
            })
        });
    }

    group.finish();
}

#[cfg(unix)]
fn bench_net_stream_hotspot_rotation(c: &mut Criterion) {
    let mut group = c.benchmark_group("net_stream_hotspot_rotation_4k");
    group.throughput(Throughput::Bytes(
        (HOTSPOT_ROTATION_TOTAL_FRAMES * HOTSPOT_ROTATION_FRAME_BYTES) as u64,
    ));

    let warmup_steps = (HOTSPOT_ROTATION_STEPS / 8).max(1);
    let warmup_heavy = (HOTSPOT_ROTATION_HEAVY_FRAMES / 4).max(1);
    let warmup_light = HOTSPOT_ROTATION_LIGHT_FRAMES.max(1);

    let mut tokio = TokioNetHarness::new();
    black_box(tokio.echo_hotspot_rotation(
        warmup_steps,
        warmup_heavy,
        warmup_light,
        HOTSPOT_ROTATION_FRAME_BYTES,
        HOTSPOT_ROTATION_WINDOW,
    ));
    group.bench_function("tokio_tcp_8streams_rotating_hotspot", |b| {
        b.iter(|| {
            black_box(tokio.echo_hotspot_rotation(
                HOTSPOT_ROTATION_STEPS,
                HOTSPOT_ROTATION_HEAVY_FRAMES,
                HOTSPOT_ROTATION_LIGHT_FRAMES,
                HOTSPOT_ROTATION_FRAME_BYTES,
                HOTSPOT_ROTATION_WINDOW,
            ))
        })
    });

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(mut spargio) = SpargioNetHarness::new_distributed() {
        black_box(spargio.echo_hotspot_rotation(
            warmup_steps,
            warmup_heavy,
            warmup_light,
            HOTSPOT_ROTATION_FRAME_BYTES,
            HOTSPOT_ROTATION_WINDOW,
        ));
        group.bench_function("spargio_tcp_8streams_rotating_hotspot", |b| {
            b.iter(|| {
                black_box(spargio.echo_hotspot_rotation(
                    HOTSPOT_ROTATION_STEPS,
                    HOTSPOT_ROTATION_HEAVY_FRAMES,
                    HOTSPOT_ROTATION_LIGHT_FRAMES,
                    HOTSPOT_ROTATION_FRAME_BYTES,
                    HOTSPOT_ROTATION_WINDOW,
                ))
            })
        });
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(mut compio) = CompioNetHarness::new() {
        black_box(compio.echo_hotspot_rotation(
            warmup_steps,
            warmup_heavy,
            warmup_light,
            HOTSPOT_ROTATION_FRAME_BYTES,
            HOTSPOT_ROTATION_WINDOW,
        ));
        group.bench_function("compio_tcp_8streams_rotating_hotspot", |b| {
            b.iter(|| {
                black_box(compio.echo_hotspot_rotation(
                    HOTSPOT_ROTATION_STEPS,
                    HOTSPOT_ROTATION_HEAVY_FRAMES,
                    HOTSPOT_ROTATION_LIGHT_FRAMES,
                    HOTSPOT_ROTATION_FRAME_BYTES,
                    HOTSPOT_ROTATION_WINDOW,
                ))
            })
        });
    }

    group.finish();
}

#[cfg(unix)]
fn bench_net_pipeline_hotspot_rotation(c: &mut Criterion) {
    let mut group = c.benchmark_group("net_pipeline_hotspot_rotation_4k_window32");
    group.throughput(Throughput::Bytes(
        (PIPELINE_TOTAL_FRAMES * PIPELINE_FRAME_BYTES) as u64,
    ));

    let warmup_frames = (PIPELINE_FRAMES_PER_STREAM / 8).max(1);
    let warmup_heavy = (PIPELINE_HEAVY_CPU_ITERS / 4).max(1);
    let warmup_light = (PIPELINE_LIGHT_CPU_ITERS / 2).max(1);

    let mut tokio = TokioNetHarness::new();
    black_box(tokio.echo_pipeline_hotspot(
        warmup_frames,
        PIPELINE_FRAME_BYTES,
        PIPELINE_WINDOW,
        PIPELINE_ROTATE_EVERY,
        warmup_heavy,
        warmup_light,
    ));
    group.bench_function("tokio_tcp_pipeline_hotspot", |b| {
        b.iter(|| {
            black_box(tokio.echo_pipeline_hotspot(
                PIPELINE_FRAMES_PER_STREAM,
                PIPELINE_FRAME_BYTES,
                PIPELINE_WINDOW,
                PIPELINE_ROTATE_EVERY,
                PIPELINE_HEAVY_CPU_ITERS,
                PIPELINE_LIGHT_CPU_ITERS,
            ))
        })
    });

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(mut spargio) = SpargioNetHarness::new_distributed() {
        black_box(spargio.echo_pipeline_hotspot(
            warmup_frames,
            PIPELINE_FRAME_BYTES,
            PIPELINE_WINDOW,
            PIPELINE_ROTATE_EVERY,
            warmup_heavy,
            warmup_light,
        ));
        group.bench_function("spargio_tcp_pipeline_hotspot", |b| {
            b.iter(|| {
                black_box(spargio.echo_pipeline_hotspot(
                    PIPELINE_FRAMES_PER_STREAM,
                    PIPELINE_FRAME_BYTES,
                    PIPELINE_WINDOW,
                    PIPELINE_ROTATE_EVERY,
                    PIPELINE_HEAVY_CPU_ITERS,
                    PIPELINE_LIGHT_CPU_ITERS,
                ))
            })
        });
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(mut compio) = CompioNetHarness::new() {
        black_box(compio.echo_pipeline_hotspot(
            warmup_frames,
            PIPELINE_FRAME_BYTES,
            PIPELINE_WINDOW,
            PIPELINE_ROTATE_EVERY,
            warmup_heavy,
            warmup_light,
        ));
        group.bench_function("compio_tcp_pipeline_hotspot", |b| {
            b.iter(|| {
                black_box(compio.echo_pipeline_hotspot(
                    PIPELINE_FRAMES_PER_STREAM,
                    PIPELINE_FRAME_BYTES,
                    PIPELINE_WINDOW,
                    PIPELINE_ROTATE_EVERY,
                    PIPELINE_HEAVY_CPU_ITERS,
                    PIPELINE_LIGHT_CPU_ITERS,
                ))
            })
        });
    }

    group.finish();
}

#[cfg(unix)]
fn bench_net_keyed_hotspot_rotation(c: &mut Criterion) {
    let mut group = c.benchmark_group("net_keyed_hotspot_rotation_4k");
    group.throughput(Throughput::Bytes(
        (KEYED_HOTSPOT_TOTAL_FRAMES * KEYED_HOTSPOT_FRAME_BYTES) as u64,
    ));

    let warmup_steps = (KEYED_HOTSPOT_STEPS / 8).max(1);
    let warmup_heavy = (KEYED_HOTSPOT_HEAVY_FRAMES / 4).max(1);
    let warmup_light = KEYED_HOTSPOT_LIGHT_FRAMES.max(1);

    let mut tokio = TokioNetHarness::new();
    black_box(tokio.echo_keyed_hotspot_rotation(
        warmup_steps,
        warmup_heavy,
        warmup_light,
        KEYED_HOTSPOT_FRAME_BYTES,
        KEYED_HOTSPOT_WINDOW,
        KEYED_HOTSPOT_OWNER_SHARDS,
    ));
    group.bench_function("tokio_tcp_keyed_router_hotspot", |b| {
        b.iter(|| {
            black_box(tokio.echo_keyed_hotspot_rotation(
                KEYED_HOTSPOT_STEPS,
                KEYED_HOTSPOT_HEAVY_FRAMES,
                KEYED_HOTSPOT_LIGHT_FRAMES,
                KEYED_HOTSPOT_FRAME_BYTES,
                KEYED_HOTSPOT_WINDOW,
                KEYED_HOTSPOT_OWNER_SHARDS,
            ))
        })
    });

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(mut spargio) = SpargioNetHarness::new_distributed() {
        black_box(spargio.echo_keyed_hotspot_rotation(
            warmup_steps,
            warmup_heavy,
            warmup_light,
            KEYED_HOTSPOT_FRAME_BYTES,
            KEYED_HOTSPOT_WINDOW,
            KEYED_HOTSPOT_OWNER_SHARDS,
        ));
        group.bench_function("spargio_tcp_keyed_router_hotspot", |b| {
            b.iter(|| {
                black_box(spargio.echo_keyed_hotspot_rotation(
                    KEYED_HOTSPOT_STEPS,
                    KEYED_HOTSPOT_HEAVY_FRAMES,
                    KEYED_HOTSPOT_LIGHT_FRAMES,
                    KEYED_HOTSPOT_FRAME_BYTES,
                    KEYED_HOTSPOT_WINDOW,
                    KEYED_HOTSPOT_OWNER_SHARDS,
                ))
            })
        });
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(mut compio) = CompioNetHarness::new() {
        black_box(compio.echo_keyed_hotspot_rotation(
            warmup_steps,
            warmup_heavy,
            warmup_light,
            KEYED_HOTSPOT_FRAME_BYTES,
            KEYED_HOTSPOT_WINDOW,
            KEYED_HOTSPOT_OWNER_SHARDS,
        ));
        group.bench_function("compio_tcp_keyed_router_hotspot", |b| {
            b.iter(|| {
                black_box(compio.echo_keyed_hotspot_rotation(
                    KEYED_HOTSPOT_STEPS,
                    KEYED_HOTSPOT_HEAVY_FRAMES,
                    KEYED_HOTSPOT_LIGHT_FRAMES,
                    KEYED_HOTSPOT_FRAME_BYTES,
                    KEYED_HOTSPOT_WINDOW,
                    KEYED_HOTSPOT_OWNER_SHARDS,
                ))
            })
        });
    }

    group.finish();
}

#[cfg(unix)]
fn bench_net_keyed_hotspot_rotation_cpu(c: &mut Criterion) {
    let mut group = c.benchmark_group("net_keyed_hotspot_rotation_4k_window64_cpu");
    group.throughput(Throughput::Bytes(
        (KEYED_CPU_HOTSPOT_TOTAL_FRAMES * KEYED_CPU_HOTSPOT_FRAME_BYTES) as u64,
    ));

    let warmup_steps = (KEYED_CPU_HOTSPOT_STEPS / 8).max(1);
    let warmup_heavy = (KEYED_CPU_HOTSPOT_HEAVY_FRAMES / 4).max(1);
    let warmup_light = KEYED_CPU_HOTSPOT_LIGHT_FRAMES.max(1);

    let mut tokio = TokioNetHarness::new();
    black_box(tokio.echo_keyed_hotspot_rotation(
        warmup_steps,
        warmup_heavy,
        warmup_light,
        KEYED_CPU_HOTSPOT_FRAME_BYTES,
        KEYED_CPU_HOTSPOT_WINDOW,
        KEYED_CPU_HOTSPOT_OWNER_SHARDS,
    ));
    black_box(keyed_hotspot_cpu_tail(
        warmup_steps,
        IMBALANCED_STREAMS,
        KEYED_CPU_HOTSPOT_HEAVY_ITERS / 2,
        (KEYED_CPU_HOTSPOT_LIGHT_ITERS / 2).max(1),
    ));
    group.bench_function("tokio_tcp_keyed_router_hotspot_window64_cpu", |b| {
        b.iter(|| {
            let io = tokio.echo_keyed_hotspot_rotation(
                KEYED_CPU_HOTSPOT_STEPS,
                KEYED_CPU_HOTSPOT_HEAVY_FRAMES,
                KEYED_CPU_HOTSPOT_LIGHT_FRAMES,
                KEYED_CPU_HOTSPOT_FRAME_BYTES,
                KEYED_CPU_HOTSPOT_WINDOW,
                KEYED_CPU_HOTSPOT_OWNER_SHARDS,
            );
            let cpu = keyed_hotspot_cpu_tail(
                KEYED_CPU_HOTSPOT_STEPS,
                IMBALANCED_STREAMS,
                KEYED_CPU_HOTSPOT_HEAVY_ITERS,
                KEYED_CPU_HOTSPOT_LIGHT_ITERS,
            );
            black_box(io.wrapping_add(cpu))
        })
    });

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(mut spargio) = SpargioNetHarness::new_distributed() {
        black_box(spargio.echo_keyed_hotspot_rotation(
            warmup_steps,
            warmup_heavy,
            warmup_light,
            KEYED_CPU_HOTSPOT_FRAME_BYTES,
            KEYED_CPU_HOTSPOT_WINDOW,
            KEYED_CPU_HOTSPOT_OWNER_SHARDS,
        ));
        black_box(keyed_hotspot_cpu_tail(
            warmup_steps,
            IMBALANCED_STREAMS,
            KEYED_CPU_HOTSPOT_HEAVY_ITERS / 2,
            (KEYED_CPU_HOTSPOT_LIGHT_ITERS / 2).max(1),
        ));
        group.bench_function("spargio_tcp_keyed_router_hotspot_window64_cpu", |b| {
            b.iter(|| {
                let io = spargio.echo_keyed_hotspot_rotation(
                    KEYED_CPU_HOTSPOT_STEPS,
                    KEYED_CPU_HOTSPOT_HEAVY_FRAMES,
                    KEYED_CPU_HOTSPOT_LIGHT_FRAMES,
                    KEYED_CPU_HOTSPOT_FRAME_BYTES,
                    KEYED_CPU_HOTSPOT_WINDOW,
                    KEYED_CPU_HOTSPOT_OWNER_SHARDS,
                );
                let cpu = keyed_hotspot_cpu_tail(
                    KEYED_CPU_HOTSPOT_STEPS,
                    IMBALANCED_STREAMS,
                    KEYED_CPU_HOTSPOT_HEAVY_ITERS,
                    KEYED_CPU_HOTSPOT_LIGHT_ITERS,
                );
                black_box(io.wrapping_add(cpu))
            })
        });
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(mut compio) = CompioNetHarness::new() {
        black_box(compio.echo_keyed_hotspot_rotation(
            warmup_steps,
            warmup_heavy,
            warmup_light,
            KEYED_CPU_HOTSPOT_FRAME_BYTES,
            KEYED_CPU_HOTSPOT_WINDOW,
            KEYED_CPU_HOTSPOT_OWNER_SHARDS,
        ));
        black_box(keyed_hotspot_cpu_tail(
            warmup_steps,
            IMBALANCED_STREAMS,
            KEYED_CPU_HOTSPOT_HEAVY_ITERS / 2,
            (KEYED_CPU_HOTSPOT_LIGHT_ITERS / 2).max(1),
        ));
        group.bench_function("compio_tcp_keyed_router_hotspot_window64_cpu", |b| {
            b.iter(|| {
                let io = compio.echo_keyed_hotspot_rotation(
                    KEYED_CPU_HOTSPOT_STEPS,
                    KEYED_CPU_HOTSPOT_HEAVY_FRAMES,
                    KEYED_CPU_HOTSPOT_LIGHT_FRAMES,
                    KEYED_CPU_HOTSPOT_FRAME_BYTES,
                    KEYED_CPU_HOTSPOT_WINDOW,
                    KEYED_CPU_HOTSPOT_OWNER_SHARDS,
                );
                let cpu = keyed_hotspot_cpu_tail(
                    KEYED_CPU_HOTSPOT_STEPS,
                    IMBALANCED_STREAMS,
                    KEYED_CPU_HOTSPOT_HEAVY_ITERS,
                    KEYED_CPU_HOTSPOT_LIGHT_ITERS,
                );
                black_box(io.wrapping_add(cpu))
            })
        });
    }

    group.finish();
}

#[cfg(unix)]
fn bench_ingress_dispatch_to_workers_rr_ack(c: &mut Criterion) {
    let mut group = c.benchmark_group("ingress_dispatch_to_workers_rr_256b_ack");
    group.throughput(Throughput::Bytes(
        (INGRESS_RR_TOTAL_FRAMES * INGRESS_RR_PAYLOAD_BYTES) as u64,
    ));

    let warmup_steps = (INGRESS_RR_STEPS / 8).max(1);

    let mut tokio = TokioNetHarness::new();
    black_box(tokio.echo_hotspot_rotation(
        warmup_steps,
        1,
        0,
        INGRESS_RR_PAYLOAD_BYTES,
        INGRESS_RR_WINDOW,
    ));
    group.bench_function("tokio_ingress_dispatch_rr_ack", |b| {
        b.iter(|| {
            black_box(tokio.echo_hotspot_rotation(
                INGRESS_RR_STEPS,
                1,
                0,
                INGRESS_RR_PAYLOAD_BYTES,
                INGRESS_RR_WINDOW,
            ))
        })
    });

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(mut spargio) = SpargioNetHarness::new_distributed() {
        black_box(spargio.echo_hotspot_rotation(
            warmup_steps,
            1,
            0,
            INGRESS_RR_PAYLOAD_BYTES,
            INGRESS_RR_WINDOW,
        ));
        group.bench_function("spargio_ingress_dispatch_rr_ack", |b| {
            b.iter(|| {
                black_box(spargio.echo_hotspot_rotation(
                    INGRESS_RR_STEPS,
                    1,
                    0,
                    INGRESS_RR_PAYLOAD_BYTES,
                    INGRESS_RR_WINDOW,
                ))
            })
        });
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(mut compio) = CompioNetHarness::new() {
        black_box(compio.echo_hotspot_rotation(
            warmup_steps,
            1,
            0,
            INGRESS_RR_PAYLOAD_BYTES,
            INGRESS_RR_WINDOW,
        ));
        group.bench_function("compio_ingress_dispatch_rr_ack", |b| {
            b.iter(|| {
                black_box(compio.echo_hotspot_rotation(
                    INGRESS_RR_STEPS,
                    1,
                    0,
                    INGRESS_RR_PAYLOAD_BYTES,
                    INGRESS_RR_WINDOW,
                ))
            })
        });
    }

    group.finish();
}

#[cfg(unix)]
fn bench_fs_net_microservice(c: &mut Criterion) {
    let mut group = c.benchmark_group("fs_net_microservice_4k_read_then_256b_reply_qd1");
    group.throughput(Throughput::Bytes(
        (FS_NET_MICRO_ROUNDS * (FS_NET_MICRO_READ_BYTES + FS_NET_MICRO_REPLY_BYTES)) as u64,
    ));

    let fixture = FsBenchFixture::new(FS_NET_MICRO_READ_BYTES, FS_NET_MICRO_FILE_BLOCKS);
    let warmup_rounds = (FS_NET_MICRO_ROUNDS / 8).max(1);

    let mut tokio = TokioNetHarness::new();
    black_box(
        fixture
            .read_qd1(warmup_rounds)
            .wrapping_add(tokio.echo_rtt(warmup_rounds, FS_NET_MICRO_REPLY_BYTES)),
    );
    group.bench_function("tokio_fs_then_net_qd1", |b| {
        b.iter(|| {
            let fs_sum = fixture.read_qd1(FS_NET_MICRO_ROUNDS);
            let net_sum = tokio.echo_rtt(FS_NET_MICRO_ROUNDS, FS_NET_MICRO_REPLY_BYTES);
            black_box(fs_sum.wrapping_add(net_sum))
        })
    });

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(mut spargio) = SpargioNetHarness::new() {
        black_box(
            fixture
                .read_qd1(warmup_rounds)
                .wrapping_add(spargio.echo_rtt(warmup_rounds, FS_NET_MICRO_REPLY_BYTES)),
        );
        group.bench_function("spargio_fs_then_net_qd1", |b| {
            b.iter(|| {
                let fs_sum = fixture.read_qd1(FS_NET_MICRO_ROUNDS);
                let net_sum = spargio.echo_rtt(FS_NET_MICRO_ROUNDS, FS_NET_MICRO_REPLY_BYTES);
                black_box(fs_sum.wrapping_add(net_sum))
            })
        });
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(mut compio) = CompioNetHarness::new() {
        black_box(
            fixture
                .read_qd1(warmup_rounds)
                .wrapping_add(compio.echo_rtt(warmup_rounds, FS_NET_MICRO_REPLY_BYTES)),
        );
        group.bench_function("compio_fs_then_net_qd1", |b| {
            b.iter(|| {
                let fs_sum = fixture.read_qd1(FS_NET_MICRO_ROUNDS);
                let net_sum = compio.echo_rtt(FS_NET_MICRO_ROUNDS, FS_NET_MICRO_REPLY_BYTES);
                black_box(fs_sum.wrapping_add(net_sum))
            })
        });
    }

    group.finish();
}

#[cfg(unix)]
fn bench_fs_net_microservice_deadline_dispatch(c: &mut Criterion) {
    let mut group = c.benchmark_group("fs_net_microservice_deadline_dispatch_4k_read_256b_reply");
    group.throughput(Throughput::Bytes(FS_NET_DEADLINE_TOTAL_BYTES as u64));

    let fixture = FsBenchFixture::new(FS_NET_MICRO_READ_BYTES, FS_NET_DEADLINE_FILE_BLOCKS);
    let warmup_epochs = (FS_NET_DEADLINE_EPOCHS / 4).max(1);

    let mut tokio = TokioNetHarness::new();
    black_box(run_fs_net_deadline_loop(
        &mut tokio,
        &fixture,
        warmup_epochs,
        FS_NET_DEADLINE_READS_PER_EPOCH,
        |h| {
            h.timer_cancel_reschedule_storm(
                FS_NET_DEADLINE_TIMER_ROUNDS,
                FS_NET_DEADLINE_TIMER_BATCH,
                FS_NET_DEADLINE_TIMER_SLEEP_US,
            )
        },
        |h| {
            h.echo_hotspot_rotation(
                FS_NET_DEADLINE_DISPATCH_STEPS_PER_EPOCH,
                1,
                0,
                FS_NET_DEADLINE_REPLY_BYTES,
                FS_NET_DEADLINE_WINDOW,
            )
        },
    ));
    group.bench_function("tokio_fs_deadline_dispatch", |b| {
        b.iter(|| {
            black_box(run_fs_net_deadline_loop(
                &mut tokio,
                &fixture,
                FS_NET_DEADLINE_EPOCHS,
                FS_NET_DEADLINE_READS_PER_EPOCH,
                |h| {
                    h.timer_cancel_reschedule_storm(
                        FS_NET_DEADLINE_TIMER_ROUNDS,
                        FS_NET_DEADLINE_TIMER_BATCH,
                        FS_NET_DEADLINE_TIMER_SLEEP_US,
                    )
                },
                |h| {
                    h.echo_hotspot_rotation(
                        FS_NET_DEADLINE_DISPATCH_STEPS_PER_EPOCH,
                        1,
                        0,
                        FS_NET_DEADLINE_REPLY_BYTES,
                        FS_NET_DEADLINE_WINDOW,
                    )
                },
            ))
        })
    });

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(mut spargio) = SpargioNetHarness::new_distributed() {
        black_box(run_fs_net_deadline_loop(
            &mut spargio,
            &fixture,
            warmup_epochs,
            FS_NET_DEADLINE_READS_PER_EPOCH,
            |h| {
                h.timer_cancel_reschedule_storm(
                    FS_NET_DEADLINE_TIMER_ROUNDS,
                    FS_NET_DEADLINE_TIMER_BATCH,
                    FS_NET_DEADLINE_TIMER_SLEEP_US,
                )
            },
            |h| {
                h.echo_hotspot_rotation(
                    FS_NET_DEADLINE_DISPATCH_STEPS_PER_EPOCH,
                    1,
                    0,
                    FS_NET_DEADLINE_REPLY_BYTES,
                    FS_NET_DEADLINE_WINDOW,
                )
            },
        ));
        group.bench_function("spargio_fs_deadline_dispatch", |b| {
            b.iter(|| {
                black_box(run_fs_net_deadline_loop(
                    &mut spargio,
                    &fixture,
                    FS_NET_DEADLINE_EPOCHS,
                    FS_NET_DEADLINE_READS_PER_EPOCH,
                    |h| {
                        h.timer_cancel_reschedule_storm(
                            FS_NET_DEADLINE_TIMER_ROUNDS,
                            FS_NET_DEADLINE_TIMER_BATCH,
                            FS_NET_DEADLINE_TIMER_SLEEP_US,
                        )
                    },
                    |h| {
                        h.echo_hotspot_rotation(
                            FS_NET_DEADLINE_DISPATCH_STEPS_PER_EPOCH,
                            1,
                            0,
                            FS_NET_DEADLINE_REPLY_BYTES,
                            FS_NET_DEADLINE_WINDOW,
                        )
                    },
                ))
            })
        });
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(mut compio) = CompioNetHarness::new() {
        black_box(run_fs_net_deadline_loop(
            &mut compio,
            &fixture,
            warmup_epochs,
            FS_NET_DEADLINE_READS_PER_EPOCH,
            |h| {
                h.timer_cancel_reschedule_storm(
                    FS_NET_DEADLINE_TIMER_ROUNDS,
                    FS_NET_DEADLINE_TIMER_BATCH,
                    FS_NET_DEADLINE_TIMER_SLEEP_US,
                )
            },
            |h| {
                h.echo_hotspot_rotation(
                    FS_NET_DEADLINE_DISPATCH_STEPS_PER_EPOCH,
                    1,
                    0,
                    FS_NET_DEADLINE_REPLY_BYTES,
                    FS_NET_DEADLINE_WINDOW,
                )
            },
        ));
        group.bench_function("compio_fs_deadline_dispatch", |b| {
            b.iter(|| {
                black_box(run_fs_net_deadline_loop(
                    &mut compio,
                    &fixture,
                    FS_NET_DEADLINE_EPOCHS,
                    FS_NET_DEADLINE_READS_PER_EPOCH,
                    |h| {
                        h.timer_cancel_reschedule_storm(
                            FS_NET_DEADLINE_TIMER_ROUNDS,
                            FS_NET_DEADLINE_TIMER_BATCH,
                            FS_NET_DEADLINE_TIMER_SLEEP_US,
                        )
                    },
                    |h| {
                        h.echo_hotspot_rotation(
                            FS_NET_DEADLINE_DISPATCH_STEPS_PER_EPOCH,
                            1,
                            0,
                            FS_NET_DEADLINE_REPLY_BYTES,
                            FS_NET_DEADLINE_WINDOW,
                        )
                    },
                ))
            })
        });
    }

    group.finish();
}

#[cfg(unix)]
fn bench_net_echo_rtt_deadline_routing(c: &mut Criterion) {
    let mut group = c.benchmark_group("net_echo_rtt_deadline_routing_256b");
    group.throughput(Throughput::Bytes(ECHO_DEADLINE_TOTAL_BYTES as u64));

    let warmup_epochs = (ECHO_DEADLINE_EPOCHS / 4).max(1);

    let mut tokio = TokioNetHarness::new();
    black_box(run_mixed_control_data_loop(
        &mut tokio,
        warmup_epochs,
        |h| {
            h.echo_hotspot_rotation(
                ECHO_DEADLINE_ROUTE_STEPS,
                1,
                0,
                ECHO_DEADLINE_PAYLOAD,
                ECHO_DEADLINE_WINDOW,
            )
            .wrapping_add(h.echo_rtt(ECHO_DEADLINE_RTT_ROUNDS, ECHO_DEADLINE_PAYLOAD))
        },
        |h| {
            h.timer_cancel_reschedule_storm(
                ECHO_DEADLINE_TIMER_ROUNDS,
                ECHO_DEADLINE_TIMER_BATCH,
                ECHO_DEADLINE_TIMER_SLEEP_US,
            )
        },
    ));
    group.bench_function("tokio_echo_rtt_deadline_routing", |b| {
        b.iter(|| {
            black_box(run_mixed_control_data_loop(
                &mut tokio,
                ECHO_DEADLINE_EPOCHS,
                |h| {
                    h.echo_hotspot_rotation(
                        ECHO_DEADLINE_ROUTE_STEPS,
                        1,
                        0,
                        ECHO_DEADLINE_PAYLOAD,
                        ECHO_DEADLINE_WINDOW,
                    )
                    .wrapping_add(h.echo_rtt(ECHO_DEADLINE_RTT_ROUNDS, ECHO_DEADLINE_PAYLOAD))
                },
                |h| {
                    h.timer_cancel_reschedule_storm(
                        ECHO_DEADLINE_TIMER_ROUNDS,
                        ECHO_DEADLINE_TIMER_BATCH,
                        ECHO_DEADLINE_TIMER_SLEEP_US,
                    )
                },
            ))
        })
    });

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(mut spargio) = SpargioNetHarness::new_distributed() {
        black_box(run_mixed_control_data_loop(
            &mut spargio,
            warmup_epochs,
            |h| {
                h.echo_hotspot_rotation(
                    ECHO_DEADLINE_ROUTE_STEPS,
                    1,
                    0,
                    ECHO_DEADLINE_PAYLOAD,
                    ECHO_DEADLINE_WINDOW,
                )
                .wrapping_add(h.echo_rtt(ECHO_DEADLINE_RTT_ROUNDS, ECHO_DEADLINE_PAYLOAD))
            },
            |h| {
                h.timer_cancel_reschedule_storm(
                    ECHO_DEADLINE_TIMER_ROUNDS,
                    ECHO_DEADLINE_TIMER_BATCH,
                    ECHO_DEADLINE_TIMER_SLEEP_US,
                )
            },
        ));
        group.bench_function("spargio_echo_rtt_deadline_routing", |b| {
            b.iter(|| {
                black_box(run_mixed_control_data_loop(
                    &mut spargio,
                    ECHO_DEADLINE_EPOCHS,
                    |h| {
                        h.echo_hotspot_rotation(
                            ECHO_DEADLINE_ROUTE_STEPS,
                            1,
                            0,
                            ECHO_DEADLINE_PAYLOAD,
                            ECHO_DEADLINE_WINDOW,
                        )
                        .wrapping_add(h.echo_rtt(ECHO_DEADLINE_RTT_ROUNDS, ECHO_DEADLINE_PAYLOAD))
                    },
                    |h| {
                        h.timer_cancel_reschedule_storm(
                            ECHO_DEADLINE_TIMER_ROUNDS,
                            ECHO_DEADLINE_TIMER_BATCH,
                            ECHO_DEADLINE_TIMER_SLEEP_US,
                        )
                    },
                ))
            })
        });
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(mut compio) = CompioNetHarness::new() {
        black_box(run_mixed_control_data_loop(
            &mut compio,
            warmup_epochs,
            |h| {
                h.echo_hotspot_rotation(
                    ECHO_DEADLINE_ROUTE_STEPS,
                    1,
                    0,
                    ECHO_DEADLINE_PAYLOAD,
                    ECHO_DEADLINE_WINDOW,
                )
                .wrapping_add(h.echo_rtt(ECHO_DEADLINE_RTT_ROUNDS, ECHO_DEADLINE_PAYLOAD))
            },
            |h| {
                h.timer_cancel_reschedule_storm(
                    ECHO_DEADLINE_TIMER_ROUNDS,
                    ECHO_DEADLINE_TIMER_BATCH,
                    ECHO_DEADLINE_TIMER_SLEEP_US,
                )
            },
        ));
        group.bench_function("compio_echo_rtt_deadline_routing", |b| {
            b.iter(|| {
                black_box(run_mixed_control_data_loop(
                    &mut compio,
                    ECHO_DEADLINE_EPOCHS,
                    |h| {
                        h.echo_hotspot_rotation(
                            ECHO_DEADLINE_ROUTE_STEPS,
                            1,
                            0,
                            ECHO_DEADLINE_PAYLOAD,
                            ECHO_DEADLINE_WINDOW,
                        )
                        .wrapping_add(h.echo_rtt(ECHO_DEADLINE_RTT_ROUNDS, ECHO_DEADLINE_PAYLOAD))
                    },
                    |h| {
                        h.timer_cancel_reschedule_storm(
                            ECHO_DEADLINE_TIMER_ROUNDS,
                            ECHO_DEADLINE_TIMER_BATCH,
                            ECHO_DEADLINE_TIMER_SLEEP_US,
                        )
                    },
                ))
            })
        });
    }

    group.finish();
}

#[cfg(unix)]
fn bench_net_stream_multitenant(c: &mut Criterion) {
    let mut group = c.benchmark_group("net_stream_multitenant_4k_window8");
    group.throughput(Throughput::Bytes(
        (MULTITENANT_TOTAL_FRAMES * MULTITENANT_PAYLOAD) as u64,
    ));

    let warmup_steps = (MULTITENANT_STEPS / 8).max(1);
    let warmup_heavy = (MULTITENANT_HEAVY_FRAMES / 4).max(1);
    let warmup_light = MULTITENANT_LIGHT_FRAMES.max(1);

    let mut tokio = TokioNetHarness::new();
    black_box(tokio.echo_keyed_hotspot_rotation(
        warmup_steps,
        warmup_heavy,
        warmup_light,
        MULTITENANT_PAYLOAD,
        MULTITENANT_WINDOW,
        MULTITENANT_OWNER_SHARDS,
    ));
    group.bench_function("tokio_multitenant_stream", |b| {
        b.iter(|| {
            black_box(tokio.echo_keyed_hotspot_rotation(
                MULTITENANT_STEPS,
                MULTITENANT_HEAVY_FRAMES,
                MULTITENANT_LIGHT_FRAMES,
                MULTITENANT_PAYLOAD,
                MULTITENANT_WINDOW,
                MULTITENANT_OWNER_SHARDS,
            ))
        })
    });

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(mut spargio) = SpargioNetHarness::new_distributed() {
        black_box(spargio.echo_keyed_hotspot_rotation(
            warmup_steps,
            warmup_heavy,
            warmup_light,
            MULTITENANT_PAYLOAD,
            MULTITENANT_WINDOW,
            MULTITENANT_OWNER_SHARDS,
        ));
        group.bench_function("spargio_multitenant_stream", |b| {
            b.iter(|| {
                black_box(spargio.echo_keyed_hotspot_rotation(
                    MULTITENANT_STEPS,
                    MULTITENANT_HEAVY_FRAMES,
                    MULTITENANT_LIGHT_FRAMES,
                    MULTITENANT_PAYLOAD,
                    MULTITENANT_WINDOW,
                    MULTITENANT_OWNER_SHARDS,
                ))
            })
        });
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(mut compio) = CompioNetHarness::new() {
        black_box(compio.echo_keyed_hotspot_rotation(
            warmup_steps,
            warmup_heavy,
            warmup_light,
            MULTITENANT_PAYLOAD,
            MULTITENANT_WINDOW,
            MULTITENANT_OWNER_SHARDS,
        ));
        group.bench_function("compio_multitenant_stream", |b| {
            b.iter(|| {
                black_box(compio.echo_keyed_hotspot_rotation(
                    MULTITENANT_STEPS,
                    MULTITENANT_HEAVY_FRAMES,
                    MULTITENANT_LIGHT_FRAMES,
                    MULTITENANT_PAYLOAD,
                    MULTITENANT_WINDOW,
                    MULTITENANT_OWNER_SHARDS,
                ))
            })
        });
    }

    group.finish();
}

#[cfg(unix)]
fn bench_net_stream_hotflip(c: &mut Criterion) {
    let mut group = c.benchmark_group("net_stream_hotflip_4k");
    group.throughput(Throughput::Bytes(
        (HOTFLIP_TOTAL_FRAMES * HOTFLIP_PAYLOAD) as u64,
    ));

    let warmup_phases = (HOTFLIP_PHASES / 8).max(1);
    let warmup_hot = (HOTFLIP_HOT_FRAMES / 4).max(1);
    let warmup_cold = HOTFLIP_COLD_FRAMES.max(1);

    let mut tokio = TokioNetHarness::new();
    black_box(tokio.echo_burst_flip_imbalance(
        warmup_phases,
        HOTFLIP_FLIP_EVERY_PHASES,
        warmup_hot,
        warmup_cold,
        HOTFLIP_PAYLOAD,
        HOTFLIP_WINDOW,
    ));
    group.bench_function("tokio_stream_hotflip", |b| {
        b.iter(|| {
            black_box(tokio.echo_burst_flip_imbalance(
                HOTFLIP_PHASES,
                HOTFLIP_FLIP_EVERY_PHASES,
                HOTFLIP_HOT_FRAMES,
                HOTFLIP_COLD_FRAMES,
                HOTFLIP_PAYLOAD,
                HOTFLIP_WINDOW,
            ))
        })
    });

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(mut spargio) = SpargioNetHarness::new_distributed() {
        black_box(spargio.echo_burst_flip_imbalance(
            warmup_phases,
            HOTFLIP_FLIP_EVERY_PHASES,
            warmup_hot,
            warmup_cold,
            HOTFLIP_PAYLOAD,
            HOTFLIP_WINDOW,
        ));
        group.bench_function("spargio_stream_hotflip", |b| {
            b.iter(|| {
                black_box(spargio.echo_burst_flip_imbalance(
                    HOTFLIP_PHASES,
                    HOTFLIP_FLIP_EVERY_PHASES,
                    HOTFLIP_HOT_FRAMES,
                    HOTFLIP_COLD_FRAMES,
                    HOTFLIP_PAYLOAD,
                    HOTFLIP_WINDOW,
                ))
            })
        });
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(mut compio) = CompioNetHarness::new() {
        black_box(compio.echo_burst_flip_imbalance(
            warmup_phases,
            HOTFLIP_FLIP_EVERY_PHASES,
            warmup_hot,
            warmup_cold,
            HOTFLIP_PAYLOAD,
            HOTFLIP_WINDOW,
        ));
        group.bench_function("compio_stream_hotflip", |b| {
            b.iter(|| {
                black_box(compio.echo_burst_flip_imbalance(
                    HOTFLIP_PHASES,
                    HOTFLIP_FLIP_EVERY_PHASES,
                    HOTFLIP_HOT_FRAMES,
                    HOTFLIP_COLD_FRAMES,
                    HOTFLIP_PAYLOAD,
                    HOTFLIP_WINDOW,
                ))
            })
        });
    }

    group.finish();
}

#[cfg(unix)]
fn bench_net_pipeline_barrier(c: &mut Criterion) {
    let mut group = c.benchmark_group("net_pipeline_barrier_4k_window4");
    group.throughput(Throughput::Bytes(
        (PIPELINE_BARRIER_TOTAL_FRAMES * PIPELINE_BARRIER_PAYLOAD) as u64,
    ));

    let warmup_phases = (PIPELINE_BARRIER_PHASES / 8).max(1);
    let warmup_frames = (PIPELINE_BARRIER_FRAMES_PER_STREAM / 2).max(1);

    let mut tokio = TokioNetHarness::new();
    black_box(tokio.echo_fanin_barrier_micro_batches(
        warmup_phases,
        warmup_frames,
        PIPELINE_BARRIER_PAYLOAD,
        PIPELINE_BARRIER_WINDOW,
    ));
    group.bench_function("tokio_pipeline_barrier", |b| {
        b.iter(|| {
            black_box(tokio.echo_fanin_barrier_micro_batches(
                PIPELINE_BARRIER_PHASES,
                PIPELINE_BARRIER_FRAMES_PER_STREAM,
                PIPELINE_BARRIER_PAYLOAD,
                PIPELINE_BARRIER_WINDOW,
            ))
        })
    });

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(mut spargio) = SpargioNetHarness::new_distributed() {
        black_box(spargio.echo_fanin_barrier_micro_batches(
            warmup_phases,
            warmup_frames,
            PIPELINE_BARRIER_PAYLOAD,
            PIPELINE_BARRIER_WINDOW,
        ));
        group.bench_function("spargio_pipeline_barrier", |b| {
            b.iter(|| {
                black_box(spargio.echo_fanin_barrier_micro_batches(
                    PIPELINE_BARRIER_PHASES,
                    PIPELINE_BARRIER_FRAMES_PER_STREAM,
                    PIPELINE_BARRIER_PAYLOAD,
                    PIPELINE_BARRIER_WINDOW,
                ))
            })
        });
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(mut compio) = CompioNetHarness::new() {
        black_box(compio.echo_fanin_barrier_micro_batches(
            warmup_phases,
            warmup_frames,
            PIPELINE_BARRIER_PAYLOAD,
            PIPELINE_BARRIER_WINDOW,
        ));
        group.bench_function("compio_pipeline_barrier", |b| {
            b.iter(|| {
                black_box(compio.echo_fanin_barrier_micro_batches(
                    PIPELINE_BARRIER_PHASES,
                    PIPELINE_BARRIER_FRAMES_PER_STREAM,
                    PIPELINE_BARRIER_PAYLOAD,
                    PIPELINE_BARRIER_WINDOW,
                ))
            })
        });
    }

    group.finish();
}

#[cfg(unix)]
fn bench_keyed_router_with_session_owner_spillover(c: &mut Criterion) {
    let mut group = c.benchmark_group("keyed_router_with_session_owner_spillover_4k");
    group.throughput(Throughput::Bytes(
        (KEYED_SPILLOVER_TOTAL_FRAMES * KEYED_SPILLOVER_PAYLOAD) as u64,
    ));

    let warmup_steps = (KEYED_SPILLOVER_STEPS / 8).max(1);
    let warmup_heavy = (KEYED_SPILLOVER_HEAVY_FRAMES / 4).max(1);
    let warmup_light = KEYED_SPILLOVER_LIGHT_FRAMES.max(1);

    let mut tokio = TokioNetHarness::new();
    black_box(tokio.echo_keyed_hotspot_rotation(
        warmup_steps,
        warmup_heavy,
        warmup_light,
        KEYED_SPILLOVER_PAYLOAD,
        KEYED_SPILLOVER_WINDOW,
        KEYED_SPILLOVER_OWNER_SHARDS,
    ));
    group.bench_function("tokio_keyed_owner_spillover", |b| {
        b.iter(|| {
            black_box(tokio.echo_keyed_hotspot_rotation(
                KEYED_SPILLOVER_STEPS,
                KEYED_SPILLOVER_HEAVY_FRAMES,
                KEYED_SPILLOVER_LIGHT_FRAMES,
                KEYED_SPILLOVER_PAYLOAD,
                KEYED_SPILLOVER_WINDOW,
                KEYED_SPILLOVER_OWNER_SHARDS,
            ))
        })
    });

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(mut spargio) = SpargioNetHarness::new_distributed() {
        black_box(spargio.echo_keyed_hotspot_rotation(
            warmup_steps,
            warmup_heavy,
            warmup_light,
            KEYED_SPILLOVER_PAYLOAD,
            KEYED_SPILLOVER_WINDOW,
            KEYED_SPILLOVER_OWNER_SHARDS,
        ));
        group.bench_function("spargio_keyed_owner_spillover", |b| {
            b.iter(|| {
                black_box(spargio.echo_keyed_hotspot_rotation(
                    KEYED_SPILLOVER_STEPS,
                    KEYED_SPILLOVER_HEAVY_FRAMES,
                    KEYED_SPILLOVER_LIGHT_FRAMES,
                    KEYED_SPILLOVER_PAYLOAD,
                    KEYED_SPILLOVER_WINDOW,
                    KEYED_SPILLOVER_OWNER_SHARDS,
                ))
            })
        });
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(mut compio) = CompioNetHarness::new() {
        black_box(compio.echo_keyed_hotspot_rotation(
            warmup_steps,
            warmup_heavy,
            warmup_light,
            KEYED_SPILLOVER_PAYLOAD,
            KEYED_SPILLOVER_WINDOW,
            KEYED_SPILLOVER_OWNER_SHARDS,
        ));
        group.bench_function("compio_keyed_owner_spillover", |b| {
            b.iter(|| {
                black_box(compio.echo_keyed_hotspot_rotation(
                    KEYED_SPILLOVER_STEPS,
                    KEYED_SPILLOVER_HEAVY_FRAMES,
                    KEYED_SPILLOVER_LIGHT_FRAMES,
                    KEYED_SPILLOVER_PAYLOAD,
                    KEYED_SPILLOVER_WINDOW,
                    KEYED_SPILLOVER_OWNER_SHARDS,
                ))
            })
        });
    }

    group.finish();
}

#[cfg(unix)]
fn bench_fs_metadata_then_reply_qd1(c: &mut Criterion) {
    let mut group = c.benchmark_group("fs_metadata_then_reply_qd1");
    group.throughput(Throughput::Bytes(FS_META_REPLY_TOTAL_BYTES as u64));

    let fixture = FsBenchFixture::new(FS_NET_MICRO_READ_BYTES, FS_META_REPLY_FILE_BLOCKS);
    let warmup_epochs = (FS_META_REPLY_EPOCHS / 4).max(1);

    let mut tokio = TokioNetHarness::new();
    black_box(run_mixed_control_data_loop(
        &mut tokio,
        warmup_epochs,
        |h| {
            fixture
                .read_qd1(FS_META_REPLY_OPS_PER_EPOCH)
                .wrapping_add(h.echo_rtt(FS_META_REPLY_OPS_PER_EPOCH, FS_META_REPLY_REPLY_BYTES))
        },
        |h| {
            fixture
                .metadata_qd1(FS_META_REPLY_OPS_PER_EPOCH)
                .wrapping_add(h.timer_cancel_reschedule_storm(
                    FS_META_REPLY_TIMER_ROUNDS,
                    FS_META_REPLY_TIMER_BATCH,
                    FS_META_REPLY_TIMER_SLEEP_US,
                ))
        },
    ));
    group.bench_function("tokio_fs_metadata_then_reply_qd1", |b| {
        b.iter(|| {
            black_box(run_mixed_control_data_loop(
                &mut tokio,
                FS_META_REPLY_EPOCHS,
                |h| {
                    fixture.read_qd1(FS_META_REPLY_OPS_PER_EPOCH).wrapping_add(
                        h.echo_rtt(FS_META_REPLY_OPS_PER_EPOCH, FS_META_REPLY_REPLY_BYTES),
                    )
                },
                |h| {
                    fixture
                        .metadata_qd1(FS_META_REPLY_OPS_PER_EPOCH)
                        .wrapping_add(h.timer_cancel_reschedule_storm(
                            FS_META_REPLY_TIMER_ROUNDS,
                            FS_META_REPLY_TIMER_BATCH,
                            FS_META_REPLY_TIMER_SLEEP_US,
                        ))
                },
            ))
        })
    });

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(mut spargio) = SpargioNetHarness::new() {
        black_box(run_mixed_control_data_loop(
            &mut spargio,
            warmup_epochs,
            |h| {
                fixture.read_qd1(FS_META_REPLY_OPS_PER_EPOCH).wrapping_add(
                    h.echo_rtt(FS_META_REPLY_OPS_PER_EPOCH, FS_META_REPLY_REPLY_BYTES),
                )
            },
            |h| {
                fixture
                    .metadata_qd1(FS_META_REPLY_OPS_PER_EPOCH)
                    .wrapping_add(h.timer_cancel_reschedule_storm(
                        FS_META_REPLY_TIMER_ROUNDS,
                        FS_META_REPLY_TIMER_BATCH,
                        FS_META_REPLY_TIMER_SLEEP_US,
                    ))
            },
        ));
        group.bench_function("spargio_fs_metadata_then_reply_qd1", |b| {
            b.iter(|| {
                black_box(run_mixed_control_data_loop(
                    &mut spargio,
                    FS_META_REPLY_EPOCHS,
                    |h| {
                        fixture.read_qd1(FS_META_REPLY_OPS_PER_EPOCH).wrapping_add(
                            h.echo_rtt(FS_META_REPLY_OPS_PER_EPOCH, FS_META_REPLY_REPLY_BYTES),
                        )
                    },
                    |h| {
                        fixture
                            .metadata_qd1(FS_META_REPLY_OPS_PER_EPOCH)
                            .wrapping_add(h.timer_cancel_reschedule_storm(
                                FS_META_REPLY_TIMER_ROUNDS,
                                FS_META_REPLY_TIMER_BATCH,
                                FS_META_REPLY_TIMER_SLEEP_US,
                            ))
                    },
                ))
            })
        });
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(mut compio) = CompioNetHarness::new() {
        black_box(run_mixed_control_data_loop(
            &mut compio,
            warmup_epochs,
            |h| {
                fixture.read_qd1(FS_META_REPLY_OPS_PER_EPOCH).wrapping_add(
                    h.echo_rtt(FS_META_REPLY_OPS_PER_EPOCH, FS_META_REPLY_REPLY_BYTES),
                )
            },
            |h| {
                fixture
                    .metadata_qd1(FS_META_REPLY_OPS_PER_EPOCH)
                    .wrapping_add(h.timer_cancel_reschedule_storm(
                        FS_META_REPLY_TIMER_ROUNDS,
                        FS_META_REPLY_TIMER_BATCH,
                        FS_META_REPLY_TIMER_SLEEP_US,
                    ))
            },
        ));
        group.bench_function("compio_fs_metadata_then_reply_qd1", |b| {
            b.iter(|| {
                black_box(run_mixed_control_data_loop(
                    &mut compio,
                    FS_META_REPLY_EPOCHS,
                    |h| {
                        fixture.read_qd1(FS_META_REPLY_OPS_PER_EPOCH).wrapping_add(
                            h.echo_rtt(FS_META_REPLY_OPS_PER_EPOCH, FS_META_REPLY_REPLY_BYTES),
                        )
                    },
                    |h| {
                        fixture
                            .metadata_qd1(FS_META_REPLY_OPS_PER_EPOCH)
                            .wrapping_add(h.timer_cancel_reschedule_storm(
                                FS_META_REPLY_TIMER_ROUNDS,
                                FS_META_REPLY_TIMER_BATCH,
                                FS_META_REPLY_TIMER_SLEEP_US,
                            ))
                    },
                ))
            })
        });
    }

    group.finish();
}

#[cfg(unix)]
fn bench_high_depth_fanout_first_k_cancel(c: &mut Criterion) {
    let mut group = c.benchmark_group("high_depth_fanout_first_k_cancel_256b_window64");
    group.throughput(Throughput::Bytes(
        (HD_FANOUT_CANCEL_TOTAL_FRAMES * HD_FANOUT_CANCEL_PAYLOAD) as u64,
    ));

    let warmup_epochs = (HD_FANOUT_CANCEL_EPOCHS / 4).max(1);

    let mut tokio = TokioNetHarness::new();
    black_box(run_mixed_control_data_loop(
        &mut tokio,
        warmup_epochs,
        |h| {
            h.echo_hotspot_rotation(
                HD_FANOUT_CANCEL_STEPS,
                HD_FANOUT_CANCEL_HEAVY_FRAMES,
                HD_FANOUT_CANCEL_LIGHT_FRAMES,
                HD_FANOUT_CANCEL_PAYLOAD,
                HD_FANOUT_CANCEL_WINDOW,
            )
        },
        |h| {
            h.timer_cancel_reschedule_storm(
                HD_FANOUT_CANCEL_TIMER_ROUNDS,
                HD_FANOUT_CANCEL_TIMER_BATCH,
                HD_FANOUT_CANCEL_TIMER_SLEEP_US,
            )
        },
    ));
    group.bench_function("tokio_high_depth_fanout_first_k_cancel", |b| {
        b.iter(|| {
            black_box(run_mixed_control_data_loop(
                &mut tokio,
                HD_FANOUT_CANCEL_EPOCHS,
                |h| {
                    h.echo_hotspot_rotation(
                        HD_FANOUT_CANCEL_STEPS,
                        HD_FANOUT_CANCEL_HEAVY_FRAMES,
                        HD_FANOUT_CANCEL_LIGHT_FRAMES,
                        HD_FANOUT_CANCEL_PAYLOAD,
                        HD_FANOUT_CANCEL_WINDOW,
                    )
                },
                |h| {
                    h.timer_cancel_reschedule_storm(
                        HD_FANOUT_CANCEL_TIMER_ROUNDS,
                        HD_FANOUT_CANCEL_TIMER_BATCH,
                        HD_FANOUT_CANCEL_TIMER_SLEEP_US,
                    )
                },
            ))
        })
    });

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(mut spargio) = SpargioNetHarness::new_distributed() {
        black_box(run_mixed_control_data_loop(
            &mut spargio,
            warmup_epochs,
            |h| {
                h.echo_hotspot_rotation(
                    HD_FANOUT_CANCEL_STEPS,
                    HD_FANOUT_CANCEL_HEAVY_FRAMES,
                    HD_FANOUT_CANCEL_LIGHT_FRAMES,
                    HD_FANOUT_CANCEL_PAYLOAD,
                    HD_FANOUT_CANCEL_WINDOW,
                )
            },
            |h| {
                h.timer_cancel_reschedule_storm(
                    HD_FANOUT_CANCEL_TIMER_ROUNDS,
                    HD_FANOUT_CANCEL_TIMER_BATCH,
                    HD_FANOUT_CANCEL_TIMER_SLEEP_US,
                )
            },
        ));
        group.bench_function("spargio_high_depth_fanout_first_k_cancel", |b| {
            b.iter(|| {
                black_box(run_mixed_control_data_loop(
                    &mut spargio,
                    HD_FANOUT_CANCEL_EPOCHS,
                    |h| {
                        h.echo_hotspot_rotation(
                            HD_FANOUT_CANCEL_STEPS,
                            HD_FANOUT_CANCEL_HEAVY_FRAMES,
                            HD_FANOUT_CANCEL_LIGHT_FRAMES,
                            HD_FANOUT_CANCEL_PAYLOAD,
                            HD_FANOUT_CANCEL_WINDOW,
                        )
                    },
                    |h| {
                        h.timer_cancel_reschedule_storm(
                            HD_FANOUT_CANCEL_TIMER_ROUNDS,
                            HD_FANOUT_CANCEL_TIMER_BATCH,
                            HD_FANOUT_CANCEL_TIMER_SLEEP_US,
                        )
                    },
                ))
            })
        });
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(mut compio) = CompioNetHarness::new() {
        black_box(run_mixed_control_data_loop(
            &mut compio,
            warmup_epochs,
            |h| {
                h.echo_hotspot_rotation(
                    HD_FANOUT_CANCEL_STEPS,
                    HD_FANOUT_CANCEL_HEAVY_FRAMES,
                    HD_FANOUT_CANCEL_LIGHT_FRAMES,
                    HD_FANOUT_CANCEL_PAYLOAD,
                    HD_FANOUT_CANCEL_WINDOW,
                )
            },
            |h| {
                h.timer_cancel_reschedule_storm(
                    HD_FANOUT_CANCEL_TIMER_ROUNDS,
                    HD_FANOUT_CANCEL_TIMER_BATCH,
                    HD_FANOUT_CANCEL_TIMER_SLEEP_US,
                )
            },
        ));
        group.bench_function("compio_high_depth_fanout_first_k_cancel", |b| {
            b.iter(|| {
                black_box(run_mixed_control_data_loop(
                    &mut compio,
                    HD_FANOUT_CANCEL_EPOCHS,
                    |h| {
                        h.echo_hotspot_rotation(
                            HD_FANOUT_CANCEL_STEPS,
                            HD_FANOUT_CANCEL_HEAVY_FRAMES,
                            HD_FANOUT_CANCEL_LIGHT_FRAMES,
                            HD_FANOUT_CANCEL_PAYLOAD,
                            HD_FANOUT_CANCEL_WINDOW,
                        )
                    },
                    |h| {
                        h.timer_cancel_reschedule_storm(
                            HD_FANOUT_CANCEL_TIMER_ROUNDS,
                            HD_FANOUT_CANCEL_TIMER_BATCH,
                            HD_FANOUT_CANCEL_TIMER_SLEEP_US,
                        )
                    },
                ))
            })
        });
    }

    group.finish();
}

#[cfg(unix)]
fn bench_high_depth_multitenant_keyed_router(c: &mut Criterion) {
    let mut group = c.benchmark_group("high_depth_multitenant_keyed_router_4k_window64");
    group.throughput(Throughput::Bytes(
        (HD_MULTITENANT_TOTAL_FRAMES * HD_MULTITENANT_PAYLOAD) as u64,
    ));

    let warmup_steps = (HD_MULTITENANT_STEPS / 8).max(1);
    let warmup_heavy = (HD_MULTITENANT_HEAVY_FRAMES / 4).max(1);
    let warmup_light = (HD_MULTITENANT_LIGHT_FRAMES / 2).max(1);

    let mut tokio = TokioNetHarness::new();
    black_box(tokio.echo_keyed_hotspot_rotation(
        warmup_steps,
        warmup_heavy,
        warmup_light,
        HD_MULTITENANT_PAYLOAD,
        HD_MULTITENANT_WINDOW,
        HD_MULTITENANT_OWNER_SHARDS,
    ));
    group.bench_function("tokio_high_depth_multitenant_keyed_router", |b| {
        b.iter(|| {
            black_box(tokio.echo_keyed_hotspot_rotation(
                HD_MULTITENANT_STEPS,
                HD_MULTITENANT_HEAVY_FRAMES,
                HD_MULTITENANT_LIGHT_FRAMES,
                HD_MULTITENANT_PAYLOAD,
                HD_MULTITENANT_WINDOW,
                HD_MULTITENANT_OWNER_SHARDS,
            ))
        })
    });

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(mut spargio) = SpargioNetHarness::new_distributed() {
        black_box(spargio.echo_keyed_hotspot_rotation(
            warmup_steps,
            warmup_heavy,
            warmup_light,
            HD_MULTITENANT_PAYLOAD,
            HD_MULTITENANT_WINDOW,
            HD_MULTITENANT_OWNER_SHARDS,
        ));
        group.bench_function("spargio_high_depth_multitenant_keyed_router", |b| {
            b.iter(|| {
                black_box(spargio.echo_keyed_hotspot_rotation(
                    HD_MULTITENANT_STEPS,
                    HD_MULTITENANT_HEAVY_FRAMES,
                    HD_MULTITENANT_LIGHT_FRAMES,
                    HD_MULTITENANT_PAYLOAD,
                    HD_MULTITENANT_WINDOW,
                    HD_MULTITENANT_OWNER_SHARDS,
                ))
            })
        });
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(mut compio) = CompioNetHarness::new() {
        black_box(compio.echo_keyed_hotspot_rotation(
            warmup_steps,
            warmup_heavy,
            warmup_light,
            HD_MULTITENANT_PAYLOAD,
            HD_MULTITENANT_WINDOW,
            HD_MULTITENANT_OWNER_SHARDS,
        ));
        group.bench_function("compio_high_depth_multitenant_keyed_router", |b| {
            b.iter(|| {
                black_box(compio.echo_keyed_hotspot_rotation(
                    HD_MULTITENANT_STEPS,
                    HD_MULTITENANT_HEAVY_FRAMES,
                    HD_MULTITENANT_LIGHT_FRAMES,
                    HD_MULTITENANT_PAYLOAD,
                    HD_MULTITENANT_WINDOW,
                    HD_MULTITENANT_OWNER_SHARDS,
                ))
            })
        });
    }

    group.finish();
}

#[cfg(unix)]
fn bench_high_depth_barriered_pipeline(c: &mut Criterion) {
    let mut group = c.benchmark_group("high_depth_barriered_pipeline_4k_window64");
    group.throughput(Throughput::Bytes(
        (HD_BARRIER_PIPELINE_TOTAL_FRAMES * HD_BARRIER_PIPELINE_PAYLOAD) as u64,
    ));

    let warmup_phases = (HD_BARRIER_PIPELINE_PHASES / 8).max(1);
    let warmup_frames = (HD_BARRIER_PIPELINE_FRAMES_PER_STREAM / 2).max(1);

    let mut tokio = TokioNetHarness::new();
    black_box(tokio.echo_fanin_barrier_micro_batches(
        warmup_phases,
        warmup_frames,
        HD_BARRIER_PIPELINE_PAYLOAD,
        HD_BARRIER_PIPELINE_WINDOW,
    ));
    group.bench_function("tokio_high_depth_barriered_pipeline", |b| {
        b.iter(|| {
            black_box(tokio.echo_fanin_barrier_micro_batches(
                HD_BARRIER_PIPELINE_PHASES,
                HD_BARRIER_PIPELINE_FRAMES_PER_STREAM,
                HD_BARRIER_PIPELINE_PAYLOAD,
                HD_BARRIER_PIPELINE_WINDOW,
            ))
        })
    });

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(mut spargio) = SpargioNetHarness::new_distributed() {
        black_box(spargio.echo_fanin_barrier_micro_batches(
            warmup_phases,
            warmup_frames,
            HD_BARRIER_PIPELINE_PAYLOAD,
            HD_BARRIER_PIPELINE_WINDOW,
        ));
        group.bench_function("spargio_high_depth_barriered_pipeline", |b| {
            b.iter(|| {
                black_box(spargio.echo_fanin_barrier_micro_batches(
                    HD_BARRIER_PIPELINE_PHASES,
                    HD_BARRIER_PIPELINE_FRAMES_PER_STREAM,
                    HD_BARRIER_PIPELINE_PAYLOAD,
                    HD_BARRIER_PIPELINE_WINDOW,
                ))
            })
        });
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(mut compio) = CompioNetHarness::new() {
        black_box(compio.echo_fanin_barrier_micro_batches(
            warmup_phases,
            warmup_frames,
            HD_BARRIER_PIPELINE_PAYLOAD,
            HD_BARRIER_PIPELINE_WINDOW,
        ));
        group.bench_function("compio_high_depth_barriered_pipeline", |b| {
            b.iter(|| {
                black_box(compio.echo_fanin_barrier_micro_batches(
                    HD_BARRIER_PIPELINE_PHASES,
                    HD_BARRIER_PIPELINE_FRAMES_PER_STREAM,
                    HD_BARRIER_PIPELINE_PAYLOAD,
                    HD_BARRIER_PIPELINE_WINDOW,
                ))
            })
        });
    }

    group.finish();
}

#[cfg(unix)]
fn bench_high_depth_deadline_gateway(c: &mut Criterion) {
    let mut group = c.benchmark_group("high_depth_deadline_gateway_256b_window64");
    group.throughput(Throughput::Bytes(HD_DEADLINE_GATEWAY_TOTAL_BYTES as u64));

    let warmup_epochs = (HD_DEADLINE_GATEWAY_EPOCHS / 4).max(1);

    let mut tokio = TokioNetHarness::new();
    black_box(run_mixed_control_data_loop(
        &mut tokio,
        warmup_epochs,
        |h| {
            h.echo_windowed(
                HD_DEADLINE_GATEWAY_STREAM_FRAMES,
                HD_DEADLINE_GATEWAY_PAYLOAD,
                HD_DEADLINE_GATEWAY_WINDOW,
            )
            .wrapping_add(h.echo_hotspot_rotation(
                HD_DEADLINE_GATEWAY_ROUTE_STEPS,
                1,
                0,
                HD_DEADLINE_GATEWAY_PAYLOAD,
                HD_DEADLINE_GATEWAY_WINDOW,
            ))
            .wrapping_add(h.echo_rtt(HD_DEADLINE_GATEWAY_RTT_ROUNDS, HD_DEADLINE_GATEWAY_PAYLOAD))
        },
        |h| {
            h.timer_cancel_reschedule_storm(
                HD_DEADLINE_GATEWAY_TIMER_ROUNDS,
                HD_DEADLINE_GATEWAY_TIMER_BATCH,
                HD_DEADLINE_GATEWAY_TIMER_SLEEP_US,
            )
        },
    ));
    group.bench_function("tokio_high_depth_deadline_gateway", |b| {
        b.iter(|| {
            black_box(run_mixed_control_data_loop(
                &mut tokio,
                HD_DEADLINE_GATEWAY_EPOCHS,
                |h| {
                    h.echo_windowed(
                        HD_DEADLINE_GATEWAY_STREAM_FRAMES,
                        HD_DEADLINE_GATEWAY_PAYLOAD,
                        HD_DEADLINE_GATEWAY_WINDOW,
                    )
                    .wrapping_add(h.echo_hotspot_rotation(
                        HD_DEADLINE_GATEWAY_ROUTE_STEPS,
                        1,
                        0,
                        HD_DEADLINE_GATEWAY_PAYLOAD,
                        HD_DEADLINE_GATEWAY_WINDOW,
                    ))
                    .wrapping_add(
                        h.echo_rtt(HD_DEADLINE_GATEWAY_RTT_ROUNDS, HD_DEADLINE_GATEWAY_PAYLOAD),
                    )
                },
                |h| {
                    h.timer_cancel_reschedule_storm(
                        HD_DEADLINE_GATEWAY_TIMER_ROUNDS,
                        HD_DEADLINE_GATEWAY_TIMER_BATCH,
                        HD_DEADLINE_GATEWAY_TIMER_SLEEP_US,
                    )
                },
            ))
        })
    });

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(mut spargio) = SpargioNetHarness::new_distributed() {
        black_box(run_mixed_control_data_loop(
            &mut spargio,
            warmup_epochs,
            |h| {
                h.echo_windowed(
                    HD_DEADLINE_GATEWAY_STREAM_FRAMES,
                    HD_DEADLINE_GATEWAY_PAYLOAD,
                    HD_DEADLINE_GATEWAY_WINDOW,
                )
                .wrapping_add(h.echo_hotspot_rotation(
                    HD_DEADLINE_GATEWAY_ROUTE_STEPS,
                    1,
                    0,
                    HD_DEADLINE_GATEWAY_PAYLOAD,
                    HD_DEADLINE_GATEWAY_WINDOW,
                ))
                .wrapping_add(
                    h.echo_rtt(HD_DEADLINE_GATEWAY_RTT_ROUNDS, HD_DEADLINE_GATEWAY_PAYLOAD),
                )
            },
            |h| {
                h.timer_cancel_reschedule_storm(
                    HD_DEADLINE_GATEWAY_TIMER_ROUNDS,
                    HD_DEADLINE_GATEWAY_TIMER_BATCH,
                    HD_DEADLINE_GATEWAY_TIMER_SLEEP_US,
                )
            },
        ));
        group.bench_function("spargio_high_depth_deadline_gateway", |b| {
            b.iter(|| {
                black_box(run_mixed_control_data_loop(
                    &mut spargio,
                    HD_DEADLINE_GATEWAY_EPOCHS,
                    |h| {
                        h.echo_windowed(
                            HD_DEADLINE_GATEWAY_STREAM_FRAMES,
                            HD_DEADLINE_GATEWAY_PAYLOAD,
                            HD_DEADLINE_GATEWAY_WINDOW,
                        )
                        .wrapping_add(h.echo_hotspot_rotation(
                            HD_DEADLINE_GATEWAY_ROUTE_STEPS,
                            1,
                            0,
                            HD_DEADLINE_GATEWAY_PAYLOAD,
                            HD_DEADLINE_GATEWAY_WINDOW,
                        ))
                        .wrapping_add(
                            h.echo_rtt(HD_DEADLINE_GATEWAY_RTT_ROUNDS, HD_DEADLINE_GATEWAY_PAYLOAD),
                        )
                    },
                    |h| {
                        h.timer_cancel_reschedule_storm(
                            HD_DEADLINE_GATEWAY_TIMER_ROUNDS,
                            HD_DEADLINE_GATEWAY_TIMER_BATCH,
                            HD_DEADLINE_GATEWAY_TIMER_SLEEP_US,
                        )
                    },
                ))
            })
        });
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(mut compio) = CompioNetHarness::new() {
        black_box(run_mixed_control_data_loop(
            &mut compio,
            warmup_epochs,
            |h| {
                h.echo_windowed(
                    HD_DEADLINE_GATEWAY_STREAM_FRAMES,
                    HD_DEADLINE_GATEWAY_PAYLOAD,
                    HD_DEADLINE_GATEWAY_WINDOW,
                )
                .wrapping_add(h.echo_hotspot_rotation(
                    HD_DEADLINE_GATEWAY_ROUTE_STEPS,
                    1,
                    0,
                    HD_DEADLINE_GATEWAY_PAYLOAD,
                    HD_DEADLINE_GATEWAY_WINDOW,
                ))
                .wrapping_add(
                    h.echo_rtt(HD_DEADLINE_GATEWAY_RTT_ROUNDS, HD_DEADLINE_GATEWAY_PAYLOAD),
                )
            },
            |h| {
                h.timer_cancel_reschedule_storm(
                    HD_DEADLINE_GATEWAY_TIMER_ROUNDS,
                    HD_DEADLINE_GATEWAY_TIMER_BATCH,
                    HD_DEADLINE_GATEWAY_TIMER_SLEEP_US,
                )
            },
        ));
        group.bench_function("compio_high_depth_deadline_gateway", |b| {
            b.iter(|| {
                black_box(run_mixed_control_data_loop(
                    &mut compio,
                    HD_DEADLINE_GATEWAY_EPOCHS,
                    |h| {
                        h.echo_windowed(
                            HD_DEADLINE_GATEWAY_STREAM_FRAMES,
                            HD_DEADLINE_GATEWAY_PAYLOAD,
                            HD_DEADLINE_GATEWAY_WINDOW,
                        )
                        .wrapping_add(h.echo_hotspot_rotation(
                            HD_DEADLINE_GATEWAY_ROUTE_STEPS,
                            1,
                            0,
                            HD_DEADLINE_GATEWAY_PAYLOAD,
                            HD_DEADLINE_GATEWAY_WINDOW,
                        ))
                        .wrapping_add(
                            h.echo_rtt(HD_DEADLINE_GATEWAY_RTT_ROUNDS, HD_DEADLINE_GATEWAY_PAYLOAD),
                        )
                    },
                    |h| {
                        h.timer_cancel_reschedule_storm(
                            HD_DEADLINE_GATEWAY_TIMER_ROUNDS,
                            HD_DEADLINE_GATEWAY_TIMER_BATCH,
                            HD_DEADLINE_GATEWAY_TIMER_SLEEP_US,
                        )
                    },
                ))
            })
        });
    }

    group.finish();
}

#[cfg(unix)]
fn bench_high_depth_fs_net_admission_control(c: &mut Criterion) {
    let mut group =
        c.benchmark_group("high_depth_fs_net_admission_control_4k_read_256b_reply_window64");
    group.throughput(Throughput::Bytes(HD_FS_ADMISSION_TOTAL_BYTES as u64));

    let fixture = FsBenchFixture::new(FS_NET_MICRO_READ_BYTES, HD_FS_ADMISSION_FILE_BLOCKS);
    let warmup_epochs = (HD_FS_ADMISSION_EPOCHS / 4).max(1);

    let mut tokio = TokioNetHarness::new();
    black_box(run_mixed_control_data_loop(
        &mut tokio,
        warmup_epochs,
        |h| {
            fixture
                .read_qd1(HD_FS_ADMISSION_READS_PER_EPOCH)
                .wrapping_add(h.echo_hotspot_rotation(
                    HD_FS_ADMISSION_DISPATCH_STEPS,
                    1,
                    0,
                    HD_FS_ADMISSION_REPLY_BYTES,
                    HD_FS_ADMISSION_WINDOW,
                ))
        },
        |h| {
            fixture
                .metadata_qd1(HD_FS_ADMISSION_META_PER_EPOCH)
                .wrapping_add(h.timer_cancel_reschedule_storm(
                    HD_FS_ADMISSION_TIMER_ROUNDS,
                    HD_FS_ADMISSION_TIMER_BATCH,
                    HD_FS_ADMISSION_TIMER_SLEEP_US,
                ))
        },
    ));
    group.bench_function("tokio_high_depth_fs_net_admission_control", |b| {
        b.iter(|| {
            black_box(run_mixed_control_data_loop(
                &mut tokio,
                HD_FS_ADMISSION_EPOCHS,
                |h| {
                    fixture
                        .read_qd1(HD_FS_ADMISSION_READS_PER_EPOCH)
                        .wrapping_add(h.echo_hotspot_rotation(
                            HD_FS_ADMISSION_DISPATCH_STEPS,
                            1,
                            0,
                            HD_FS_ADMISSION_REPLY_BYTES,
                            HD_FS_ADMISSION_WINDOW,
                        ))
                },
                |h| {
                    fixture
                        .metadata_qd1(HD_FS_ADMISSION_META_PER_EPOCH)
                        .wrapping_add(h.timer_cancel_reschedule_storm(
                            HD_FS_ADMISSION_TIMER_ROUNDS,
                            HD_FS_ADMISSION_TIMER_BATCH,
                            HD_FS_ADMISSION_TIMER_SLEEP_US,
                        ))
                },
            ))
        })
    });

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(mut spargio) = SpargioNetHarness::new_distributed() {
        black_box(run_mixed_control_data_loop(
            &mut spargio,
            warmup_epochs,
            |h| {
                fixture
                    .read_qd1(HD_FS_ADMISSION_READS_PER_EPOCH)
                    .wrapping_add(h.echo_hotspot_rotation(
                        HD_FS_ADMISSION_DISPATCH_STEPS,
                        1,
                        0,
                        HD_FS_ADMISSION_REPLY_BYTES,
                        HD_FS_ADMISSION_WINDOW,
                    ))
            },
            |h| {
                fixture
                    .metadata_qd1(HD_FS_ADMISSION_META_PER_EPOCH)
                    .wrapping_add(h.timer_cancel_reschedule_storm(
                        HD_FS_ADMISSION_TIMER_ROUNDS,
                        HD_FS_ADMISSION_TIMER_BATCH,
                        HD_FS_ADMISSION_TIMER_SLEEP_US,
                    ))
            },
        ));
        group.bench_function("spargio_high_depth_fs_net_admission_control", |b| {
            b.iter(|| {
                black_box(run_mixed_control_data_loop(
                    &mut spargio,
                    HD_FS_ADMISSION_EPOCHS,
                    |h| {
                        fixture
                            .read_qd1(HD_FS_ADMISSION_READS_PER_EPOCH)
                            .wrapping_add(h.echo_hotspot_rotation(
                                HD_FS_ADMISSION_DISPATCH_STEPS,
                                1,
                                0,
                                HD_FS_ADMISSION_REPLY_BYTES,
                                HD_FS_ADMISSION_WINDOW,
                            ))
                    },
                    |h| {
                        fixture
                            .metadata_qd1(HD_FS_ADMISSION_META_PER_EPOCH)
                            .wrapping_add(h.timer_cancel_reschedule_storm(
                                HD_FS_ADMISSION_TIMER_ROUNDS,
                                HD_FS_ADMISSION_TIMER_BATCH,
                                HD_FS_ADMISSION_TIMER_SLEEP_US,
                            ))
                    },
                ))
            })
        });
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(mut compio) = CompioNetHarness::new() {
        black_box(run_mixed_control_data_loop(
            &mut compio,
            warmup_epochs,
            |h| {
                fixture
                    .read_qd1(HD_FS_ADMISSION_READS_PER_EPOCH)
                    .wrapping_add(h.echo_hotspot_rotation(
                        HD_FS_ADMISSION_DISPATCH_STEPS,
                        1,
                        0,
                        HD_FS_ADMISSION_REPLY_BYTES,
                        HD_FS_ADMISSION_WINDOW,
                    ))
            },
            |h| {
                fixture
                    .metadata_qd1(HD_FS_ADMISSION_META_PER_EPOCH)
                    .wrapping_add(h.timer_cancel_reschedule_storm(
                        HD_FS_ADMISSION_TIMER_ROUNDS,
                        HD_FS_ADMISSION_TIMER_BATCH,
                        HD_FS_ADMISSION_TIMER_SLEEP_US,
                    ))
            },
        ));
        group.bench_function("compio_high_depth_fs_net_admission_control", |b| {
            b.iter(|| {
                black_box(run_mixed_control_data_loop(
                    &mut compio,
                    HD_FS_ADMISSION_EPOCHS,
                    |h| {
                        fixture
                            .read_qd1(HD_FS_ADMISSION_READS_PER_EPOCH)
                            .wrapping_add(h.echo_hotspot_rotation(
                                HD_FS_ADMISSION_DISPATCH_STEPS,
                                1,
                                0,
                                HD_FS_ADMISSION_REPLY_BYTES,
                                HD_FS_ADMISSION_WINDOW,
                            ))
                    },
                    |h| {
                        fixture
                            .metadata_qd1(HD_FS_ADMISSION_META_PER_EPOCH)
                            .wrapping_add(h.timer_cancel_reschedule_storm(
                                HD_FS_ADMISSION_TIMER_ROUNDS,
                                HD_FS_ADMISSION_TIMER_BATCH,
                                HD_FS_ADMISSION_TIMER_SLEEP_US,
                            ))
                    },
                ))
            })
        });
    }

    group.finish();
}

#[cfg(unix)]
fn bench_fanout_fanin_rotating_hot_partition(c: &mut Criterion) {
    let mut group = c.benchmark_group("fanout_fanin_rotating_hot_partition_4k_window32");
    group.throughput(Throughput::Bytes(
        (FANOUT_ROTATING_TOTAL_FRAMES * FANOUT_ROTATING_FRAME_BYTES) as u64,
    ));

    let warmup_frames = (FANOUT_ROTATING_FRAMES_PER_STREAM / 8).max(1);
    let warmup_heavy = (FANOUT_ROTATING_HEAVY_CPU_ITERS / 4).max(1);
    let warmup_light = (FANOUT_ROTATING_LIGHT_CPU_ITERS / 2).max(1);

    let mut tokio = TokioNetHarness::new();
    black_box(tokio.echo_pipeline_hotspot(
        warmup_frames,
        FANOUT_ROTATING_FRAME_BYTES,
        FANOUT_ROTATING_WINDOW,
        FANOUT_ROTATING_ROTATE_EVERY,
        warmup_heavy,
        warmup_light,
    ));
    group.bench_function("tokio_fanout_fanin_rotating_hot_partition", |b| {
        b.iter(|| {
            black_box(tokio.echo_pipeline_hotspot(
                FANOUT_ROTATING_FRAMES_PER_STREAM,
                FANOUT_ROTATING_FRAME_BYTES,
                FANOUT_ROTATING_WINDOW,
                FANOUT_ROTATING_ROTATE_EVERY,
                FANOUT_ROTATING_HEAVY_CPU_ITERS,
                FANOUT_ROTATING_LIGHT_CPU_ITERS,
            ))
        })
    });

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(mut spargio) = SpargioNetHarness::new_distributed() {
        black_box(spargio.echo_pipeline_hotspot(
            warmup_frames,
            FANOUT_ROTATING_FRAME_BYTES,
            FANOUT_ROTATING_WINDOW,
            FANOUT_ROTATING_ROTATE_EVERY,
            warmup_heavy,
            warmup_light,
        ));
        group.bench_function("spargio_fanout_fanin_rotating_hot_partition", |b| {
            b.iter(|| {
                black_box(spargio.echo_pipeline_hotspot(
                    FANOUT_ROTATING_FRAMES_PER_STREAM,
                    FANOUT_ROTATING_FRAME_BYTES,
                    FANOUT_ROTATING_WINDOW,
                    FANOUT_ROTATING_ROTATE_EVERY,
                    FANOUT_ROTATING_HEAVY_CPU_ITERS,
                    FANOUT_ROTATING_LIGHT_CPU_ITERS,
                ))
            })
        });
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(mut compio) = CompioNetHarness::new() {
        black_box(compio.echo_pipeline_hotspot(
            warmup_frames,
            FANOUT_ROTATING_FRAME_BYTES,
            FANOUT_ROTATING_WINDOW,
            FANOUT_ROTATING_ROTATE_EVERY,
            warmup_heavy,
            warmup_light,
        ));
        group.bench_function("compio_fanout_fanin_rotating_hot_partition", |b| {
            b.iter(|| {
                black_box(compio.echo_pipeline_hotspot(
                    FANOUT_ROTATING_FRAMES_PER_STREAM,
                    FANOUT_ROTATING_FRAME_BYTES,
                    FANOUT_ROTATING_WINDOW,
                    FANOUT_ROTATING_ROTATE_EVERY,
                    FANOUT_ROTATING_HEAVY_CPU_ITERS,
                    FANOUT_ROTATING_LIGHT_CPU_ITERS,
                ))
            })
        });
    }

    group.finish();
}

#[cfg(unix)]
fn bench_session_owner_with_spillover(c: &mut Criterion) {
    let mut group = c.benchmark_group("session_owner_with_spillover_4k");
    group.throughput(Throughput::Bytes(
        (SESSION_OWNER_SPILLOVER_TOTAL_FRAMES * SESSION_OWNER_SPILLOVER_FRAME_BYTES) as u64,
    ));

    let warmup_steps = (SESSION_OWNER_SPILLOVER_STEPS / 8).max(1);
    let warmup_heavy = (SESSION_OWNER_SPILLOVER_HEAVY_FRAMES / 4).max(1);
    let warmup_light = SESSION_OWNER_SPILLOVER_LIGHT_FRAMES.max(1);

    let mut tokio = TokioNetHarness::new();
    black_box(tokio.echo_hotspot_rotation(
        warmup_steps,
        warmup_heavy,
        warmup_light,
        SESSION_OWNER_SPILLOVER_FRAME_BYTES,
        SESSION_OWNER_SPILLOVER_WINDOW,
    ));
    group.bench_function("tokio_session_owner_with_spillover", |b| {
        b.iter(|| {
            black_box(tokio.echo_hotspot_rotation(
                SESSION_OWNER_SPILLOVER_STEPS,
                SESSION_OWNER_SPILLOVER_HEAVY_FRAMES,
                SESSION_OWNER_SPILLOVER_LIGHT_FRAMES,
                SESSION_OWNER_SPILLOVER_FRAME_BYTES,
                SESSION_OWNER_SPILLOVER_WINDOW,
            ))
        })
    });

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(mut spargio) = SpargioNetHarness::new_distributed() {
        black_box(spargio.echo_hotspot_rotation(
            warmup_steps,
            warmup_heavy,
            warmup_light,
            SESSION_OWNER_SPILLOVER_FRAME_BYTES,
            SESSION_OWNER_SPILLOVER_WINDOW,
        ));
        group.bench_function("spargio_session_owner_with_spillover", |b| {
            b.iter(|| {
                black_box(spargio.echo_hotspot_rotation(
                    SESSION_OWNER_SPILLOVER_STEPS,
                    SESSION_OWNER_SPILLOVER_HEAVY_FRAMES,
                    SESSION_OWNER_SPILLOVER_LIGHT_FRAMES,
                    SESSION_OWNER_SPILLOVER_FRAME_BYTES,
                    SESSION_OWNER_SPILLOVER_WINDOW,
                ))
            })
        });
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(mut compio) = CompioNetHarness::new() {
        black_box(compio.echo_hotspot_rotation(
            warmup_steps,
            warmup_heavy,
            warmup_light,
            SESSION_OWNER_SPILLOVER_FRAME_BYTES,
            SESSION_OWNER_SPILLOVER_WINDOW,
        ));
        group.bench_function("compio_session_owner_with_spillover", |b| {
            b.iter(|| {
                black_box(compio.echo_hotspot_rotation(
                    SESSION_OWNER_SPILLOVER_STEPS,
                    SESSION_OWNER_SPILLOVER_HEAVY_FRAMES,
                    SESSION_OWNER_SPILLOVER_LIGHT_FRAMES,
                    SESSION_OWNER_SPILLOVER_FRAME_BYTES,
                    SESSION_OWNER_SPILLOVER_WINDOW,
                ))
            })
        });
    }

    group.finish();
}

#[cfg(unix)]
fn bench_net_burst_flip_imbalance(c: &mut Criterion) {
    let mut group = c.benchmark_group("net_burst_flip_imbalance_4k");
    group.throughput(Throughput::Bytes(
        (BURST_FLIP_TOTAL_FRAMES * BURST_FLIP_FRAME_BYTES) as u64,
    ));

    let warmup_phases = (BURST_FLIP_PHASES / 8).max(1);
    let warmup_hot = (BURST_FLIP_HOT_FRAMES / 4).max(1);
    let warmup_cold = BURST_FLIP_COLD_FRAMES.max(1);

    let mut tokio = TokioNetHarness::new();
    black_box(tokio.echo_burst_flip_imbalance(
        warmup_phases,
        BURST_FLIP_FLIP_EVERY_PHASES,
        warmup_hot,
        warmup_cold,
        BURST_FLIP_FRAME_BYTES,
        BURST_FLIP_WINDOW,
    ));
    group.bench_function("tokio_burst_flip_hotcold", |b| {
        b.iter(|| {
            black_box(tokio.echo_burst_flip_imbalance(
                BURST_FLIP_PHASES,
                BURST_FLIP_FLIP_EVERY_PHASES,
                BURST_FLIP_HOT_FRAMES,
                BURST_FLIP_COLD_FRAMES,
                BURST_FLIP_FRAME_BYTES,
                BURST_FLIP_WINDOW,
            ))
        })
    });

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(mut spargio) = SpargioNetHarness::new_distributed() {
        black_box(spargio.echo_burst_flip_imbalance(
            warmup_phases,
            BURST_FLIP_FLIP_EVERY_PHASES,
            warmup_hot,
            warmup_cold,
            BURST_FLIP_FRAME_BYTES,
            BURST_FLIP_WINDOW,
        ));
        group.bench_function("spargio_burst_flip_hotcold", |b| {
            b.iter(|| {
                black_box(spargio.echo_burst_flip_imbalance(
                    BURST_FLIP_PHASES,
                    BURST_FLIP_FLIP_EVERY_PHASES,
                    BURST_FLIP_HOT_FRAMES,
                    BURST_FLIP_COLD_FRAMES,
                    BURST_FLIP_FRAME_BYTES,
                    BURST_FLIP_WINDOW,
                ))
            })
        });
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(mut compio) = CompioNetHarness::new() {
        black_box(compio.echo_burst_flip_imbalance(
            warmup_phases,
            BURST_FLIP_FLIP_EVERY_PHASES,
            warmup_hot,
            warmup_cold,
            BURST_FLIP_FRAME_BYTES,
            BURST_FLIP_WINDOW,
        ));
        group.bench_function("compio_burst_flip_hotcold", |b| {
            b.iter(|| {
                black_box(compio.echo_burst_flip_imbalance(
                    BURST_FLIP_PHASES,
                    BURST_FLIP_FLIP_EVERY_PHASES,
                    BURST_FLIP_HOT_FRAMES,
                    BURST_FLIP_COLD_FRAMES,
                    BURST_FLIP_FRAME_BYTES,
                    BURST_FLIP_WINDOW,
                ))
            })
        });
    }

    group.finish();
}

#[cfg(unix)]
fn bench_fanin_barrier_micro_batches(c: &mut Criterion) {
    let mut group = c.benchmark_group("fanin_barrier_micro_batches_1k");
    group.throughput(Throughput::Bytes(
        (FANIN_BARRIER_TOTAL_FRAMES * FANIN_BARRIER_PAYLOAD_BYTES) as u64,
    ));

    let warmup_phases = (FANIN_BARRIER_PHASES / 8).max(1);
    let warmup_frames = (FANIN_BARRIER_FRAMES_PER_STREAM / 2).max(1);

    let mut tokio = TokioNetHarness::new();
    black_box(tokio.echo_fanin_barrier_micro_batches(
        warmup_phases,
        warmup_frames,
        FANIN_BARRIER_PAYLOAD_BYTES,
        FANIN_BARRIER_WINDOW,
    ));
    group.bench_function("tokio_fanin_barrier_micro_batches", |b| {
        b.iter(|| {
            black_box(tokio.echo_fanin_barrier_micro_batches(
                FANIN_BARRIER_PHASES,
                FANIN_BARRIER_FRAMES_PER_STREAM,
                FANIN_BARRIER_PAYLOAD_BYTES,
                FANIN_BARRIER_WINDOW,
            ))
        })
    });

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(mut spargio) = SpargioNetHarness::new_distributed() {
        black_box(spargio.echo_fanin_barrier_micro_batches(
            warmup_phases,
            warmup_frames,
            FANIN_BARRIER_PAYLOAD_BYTES,
            FANIN_BARRIER_WINDOW,
        ));
        group.bench_function("spargio_fanin_barrier_micro_batches", |b| {
            b.iter(|| {
                black_box(spargio.echo_fanin_barrier_micro_batches(
                    FANIN_BARRIER_PHASES,
                    FANIN_BARRIER_FRAMES_PER_STREAM,
                    FANIN_BARRIER_PAYLOAD_BYTES,
                    FANIN_BARRIER_WINDOW,
                ))
            })
        });
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(mut compio) = CompioNetHarness::new() {
        black_box(compio.echo_fanin_barrier_micro_batches(
            warmup_phases,
            warmup_frames,
            FANIN_BARRIER_PAYLOAD_BYTES,
            FANIN_BARRIER_WINDOW,
        ));
        group.bench_function("compio_fanin_barrier_micro_batches", |b| {
            b.iter(|| {
                black_box(compio.echo_fanin_barrier_micro_batches(
                    FANIN_BARRIER_PHASES,
                    FANIN_BARRIER_FRAMES_PER_STREAM,
                    FANIN_BARRIER_PAYLOAD_BYTES,
                    FANIN_BARRIER_WINDOW,
                ))
            })
        });
    }

    group.finish();
}

#[cfg(unix)]
fn bench_serial_dep_chain_rpc(c: &mut Criterion) {
    let mut group = c.benchmark_group("serial_dep_chain_rpc_256b");
    group.throughput(Throughput::Bytes(
        (SERIAL_DEP_CHAIN_ROUNDS * SERIAL_DEP_CHAIN_PAYLOAD) as u64,
    ));

    let warmup_rounds = (SERIAL_DEP_CHAIN_ROUNDS / 8).max(1);

    let mut tokio = TokioNetHarness::new();
    black_box(tokio.echo_rtt(warmup_rounds, SERIAL_DEP_CHAIN_PAYLOAD));
    group.bench_function("tokio_serial_dep_chain_rpc", |b| {
        b.iter(|| black_box(tokio.echo_rtt(SERIAL_DEP_CHAIN_ROUNDS, SERIAL_DEP_CHAIN_PAYLOAD)))
    });

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(mut spargio) = SpargioNetHarness::new() {
        black_box(spargio.echo_rtt(warmup_rounds, SERIAL_DEP_CHAIN_PAYLOAD));
        group.bench_function("spargio_serial_dep_chain_rpc", |b| {
            b.iter(|| {
                black_box(spargio.echo_rtt(SERIAL_DEP_CHAIN_ROUNDS, SERIAL_DEP_CHAIN_PAYLOAD))
            })
        });
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(mut compio) = CompioNetHarness::new() {
        black_box(compio.echo_rtt(warmup_rounds, SERIAL_DEP_CHAIN_PAYLOAD));
        group.bench_function("compio_serial_dep_chain_rpc", |b| {
            b.iter(|| black_box(compio.echo_rtt(SERIAL_DEP_CHAIN_ROUNDS, SERIAL_DEP_CHAIN_PAYLOAD)))
        });
    }

    group.finish();
}

#[cfg(unix)]
fn bench_keyed_hotspot_flip_p99(c: &mut Criterion) {
    let mut group = c.benchmark_group("keyed_hotspot_flip_p99_4k");
    group.throughput(Throughput::Bytes(
        (KEYED_FLIP_TOTAL_FRAMES * KEYED_FLIP_FRAME_BYTES) as u64,
    ));

    let warmup_steps = (KEYED_FLIP_STEPS / 8).max(1);
    let warmup_hot = (KEYED_FLIP_HOT_FRAMES / 4).max(1);
    let warmup_cold = KEYED_FLIP_COLD_FRAMES.max(1);

    let mut tokio = TokioNetHarness::new();
    black_box(tokio.echo_burst_flip_imbalance(
        warmup_steps,
        1,
        warmup_hot,
        warmup_cold,
        KEYED_FLIP_FRAME_BYTES,
        KEYED_FLIP_WINDOW,
    ));
    group.bench_function("tokio_keyed_hotspot_flip_p99", |b| {
        b.iter(|| {
            black_box(tokio.echo_burst_flip_imbalance(
                KEYED_FLIP_STEPS,
                1,
                KEYED_FLIP_HOT_FRAMES,
                KEYED_FLIP_COLD_FRAMES,
                KEYED_FLIP_FRAME_BYTES,
                KEYED_FLIP_WINDOW,
            ))
        })
    });

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(mut spargio) = SpargioNetHarness::new_distributed() {
        black_box(spargio.echo_burst_flip_imbalance(
            warmup_steps,
            1,
            warmup_hot,
            warmup_cold,
            KEYED_FLIP_FRAME_BYTES,
            KEYED_FLIP_WINDOW,
        ));
        group.bench_function("spargio_keyed_hotspot_flip_p99", |b| {
            b.iter(|| {
                black_box(spargio.echo_burst_flip_imbalance(
                    KEYED_FLIP_STEPS,
                    1,
                    KEYED_FLIP_HOT_FRAMES,
                    KEYED_FLIP_COLD_FRAMES,
                    KEYED_FLIP_FRAME_BYTES,
                    KEYED_FLIP_WINDOW,
                ))
            })
        });
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(mut compio) = CompioNetHarness::new() {
        black_box(compio.echo_burst_flip_imbalance(
            warmup_steps,
            1,
            warmup_hot,
            warmup_cold,
            KEYED_FLIP_FRAME_BYTES,
            KEYED_FLIP_WINDOW,
        ));
        group.bench_function("compio_keyed_hotspot_flip_p99", |b| {
            b.iter(|| {
                black_box(compio.echo_burst_flip_imbalance(
                    KEYED_FLIP_STEPS,
                    1,
                    KEYED_FLIP_HOT_FRAMES,
                    KEYED_FLIP_COLD_FRAMES,
                    KEYED_FLIP_FRAME_BYTES,
                    KEYED_FLIP_WINDOW,
                ))
            })
        });
    }

    group.finish();
}

#[cfg(unix)]
fn bench_fanin_barrier_rounds(c: &mut Criterion) {
    let mut group = c.benchmark_group("fanin_barrier_rounds_1k");
    group.throughput(Throughput::Bytes(
        (FANIN_ROUNDS_TOTAL_FRAMES * FANIN_ROUNDS_PAYLOAD) as u64,
    ));

    let warmup_phases = (FANIN_ROUNDS_PHASES / 8).max(1);
    let warmup_frames = (FANIN_ROUNDS_FRAMES_PER_STREAM / 2).max(1);

    let mut tokio = TokioNetHarness::new();
    black_box(tokio.echo_fanin_barrier_micro_batches(
        warmup_phases,
        warmup_frames,
        FANIN_ROUNDS_PAYLOAD,
        FANIN_ROUNDS_WINDOW,
    ));
    group.bench_function("tokio_fanin_barrier_rounds", |b| {
        b.iter(|| {
            black_box(tokio.echo_fanin_barrier_micro_batches(
                FANIN_ROUNDS_PHASES,
                FANIN_ROUNDS_FRAMES_PER_STREAM,
                FANIN_ROUNDS_PAYLOAD,
                FANIN_ROUNDS_WINDOW,
            ))
        })
    });

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(mut spargio) = SpargioNetHarness::new_distributed() {
        black_box(spargio.echo_fanin_barrier_micro_batches(
            warmup_phases,
            warmup_frames,
            FANIN_ROUNDS_PAYLOAD,
            FANIN_ROUNDS_WINDOW,
        ));
        group.bench_function("spargio_fanin_barrier_rounds", |b| {
            b.iter(|| {
                black_box(spargio.echo_fanin_barrier_micro_batches(
                    FANIN_ROUNDS_PHASES,
                    FANIN_ROUNDS_FRAMES_PER_STREAM,
                    FANIN_ROUNDS_PAYLOAD,
                    FANIN_ROUNDS_WINDOW,
                ))
            })
        });
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(mut compio) = CompioNetHarness::new() {
        black_box(compio.echo_fanin_barrier_micro_batches(
            warmup_phases,
            warmup_frames,
            FANIN_ROUNDS_PAYLOAD,
            FANIN_ROUNDS_WINDOW,
        ));
        group.bench_function("compio_fanin_barrier_rounds", |b| {
            b.iter(|| {
                black_box(compio.echo_fanin_barrier_micro_batches(
                    FANIN_ROUNDS_PHASES,
                    FANIN_ROUNDS_FRAMES_PER_STREAM,
                    FANIN_ROUNDS_PAYLOAD,
                    FANIN_ROUNDS_WINDOW,
                ))
            })
        });
    }

    group.finish();
}

#[cfg(unix)]
fn bench_wakeup_sparse_event_rtt(c: &mut Criterion) {
    let mut group = c.benchmark_group("wakeup_sparse_event_rtt_64b");
    group.throughput(Throughput::Elements(WAKEUP_SPARSE_EVENTS as u64));

    let warmup_events = (WAKEUP_SPARSE_EVENTS / 8).max(1);

    let mut tokio = TokioNetHarness::new();
    black_box(run_sparse_event_loop(
        warmup_events,
        WAKEUP_SPARSE_IDLE_US,
        || tokio.echo_rtt(1, WAKEUP_SPARSE_PAYLOAD),
    ));
    group.bench_function("tokio_wakeup_sparse_event_rtt", |b| {
        b.iter(|| {
            black_box(run_sparse_event_loop(
                WAKEUP_SPARSE_EVENTS,
                WAKEUP_SPARSE_IDLE_US,
                || tokio.echo_rtt(1, WAKEUP_SPARSE_PAYLOAD),
            ))
        })
    });

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(mut spargio) = SpargioNetHarness::new() {
        black_box(run_sparse_event_loop(
            warmup_events,
            WAKEUP_SPARSE_IDLE_US,
            || spargio.echo_rtt(1, WAKEUP_SPARSE_PAYLOAD),
        ));
        group.bench_function("spargio_wakeup_sparse_event_rtt", |b| {
            b.iter(|| {
                black_box(run_sparse_event_loop(
                    WAKEUP_SPARSE_EVENTS,
                    WAKEUP_SPARSE_IDLE_US,
                    || spargio.echo_rtt(1, WAKEUP_SPARSE_PAYLOAD),
                ))
            })
        });
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(mut compio) = CompioNetHarness::new() {
        black_box(run_sparse_event_loop(
            warmup_events,
            WAKEUP_SPARSE_IDLE_US,
            || compio.echo_rtt(1, WAKEUP_SPARSE_PAYLOAD),
        ));
        group.bench_function("compio_wakeup_sparse_event_rtt", |b| {
            b.iter(|| {
                black_box(run_sparse_event_loop(
                    WAKEUP_SPARSE_EVENTS,
                    WAKEUP_SPARSE_IDLE_US,
                    || compio.echo_rtt(1, WAKEUP_SPARSE_PAYLOAD),
                ))
            })
        });
    }

    group.finish();
}

#[cfg(unix)]
fn bench_timer_cancel_reschedule_storm(c: &mut Criterion) {
    let mut group = c.benchmark_group("timer_cancel_reschedule_storm");
    group.throughput(Throughput::Elements(
        (TIMER_STORM_ROUNDS * TIMER_STORM_BATCH) as u64,
    ));

    let warmup_rounds = (TIMER_STORM_ROUNDS / 8).max(1);

    let mut tokio = TokioNetHarness::new();
    black_box(tokio.timer_cancel_reschedule_storm(
        warmup_rounds,
        TIMER_STORM_BATCH,
        TIMER_STORM_SLEEP_US,
    ));
    group.bench_function("tokio_timer_cancel_reschedule_storm", |b| {
        b.iter(|| {
            black_box(tokio.timer_cancel_reschedule_storm(
                TIMER_STORM_ROUNDS,
                TIMER_STORM_BATCH,
                TIMER_STORM_SLEEP_US,
            ))
        })
    });

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(mut spargio) = SpargioNetHarness::new() {
        black_box(spargio.timer_cancel_reschedule_storm(
            warmup_rounds,
            TIMER_STORM_BATCH,
            TIMER_STORM_SLEEP_US,
        ));
        group.bench_function("spargio_timer_cancel_reschedule_storm", |b| {
            b.iter(|| {
                black_box(spargio.timer_cancel_reschedule_storm(
                    TIMER_STORM_ROUNDS,
                    TIMER_STORM_BATCH,
                    TIMER_STORM_SLEEP_US,
                ))
            })
        });
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(mut compio) = CompioNetHarness::new() {
        black_box(compio.timer_cancel_reschedule_storm(
            warmup_rounds,
            TIMER_STORM_BATCH,
            TIMER_STORM_SLEEP_US,
        ));
        group.bench_function("compio_timer_cancel_reschedule_storm", |b| {
            b.iter(|| {
                black_box(compio.timer_cancel_reschedule_storm(
                    TIMER_STORM_ROUNDS,
                    TIMER_STORM_BATCH,
                    TIMER_STORM_SLEEP_US,
                ))
            })
        });
    }

    group.finish();
}

#[cfg(unix)]
fn bench_mixed_control_data_plane(c: &mut Criterion) {
    let mut group = c.benchmark_group("mixed_control_data_plane_4k_plus_64b");
    group.throughput(Throughput::Bytes(MIXED_CONTROL_TOTAL_BYTES as u64));

    let warmup_epochs = (MIXED_CONTROL_EPOCHS / 2).max(1);

    let mut tokio = TokioNetHarness::new();
    black_box(run_mixed_control_data_loop(
        &mut tokio,
        warmup_epochs,
        |h| {
            h.echo_windowed(
                MIXED_CONTROL_DATA_FRAMES,
                MIXED_CONTROL_DATA_PAYLOAD,
                MIXED_CONTROL_DATA_WINDOW,
            )
        },
        |h| h.echo_rtt(MIXED_CONTROL_CTRL_ROUNDS, MIXED_CONTROL_CTRL_PAYLOAD),
    ));
    group.bench_function("tokio_mixed_control_data_plane", |b| {
        b.iter(|| {
            black_box(run_mixed_control_data_loop(
                &mut tokio,
                MIXED_CONTROL_EPOCHS,
                |h| {
                    h.echo_windowed(
                        MIXED_CONTROL_DATA_FRAMES,
                        MIXED_CONTROL_DATA_PAYLOAD,
                        MIXED_CONTROL_DATA_WINDOW,
                    )
                },
                |h| h.echo_rtt(MIXED_CONTROL_CTRL_ROUNDS, MIXED_CONTROL_CTRL_PAYLOAD),
            ))
        })
    });

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(mut spargio) = SpargioNetHarness::new() {
        black_box(run_mixed_control_data_loop(
            &mut spargio,
            warmup_epochs,
            |h| {
                h.echo_windowed(
                    MIXED_CONTROL_DATA_FRAMES,
                    MIXED_CONTROL_DATA_PAYLOAD,
                    MIXED_CONTROL_DATA_WINDOW,
                )
            },
            |h| h.echo_rtt(MIXED_CONTROL_CTRL_ROUNDS, MIXED_CONTROL_CTRL_PAYLOAD),
        ));
        group.bench_function("spargio_mixed_control_data_plane", |b| {
            b.iter(|| {
                black_box(run_mixed_control_data_loop(
                    &mut spargio,
                    MIXED_CONTROL_EPOCHS,
                    |h| {
                        h.echo_windowed(
                            MIXED_CONTROL_DATA_FRAMES,
                            MIXED_CONTROL_DATA_PAYLOAD,
                            MIXED_CONTROL_DATA_WINDOW,
                        )
                    },
                    |h| h.echo_rtt(MIXED_CONTROL_CTRL_ROUNDS, MIXED_CONTROL_CTRL_PAYLOAD),
                ))
            })
        });
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(mut compio) = CompioNetHarness::new() {
        black_box(run_mixed_control_data_loop(
            &mut compio,
            warmup_epochs,
            |h| {
                h.echo_windowed(
                    MIXED_CONTROL_DATA_FRAMES,
                    MIXED_CONTROL_DATA_PAYLOAD,
                    MIXED_CONTROL_DATA_WINDOW,
                )
            },
            |h| h.echo_rtt(MIXED_CONTROL_CTRL_ROUNDS, MIXED_CONTROL_CTRL_PAYLOAD),
        ));
        group.bench_function("compio_mixed_control_data_plane", |b| {
            b.iter(|| {
                black_box(run_mixed_control_data_loop(
                    &mut compio,
                    MIXED_CONTROL_EPOCHS,
                    |h| {
                        h.echo_windowed(
                            MIXED_CONTROL_DATA_FRAMES,
                            MIXED_CONTROL_DATA_PAYLOAD,
                            MIXED_CONTROL_DATA_WINDOW,
                        )
                    },
                    |h| h.echo_rtt(MIXED_CONTROL_CTRL_ROUNDS, MIXED_CONTROL_CTRL_PAYLOAD),
                ))
            })
        });
    }

    group.finish();
}

#[cfg(unix)]
fn bench_bounded_pipeline_backpressure(c: &mut Criterion) {
    let mut group = c.benchmark_group("bounded_pipeline_backpressure_4k_window2");
    group.throughput(Throughput::Bytes(
        (BOUNDED_BP_TOTAL_FRAMES * BOUNDED_BP_PAYLOAD) as u64,
    ));

    let warmup_frames = (BOUNDED_BP_FRAMES_PER_STREAM / 8).max(1);
    let warmup_heavy = (BOUNDED_BP_HEAVY_ITERS / 3).max(1);
    let warmup_light = (BOUNDED_BP_LIGHT_ITERS / 2).max(1);

    let mut tokio = TokioNetHarness::new();
    black_box(tokio.echo_pipeline_hotspot(
        warmup_frames,
        BOUNDED_BP_PAYLOAD,
        BOUNDED_BP_WINDOW,
        BOUNDED_BP_ROTATE_EVERY,
        warmup_heavy,
        warmup_light,
    ));
    group.bench_function("tokio_bounded_pipeline_backpressure", |b| {
        b.iter(|| {
            black_box(tokio.echo_pipeline_hotspot(
                BOUNDED_BP_FRAMES_PER_STREAM,
                BOUNDED_BP_PAYLOAD,
                BOUNDED_BP_WINDOW,
                BOUNDED_BP_ROTATE_EVERY,
                BOUNDED_BP_HEAVY_ITERS,
                BOUNDED_BP_LIGHT_ITERS,
            ))
        })
    });

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(mut spargio) = SpargioNetHarness::new_distributed() {
        black_box(spargio.echo_pipeline_hotspot(
            warmup_frames,
            BOUNDED_BP_PAYLOAD,
            BOUNDED_BP_WINDOW,
            BOUNDED_BP_ROTATE_EVERY,
            warmup_heavy,
            warmup_light,
        ));
        group.bench_function("spargio_bounded_pipeline_backpressure", |b| {
            b.iter(|| {
                black_box(spargio.echo_pipeline_hotspot(
                    BOUNDED_BP_FRAMES_PER_STREAM,
                    BOUNDED_BP_PAYLOAD,
                    BOUNDED_BP_WINDOW,
                    BOUNDED_BP_ROTATE_EVERY,
                    BOUNDED_BP_HEAVY_ITERS,
                    BOUNDED_BP_LIGHT_ITERS,
                ))
            })
        });
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(mut compio) = CompioNetHarness::new() {
        black_box(compio.echo_pipeline_hotspot(
            warmup_frames,
            BOUNDED_BP_PAYLOAD,
            BOUNDED_BP_WINDOW,
            BOUNDED_BP_ROTATE_EVERY,
            warmup_heavy,
            warmup_light,
        ));
        group.bench_function("compio_bounded_pipeline_backpressure", |b| {
            b.iter(|| {
                black_box(compio.echo_pipeline_hotspot(
                    BOUNDED_BP_FRAMES_PER_STREAM,
                    BOUNDED_BP_PAYLOAD,
                    BOUNDED_BP_WINDOW,
                    BOUNDED_BP_ROTATE_EVERY,
                    BOUNDED_BP_HEAVY_ITERS,
                    BOUNDED_BP_LIGHT_ITERS,
                ))
            })
        });
    }

    group.finish();
}

#[cfg(unix)]
fn bench_post_io_cpu_locality(c: &mut Criterion) {
    let mut group = c.benchmark_group("post_io_cpu_locality_4k_window1");
    group.throughput(Throughput::Bytes(
        (POST_IO_LOCALITY_TOTAL_FRAMES * POST_IO_LOCALITY_PAYLOAD) as u64,
    ));

    let warmup_frames = (POST_IO_LOCALITY_FRAMES_PER_STREAM / 8).max(1);
    let warmup_heavy = (POST_IO_LOCALITY_HEAVY_ITERS / 4).max(1);
    let warmup_light = (POST_IO_LOCALITY_LIGHT_ITERS / 2).max(1);

    let mut tokio = TokioNetHarness::new();
    black_box(tokio.echo_pipeline_hotspot(
        warmup_frames,
        POST_IO_LOCALITY_PAYLOAD,
        POST_IO_LOCALITY_WINDOW,
        POST_IO_LOCALITY_ROTATE_EVERY,
        warmup_heavy,
        warmup_light,
    ));
    group.bench_function("tokio_post_io_cpu_locality", |b| {
        b.iter(|| {
            black_box(tokio.echo_pipeline_hotspot(
                POST_IO_LOCALITY_FRAMES_PER_STREAM,
                POST_IO_LOCALITY_PAYLOAD,
                POST_IO_LOCALITY_WINDOW,
                POST_IO_LOCALITY_ROTATE_EVERY,
                POST_IO_LOCALITY_HEAVY_ITERS,
                POST_IO_LOCALITY_LIGHT_ITERS,
            ))
        })
    });

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(mut spargio) = SpargioNetHarness::new_distributed() {
        black_box(spargio.echo_pipeline_hotspot(
            warmup_frames,
            POST_IO_LOCALITY_PAYLOAD,
            POST_IO_LOCALITY_WINDOW,
            POST_IO_LOCALITY_ROTATE_EVERY,
            warmup_heavy,
            warmup_light,
        ));
        group.bench_function("spargio_post_io_cpu_locality", |b| {
            b.iter(|| {
                black_box(spargio.echo_pipeline_hotspot(
                    POST_IO_LOCALITY_FRAMES_PER_STREAM,
                    POST_IO_LOCALITY_PAYLOAD,
                    POST_IO_LOCALITY_WINDOW,
                    POST_IO_LOCALITY_ROTATE_EVERY,
                    POST_IO_LOCALITY_HEAVY_ITERS,
                    POST_IO_LOCALITY_LIGHT_ITERS,
                ))
            })
        });
    }

    #[cfg(all(feature = "uring-native", target_os = "linux"))]
    if let Some(mut compio) = CompioNetHarness::new() {
        black_box(compio.echo_pipeline_hotspot(
            warmup_frames,
            POST_IO_LOCALITY_PAYLOAD,
            POST_IO_LOCALITY_WINDOW,
            POST_IO_LOCALITY_ROTATE_EVERY,
            warmup_heavy,
            warmup_light,
        ));
        group.bench_function("compio_post_io_cpu_locality", |b| {
            b.iter(|| {
                black_box(compio.echo_pipeline_hotspot(
                    POST_IO_LOCALITY_FRAMES_PER_STREAM,
                    POST_IO_LOCALITY_PAYLOAD,
                    POST_IO_LOCALITY_WINDOW,
                    POST_IO_LOCALITY_ROTATE_EVERY,
                    POST_IO_LOCALITY_HEAVY_ITERS,
                    POST_IO_LOCALITY_LIGHT_ITERS,
                ))
            })
        });
    }

    group.finish();
}

#[cfg(unix)]
criterion_group!(
    benches,
    bench_net_echo_rtt,
    bench_net_stream_throughput,
    bench_net_stream_imbalanced,
    bench_net_stream_hotspot_rotation,
    bench_net_pipeline_hotspot_rotation,
    bench_net_keyed_hotspot_rotation,
    bench_net_keyed_hotspot_rotation_cpu,
    bench_ingress_dispatch_to_workers_rr_ack,
    bench_fs_net_microservice,
    bench_fs_net_microservice_deadline_dispatch,
    bench_net_echo_rtt_deadline_routing,
    bench_net_stream_multitenant,
    bench_net_stream_hotflip,
    bench_net_pipeline_barrier,
    bench_keyed_router_with_session_owner_spillover,
    bench_fs_metadata_then_reply_qd1,
    bench_high_depth_fanout_first_k_cancel,
    bench_high_depth_multitenant_keyed_router,
    bench_high_depth_barriered_pipeline,
    bench_high_depth_deadline_gateway,
    bench_high_depth_fs_net_admission_control,
    bench_fanout_fanin_rotating_hot_partition,
    bench_session_owner_with_spillover,
    bench_net_burst_flip_imbalance,
    bench_fanin_barrier_micro_batches,
    bench_serial_dep_chain_rpc,
    bench_keyed_hotspot_flip_p99,
    bench_fanin_barrier_rounds,
    bench_wakeup_sparse_event_rtt,
    bench_timer_cancel_reschedule_storm,
    bench_mixed_control_data_plane,
    bench_bounded_pipeline_backpressure,
    bench_post_io_cpu_locality
);
#[cfg(unix)]
criterion_main!(benches);

#[cfg(not(unix))]
fn main() {}
