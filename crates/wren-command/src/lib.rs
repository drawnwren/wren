#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
#[cfg(any(test, feature = "benchmarking"))]
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use thiserror::Error;
use wren_types::{CommandTask, CommandTaskId, DocumentId, Effects};

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum TaskFailure {
    #[error("task was cancelled")]
    Cancelled,
    #[error("task failed: {0}")]
    Failed(Box<str>),
    #[error("task panicked")]
    Panicked,
}

#[derive(Debug, Error)]
pub enum TaskRunnerError {
    #[error("task queue is full")]
    QueueFull,
    #[error("spawn task worker: {0}")]
    Spawn(Box<str>),
    #[error("task ID {0:?} is already running")]
    DuplicateTask(CommandTaskId),
}

#[derive(Debug, Clone, Default)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
}

impl CancellationToken {
    #[must_use]
    pub fn new() -> Self {
        Self { cancelled: Arc::new(AtomicBool::new(false)) }
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

#[derive(Debug)]
pub struct TaskContext {
    cancellation: CancellationToken,
    #[cfg(any(test, feature = "benchmarking"))]
    last_checkpoint: Instant,
    #[cfg(any(test, feature = "benchmarking"))]
    max_checkpoint_gap: Duration,
    #[cfg(any(test, feature = "benchmarking"))]
    checkpoints: u64,
}

impl TaskContext {
    fn new(cancellation: CancellationToken) -> Self {
        Self {
            cancellation,
            #[cfg(any(test, feature = "benchmarking"))]
            last_checkpoint: Instant::now(),
            #[cfg(any(test, feature = "benchmarking"))]
            max_checkpoint_gap: Duration::ZERO,
            #[cfg(any(test, feature = "benchmarking"))]
            checkpoints: 0,
        }
    }

    pub fn checkpoint(&mut self) -> Result<(), TaskFailure> {
        self.checkpoint_with(std::thread::yield_now)
    }

    fn checkpoint_with(&mut self, yield_worker: impl FnOnce()) -> Result<(), TaskFailure> {
        #[cfg(any(test, feature = "benchmarking"))]
        {
            let now = Instant::now();
            self.max_checkpoint_gap = self.max_checkpoint_gap.max(now.saturating_duration_since(self.last_checkpoint));
            self.checkpoints = self.checkpoints.saturating_add(1);
        }
        if self.cancellation.is_cancelled() {
            return Err(TaskFailure::Cancelled);
        }
        yield_worker();
        // A scheduler stall after yielding is time made available to the UI,
        // not time during which this task withheld its next checkpoint.
        #[cfg(any(test, feature = "benchmarking"))]
        {
            self.last_checkpoint = Instant::now();
        }
        Ok(())
    }

    #[must_use]
    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancellation.clone()
    }
}

#[derive(Debug)]
pub struct TaskResult {
    pub task: CommandTask,
    pub outcome: Result<Effects, TaskFailure>,
    #[cfg(feature = "benchmarking")]
    pub elapsed: Duration,
    #[cfg(feature = "benchmarking")]
    pub max_checkpoint_gap: Duration,
    #[cfg(any(test, feature = "benchmarking"))]
    pub checkpoints: u64,
}

#[derive(Debug, Default)]
struct BarrierState {
    tasks: HashMap<CommandTaskId, Vec<DocumentId>>,
    documents: HashMap<DocumentId, usize>,
}

pub struct TaskRunner {
    pool: rayon::ThreadPool,
    results: Receiver<TaskResult>,
    result_sender: SyncSender<TaskResult>,
    barriers: Arc<Mutex<BarrierState>>,
    pending: Arc<AtomicUsize>,
    capacity: usize,
}

impl TaskRunner {
    pub fn new(worker_count: usize, queue_capacity: usize) -> Result<Self, TaskRunnerError> {
        let workers = worker_count.max(1);
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(workers)
            .thread_name(|index| format!("wren-command-{index}"))
            .start_handler(|_| wren_scheduling::mark_background())
            .build()
            .map_err(|error| TaskRunnerError::Spawn(error.to_string().into()))?;
        let capacity = workers.saturating_add(queue_capacity.max(1));
        let (result_sender, results) = mpsc::sync_channel(capacity);
        Ok(Self { pool, results, result_sender, barriers: Arc::new(Mutex::new(BarrierState::default())), pending: Arc::new(AtomicUsize::new(0)), capacity })
    }

    pub fn submit(
        &self,
        task: CommandTask,
        work: impl FnOnce(&mut TaskContext) -> Result<Effects, TaskFailure> + Send + 'static,
    ) -> Result<CancellationToken, TaskRunnerError> {
        let mut barriers = self.barriers.lock();
        if barriers.tasks.contains_key(&task.task_id) {
            return Err(TaskRunnerError::DuplicateTask(task.task_id));
        }
        self.pending
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |pending| (pending < self.capacity).then_some(pending + 1))
            .map_err(|_| TaskRunnerError::QueueFull)?;
        for document_id in &task.affected_documents {
            *barriers.documents.entry(*document_id).or_default() += 1;
        }
        barriers.tasks.insert(task.task_id, task.affected_documents.clone());
        drop(barriers);
        let cancellation = CancellationToken::new();
        let worker_cancellation = cancellation.clone();
        let barriers = Arc::clone(&self.barriers);
        let pending = Arc::clone(&self.pending);
        let results = self.result_sender.clone();
        self.pool.spawn(move || {
            #[cfg(feature = "benchmarking")]
            let started = Instant::now();
            let mut context = TaskContext::new(worker_cancellation);
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work(&mut context))).unwrap_or(Err(TaskFailure::Panicked));
            release_barrier(&barriers, task.task_id);
            pending.fetch_sub(1, Ordering::AcqRel);
            let _ = results.send(TaskResult {
                task,
                outcome,
                #[cfg(feature = "benchmarking")]
                elapsed: started.elapsed(),
                #[cfg(feature = "benchmarking")]
                max_checkpoint_gap: context.max_checkpoint_gap,
                #[cfg(any(test, feature = "benchmarking"))]
                checkpoints: context.checkpoints,
            });
        });
        Ok(cancellation)
    }

    pub fn try_result(&self) -> Option<TaskResult> {
        self.results.try_recv().ok()
    }

    #[must_use]
    pub fn is_document_blocked(&self, document_id: DocumentId) -> bool {
        self.barriers.lock().documents.contains_key(&document_id)
    }
}

fn release_barrier(barriers: &Mutex<BarrierState>, task_id: CommandTaskId) {
    release_barrier_state(&mut barriers.lock(), task_id);
}

fn release_barrier_state(barriers: &mut BarrierState, task_id: CommandTaskId) {
    let Some(documents) = barriers.tasks.remove(&task_id) else {
        return;
    };
    for document_id in documents {
        if let Some(count) = barriers.documents.get_mut(&document_id) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                barriers.documents.remove(&document_id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::sync::mpsc;
    use std::thread;

    use super::*;

    fn task(id: u64) -> CommandTask {
        CommandTask { task_id: CommandTaskId::new(id), affected_documents: vec![DocumentId::new(4)], label: "test".into() }
    }

    fn wait_result(runner: &TaskRunner) -> TaskResult {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(result) = runner.try_result() {
                return result;
            }
            assert!(Instant::now() < deadline, "task timed out");
            thread::yield_now();
        }
    }

    #[test]
    fn task_runs_off_thread_and_document_barrier_clears_on_completion() {
        let runner = TaskRunner::new(1, 2).expect("runner");
        let (started, wait) = mpsc::channel();
        let (release, released) = mpsc::channel();
        runner
            .submit(task(1), move |context| {
                started.send(()).expect("started");
                released.recv().expect("release");
                context.checkpoint()?;
                Ok(Effects { messages: vec!["complete".into()], ..Effects::default() })
            })
            .expect("submit");
        wait.recv().expect("task started");
        assert!(runner.is_document_blocked(DocumentId::new(4)));
        release.send(()).expect("release task");
        let result = wait_result(&runner);
        assert_eq!(result.outcome.expect("success").messages, vec![Box::<str>::from("complete")]);
        assert!(!runner.is_document_blocked(DocumentId::new(4)));
    }

    #[test]
    fn checkpoint_gap_restarts_after_the_worker_yields() {
        let mut context = TaskContext::new(CancellationToken::new());
        let yielded_at = Cell::new(None);
        context.checkpoint_with(|| yielded_at.set(Some(Instant::now()))).expect("checkpoint");

        assert!(context.last_checkpoint >= yielded_at.get().expect("yield timestamp"));
    }

    #[test]
    fn cooperative_cancellation_is_reported_and_releases_barrier() {
        let runner = TaskRunner::new(1, 2).expect("runner");
        let (started, wait) = mpsc::channel();
        let cancellation = runner
            .submit(task(2), move |context| {
                started.send(()).expect("started");
                loop {
                    context.checkpoint()?;
                }
            })
            .expect("submit");
        wait.recv().expect("task started");
        cancellation.cancel();
        let result = wait_result(&runner);
        assert_eq!(result.outcome, Err(TaskFailure::Cancelled));
        assert!(result.checkpoints > 0);
        assert!(!runner.is_document_blocked(DocumentId::new(4)));
    }

    #[test]
    fn panics_are_contained_at_the_task_failure_boundary() {
        let runner = TaskRunner::new(1, 1).expect("runner");
        runner.submit(task(3), |_| panic!("provider bug")).expect("submit");
        let result = wait_result(&runner);
        assert_eq!(result.outcome, Err(TaskFailure::Panicked));
    }
}
