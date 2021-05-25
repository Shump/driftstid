use iou::IoUring;
use std::future::Future;
use std::task::{Context, RawWaker, Poll, Waker};
use std::collections::{HashMap, VecDeque};
use std::pin::Pin;
use std::cell::RefCell;

mod v_table {
    use std::task::{RawWaker, RawWakerVTable};

    pub unsafe fn clone(data: *const ()) -> RawWaker {
        RawWaker::new(data, &V_TABLE)
    }

    pub unsafe fn wake(data: *const ()) {
        use super::inner::Waker;
        let waker = data as *const super::inner::Waker;
        let Waker { id: _, task_id } = *waker;
        println!("setting task as ready: {}", task_id);
        super::READY_TASKS.with(|ready_tasks| ready_tasks.borrow_mut().push_back(task_id));
    }

    pub unsafe fn wake_by_ref(data: *const()) {
        use super::inner::Waker;
        let waker = data as *const super::inner::Waker;
        let Waker { id: _, task_id } = *waker;
        println!("setting task as ready: {}", task_id);
        super::READY_TASKS.with(|ready_tasks| ready_tasks.borrow_mut().push_back(task_id));
    }

    pub unsafe fn drop(_data: *const ()) {
    }

    pub const V_TABLE: RawWakerVTable = RawWakerVTable::new(
        clone,
        wake,
        wake_by_ref,
        drop,
    );
}

pub enum Event {
    Timeout {
        waker: std::task::Waker,
        timespec: uring_sys::__kernel_timespec,
        result: Option<Result<u32, std::io::Error>>,
    },
    // Accept {
    //     waker: std::task::Waker,
    //     result: std::rc::Rc<std::cell::Cell<Option<u32>>>,
    // },
}

mod inner {
    use std::future::Future;
    use std::pin::Pin;

    pub struct Task {
        pub id: usize,
        pub future: Pin<Box<dyn Future<Output = ()>>>, // XXX support return types
    }

    pub struct Waker {
        pub id: usize,
        pub task_id: usize,
    }

}

thread_local!(static RING: RefCell<iou::IoUring> = RefCell::new(IoUring::new(16).unwrap()));
thread_local!(static EVENTS: RefCell<HashMap<usize, Event>> = RefCell::new(HashMap::new()));
thread_local!(static NEXT_EVENT_ID: RefCell<usize> = RefCell::new(0));
thread_local!(static EVENTS_PREPARED: RefCell<bool> = RefCell::new(false));
thread_local!(static READY_TASKS: RefCell<VecDeque<usize>> = RefCell::new(VecDeque::new()));
thread_local!(static WAKERS: RefCell<HashMap<usize, inner::Waker>> = RefCell::new(HashMap::new()));
thread_local!(static TASKS: RefCell<HashMap<usize, inner::Task>> = RefCell::new(HashMap::new()));
thread_local!(static NEXT_TASK_ID: RefCell<usize> = RefCell::new(0));
thread_local!(static NEXT_WAKER_ID: RefCell<usize> = RefCell::new(0));

thread_local!(static NEXT_GLOBAL_ID: RefCell<usize> = RefCell::new(0));

pub mod system {
    use super::NEXT_EVENT_ID;
    use super::EVENTS_PREPARED;
    use super::RING;
    use super::EVENTS;
    use std::future::Future;
    use std::task::{Context, Poll};
    use std::pin::Pin;
    use std::time::{Duration, Instant};
    use crate::Event;

    pub async fn timeout(duration: Duration) -> Result<u32, std::io::Error> {
        Timeout {
            state: State::Waiting,
            duration,
        }.await
    }

    #[derive(Copy, Clone)]
    enum State {
        Waiting,
        Pending(usize),
    }

    struct Timeout {
        state: State,
        duration: Duration,
    }

    impl Drop for Timeout {
        fn drop(&mut self) {
            match self.state {
                State::Pending(event_id) => {
                    EVENTS.with(|events| {
                        events.borrow_mut().remove(&event_id);
                    })
                }
                _ => {}
            }
        }
    }

    impl Future for Timeout {
        type Output = Result<u32, std::io::Error>;

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            match self.as_ref().state {
                State::Waiting => {
                    println!("I was waiting");
                    let event_id = NEXT_EVENT_ID.with(|next_event_id| {
                        let mut next_event_id = next_event_id.borrow_mut();
                        let event_id = *next_event_id;
                        *next_event_id += 1;
                        event_id;
                        crate::NEXT_GLOBAL_ID.with(|id| {
                            let i = *id.borrow() + 1;
                            id.replace(i)
                        })
                    });

                    RING.with(|ring| { EVENTS.with(|events| {
                        use std::collections::hash_map::Entry;
                        use iou::sqe::TimeoutFlags;

                        let mut ring = ring.borrow_mut();
                        let mut events = events.borrow_mut();

                        let mut sqe = ring.prepare_sqe().unwrap();
                        if let Entry::Vacant(entry) = events.entry(event_id) {
                            println!("creating event: {}", event_id);
                            let timespec = make_timespec(self.duration);
                            let waker = cx.waker().clone();
                            let event = Event::Timeout {waker, timespec, result: None};
                            if let Event::Timeout {timespec, ..} = entry.insert(event) {
                                unsafe {
                                    sqe.prep_timeout(&timespec, 0, TimeoutFlags::empty());
                                    sqe.set_user_data(event_id as u64);
                                }
                            }
                        }
                    })});

                    EVENTS_PREPARED.with(|events_prepared| {
                        *events_prepared.borrow_mut() = true;
                    });

                    self.as_mut().state = State::Pending(event_id);

                    Poll::Pending
                }
                State::Pending(event_id) => {
                    EVENTS.with(|events| {
                        let mut events = events.borrow_mut();
                        use std::collections::hash_map::Entry;
                        match events.entry(event_id) {
                            Entry::Vacant(entry) => panic!("missing event"),
                            Entry::Occupied(entry) => {
                                match entry.get() {
                                    Event::Timeout {result: Some(_), ..} => {
                                        let result = match entry.remove() {
                                            Event::Timeout {result: Some(result), ..} => result,
                                            event => panic!("unexpected state"), // XXX
                                        };
                                        Poll::Ready(result)
                                    }
                                    Event::Timeout {result: None, ..} => {
                                        println!("result still pending");
                                        Poll::Pending
                                    }
                                    event => panic!("unexpected event"),
                                }
                            }
                        }
                    })
                }
            }
        }
    }

    fn make_timespec(duration: Duration) -> uring_sys::__kernel_timespec {
        uring_sys::__kernel_timespec {
            tv_sec: duration.as_secs() as i64, // TODO
            tv_nsec: duration.subsec_nanos() as i64, // TODO
        }
    }

    struct Accept {
        state: State,
    }

    // impl Future for Accept {
    //     type Output = Result<u32, std::io::Error>;

    //     fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
    //         match self.as_ref().state {
    //             State::Waiting => {
    //             }
    //             State::Pending(event_id) => {
    //             }
    //         }
    //     }
    // }
}

// TODO enhance
#[derive(Debug)]
struct Error {
    msg: String,
}

impl Error {
    pub fn new<E: std::error::Error>(e: E) -> Self {
        Self {
            msg: format!("{}", e),
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> Result<(), std::fmt::Error> {
        write!(f, "{}", self.msg)
    }
}

impl std::error::Error for Error {}

pub mod net {
    use std::net::ToSocketAddrs;
    use super::Error;

    pub struct TcpListener {
        listener: std::net::TcpListener,
    }

    impl TcpListener {
        // TODO tokio uses custom ToSocketAddrs
        pub async fn bind<A: ToSocketAddrs>(addr: A) -> Result<Self, Error> {
            let listener = Self {
                listener: std::net::TcpListener::bind(addr).map_err(Error::new)?,
            };
            Ok(listener)
        }
    }

    pub struct TcpAccept {
    }
}

// XXX Not sure this future is valid?
pub struct ToggleFuture {
    toggled: bool
}

impl ToggleFuture {
    pub fn new() -> Self {
        ToggleFuture { toggled: false }
    }
}

impl Future for ToggleFuture {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        let toggled = &mut self.as_mut().toggled;
        if *toggled {
            println!("toggled!");
            Poll::Ready(())
        } else {
            println!("toggling!");
            *toggled = true;
            Poll::Pending
        }
    }
}

pub struct Runtime {
}

impl Runtime {

    pub fn new() -> Self {
        Runtime {}
    }

    pub fn run<T, F: Future<Output = T>>(self, mut main_task: F) -> T {
        let mut main_task = unsafe { Pin::new_unchecked(&mut main_task) };
        let main_task_id = NEXT_TASK_ID.with(|next_task_id| { READY_TASKS.with(|ready_tasks| {
            let task_id = {
                let mut next_task_id = next_task_id.borrow_mut();
                let task_id = *next_task_id;
                *next_task_id += 1;
                task_id;
                        crate::NEXT_GLOBAL_ID.with(|id| {
                            let i = *id.borrow() + 1;
                            id.replace(i)
                        })
            };
            ready_tasks.borrow_mut().push_back(task_id);
            task_id
        })});

        let mut iterations = 0;

        let mut ready_tasks = VecDeque::new();
        loop {
            if iterations > 30 { // XXX
                std::panic!("too many iterations!");
            }
            iterations += 1;

            println!("Entered event loop iteration");
            loop {
                ready_tasks = READY_TASKS.with(|shared_ready_tasks| {
                    shared_ready_tasks.replace(ready_tasks)
                });

                println!("Processing ready tasks: {}", ready_tasks.len());
                while let Some(task_id) = ready_tasks.pop_front() {
                    println!("Processing task: {}", task_id);

                    let waker_id = NEXT_WAKER_ID.with(|next_waker_id| {
                        let mut next_waker_id = next_waker_id.borrow_mut();
                        let waker_id = *next_waker_id;
                        *next_waker_id += 1;
                        waker_id;

                        crate::NEXT_GLOBAL_ID.with(|id| {
                            let i = *id.borrow() + 1;
                            id.replace(i)
                        })
                    });

                    println!("Creating waker: {}", waker_id);
                    let waker = WAKERS.with(|wakers| {
                        use std::collections::hash_map::Entry::*;

                        let mut wakers = wakers.borrow_mut();
                        let entry = if let Vacant(entry) = wakers.entry(waker_id) {
                            entry
                        } else {
                            todo!()
                        };
                        let waker = inner::Waker { id: waker_id, task_id };
                        let waker = entry.insert(waker);
                        let waker = &*waker as *const inner::Waker as *const ();
                        let waker = RawWaker::new(waker, &v_table::V_TABLE);
                        let waker = unsafe { Waker::from_raw(waker) };
                        waker
                    });

                    let mut context = Context::from_waker(&waker);

                    if task_id == main_task_id {
                        match main_task.as_mut().poll(&mut context) {
                            Poll::Pending => {
                                println!("Main task was pending");
                            }
                            Poll::Ready(val) => {
                                println!("Main task was ready");
                                return val;
                            }
                        }
                    } else {
                        let mut task = TASKS.with(|tasks| {
                            tasks.borrow_mut().remove(&task_id).unwrap()
                        });
                        match task.future.as_mut().poll(&mut context) {
                            Poll::Pending => {
                                println!("Task was pending: {}", task_id);
                                TASKS.with(|tasks| tasks.borrow_mut().insert(task_id, task));
                            }
                            Poll::Ready(_) => {
                                println!("Task was ready: {}", task_id);
                            }
                        }
                    }
                }

                let ready_tasks_empty = READY_TASKS.with(|ready_tasks| ready_tasks.borrow().is_empty());
                if ready_tasks_empty {
                    break;
                }
            }

            let should_continue = EVENTS.with(|events| { RING.with(|ring| { EVENTS_PREPARED.with(|events_prepared| {
                if !events.borrow().is_empty()  {
                    println!("Submitting and waiting for events: {}", events.borrow().len());
                    if let Err(e) = ring.borrow_mut().submit_sqes_and_wait(1) {
                        panic!("IO error while submitting events: {:?}", e);
                    }

                    let mut completed_events = Vec::new();
                    for cqe in ring.borrow_mut().cqes() {
                        let event_id = cqe.user_data() as usize;
                        let result = cqe.result();
                        completed_events.push((event_id, result));
                    }
                    for (event_id, result) in completed_events {
                        println!("Waking up waker: {}", event_id);
                        match events.borrow_mut().get_mut(&event_id) {
                            Some(Event::Timeout {waker, timespec: _, result: res}) => {
                                // println!("timeout result: {:?}", result);
                                waker.wake_by_ref();
                                *res = Some(result);
                            }
                            None => {
                                // XXX
                            }
                        }
                    }
                    
                    *events_prepared.borrow_mut() = false;
                    return true;
                }
                false
            })})});
            if should_continue {
                continue;
            }
        }
    }
}

pub fn spawn<T: 'static>(f: impl Future<Output = T> + 'static) -> impl Future<Output = T> {
    use std::rc::Rc;

    let task_id = NEXT_TASK_ID.with(|next_task_id| {
        let mut next_task_id = next_task_id.borrow_mut();
        let task_id = *next_task_id;
        *next_task_id += 1;
        task_id;
                crate::NEXT_GLOBAL_ID.with(|id| {
                    let i = *id.borrow() + 1;
                    id.replace(i)
                })
    });

    let storage = Rc::new(RefCell::new(TaskStorage::new()));

    TASKS.with(|tasks| {
        let storage = storage.clone();
        let task = inner::Task {
            id: task_id,
            future: Box::pin(async move {
                println!("waiting submitted subtask");
                let ret = f.await;
                println!("got result of subtask");
                let mut storage = storage.borrow_mut();
                storage.result = Some(ret);
                if let Some(waker) = storage.waker.take() {
                    println!("waker stored! waking up");
                    waker.wake();
                }
            }),
        };

        tasks.borrow_mut().insert(task_id, task); 
    });

    READY_TASKS.with(|ready_tasks| {
        ready_tasks.borrow_mut().push_back(task_id);
    });

    Task {
        task_id,
        storage,
    }
}

struct TaskStorage<T> {
    result: Option<T>,
    waker: Option<Waker>,
}

impl<T> TaskStorage<T> {
    fn new() -> Self {
        Self {
            result: None,
            waker: None,
        }
    }
}

struct Task<T> {
    task_id: usize,
    storage: std::rc::Rc<RefCell<TaskStorage<T>>>,
}

impl<T> Future for Task<T> {
    type Output = T;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let mut storage = self.storage.borrow_mut();

        if let Some(ret) = storage.result.take() {
            return Poll::Ready(ret);
        }

        let waker = cx.waker().clone();
        storage.waker = Some(waker);

        Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::future::FutureExt;

    #[test]
    fn ready() {
        let runtime = Runtime {};
        let ret = runtime.run(
            async {
                println!("entered future");
            }
            .then(|_| async {
                println!("entered second future");
            })
            .then(|_| async {
                println!("returning 42");
                42
            })
        );
        assert_eq!(42, ret);
    }

    #[test]
    fn sleep() {
        use std::time::Duration;

        let runtime = Runtime {};
        let ret = runtime.run(
            system::timeout(Duration::from_secs(1))
            .then(|result| {
                system::timeout(Duration::from_secs(1))
            })
            .map(|_| 42)
        );
        assert_eq!(42, ret);
    }

    #[test]
    fn spawn_ready() {
        let runtime = Runtime {};
        let ret = runtime.run(async {
            spawn(async {
                42usize
            }).await
        });
        assert_eq!(42, ret);
    }

    #[test]
    fn spawn_sleep() {
        use std::time::{Duration, Instant};

        let runtime = Runtime {};
        let duration = runtime.run(async {
            let start = Instant::now();
            let t = spawn(async {
                println!("long sleep");
                system::timeout(Duration::from_secs(3)).await;
                println!("long sleep finished");
            });
            println!("short sleep");
            system::timeout(Duration::from_secs(2)).await;
            println!("short sleep finished");
            println!("waiting for long sleep");
            t.await;
            Instant::now() - start
        });
        println!("Slept {} ms", duration.as_millis());
        assert_eq!(true, duration < Duration::from_secs(5));
    }
}