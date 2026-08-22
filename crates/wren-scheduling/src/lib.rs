//! Safe, cross-platform scheduling classes for Wren-owned threads.

use std::io;
use std::sync::mpsc::{self, Receiver};
use std::thread::{self, JoinHandle};

/// The process-wide admission budget for bounded asynchronous lanes. These
/// values are intentionally product limits rather than incidental channel
/// literals: callers may derive a profile at startup and expose it in the
/// debug UI, while every producer has an explicit overload policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeLimits {
    pub frame_slots: usize,
    pub pending_mutations: usize,
    pub provider_revision_slots: usize,
    pub provider_demand_documents: usize,
    pub task_slots: usize,
    pub provider_snapshot_bytes: usize,
    pub bulk_chunk_bytes: usize,
    pub control_frame_bytes: usize,
    pub retained_row_bytes: usize,
}

impl RuntimeLimits {
    #[must_use]
    pub fn for_terminal(columns: usize, rows: usize) -> Self {
        let mut limits = Self::default();
        // Three retained frames plus a modest amount of row-cache headroom.
        // Cells currently occupy more than one byte, so this is a byte budget
        // for admission/reporting rather than a packed-memory assumption.
        limits.retained_row_bytes = columns.max(1).saturating_mul(rows.max(1)).saturating_mul(3).saturating_mul(32);
        limits
    }
}

impl Default for RuntimeLimits {
    fn default() -> Self {
        Self {
            frame_slots: 3,
            pending_mutations: 64,
            provider_revision_slots: 8,
            provider_demand_documents: 8,
            task_slots: 9,
            provider_snapshot_bytes: 16 * 1024 * 1024,
            bulk_chunk_bytes: 256 * 1024,
            control_frame_bytes: 64 * 1024,
            retained_row_bytes: 80 * 24 * 3 * 32,
        }
    }
}

/// Marks the calling thread as latency-sensitive foreground work.
pub fn mark_interactive() {
    let _ = qos_threads::set_current_thread(qos_threads::Qos::High);

    #[cfg(target_vendor = "apple")]
    objc2_foundation::NSThread::currentThread().setQualityOfService(objc2_foundation::NSQualityOfService::UserInteractive);
}

/// Marks the calling thread as throughput-oriented background work.
pub fn mark_background() {
    let _ = qos_threads::set_current_thread(qos_threads::Qos::Low);
}

/// Spawns owned work with its scheduling class installed before user code can
/// run. Keeping this boundary here prevents worker call sites from forgetting
/// the product's foreground/background policy.
pub fn spawn_background<T: Send + 'static>(name: impl Into<String>, task: impl FnOnce() -> T + Send + 'static) -> io::Result<JoinHandle<T>> {
    thread::Builder::new().name(name.into()).spawn(move || {
        mark_background();
        task()
    })
}

pub fn spawn_interactive<T: Send + 'static>(name: impl Into<String>, task: impl FnOnce() -> T + Send + 'static) -> io::Result<JoinHandle<T>> {
    thread::Builder::new().name(name.into()).spawn(move || {
        mark_interactive();
        task()
    })
}

/// Starts a detached calculation and exposes its single result as a channel.
/// This is the common ownership boundary for UI jobs: dropping the receiver
/// detaches the calculation, while completion never blocks on a stale caller.
pub fn spawn_background_result<T: Send + 'static>(name: impl Into<String>, task: impl FnOnce() -> T + Send + 'static) -> io::Result<Receiver<T>> {
    let (sender, receiver) = mpsc::channel();
    spawn_background(name, move || {
        let _ = sender.send(task());
    })?;
    Ok(receiver)
}
