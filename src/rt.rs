//! The main loop: one thread owns all UI state; everything else (the X
//! event reader, child process waiters, stderr readers, the IPC listener)
//! runs on helper threads and posts messages here. Timers live in the loop.

use std::any::Any;
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

pub enum Msg {
    X(x11rb::protocol::Event),
    Deliver(u64, Box<dyn Any + Send>),
    Close(u64),
    Ipc(Vec<String>, std::os::unix::net::UnixStream),
    Quit,
}

static TX: OnceLock<Sender<Msg>> = OnceLock::new();

pub fn send(msg: Msg) {
    if let Some(tx) = TX.get() {
        let _ = tx.send(msg);
    }
}

pub fn init() -> Receiver<Msg> {
    let (tx, rx) = mpsc::channel();
    let _ = TX.set(tx);
    rx
}

pub fn quit() {
    send(Msg::Quit);
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct TimerId(u64);

enum TimerFn {
    Once(Box<dyn FnOnce()>),
    Repeat(Box<dyn FnMut() -> bool>, Duration),
}

#[derive(Default)]
struct State {
    next_id: u64,
    timers: HashMap<u64, (Instant, TimerFn)>,
    handlers: HashMap<u64, Box<dyn FnMut(Box<dyn Any + Send>)>>,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

fn next_id() -> u64 {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        s.next_id += 1;
        s.next_id
    })
}

/// Run `f` every `every` until it returns false.
pub fn timeout(every: Duration, f: impl FnMut() -> bool + 'static) -> TimerId {
    let id = next_id();
    STATE.with(|s| s.borrow_mut().timers.insert(id, (Instant::now() + every, TimerFn::Repeat(Box::new(f), every))));
    TimerId(id)
}

pub fn once(after: Duration, f: impl FnOnce() + 'static) -> TimerId {
    let id = next_id();
    STATE.with(|s| s.borrow_mut().timers.insert(id, (Instant::now() + after, TimerFn::Once(Box::new(f)))));
    TimerId(id)
}

pub fn once_ms(ms: u64, f: impl FnOnce() + 'static) -> TimerId {
    once(Duration::from_millis(ms), f)
}

/// Run on the next loop iteration (e.g. to drop an object from inside one
/// of its own handlers).
pub fn idle(f: impl FnOnce() + 'static) {
    once(Duration::ZERO, f);
}

pub fn cancel(id: TimerId) {
    STATE.with(|s| s.borrow_mut().timers.remove(&id.0));
}

struct RemoteInner(u64);

impl Drop for RemoteInner {
    fn drop(&mut self) {
        send(Msg::Close(self.0));
    }
}

/// Sending half of a channel into the main loop; cheap to clone, usable
/// from any thread. The handler is dropped once every clone is gone (after
/// all values sent before that have been delivered).
pub struct Remote<T> {
    inner: Arc<RemoteInner>,
    _t: std::marker::PhantomData<fn(T)>,
}

impl<T> Clone for Remote<T> {
    fn clone(&self) -> Self {
        Remote { inner: self.inner.clone(), _t: std::marker::PhantomData }
    }
}

impl<T: Send + 'static> Remote<T> {
    pub fn send(&self, v: T) {
        send(Msg::Deliver(self.inner.0, Box::new(v)));
    }
}

/// Create a channel whose values are handled on the main thread.
pub fn channel<T: Send + 'static>(mut handler: impl FnMut(T) + 'static) -> Remote<T> {
    let id = next_id();
    let h: Box<dyn FnMut(Box<dyn Any + Send>)> = Box::new(move |v| {
        if let Ok(v) = v.downcast::<T>() {
            handler(*v);
        }
    });
    STATE.with(|s| s.borrow_mut().handlers.insert(id, h));
    Remote { inner: Arc::new(RemoteInner(id)), _t: std::marker::PhantomData }
}

/// Run `work` on a new thread and hand its result to `done` on the main thread.
pub fn background<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static, done: impl FnOnce(T) + 'static) {
    let mut done = Some(done);
    let remote = channel(move |v: T| {
        if let Some(f) = done.take() {
            f(v);
        }
    });
    std::thread::spawn(move || remote.send(work()));
}

fn run_due_timers() -> Option<Instant> {
    loop {
        let now = Instant::now();
        let due = STATE.with(|s| {
            let s = s.borrow();
            s.timers.iter().filter(|(_, (at, _))| *at <= now).min_by_key(|(_, (at, _))| *at).map(|(id, _)| *id)
        });
        let Some(id) = due else { break };
        let Some((_, f)) = STATE.with(|s| s.borrow_mut().timers.remove(&id)) else { continue };
        match f {
            TimerFn::Once(f) => f(),
            TimerFn::Repeat(mut f, every) => {
                if f() {
                    // Keep the id stable so cancel() still works; skip if cancelled meanwhile.
                    STATE.with(|s| s.borrow_mut().timers.insert(id, (Instant::now() + every, TimerFn::Repeat(f, every))));
                }
            }
        }
    }
    STATE.with(|s| s.borrow().timers.values().map(|(at, _)| *at).min())
}

/// Run until `quit()`.
/// `after` runs after every batch of work (to flush the X connection).
pub fn run(
    rx: Receiver<Msg>,
    mut on_x: impl FnMut(x11rb::protocol::Event),
    mut on_ipc: impl FnMut(Vec<String>, std::os::unix::net::UnixStream),
    mut after: impl FnMut(),
) {
    loop {
        let next = run_due_timers();
        after();
        let msg = match next {
            Some(at) => match rx.recv_timeout(at.saturating_duration_since(Instant::now())) {
                Ok(m) => m,
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => return,
            },
            None => match rx.recv() {
                Ok(m) => m,
                Err(_) => return,
            },
        };
        match msg {
            Msg::X(ev) => on_x(ev),
            Msg::Deliver(id, v) => {
                let h = STATE.with(|s| s.borrow_mut().handlers.remove(&id));
                if let Some(mut h) = h {
                    h(v);
                    // Put it back unless it was closed while running.
                    STATE.with(|s| {
                        s.borrow_mut().handlers.entry(id).or_insert(h);
                    });
                }
            }
            Msg::Close(id) => {
                STATE.with(|s| s.borrow_mut().handlers.remove(&id));
            }
            Msg::Ipc(args, stream) => on_ipc(args, stream),
            Msg::Quit => return,
        }
        after();
    }
}
