#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use std::fmt::Display;
use std::io;
use std::marker::PhantomData;
use std::sync::Arc;
#[cfg(any(test, feature = "benchmarking"))]
use std::sync::atomic::AtomicU64;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};

use crossbeam_queue::ArrayQueue;
use thiserror::Error;
use wren_term::{ClipboardSelection, TerminalBackend};
use wren_view::{DesiredGrid, TerminalUpdate, diff_into};

#[cfg(any(test, feature = "benchmarking"))]
pub type PresentationObserver = Arc<dyn Fn(u64) + Send + Sync + 'static>;

#[derive(Debug, Error)]
pub enum PresenterError {
    #[error("spawn presenter thread: {0}")]
    Spawn(#[source] io::Error),
    #[error("presenter has stopped")]
    Stopped,
    #[error("presenter thread panicked")]
    Panicked,
    #[error("terminal presenter failed: {0}")]
    Backend(String),
}

#[cfg(any(test, feature = "benchmarking"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PresenterStats {
    pub published_frames: u64,
    pub dropped_frames: u64,
    pub presented_frames: u64,
    pub last_presented_epoch: u64,
}

#[cfg(not(any(test, feature = "benchmarking")))]
pub type PresenterStats = ();

/// A fixed three-slot, latest-wins exchange between the input thread and
/// presenter.
///
/// Ownership of a complete grid moves directly through the bounded exchange:
/// no frame `Arc` is allocated or cloned at the input/presenter boundary. A
/// producer discards an unpublished stale grid when all slots are occupied;
/// it never waits for terminal I/O. The presenter drains the exchange and
/// diffs only the highest-epoch complete grid against its last written grid.
#[derive(Debug)]
struct FrameSlotExchange {
    frames: ArrayQueue<DesiredGrid>,
    controls: ArrayQueue<TerminalControl>,
    stopped: AtomicBool,
    #[cfg(any(test, feature = "benchmarking"))]
    published: AtomicU64,
    #[cfg(any(test, feature = "benchmarking"))]
    dropped: AtomicU64,
}

impl Default for FrameSlotExchange {
    fn default() -> Self {
        Self::new(wren_scheduling::RuntimeLimits::default())
    }
}

impl FrameSlotExchange {
    fn new(limits: wren_scheduling::RuntimeLimits) -> Self {
        Self {
            frames: ArrayQueue::new(limits.frame_slots.max(2)),
            controls: ArrayQueue::new(limits.pending_mutations.min(8).max(1)),
            stopped: AtomicBool::new(false),
            #[cfg(any(test, feature = "benchmarking"))]
            published: AtomicU64::new(0),
            #[cfg(any(test, feature = "benchmarking"))]
            dropped: AtomicU64::new(0),
        }
    }
    fn publish(&self, mut frame: DesiredGrid) -> Result<(), PresenterError> {
        if self.stopped.load(Ordering::Acquire) {
            return Err(PresenterError::Stopped);
        }
        loop {
            match self.frames.push(frame) {
                Ok(()) => break,
                Err(returned) => {
                    frame = returned;
                    if self.frames.pop().is_some() {
                        #[cfg(any(test, feature = "benchmarking"))]
                        self.dropped.fetch_add(1, Ordering::AcqRel);
                    }
                }
            }
        }
        #[cfg(any(test, feature = "benchmarking"))]
        self.published.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    fn take(&self) -> Option<DesiredGrid> {
        loop {
            let mut newest = self.frames.pop();
            while let Some(frame) = self.frames.pop() {
                if newest.as_ref().is_none_or(|current| frame.epoch > current.epoch) {
                    if newest.replace(frame).is_some() {
                        #[cfg(any(test, feature = "benchmarking"))]
                        self.dropped.fetch_add(1, Ordering::AcqRel);
                    }
                } else {
                    #[cfg(any(test, feature = "benchmarking"))]
                    self.dropped.fetch_add(1, Ordering::AcqRel);
                }
            }
            if newest.is_some() {
                return newest;
            }
            if self.stopped.load(Ordering::Acquire) {
                return None;
            }
            thread::park();
        }
    }

    fn enqueue_control(&self, control: TerminalControl) -> Result<(), TerminalControl> {
        if self.stopped.load(Ordering::Acquire) {
            return Err(control);
        }
        self.controls.push(control)
    }

    fn take_control(&self) -> Option<TerminalControl> {
        self.controls.pop()
    }

    fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
    }
}

#[derive(Debug)]
enum TerminalControl {
    Clipboard { selection: ClipboardSelection, text: Box<str> },
}

pub struct Presenter<B>
where
    B: TerminalBackend + Send + 'static,
    B::Error: Display + Send + 'static,
{
    queue: Arc<FrameSlotExchange>,
    failed: Arc<AtomicBool>,
    #[cfg(any(test, feature = "benchmarking"))]
    presented: Arc<AtomicU64>,
    #[cfg(any(test, feature = "benchmarking"))]
    last_epoch: Arc<AtomicU64>,
    worker: thread::Thread,
    join: Option<JoinHandle<()>>,
    marker: PhantomData<fn() -> B>,
}

impl<B> Presenter<B>
where
    B: TerminalBackend + Send + 'static,
    B::Error: Display + Send + 'static,
{
    pub fn start(backend: B) -> Result<Self, PresenterError> {
        Self::start_with_limits(backend, wren_scheduling::RuntimeLimits::default())
    }

    pub fn start_with_limits(backend: B, limits: wren_scheduling::RuntimeLimits) -> Result<Self, PresenterError> {
        Self::start_inner(
            backend,
            limits,
            #[cfg(any(test, feature = "benchmarking"))]
            None,
        )
    }

    #[cfg(any(test, feature = "benchmarking"))]
    pub fn start_observed(backend: B, observer: Option<PresentationObserver>) -> Result<Self, PresenterError> {
        Self::start_inner(backend, wren_scheduling::RuntimeLimits::default(), observer)
    }

    fn start_inner(
        mut backend: B,
        limits: wren_scheduling::RuntimeLimits,
        #[cfg(any(test, feature = "benchmarking"))] observer: Option<PresentationObserver>,
    ) -> Result<Self, PresenterError> {
        let queue = Arc::new(FrameSlotExchange::new(limits));
        let failed = Arc::new(AtomicBool::new(false));
        #[cfg(any(test, feature = "benchmarking"))]
        let presented = Arc::new(AtomicU64::new(0));
        #[cfg(any(test, feature = "benchmarking"))]
        let last_epoch = Arc::new(AtomicU64::new(0));
        let thread_queue = Arc::clone(&queue);
        let thread_failed = Arc::clone(&failed);
        #[cfg(any(test, feature = "benchmarking"))]
        let thread_presented = Arc::clone(&presented);
        #[cfg(any(test, feature = "benchmarking"))]
        let thread_last_epoch = Arc::clone(&last_epoch);
        #[cfg(any(test, feature = "benchmarking"))]
        let thread_observer = observer;
        let join = wren_scheduling::spawn_interactive("wren-presenter", move || {
            presenter_loop(
                &mut backend,
                &thread_queue,
                &thread_failed,
                #[cfg(any(test, feature = "benchmarking"))]
                &thread_presented,
                #[cfg(any(test, feature = "benchmarking"))]
                &thread_last_epoch,
                #[cfg(any(test, feature = "benchmarking"))]
                thread_observer.as_ref(),
            );
        })
        .map_err(PresenterError::Spawn)?;
        let worker = join.thread().clone();
        Ok(Self {
            queue,
            failed,
            #[cfg(any(test, feature = "benchmarking"))]
            presented,
            #[cfg(any(test, feature = "benchmarking"))]
            last_epoch,
            worker,
            join: Some(join),
            marker: PhantomData,
        })
    }

    pub fn publish(&self, frame: DesiredGrid) -> Result<(), PresenterError> {
        self.check_failure()?;
        self.queue.publish(frame)?;
        self.worker.unpark();
        Ok(())
    }

    /// Enqueues a bounded terminal control operation for the presenter thread.
    /// This deliberately never waits for a frame write or acquires the
    /// terminal writer from physical input.
    pub fn try_copy_osc52(&self, selection: ClipboardSelection, text: Box<str>) -> Result<(), PresenterError> {
        self.check_failure()?;
        self.queue.enqueue_control(TerminalControl::Clipboard { selection, text }).map_err(|_| PresenterError::Stopped)?;
        self.worker.unpark();
        Ok(())
    }

    pub fn check_failure(&self) -> Result<(), PresenterError> {
        if self.failed.load(Ordering::Acquire) {
            return Err(PresenterError::Backend("terminal presenter failed".to_owned()));
        }
        Ok(())
    }

    #[cfg(any(test, feature = "benchmarking"))]
    pub fn stats(&self) -> Result<PresenterStats, PresenterError> {
        Ok(PresenterStats {
            published_frames: self.queue.published.load(Ordering::Acquire),
            dropped_frames: self.queue.dropped.load(Ordering::Acquire),
            presented_frames: self.presented.load(Ordering::Acquire),
            last_presented_epoch: self.last_epoch.load(Ordering::Acquire),
        })
    }

    pub fn finish(mut self) -> Result<PresenterStats, PresenterError> {
        self.stop_and_join()?;
        self.check_failure()?;
        #[cfg(any(test, feature = "benchmarking"))]
        return self.stats();
        #[cfg(not(any(test, feature = "benchmarking")))]
        Ok(())
    }

    fn stop_and_join(&mut self) -> Result<(), PresenterError> {
        self.queue.stop();
        self.worker.unpark();
        if let Some(join) = self.join.take() {
            join.join().map_err(|_| PresenterError::Panicked)?;
        }
        Ok(())
    }
}

impl<B> Drop for Presenter<B>
where
    B: TerminalBackend + Send + 'static,
    B::Error: Display + Send + 'static,
{
    fn drop(&mut self) {
        let _ = self.stop_and_join();
    }
}

fn presenter_loop<B>(
    backend: &mut B,
    queue: &FrameSlotExchange,
    failed: &AtomicBool,
    #[cfg(any(test, feature = "benchmarking"))] presented: &AtomicU64,
    #[cfg(any(test, feature = "benchmarking"))] last_epoch: &AtomicU64,
    #[cfg(any(test, feature = "benchmarking"))] observer: Option<&PresentationObserver>,
) where
    B: TerminalBackend,
    B::Error: Display,
{
    let mut last_fully_written: Option<DesiredGrid> = None;
    let mut update = TerminalUpdate::default();
    loop {
        while let Some(control) = queue.take_control() {
            let TerminalControl::Clipboard { selection, text } = control;
            if let Err(error) = backend.copy_osc52(selection, &text) {
                store_failure(failed, error);
                queue.stop();
                return;
            }
        }
        let Some(frame) = queue.take() else { return };
        diff_into(last_fully_written.as_ref(), &frame, &mut update);
        let write_result = backend.submit(&update).map_err(|error| error.to_string());
        if let Err(error) = write_result {
            store_failure(failed, error);
            queue.stop();
            return;
        }
        #[cfg(any(test, feature = "benchmarking"))]
        {
            last_epoch.store(frame.epoch, Ordering::Release);
            presented.fetch_add(1, Ordering::AcqRel);
            if let Some(observer) = observer {
                observer(frame.epoch);
            }
        }
        last_fully_written = Some(frame);
    }
}

fn store_failure(failed: &AtomicBool, _error: String) {
    failed.store(true, Ordering::Release);
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;
    use std::time::Duration;

    use wren_view::CellRow;

    use super::*;

    #[derive(Default)]
    struct SlowBackend;

    impl TerminalBackend for SlowBackend {
        type Error = Infallible;

        fn submit(&mut self, _update: &wren_view::TerminalUpdate) -> Result<(), Self::Error> {
            thread::sleep(Duration::from_millis(1));
            Ok(())
        }
    }

    fn grid(epoch: u64) -> DesiredGrid {
        DesiredGrid { epoch, width: 1, height: 1, rows: vec![Arc::new(CellRow::default())], cursor: (0, 0), raster_overlay: None }
    }

    #[test]
    fn three_slot_queue_drops_intermediate_grids_but_writes_the_latest() {
        let backend = SlowBackend;
        let presenter = Presenter::start(backend).expect("presenter");
        for epoch in 1..=100 {
            presenter.publish(grid(epoch)).expect("publish");
        }
        let stats = presenter.finish().expect("finish");
        assert_eq!(stats.published_frames, 100);
        assert!(stats.dropped_frames > 0);
        assert!(stats.presented_frames < stats.published_frames);
        assert_eq!(stats.last_presented_epoch, 100);
    }

    struct BrokenBackend;

    impl TerminalBackend for BrokenBackend {
        type Error = &'static str;

        fn submit(&mut self, _update: &wren_view::TerminalUpdate) -> Result<(), Self::Error> {
            Err("write failed")
        }
    }

    #[test]
    fn backend_failure_stops_without_advancing_fully_written_epoch() {
        let backend = BrokenBackend;
        let presenter = Presenter::start(backend).expect("presenter");
        presenter.publish(grid(7)).expect("publish");
        let error = presenter.finish().expect_err("backend failure");
        assert!(error.to_string().contains("terminal presenter failed"));
    }

    #[test]
    fn observer_runs_only_after_a_frame_is_fully_written() {
        let backend = SlowBackend;
        let epochs = Arc::new(std::sync::Mutex::new(Vec::new()));
        let observed = Arc::clone(&epochs);
        let observer: PresentationObserver = Arc::new(move |epoch| observed.lock().map(|mut epochs| epochs.push(epoch)).unwrap_or_default());
        let presenter = Presenter::start_observed(backend, Some(observer)).expect("presenter");
        presenter.publish(grid(3)).expect("publish");
        presenter.finish().expect("finish");
        assert_eq!(*epochs.lock().expect("epoch observer mutex"), vec![3]);
    }
}
