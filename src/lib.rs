use std::{
    collections::BTreeMap,
    error::Error,
    fmt,
    sync::{
        atomic::{AtomicBool, Ordering, AtomicU64},
        Arc, Condvar, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

pub type Task = fn(u64);
pub type Errors = Option<fn(&dyn Error)>;

#[derive(Debug)]
struct TimerPanic;

impl fmt::Display for TimerPanic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "task panicked")
    }
}

impl Error for TimerPanic {}

pub struct TimerQueue {
    timers: Arc<(Mutex<BTreeMap<Instant, (u64, Duration, Task)>>, Condvar)>,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
    next_id: AtomicU64,
    errors: Errors,
}

impl TimerQueue {
    pub fn new(errors: Errors) -> Self {
        TimerQueue {
            timers: Arc::new((Mutex::new(BTreeMap::new()), Condvar::new())),
            stop: Arc::new(AtomicBool::new(false)),
            thread: None,
            next_id: AtomicU64::new(0),
            errors,
        }
    }

    pub fn start(&mut self) {
        let timers = Arc::clone(&self.timers);
        let stop = Arc::clone(&self.stop);
        let errors = self.errors;
        self.thread = Some(thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let (lock, cvar) = &*timers;
                let mut queue = lock.lock().unwrap();

                if queue.is_empty() {
                    let _queue = cvar.wait(queue).unwrap();
                    continue;
                }

                let now = Instant::now();
                let (expire_time, (id, period, task)) = match queue.iter().next() {
                    Some((&t, v)) if t <= now => (t, v.clone()),
                    Some((&t, _)) => {
                        let _queue = cvar.wait_timeout(queue, t - now).unwrap().0;
                        continue;
                    }
                    None => continue,
                };

                queue.remove(&expire_time);
                if period > Duration::from_millis(0) {
                    let new_time = expire_time + period;
                    queue.insert(new_time, (id, period, task));
                }
                drop(queue);

                let result = std::panic::catch_unwind(|| (task)(id));
                if result.is_err() {
                    if let Some(handler) = errors {
                        handler(&TimerPanic);
                    }
                }
            }
        }));
    }

    pub fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let (_, cvar) = &*self.timers;
        cvar.notify_all();
        if let Some(t) = self.thread.take() {
            t.join().ok();
        }
    }

    pub fn schedule_at(&self, instant: Instant, task: Task) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (lock, cvar) = &*self.timers;
        let mut queue = lock.lock().unwrap();
        queue.insert(instant, (id, Duration::ZERO, task));
        cvar.notify_all();
        id
    }

    pub fn schedule_periodic(&self, period: Duration, task: Task) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let now = Instant::now();
        let (lock, cvar) = &*self.timers;
        let mut queue = lock.lock().unwrap();
        queue.insert(now + period, (id, period, task));
        cvar.notify_all();
        id
    }

    pub fn schedule_custom(&self, first: Instant, period: Duration, task: Task) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (lock, cvar) = &*self.timers;
        let mut queue = lock.lock().unwrap();
        queue.insert(first, (id, period, task));
        cvar.notify_all();
        id
    }

    pub fn contains_id(&self, id: u64) -> bool {
        let (lock, _cvar) = &*self.timers;
        let queue = lock.lock().unwrap();
        queue.iter().any(|(_, &(i, _, _))| i == id)
    }

    pub fn cancel(&self, id: u64) -> bool {
        let (lock, cvar) = &*self.timers;
        let mut queue = lock.lock().unwrap();
        let found = queue.iter().any(|(_, &(i, _, _))| i == id);
        if found {
            queue.retain(|_, &mut (i, _, _)| i != id);
            cvar.notify_all();
            true
        } else {
            false
        }
    }

    pub fn set_periodic(&self, id: u64, new_period: Duration) -> bool {
        let (lock, cvar) = &*self.timers;
        let mut queue = lock.lock().unwrap();
        if let Some((_instant, entry)) = queue.iter_mut().find(|(_, (i, _, _))| *i == id) {
            entry.1 = new_period;
            cvar.notify_all();
            true
        } else {
            false
        }
    }

    pub fn cancel_periodic(&self, id: u64) -> bool {
        self.set_periodic(id, Duration::ZERO)
    }

    pub fn clear(&self) {
        let (lock, _) = &*self.timers;
        lock.lock().unwrap().clear();
    }

    pub fn is_empty(&self) -> bool {
        self.stop.load(Ordering::Relaxed) || self.timers.0.lock().unwrap().is_empty()
    }

    pub fn len(&self) -> usize {
        self.timers.0.lock().unwrap().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;

    static FAST: AtomicU64 = AtomicU64::new(0);
    static SLOW: AtomicU64 = AtomicU64::new(0);

    fn fast_handler(_id: u64) {
        FAST.fetch_add(1, Ordering::Relaxed);
    }

    fn slow_handler(_id: u64) {
        SLOW.fetch_add(1, Ordering::Relaxed);
    }

    fn test_error_handler(err: &dyn Error) {
        println!("Test Error Handler Called: {}", err);
    }

    #[test]
    fn test_timer_queue_with_handler() {
        let timer = TimerQueue::new(Some(test_error_handler));

        assert!(timer.thread.is_none());
        assert!(timer.stop.load(std::sync::atomic::Ordering::Relaxed) == false);
        assert!(timer.errors.is_some());

        // Simulate an error handling call
        if let Some(handler) = timer.errors {
            handler(&TimerPanic);
        }
    }

    #[test]
    fn test_timer_queue_new_without_handler() {
        let timer = TimerQueue::new(None);
        assert!(timer.thread.is_none());
        assert!(!timer.stop.load(Ordering::Relaxed));
        assert!(timer.errors.is_none());
    }

    #[test]
    fn test_timer_startup_and_shutdown() {
        let mut timer = TimerQueue::new(None);
        timer.start();
        assert!(!timer.thread.is_none());
        assert!(!timer.stop.load(Ordering::Relaxed));
        timer.shutdown();
        assert!(timer.stop.load(Ordering::Relaxed));
        assert!(timer.len() == 0);
        assert!(timer.is_empty());
    }

    #[test]
    fn test_timer_run_fast_and_slow() {
        let mut timer = TimerQueue::new(None);
        timer.start();
        let id = timer.schedule_periodic(Duration::from_millis(50), fast_handler);
        timer.schedule_periodic(Duration::from_millis(150), slow_handler);
        thread::sleep(Duration::from_millis(400));
        assert!(timer.len() == 2);
        assert!(timer.contains_id(id));
        timer.cancel(id);
        assert!(!timer.contains_id(id));
        let saved = FAST.load(Ordering::Relaxed);
        let prior = SLOW.load(Ordering::Relaxed);

        thread::sleep(Duration::from_millis(200));
        assert!(FAST.load(Ordering::Relaxed) == saved);
        timer.shutdown();
        assert!(SLOW.load(Ordering::Relaxed) >= 2 && FAST.load(Ordering::Relaxed) > SLOW.load(Ordering::Relaxed) && SLOW.load(Ordering::Relaxed) <= 5);
        assert!(SLOW.load(Ordering::Relaxed) > prior);
    }
}

