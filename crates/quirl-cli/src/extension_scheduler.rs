//! Bounded host-side orchestration for trusted extension callbacks.
//!
//! # Concurrency and cleanup failure model
//!
//! Extension input can register many callbacks, callbacks can run until their
//! Lua deadline, prompt generations can arrive faster than they finish, and a
//! callback can still be inside a process host when the terminal session is
//! shutting down. The orchestration invariants are therefore:
//!
//! - exactly [`EXTENSION_WORKER_COUNT`] callback workers and one deadline
//!   monitor are created for a host; plugin cardinality never creates threads;
//! - the shared queue retains at most [`MAX_QUEUED_EXTENSION_WORK`] closures,
//!   and a batch is admitted atomically or rejected with `ResourceLimit`;
//! - every job belongs to one installed runtime generation and has one absolute
//!   aggregate deadline; activating a generation discards queued stale work and
//!   cooperatively cancels stale work that has begun executing;
//! - one scheduler key has at most one claimed job, so callbacks for the same
//!   Lua VM remain FIFO even when an earlier event times out and a later event
//!   is submitted before cooperative cancellation completes;
//! - a worker marks a callback active only after its owning runtime is
//!   exclusively leased, so cancelling a queued callback cannot poison another
//!   callback using the same Lua VM;
//! - at most one current generation plus one claimed generation per worker can
//!   retain runtimes, giving the compile-time
//!   [`MAX_RETAINED_EXTENSION_GENERATIONS`] bound;
//! - callers attach deterministic indices to results and compose them only
//!   after collection; worker completion order never changes plugin order;
//! - callback panics end that job but are caught at the worker boundary, so one
//!   plugin cannot kill a shared worker or strand its in-flight accounting;
//! - shutdown rejects new work, discards the bounded queue, cancels active
//!   callbacks, and waits only for a caller-supplied duration. Workers that do
//!   not acknowledge cancellation are detached with `Arc`-owned state rather
//!   than delaying terminal, child, or persistence cleanup.
//!
//! Callback owners remain responsible for clearing a runtime cancellation flag
//! while holding that runtime's exclusive lease. The scheduler never runs Lua
//! itself and never owns process, terminal, job, or persistence state.

use quirl_core::{ErrorCode, ShellError};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    panic::{self, AssertUnwindSafe},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Condvar, Mutex, MutexGuard,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

pub(crate) const EXTENSION_WORKER_COUNT: usize = 4;
pub(crate) const MAX_QUEUED_EXTENSION_WORK: usize = 64;
pub(crate) const MAX_RETAINED_EXTENSION_GENERATIONS: usize = EXTENSION_WORKER_COUNT + 1;

type CancellationCallback = Arc<dyn Fn() + Send + Sync + 'static>;
type WorkCallback = Box<dyn FnOnce(ExtensionWorkContext) + Send + 'static>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WorkPriority {
    Command,
    Event,
    Prompt,
}

pub(crate) struct ExtensionWork {
    runtime_key: u64,
    pub(crate) callback: WorkCallback,
}

impl ExtensionWork {
    pub(crate) fn new(
        runtime_key: u64,
        callback: impl FnOnce(ExtensionWorkContext) + Send + 'static,
    ) -> Self {
        Self {
            runtime_key,
            callback: Box::new(callback),
        }
    }
}

struct QueuedWork {
    id: u64,
    generation: u64,
    deadline: Instant,
    priority: WorkPriority,
    runtime_key: u64,
    cancelled: Arc<AtomicBool>,
    callback: WorkCallback,
}

struct InFlightWork {
    generation: u64,
    deadline: Instant,
    runtime_key: u64,
    cancelled: Arc<AtomicBool>,
    cancellation: Option<CancellationCallback>,
}

#[derive(Default)]
struct SchedulerState {
    queue: VecDeque<QueuedWork>,
    in_flight: HashMap<u64, InFlightWork>,
    active_runtime_keys: HashSet<u64>,
    latest_generation: u64,
    next_work_id: u64,
    shutdown: bool,
    workers_started: usize,
    workers_exited: usize,
    deadline_monitor_started: bool,
    deadline_monitor_exited: bool,
}

struct SchedulerShared {
    state: Mutex<SchedulerState>,
    work_ready: Condvar,
    state_changed: Condvar,
}

/// IDs for an atomically admitted batch of callback work.
#[derive(Debug)]
pub(crate) struct ExtensionWorkBatch {
    ids: Vec<u64>,
}

/// Cloneable control plane used to await safe points without retaining the
/// scheduler's thread handles or the extension-host mutex.
#[derive(Clone)]
pub(crate) struct ExtensionSchedulerHandle {
    shared: Arc<SchedulerShared>,
}

/// Observable result of a bounded scheduler shutdown attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ExtensionShutdownReport {
    pub(crate) queued_discarded: usize,
    pub(crate) in_flight_cancelled: usize,
    pub(crate) workers_exited: usize,
    pub(crate) workers_started: usize,
    pub(crate) clean: bool,
}

/// Fixed-size callback scheduler shared by event and prompt orchestration.
pub(crate) struct ExtensionScheduler {
    shared: Arc<SchedulerShared>,
    workers: Vec<JoinHandle<()>>,
    deadline_monitor: Option<JoinHandle<()>>,
    startup_error: Option<ShellError>,
}

/// Per-job control used after the callback owner has leased its runtime.
pub(crate) struct ExtensionWorkContext {
    id: u64,
    generation: u64,
    deadline: Instant,
    cancelled: Arc<AtomicBool>,
    shared: Arc<SchedulerShared>,
    finished: bool,
}

impl ExtensionScheduler {
    pub(crate) fn new() -> Self {
        let shared = Arc::new(SchedulerShared {
            state: Mutex::new(SchedulerState::default()),
            work_ready: Condvar::new(),
            state_changed: Condvar::new(),
        });
        let mut scheduler = Self {
            shared,
            workers: Vec::with_capacity(EXTENSION_WORKER_COUNT),
            deadline_monitor: None,
            startup_error: None,
        };
        scheduler.start_threads();
        scheduler
    }

    fn start_threads(&mut self) {
        for index in 0..EXTENSION_WORKER_COUNT {
            let shared = Arc::clone(&self.shared);
            let name = format!("quirl-extension-{index}");
            match thread::Builder::new()
                .name(name)
                .spawn(move || worker_loop(shared))
            {
                Ok(worker) => {
                    lock_recover(&self.shared.state).workers_started += 1;
                    self.workers.push(worker);
                }
                Err(error) => {
                    self.record_startup_error("extension callback worker", error);
                    return;
                }
            }
        }

        let shared = Arc::clone(&self.shared);
        match thread::Builder::new()
            .name("quirl-extension-deadlines".to_owned())
            .spawn(move || deadline_monitor_loop(shared))
        {
            Ok(monitor) => {
                lock_recover(&self.shared.state).deadline_monitor_started = true;
                self.deadline_monitor = Some(monitor);
            }
            Err(error) => self.record_startup_error("extension deadline monitor", error),
        }
    }

    fn record_startup_error(&mut self, component: &str, error: std::io::Error) {
        self.startup_error = Some(
            ShellError::new(
                ErrorCode::ResourceLimit,
                format!("could not start the bounded {component}"),
            )
            .with_context(error.to_string())
            .with_help("Reduce the process thread load, then restart Quirl"),
        );
        let cancellations = {
            let mut state = lock_recover(&self.shared.state);
            state.shutdown = true;
            state
                .in_flight
                .values_mut()
                .filter_map(cancel_in_flight)
                .collect::<Vec<_>>()
        };
        self.shared.work_ready.notify_all();
        self.shared.state_changed.notify_all();
        run_cancellations(cancellations);
    }

    pub(crate) fn take_startup_error(&mut self) -> Option<ShellError> {
        self.startup_error.take()
    }

    pub(crate) fn handle(&self) -> ExtensionSchedulerHandle {
        ExtensionSchedulerHandle {
            shared: Arc::clone(&self.shared),
        }
    }

    pub(crate) fn activate_generation(&self, generation: u64) -> Result<(), ShellError> {
        let cancellations = {
            let mut state = lock_recover(&self.shared.state);
            if state.shutdown {
                return Err(unavailable_scheduler_error());
            }
            if generation < state.latest_generation {
                return Err(ShellError::new(
                    ErrorCode::Validation,
                    "extension runtime generations must not move backwards",
                )
                .with_context(format!(
                    "observed: {generation}; current: {}",
                    state.latest_generation
                ))
                .with_help("Install only a newly validated extension generation"));
            }
            state.latest_generation = generation;
            state.queue.retain(|work| {
                let keep = work.generation == generation;
                if !keep {
                    work.cancelled.store(true, Ordering::Release);
                }
                keep
            });
            let cancellations = state
                .in_flight
                .values_mut()
                .filter(|work| work.generation != generation)
                .filter_map(cancel_in_flight)
                .collect::<Vec<_>>();
            debug_assert!(retained_generation_count(&state) <= MAX_RETAINED_EXTENSION_GENERATIONS);
            cancellations
        };
        self.shared.work_ready.notify_all();
        self.shared.state_changed.notify_all();
        run_cancellations(cancellations);
        Ok(())
    }

    pub(crate) fn submit_batch(
        &self,
        generation: u64,
        deadline: Instant,
        priority: WorkPriority,
        work: Vec<ExtensionWork>,
    ) -> Result<ExtensionWorkBatch, ShellError> {
        if work.len() > MAX_QUEUED_EXTENSION_WORK {
            return Err(queue_limit_error(work.len()));
        }
        if deadline <= Instant::now() {
            return Err(ShellError::new(
                ErrorCode::ResourceLimit,
                "extension callback batch deadline elapsed before admission",
            )
            .with_help("Retry the callback at the next safe extension boundary"));
        }

        let mut state = lock_recover(&self.shared.state);
        if state.shutdown || self.startup_error.is_some() {
            return Err(unavailable_scheduler_error());
        }
        if generation != state.latest_generation {
            return Err(ShellError::new(
                ErrorCode::ResourceLimit,
                "extension callback batch belongs to a stale runtime generation",
            )
            .with_context(format!(
                "observed: {generation}; current: {}",
                state.latest_generation
            ))
            .with_help("Retry using the active extension generation"));
        }
        let observed = state.queue.len().saturating_add(work.len());
        if observed > MAX_QUEUED_EXTENSION_WORK {
            return Err(queue_limit_error(observed));
        }
        let count = u64::try_from(work.len()).map_err(|_| queue_limit_error(work.len()))?;
        let first_id = state
            .next_work_id
            .checked_add(1)
            .ok_or_else(work_id_limit_error)?;
        let last_id = state
            .next_work_id
            .checked_add(count)
            .ok_or_else(work_id_limit_error)?;
        state.next_work_id = last_id;

        let mut queued = Vec::with_capacity(work.len());
        let mut ids = Vec::with_capacity(work.len());
        for (offset, work) in work.into_iter().enumerate() {
            let offset = u64::try_from(offset).map_err(|_| queue_limit_error(offset))?;
            let id = first_id
                .checked_add(offset)
                .ok_or_else(work_id_limit_error)?;
            ids.push(id);
            queued.push(QueuedWork {
                id,
                generation,
                deadline,
                priority,
                runtime_key: work.runtime_key,
                cancelled: Arc::new(AtomicBool::new(false)),
                callback: work.callback,
            });
        }
        match priority {
            WorkPriority::Command => {
                for (offset, work) in queued.into_iter().enumerate() {
                    state.queue.insert(offset, work);
                }
            }
            WorkPriority::Event => {
                let insertion = state
                    .queue
                    .iter()
                    .position(|item| item.priority == WorkPriority::Prompt)
                    .unwrap_or(state.queue.len());
                for (offset, work) in queued.into_iter().enumerate() {
                    state.queue.insert(insertion + offset, work);
                }
            }
            WorkPriority::Prompt => state.queue.extend(queued),
        }
        debug_assert!(state.queue.len() <= MAX_QUEUED_EXTENSION_WORK);
        drop(state);
        self.shared.work_ready.notify_all();
        Ok(ExtensionWorkBatch { ids })
    }

    pub(crate) fn cancel_batch(&self, batch: &ExtensionWorkBatch) -> usize {
        let (cancelled_count, cancellations) = {
            let mut state = lock_recover(&self.shared.state);
            let before = state.queue.len();
            state.queue.retain(|work| {
                let cancelled = batch.ids.contains(&work.id);
                if cancelled {
                    work.cancelled.store(true, Ordering::Release);
                }
                !cancelled
            });
            let mut cancelled_count = before.saturating_sub(state.queue.len());
            let mut cancellations = Vec::new();
            for id in &batch.ids {
                if let Some(work) = state.in_flight.get_mut(id) {
                    cancelled_count = cancelled_count.saturating_add(1);
                    if let Some(cancellation) = cancel_in_flight(work) {
                        cancellations.push(cancellation);
                    }
                }
            }
            (cancelled_count, cancellations)
        };
        self.shared.work_ready.notify_all();
        self.shared.state_changed.notify_all();
        run_cancellations(cancellations);
        cancelled_count
    }

    pub(crate) fn shutdown(&mut self, timeout: Duration) -> ExtensionShutdownReport {
        let (queued_discarded, in_flight_cancelled, cancellations) = {
            let mut state = lock_recover(&self.shared.state);
            state.shutdown = true;
            let queued_discarded = state.queue.len();
            for work in state.queue.drain(..) {
                work.cancelled.store(true, Ordering::Release);
            }
            let in_flight_cancelled = state.in_flight.len();
            let cancellations = state
                .in_flight
                .values_mut()
                .filter_map(cancel_in_flight)
                .collect::<Vec<_>>();
            (queued_discarded, in_flight_cancelled, cancellations)
        };
        self.shared.work_ready.notify_all();
        self.shared.state_changed.notify_all();
        run_cancellations(cancellations);

        let report = {
            let state = lock_recover(&self.shared.state);
            let waited = self
                .shared
                .state_changed
                .wait_timeout_while(state, timeout, |state| {
                    !scheduler_threads_exited(state) || !state.in_flight.is_empty()
                });
            let state = match waited {
                Ok((state, _)) => state,
                Err(poisoned) => poisoned.into_inner().0,
            };
            let clean = scheduler_threads_exited(&state) && state.in_flight.is_empty();
            ExtensionShutdownReport {
                queued_discarded,
                in_flight_cancelled,
                workers_exited: state.workers_exited,
                workers_started: state.workers_started,
                clean,
            }
        };
        if report.clean {
            self.join_finished_threads();
        }
        report
    }

    fn join_finished_threads(&mut self) {
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
        if let Some(monitor) = self.deadline_monitor.take() {
            let _ = monitor.join();
        }
    }
}

impl ExtensionSchedulerHandle {
    pub(crate) fn cancel_generation(&self, generation: u64) -> usize {
        let (cancelled, cancellations) = {
            let mut state = lock_recover(&self.shared.state);
            let before = state.queue.len();
            state.queue.retain(|work| {
                let cancelled = work.generation == generation;
                if cancelled {
                    work.cancelled.store(true, Ordering::Release);
                }
                !cancelled
            });
            let mut cancelled = before.saturating_sub(state.queue.len());
            let mut cancellations = Vec::new();
            for work in state
                .in_flight
                .values_mut()
                .filter(|work| work.generation == generation)
            {
                cancelled = cancelled.saturating_add(1);
                if let Some(cancellation) = cancel_in_flight(work) {
                    cancellations.push(cancellation);
                }
            }
            (cancelled, cancellations)
        };
        self.shared.work_ready.notify_all();
        self.shared.state_changed.notify_all();
        run_cancellations(cancellations);
        cancelled
    }

    pub(crate) fn wait_generation_idle(&self, generation: u64, timeout: Duration) -> bool {
        let state = lock_recover(&self.shared.state);
        let waited = self
            .shared
            .state_changed
            .wait_timeout_while(state, timeout, |state| {
                generation_is_retained(state, generation)
            });
        match waited {
            Ok((state, _)) => !generation_is_retained(&state, generation),
            Err(poisoned) => !generation_is_retained(&poisoned.into_inner().0, generation),
        }
    }
}

impl Drop for ExtensionScheduler {
    fn drop(&mut self) {
        let _ = self.shutdown(Duration::from_millis(25));
        // Any remaining handles are deliberately detached. Every thread and
        // callback owns the shared state it still needs through `Arc`.
        self.workers.clear();
        self.deadline_monitor.take();
    }
}

impl ExtensionWorkContext {
    pub(crate) fn begin(&self, cancellation: CancellationCallback) -> bool {
        let mut state = lock_recover(&self.shared.state);
        let current_generation = state.latest_generation;
        let shutdown = state.shutdown;
        let Some(work) = state.in_flight.get_mut(&self.id) else {
            self.cancelled.store(true, Ordering::Release);
            return false;
        };
        let admitted = !shutdown
            && self.generation == current_generation
            && !self.cancelled.load(Ordering::Acquire)
            && Instant::now() < self.deadline;
        if admitted {
            work.cancellation = Some(cancellation);
            self.shared.state_changed.notify_all();
        } else {
            self.cancelled.store(true, Ordering::Release);
        }
        admitted
    }

    pub(crate) fn deadline(&self) -> Instant {
        self.deadline
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    pub(crate) fn finish(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        let mut state = lock_recover(&self.shared.state);
        if let Some(work) = state.in_flight.remove(&self.id) {
            state.active_runtime_keys.remove(&work.runtime_key);
        }
        self.shared.state_changed.notify_all();
        self.shared.work_ready.notify_all();
    }
}

impl Drop for ExtensionWorkContext {
    fn drop(&mut self) {
        self.finish();
    }
}

fn worker_loop(shared: Arc<SchedulerShared>) {
    loop {
        let work = {
            let mut state = lock_recover(&shared.state);
            while !state.shutdown
                && !state
                    .queue
                    .iter()
                    .any(|work| !state.active_runtime_keys.contains(&work.runtime_key))
            {
                state = match shared.work_ready.wait(state) {
                    Ok(state) => state,
                    Err(poisoned) => poisoned.into_inner(),
                };
            }
            if state.shutdown {
                state.workers_exited = state.workers_exited.saturating_add(1);
                shared.state_changed.notify_all();
                return;
            }
            let Some(position) = state
                .queue
                .iter()
                .position(|work| !state.active_runtime_keys.contains(&work.runtime_key))
            else {
                continue;
            };
            let Some(work) = state.queue.remove(position) else {
                continue;
            };
            state.active_runtime_keys.insert(work.runtime_key);
            state.in_flight.insert(
                work.id,
                InFlightWork {
                    generation: work.generation,
                    deadline: work.deadline,
                    runtime_key: work.runtime_key,
                    cancelled: Arc::clone(&work.cancelled),
                    cancellation: None,
                },
            );
            debug_assert!(state.in_flight.len() <= EXTENSION_WORKER_COUNT);
            work
        };
        let context = ExtensionWorkContext {
            id: work.id,
            generation: work.generation,
            deadline: work.deadline,
            cancelled: work.cancelled,
            shared: Arc::clone(&shared),
            finished: false,
        };
        let _ = panic::catch_unwind(AssertUnwindSafe(|| (work.callback)(context)));
    }
}

fn deadline_monitor_loop(shared: Arc<SchedulerShared>) {
    loop {
        let cancellations = {
            let mut state = lock_recover(&shared.state);
            if state.shutdown {
                state.deadline_monitor_exited = true;
                shared.state_changed.notify_all();
                return;
            }
            let now = Instant::now();
            let next_deadline = state
                .in_flight
                .values()
                .filter(|work| work.cancellation.is_some())
                .map(|work| work.deadline)
                .min();
            match next_deadline {
                Some(deadline) if deadline <= now => state
                    .in_flight
                    .values_mut()
                    .filter(|work| work.deadline <= now && work.cancellation.is_some())
                    .filter_map(cancel_in_flight)
                    .collect::<Vec<_>>(),
                Some(deadline) => {
                    let duration = deadline.saturating_duration_since(now);
                    let waited = shared.state_changed.wait_timeout(state, duration);
                    drop(match waited {
                        Ok((state, _)) => state,
                        Err(poisoned) => poisoned.into_inner().0,
                    });
                    continue;
                }
                None => {
                    drop(match shared.state_changed.wait(state) {
                        Ok(state) => state,
                        Err(poisoned) => poisoned.into_inner(),
                    });
                    continue;
                }
            }
        };
        run_cancellations(cancellations);
    }
}

fn cancel_in_flight(work: &mut InFlightWork) -> Option<CancellationCallback> {
    work.cancelled.store(true, Ordering::Release);
    work.cancellation.take()
}

fn run_cancellations(cancellations: Vec<CancellationCallback>) {
    for cancellation in cancellations {
        cancellation();
    }
}

fn retained_generation_count(state: &SchedulerState) -> usize {
    let mut generations = Vec::with_capacity(EXTENSION_WORKER_COUNT + 1);
    generations.push(state.latest_generation);
    for work in state.in_flight.values() {
        if !generations.contains(&work.generation) {
            generations.push(work.generation);
        }
    }
    generations.len()
}

fn generation_is_retained(state: &SchedulerState, generation: u64) -> bool {
    state.queue.iter().any(|work| work.generation == generation)
        || state
            .in_flight
            .values()
            .any(|work| work.generation == generation)
}

fn scheduler_threads_exited(state: &SchedulerState) -> bool {
    let workers_exited = state.workers_exited == state.workers_started;
    let monitor_exited = !state.deadline_monitor_started || state.deadline_monitor_exited;
    workers_exited && monitor_exited
}

fn queue_limit_error(observed: usize) -> ShellError {
    ShellError::new(
        ErrorCode::ResourceLimit,
        "extension callback queue capacity was exceeded",
    )
    .with_context(format!(
        "queued work: {observed}; limit: {MAX_QUEUED_EXTENSION_WORK}"
    ))
    .with_help("Reduce enabled extension callbacks or wait for the current turn to finish")
}

fn work_id_limit_error() -> ShellError {
    ShellError::new(
        ErrorCode::ResourceLimit,
        "extension callback generation counter was exhausted",
    )
    .with_help("Restart Quirl to create a fresh bounded extension scheduler")
}

fn unavailable_scheduler_error() -> ShellError {
    ShellError::new(
        ErrorCode::ResourceLimit,
        "the bounded extension callback scheduler is unavailable",
    )
    .with_help("Restart Quirl after reducing process or extension resource pressure")
}

fn lock_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{atomic::AtomicUsize, mpsc};

    struct Gate {
        open: Mutex<bool>,
        changed: Condvar,
    }

    impl Gate {
        fn closed() -> Self {
            Self {
                open: Mutex::new(false),
                changed: Condvar::new(),
            }
        }

        fn wait(&self) {
            let mut open = lock_recover(&self.open);
            while !*open {
                open = match self.changed.wait(open) {
                    Ok(open) => open,
                    Err(poisoned) => poisoned.into_inner(),
                };
            }
        }

        fn release(&self) {
            *lock_recover(&self.open) = true;
            self.changed.notify_all();
        }
    }

    fn future_deadline() -> Instant {
        Instant::now().checked_add(Duration::from_secs(5)).unwrap()
    }

    fn begin_unchecked(control: &ExtensionWorkContext) {
        assert!(control.begin(Arc::new(|| {})));
    }

    #[test]
    fn one_runtime_keeps_fifo_order_across_the_shared_workers() {
        let mut scheduler = ExtensionScheduler::new();
        assert!(scheduler.take_startup_error().is_none());
        scheduler.activate_generation(1).unwrap();
        let gate = Arc::new(Gate::closed());
        let order = Arc::new(Mutex::new(Vec::new()));
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let (finished_tx, finished_rx) = mpsc::sync_channel(1);

        let first_gate = Arc::clone(&gate);
        let first_order = Arc::clone(&order);
        let first = ExtensionWork::new(7, move |control| {
            begin_unchecked(&control);
            started_tx.send(()).unwrap();
            first_gate.wait();
            lock_recover(&first_order).push(1_u8);
        });
        let second_order = Arc::clone(&order);
        let second = ExtensionWork::new(7, move |control| {
            begin_unchecked(&control);
            lock_recover(&second_order).push(2_u8);
            finished_tx.send(()).unwrap();
        });
        scheduler
            .submit_batch(
                1,
                future_deadline(),
                WorkPriority::Event,
                vec![first, second],
            )
            .unwrap();

        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(matches!(
            finished_rx.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        gate.release();
        finished_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(scheduler
            .handle()
            .wait_generation_idle(1, Duration::from_secs(1)));
        assert_eq!(*lock_recover(&order), vec![1, 2]);
    }

    #[test]
    fn exact_queue_capacity_is_admitted_and_capacity_plus_one_is_rejected() {
        let mut scheduler = ExtensionScheduler::new();
        assert!(scheduler.take_startup_error().is_none());
        scheduler.activate_generation(1).unwrap();
        let gate = Arc::new(Gate::closed());
        let (started_tx, started_rx) = mpsc::sync_channel(EXTENSION_WORKER_COUNT);
        let mut active = Vec::new();
        for key in 1..=EXTENSION_WORKER_COUNT {
            let gate = Arc::clone(&gate);
            let started_tx = started_tx.clone();
            active.push(ExtensionWork::new(
                u64::try_from(key).unwrap(),
                move |control| {
                    begin_unchecked(&control);
                    started_tx.send(()).unwrap();
                    gate.wait();
                },
            ));
        }
        scheduler
            .submit_batch(1, future_deadline(), WorkPriority::Prompt, active)
            .unwrap();
        for _ in 0..EXTENSION_WORKER_COUNT {
            started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        }

        let queued = (0..MAX_QUEUED_EXTENSION_WORK)
            .map(|_| ExtensionWork::new(1, |_| {}))
            .collect();
        let queued_batch = scheduler
            .submit_batch(1, future_deadline(), WorkPriority::Prompt, queued)
            .unwrap();
        let error = scheduler
            .submit_batch(
                1,
                future_deadline(),
                WorkPriority::Event,
                vec![ExtensionWork::new(1, |_| {})],
            )
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        scheduler.cancel_batch(&queued_batch);
        gate.release();
        assert!(scheduler
            .handle()
            .wait_generation_idle(1, Duration::from_secs(1)));
    }

    #[test]
    fn activating_a_generation_removes_stale_queued_work() {
        let mut scheduler = ExtensionScheduler::new();
        assert!(scheduler.take_startup_error().is_none());
        scheduler.activate_generation(1).unwrap();
        let gate = Arc::new(Gate::closed());
        let stale_ran = Arc::new(AtomicUsize::new(0));
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let active_gate = Arc::clone(&gate);
        let active = ExtensionWork::new(9, move |control| {
            begin_unchecked(&control);
            started_tx.send(()).unwrap();
            active_gate.wait();
        });
        let stale_count = Arc::clone(&stale_ran);
        let queued = ExtensionWork::new(9, move |_| {
            stale_count.fetch_add(1, Ordering::Relaxed);
        });
        scheduler
            .submit_batch(
                1,
                future_deadline(),
                WorkPriority::Prompt,
                vec![active, queued],
            )
            .unwrap();
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();

        scheduler.activate_generation(2).unwrap();
        let (new_tx, new_rx) = mpsc::sync_channel(1);
        scheduler
            .submit_batch(
                2,
                future_deadline(),
                WorkPriority::Prompt,
                vec![ExtensionWork::new(10, move |control| {
                    begin_unchecked(&control);
                    new_tx.send(()).unwrap();
                })],
            )
            .unwrap();
        new_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        gate.release();
        assert!(scheduler
            .handle()
            .wait_generation_idle(1, Duration::from_secs(1)));
        assert_eq!(stale_ran.load(Ordering::Relaxed), 0);
        assert_eq!(
            MAX_RETAINED_EXTENSION_GENERATIONS,
            EXTENSION_WORKER_COUNT + 1
        );
    }

    #[test]
    fn callback_panic_releases_the_runtime_fifo_for_later_work() {
        let mut scheduler = ExtensionScheduler::new();
        assert!(scheduler.take_startup_error().is_none());
        scheduler.activate_generation(1).unwrap();
        let (finished_tx, finished_rx) = mpsc::sync_channel(1);
        scheduler
            .submit_batch(
                1,
                future_deadline(),
                WorkPriority::Event,
                vec![
                    ExtensionWork::new(12, |control| {
                        begin_unchecked(&control);
                        panic!("modeled callback panic");
                    }),
                    ExtensionWork::new(12, move |control| {
                        begin_unchecked(&control);
                        finished_tx.send(()).unwrap();
                    }),
                ],
            )
            .unwrap();
        finished_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(scheduler
            .handle()
            .wait_generation_idle(1, Duration::from_secs(1)));
    }

    #[test]
    fn expired_queued_work_never_invokes_its_callback() {
        let mut scheduler = ExtensionScheduler::new();
        assert!(scheduler.take_startup_error().is_none());
        scheduler.activate_generation(1).unwrap();
        let gate = Arc::new(Gate::closed());
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let worker_gate = Arc::clone(&gate);
        let expired_ran = Arc::new(AtomicBool::new(false));
        scheduler
            .submit_batch(
                1,
                future_deadline(),
                WorkPriority::Prompt,
                vec![ExtensionWork::new(13, move |control| {
                    begin_unchecked(&control);
                    started_tx.send(()).unwrap();
                    worker_gate.wait();
                })],
            )
            .unwrap();
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let expired_flag = Arc::clone(&expired_ran);
        scheduler
            .submit_batch(
                1,
                Instant::now()
                    .checked_add(Duration::from_millis(10))
                    .unwrap(),
                WorkPriority::Prompt,
                vec![ExtensionWork::new(13, move |control| {
                    if control.begin(Arc::new(|| {})) {
                        expired_flag.store(true, Ordering::Relaxed);
                    }
                })],
            )
            .unwrap();
        thread::sleep(Duration::from_millis(25));
        gate.release();
        assert!(scheduler
            .handle()
            .wait_generation_idle(1, Duration::from_secs(1)));
        assert!(!expired_ran.load(Ordering::Relaxed));
    }

    #[test]
    fn blocked_provider_makes_shutdown_incomplete_without_blocking_the_caller() {
        let mut scheduler = ExtensionScheduler::new();
        assert!(scheduler.take_startup_error().is_none());
        scheduler.activate_generation(1).unwrap();
        let gate = Arc::new(Gate::closed());
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let worker_gate = Arc::clone(&gate);
        scheduler
            .submit_batch(
                1,
                future_deadline(),
                WorkPriority::Prompt,
                vec![ExtensionWork::new(15, move |control| {
                    begin_unchecked(&control);
                    started_tx.send(()).unwrap();
                    worker_gate.wait();
                })],
            )
            .unwrap();
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();

        let blocked = scheduler.shutdown(Duration::ZERO);
        assert!(!blocked.clean);
        assert_eq!(blocked.in_flight_cancelled, 1);
        gate.release();
        let clean = scheduler.shutdown(Duration::from_secs(1));
        assert!(clean.clean);
        assert_eq!(clean.workers_exited, clean.workers_started);
    }

    #[test]
    fn blocked_callback_cannot_cross_a_host_commit_safe_point() {
        let mut scheduler = ExtensionScheduler::new();
        assert!(scheduler.take_startup_error().is_none());
        scheduler.activate_generation(1).unwrap();
        let gate = Arc::new(Gate::closed());
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let worker_gate = Arc::clone(&gate);
        scheduler
            .submit_batch(
                1,
                future_deadline(),
                WorkPriority::Prompt,
                vec![ExtensionWork::new(19, move |control| {
                    begin_unchecked(&control);
                    started_tx.send(()).unwrap();
                    worker_gate.wait();
                })],
            )
            .unwrap();
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();

        let handle = scheduler.handle();
        handle.cancel_generation(1);
        let mut commit_marker = false;
        if handle.wait_generation_idle(1, Duration::ZERO) {
            commit_marker = true;
        }
        assert!(!commit_marker);
        gate.release();
        assert!(handle.wait_generation_idle(1, Duration::from_secs(1)));
    }
}
