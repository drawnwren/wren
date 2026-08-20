//! Safe, cross-platform scheduling classes for Wren-owned threads.

use std::io;
use std::sync::mpsc::{self, Receiver};
use std::thread::{self, JoinHandle};

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
