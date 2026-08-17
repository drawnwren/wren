//! Safe, cross-platform scheduling classes for Wren-owned threads.

/// Marks the calling thread as latency-sensitive foreground work.
pub fn mark_interactive() {
    #[cfg(target_vendor = "apple")]
    objc2_foundation::NSThread::currentThread()
        .setQualityOfService(objc2_foundation::NSQualityOfService::UserInteractive);

    #[cfg(not(target_vendor = "apple"))]
    let _ = qos_threads::set_current_thread(qos_threads::Qos::High);
}

/// Marks the calling thread as throughput-oriented background work.
pub fn mark_background() {
    let _ = qos_threads::set_current_thread(qos_threads::Qos::Low);
}
