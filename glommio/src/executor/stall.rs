// Unless explicitly stated otherwise all files in this repository are licensed
// under the MIT/Apache-2.0 License, at your convenience
//
// This product includes software developed at Datadog (https://www.datadoghq.com/). Copyright 2022 Datadog, Inc.
//

use nix::sys;
use std::{
    fmt,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};
use crate::executor::TaskQueueHandle;

pub struct StallDetection<'a> {
    executor: usize,
    queue_handle: TaskQueueHandle,
    queue_name: &'a str,
    trace: backtrace::Backtrace,
    budget: Duration,
    overage: Duration,
}

impl fmt::Debug for StallDetection<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StallDetection")
            .field("executor", &self.executor)
            .field("queue_handle", &self.queue_handle)
            .field("queue_name", &self.queue_name)
            .field("trace", &self.trace)
            .field("budget", &self.budget)
            .field("overage", &self.overage)
            .finish()
    }
}

impl fmt::Display for StallDetection<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "[stall-detector -- executor {}] task queue {} went over-budget: {:#?} (budget: \
             {:#?}). Backtrace: {:#?}",
            self.executor, self.queue_name, self.overage, self.budget, self.trace,
        )
    }
}

/// Trait describing what signal to use to trigger stall detection,
/// how far past expected execution time to trigger a stall,
/// and how to handle a stall detection once triggered.
pub trait StallDetectionHandler: std::fmt::Debug + Send + Sync {
    /// How far past the preemption timer should qualify as a stall
    /// If None is returned, don't use the stall detector for this task queue.
    fn high_water_mark(&self, queue_handle: TaskQueueHandle, max_expected_runtime: Duration) -> Option<Duration>;

    /// What signal number to use; see values in libc::SIG*
    fn signal(&self) -> u8;

    /// Handler called when a task exceeds its budget
    fn stall(&self, detection: StallDetection<'_>);
}

/// Default settings for signal number, high water mark and stall handler.
/// By default, the high water mark to consider a task queue stalled is set to
/// 10% of the expected run time. The default handler will log a stack trace of the currently
/// executing task queue. The default signal number is SIGUSR1.
#[derive(Debug)]
pub struct DefaultStallDetectionHandler {}

impl StallDetectionHandler for DefaultStallDetectionHandler {
    /// The default high water mark is 10% of the preemption time,
    /// capped at 10ms.
    fn high_water_mark(&self, _queue_handle: TaskQueueHandle, max_expected_runtime: Duration) -> Option<Duration> {
        // We consider a queue to be stalling the system if it failed to yield in due
        // time. For a given maximum expected runtime, we allow a margin of error f 10%
        // (and an absolute minimum of 10ms) after which we record a stacktrace. i.e. a
        // task queue has should return shortly after `need_preempt()` returns
        // true or the stall detector triggers. For example::
        // * If a task queue has a preempt timer of 100ms the the stall detector
        // triggers if it doesn't yield after running for 110ms.
        // * If a task queue has a preempt timer of 5ms the the stall detector
        // triggers if it doesn't yield after running for 15ms.
        Some(Duration::from_millis((max_expected_runtime.as_millis() as f64 * 0.1) as u64)
            .max(Duration::from_millis(10)))
    }

    /// The default signal is SIGUSR1.
    fn signal(&self) -> u8 {
        nix::libc::SIGUSR1 as u8
    }

    /// The default stall reporting mechanism is to log a warning.
    fn stall(&self, detection: StallDetection<'_>) {
        log::warn!("{}", detection);
    }
}

#[derive(Debug)]
pub(crate) struct StallDetector {
    timer: Arc<sys::timerfd::TimerFd>,
    stall_handler: Box<dyn StallDetectionHandler + 'static>,
    timer_handler: Option<JoinHandle<()>>,
    id: usize,
    terminated: Arc<AtomicBool>,
    // NOTE: we don't use signal_hook::low_level::channel as backtraces
    // have too many elements
    pub(crate) tx: crossbeam::channel::Sender<backtrace::BacktraceFrame>,
    pub(crate) rx: crossbeam::channel::Receiver<backtrace::BacktraceFrame>,
}

impl StallDetector {
    pub(crate) fn new(
        executor_id: usize,
        stall_handler: Box<dyn StallDetectionHandler + 'static>,
    ) -> std::io::Result<StallDetector> {
        let timer = Arc::new(
            sys::timerfd::TimerFd::new(
                sys::timerfd::ClockId::CLOCK_MONOTONIC,
                sys::timerfd::TimerFlags::empty(),
            )
            .map_err(std::io::Error::from)?,
        );
        let tid = unsafe { nix::libc::pthread_self() };
        let terminated = Arc::new(AtomicBool::new(false));
        let sig = stall_handler.signal();
        let timer_handler = std::thread::spawn(enclose::enclose! { (terminated, timer) move || {
            while timer.wait().is_ok() {
                if terminated.load(Ordering::Relaxed) {
                    return
                }
                unsafe { nix::libc::pthread_kill(tid, sig.into()) };
            }
        }});
        let (tx, rx) = crossbeam::channel::bounded(1 << 10);

        Ok(Self {
            timer,
            timer_handler: Some(timer_handler),
            stall_handler,
            id: executor_id,
            terminated,
            tx,
            rx,
        })
    }

    pub(crate) fn enter_task_queue(
        &self,
        queue_handle: TaskQueueHandle,
        queue_name: String,
        start: Instant,
        max_expected_runtime: Duration,
    ) -> Option<StallDetectorGuard<'_>> {
        self.stall_handler.high_water_mark(queue_handle, max_expected_runtime).map(|hwm| {
            StallDetectorGuard::new(
                self,
                queue_handle,
                queue_name,
                start,
                max_expected_runtime.saturating_add(hwm),
            ).expect("Unable to create StallDetectorGuard, giving up")
        })
    }

    pub(crate) fn arm(&self, threshold: Duration) -> nix::Result<()> {
        self.timer.set(
            sys::timerfd::Expiration::OneShot(sys::time::TimeSpec::from(threshold)),
            sys::timerfd::TimerSetTimeFlags::empty(),
        )
    }

    pub(crate) fn disarm(&self) -> nix::Result<()> {
        self.timer.unset()
    }
}

impl Drop for StallDetector {
    fn drop(&mut self) {
        let timer_handler = self.timer_handler.take().unwrap();
        self.terminated.store(true, Ordering::Relaxed);

        self.timer
            .set(
                sys::timerfd::Expiration::Interval(sys::time::TimeSpec::from(
                    Duration::from_millis(1),
                )),
                sys::timerfd::TimerSetTimeFlags::empty(),
            )
            .expect("failed wake the timer for termination");

        let _ = timer_handler.join();
    }
}

pub(crate) struct StallDetectorGuard<'detector> {
    detector: &'detector StallDetector,
    queue_handle: TaskQueueHandle,
    queue_name: String,
    start: Instant,
    threshold: Duration,
}

impl<'detector> StallDetectorGuard<'detector> {
    fn new(
        detector: &'detector StallDetector,
        queue_handle: TaskQueueHandle,
        queue_name: String,
        start: Instant,
        threshold: Duration,
    ) -> nix::Result<Self> {
        detector.arm(threshold).expect("Unable to arm stall detector, giving up");
        Ok(Self {
            detector,
            queue_handle,
            queue_name,
            start,
            threshold,
        })
    }
}

impl<'detector> Drop for StallDetectorGuard<'detector> {
    fn drop(&mut self) {
        let _ = self.detector.disarm();

        let mut frames = vec![];
        while let Ok(frame) = self.detector.rx.try_recv() {
            frames.push(frame);
        }
        let mut strace = backtrace::Backtrace::from(frames);

        if strace.frames().is_empty() {
            return;
        }

        let elapsed = self.start.elapsed();
        strace.resolve();
        self.detector.stall_handler.stall(StallDetection {
            executor: self.detector.id,
            queue_name: &self.queue_name,
            queue_handle: self.queue_handle,
            trace: strace,
            budget: self.threshold,
            overage: elapsed.saturating_sub(self.threshold),
        });
    }
}

#[cfg(test)]
mod test {
    use crate::{
        executor::stall::DefaultStallDetectionHandler,
        timer::sleep,
        LocalExecutorBuilder,
    };
    use logtest::Logger;
    use std::time::{Duration, Instant};

    enum ExpectedLog {
        Expected(&'static str),
        NotExpected(&'static str),
    }

    fn search_logs_for(logger: &mut Logger, expected: ExpectedLog) -> bool {
        let mut found = false;
        while let Some(event) = logger.pop() {
            match expected {
                ExpectedLog::Expected(str) => found |= event.args().contains(str),
                ExpectedLog::NotExpected(str) => found |= event.args().contains(str),
            }
        }

        match expected {
            ExpectedLog::Expected(_) => found,
            ExpectedLog::NotExpected(_) => !found,
        }
    }

    #[test]
    fn executor_stall_detector() {
        LocalExecutorBuilder::default()
            .detect_stalls(Some(Box::new(DefaultStallDetectionHandler {})))
            .preempt_timer(Duration::from_millis(50))
            .make()
            .unwrap()
            .run(async {
                let mut logger = Logger::start();
                let now = Instant::now();

                // will trigger the stall detector because we go over budget
                while now.elapsed() < Duration::from_millis(100) {}

                assert!(search_logs_for(
                    &mut logger,
                    ExpectedLog::NotExpected("task queue default went over-budget"),
                ));

                crate::executor().yield_task_queue_now().await; // yield the queue

                assert!(search_logs_for(
                    &mut logger,
                    ExpectedLog::Expected("task queue default went over-budget")
                ));

                // no stall because < 50ms of un-cooperativeness
                let now = Instant::now();
                while now.elapsed() < Duration::from_millis(40) {}

                assert!(search_logs_for(
                    &mut logger,
                    ExpectedLog::NotExpected("task queue default went over-budget"),
                ));

                crate::executor().yield_task_queue_now().await; // yield the queue

                // no stall because a timer yields internally
                sleep(Duration::from_millis(100)).await;

                crate::executor().yield_task_queue_now().await; // yield the queue

                assert!(search_logs_for(
                    &mut logger,
                    ExpectedLog::NotExpected("task queue default went over-budget"),
                ));

                // trigger one last time
                let now = Instant::now();
                while now.elapsed() < Duration::from_millis(100) {}

                crate::executor().yield_task_queue_now().await; // yield the queue

                assert!(search_logs_for(
                    &mut logger,
                    ExpectedLog::Expected("task queue default went over-budget")
                ));
            });
    }
}
