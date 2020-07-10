use super::{Batch, Scheduler, Task, Thread, ThreadState, Worker, WorkerRef};
use crate::sync::CachePadded;
use core::{
    cell::Cell,
    marker::{PhantomData, PhantomPinned},
    num::NonZeroUsize,
    pin::Pin,
    ptr::NonNull,
    sync::atomic::{spin_loop_hint, AtomicUsize, Ordering},
};

#[repr(C)]
#[derive(Debug, Default)]
pub struct Cluster {
    head: Option<NonNull<Node>>,
    tail: Option<NonNull<Node>>,
    size: usize,
}

impl From<Pin<&mut Node>> for Cluster {
    fn from(node: Pin<&mut Node>) -> Self {
        let node = Some(NonNull::from(unsafe {
            let node = Pin::into_inner_unchecked(node);
            node.next = NonNull::new(node);
            node
        }));
        Self {
            head: node,
            tail: node,
            size: 1,
        }
    }
}

impl Cluster {
    pub const fn new() -> Self {
        Self {
            head: None,
            tail: None,
            size: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.size
    }

    pub fn iter<'a>(&'a self) -> impl Iterator<Item = Pin<&'a Node>> + 'a {
        NodeIter::from(self.head)
    }
}

#[repr(usize)]
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
enum IdleState {
    Ready = 0,
    Waking = 1,
    Notified = 2,
    Shutdown = 3,
}

impl IdleState {
    fn encode(self, worker_index: Option<NonZeroUsize>, aba_tag: u8) -> usize {
        (worker_index.map(|i| i.get()).unwrap_or(0) << 10)
            | ((self as usize) << 8)
            | (aba_tag as usize)
    }

    fn decode(value: usize) -> (Self, Option<NonZeroUsize>, u8) {
        (
            match (value >> 8) & 0b11 {
                0 => Self::Ready,
                1 => Self::Waking,
                2 => Self::Notified,
                3 => Self::Shutdown,
                _ => unreachable!(),
            },
            NonZeroUsize::new(value >> 10),
            value as u8,
        )
    }
}

#[derive(Debug)]
pub(crate) enum ResumeResult {
    Notified,
    Spawn(NonNull<Worker>),
    Resume(NonNull<Thread>),
}

#[repr(align(4))]
pub struct Node {
    _pinned: PhantomPinned,
    next: Option<NonNull<Self>>,
    scheduler: Option<NonNull<Scheduler>>,
    workers_ptr: Option<NonNull<Worker>>,
    workers_len: usize,
    workers_active: CachePadded<AtomicUsize>,
    idle_queue: AtomicUsize,
    runq_polling: CachePadded<AtomicUsize>,
    runq_head: CachePadded<AtomicUsize>,
    runq_tail: CachePadded<Cell<NonNull<Task>>>,
    runq_stub_next: Cell<Option<NonNull<Task>>>,
}

unsafe impl Sync for Node {}

impl Node {
    /// For idle_queue value:
    /// - usize:[worker_index, u2:idle_state, u8:aba_tag]
    /// - where worker_index = 0 = null
    pub const MAX_WORKERS: usize = (1 << (usize::max_value().count_ones() - 8 - 2)) - 1;

    pub fn new(workers: &mut [Worker]) -> Self {
        let mut idle_queue: Option<NonZeroUsize> = None;
        for index in 0..workers.len().min(Self::MAX_WORKERS) {
            let worker_ptr = idle_queue.map(|i| NonNull::from(&workers[i.get() - 1]));
            let worker_ref = WorkerRef::Worker(worker_ptr);
            idle_queue = NonZeroUsize::new(index + 1);
            workers[index] = Worker {
                ptr: AtomicUsize::new(worker_ref.into()),
            };
        }

        Self {
            _pinned: PhantomPinned,
            next: None,
            scheduler: None,
            workers_ptr: workers.first().map(NonNull::from),
            workers_len: workers.len().min(Self::MAX_WORKERS),
            workers_active: CachePadded::new(AtomicUsize::new(0)),
            idle_queue: AtomicUsize::new(IdleState::Ready.encode(idle_queue, 0)),
            runq_polling: CachePadded::new(AtomicUsize::new(0)),
            runq_head: CachePadded::new(AtomicUsize::default()),
            runq_tail: CachePadded::new(Cell::new(NonNull::dangling())),
            runq_stub_next: Cell::new(None),
        }
    }

    fn runq_stub_ptr(&self) -> *const Task {
        &self.runq_stub_next as *const _ as *const Task
    }

    pub(crate) fn init(&self) {
        assert_eq!(self.runq_polling.load(Ordering::Relaxed), 0);
        let runq_stub_ptr = self.runq_stub_ptr() as usize;
        self.runq_stub_next.set(None);
        self.runq_head.store(runq_stub_ptr, Ordering::Relaxed);
        self.runq_tail
            .set(NonNull::from(unsafe { &*(runq_stub_ptr as *const _) }));
    }

    pub(crate) fn deinit(&self) {
        assert_eq!(self.workers_active.load(Ordering::Relaxed), 0);
        assert_eq!(
            IdleState::Shutdown,
            IdleState::decode(self.idle_queue.load(Ordering::Relaxed)).0,
        );

        let runq_stub_ptr = self.runq_stub_ptr();
        assert_eq!(self.runq_polling.load(Ordering::Relaxed), 0);
        assert_eq!(
            self.runq_head.load(Ordering::Relaxed),
            runq_stub_ptr as usize
        );
    }

    pub fn iter<'a>(self: Pin<&'a Self>) -> impl Iterator<Item = Pin<&'a Node>> + 'a {
        (unsafe { Pin::into_inner_unchecked(self) }).iter_nodes()
    }

    fn iter_nodes<'a>(&'a self) -> impl Iterator<Item = Pin<&'a Node>> + 'a {
        NodeIter::from(Some(NonNull::from(self)))
    }

    pub fn threads<'a>(self: Pin<&'a Self>) -> impl Iterator<Item = Pin<&'a Thread>> + 'a {
        (unsafe { Pin::into_inner_unchecked(self) })
            .workers()
            .iter()
            .filter_map(|worker| {
                let ptr = worker.ptr.load(Ordering::Acquire);
                match WorkerRef::from(ptr) {
                    WorkerRef::Thread(thread) => {
                        Some(unsafe { Pin::new_unchecked(&*thread.as_ptr()) })
                    }
                    _ => None,
                }
            })
    }

    pub(crate) fn workers<'a>(&'a self) -> &'a [Worker] {
        let mut len = self.workers_len;
        let ptr = self
            .workers_ptr
            .unwrap_or_else(|| {
                len = 0;
                NonNull::dangling()
            })
            .as_ptr();
        unsafe { core::slice::from_raw_parts(ptr, len) }
    }

    unsafe fn worker_index_of(&self, worker: NonNull<Worker>) -> NonZeroUsize {
        let worker_ptr = worker.as_ptr() as usize;
        let base_ptr = self.workers_ptr.unwrap().as_ptr() as usize;
        let index = (worker_ptr - base_ptr) / core::mem::size_of::<Worker>();
        NonZeroUsize::new_unchecked(index + 1)
    }

    pub(crate) fn try_resume_some_worker(&self) -> Option<ResumeResult> {
        self.iter_nodes()
            .filter_map(|node| node.try_resume_worker())
            .next()
    }

    pub(crate) fn try_resume_worker(&self) -> Option<ResumeResult> {
        unsafe { self.resume_worker(false) }
    }

    pub(crate) fn stop_waking(&self) -> Option<ResumeResult> {
        (unsafe { self.resume_worker(true) }).or_else(|| {
            self.iter_nodes()
                .skip(1)
                .filter_map(|node| node.try_resume_some_worker())
                .next()
        })
    }

    unsafe fn resume_worker(&self, was_waking: bool) -> Option<ResumeResult> {
        let mut idle_queue = self.idle_queue.load(Ordering::Relaxed);
        loop {
            let (mut idle_state, mut worker_index, aba_tag) = IdleState::decode(idle_queue);

            match idle_state {
                IdleState::Shutdown => unreachable!("Node::resume_worker() when shutdown"),
                IdleState::Notified => return None,
                IdleState::Ready => {
                    idle_state = IdleState::Waking;
                }
                IdleState::Waking => {
                    if !was_waking {
                        return None;
                    }
                }
            }

            let resume_result = if let Some(index) = worker_index {
                let (worker, worker_ref) = {
                    let worker = &self.workers()[index.get() - 1];
                    let worker_ref = WorkerRef::from(worker.ptr.load(Ordering::Acquire));
                    (NonNull::from(worker), worker_ref)
                };

                match worker_ref {
                    WorkerRef::ThreadId(_) => {
                        unreachable!("Node::resume_worker() with shutdown worker")
                    }
                    WorkerRef::Node(_) => {
                        unreachable!("Node::resume_worker() with spawning worker")
                    }
                    WorkerRef::Thread(thread) => {
                        worker_index = thread.as_ref().next_index.get();
                        ResumeResult::Resume(thread)
                    }
                    WorkerRef::Worker(next_worker) => {
                        worker_index = next_worker.map(|w| self.worker_index_of(w));
                        ResumeResult::Spawn(worker)
                    }
                }
            } else {
                idle_state = IdleState::Notified;
                ResumeResult::Notified
            };

            if let Err(e) = self.idle_queue.compare_exchange_weak(
                idle_queue,
                idle_state.encode(worker_index, aba_tag),
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                idle_queue = e;
                spin_loop_hint();
                continue;
            }

            let new_active_worker = match resume_result {
                ResumeResult::Notified => false,
                ResumeResult::Resume(thread) => {
                    thread.as_ref().state.set(ThreadState::Waking);
                    true
                }
                ResumeResult::Spawn(mut worker) => {
                    let new_worker_ref = WorkerRef::Node(NonNull::from(self));
                    *worker.as_mut().ptr.get_mut() = new_worker_ref.into();
                    true
                }
            };

            if new_active_worker && self.workers_active.fetch_add(1, Ordering::AcqRel) == 0 {
                self.scheduler
                    .expect("Node::resume_worker() without a scheduler")
                    .as_ref()
                    .nodes_active
                    .fetch_add(1, Ordering::Relaxed);
            }

            return Some(resume_result);
        }
    }

    pub(crate) unsafe fn suspend_worker(
        &self,
        thread: &Thread,
    ) -> Option<impl Iterator<Item = NonNull<Thread>>> {
        let old_thread_state = match thread.state.replace(ThreadState::Suspended) {
            ThreadState::Shutdown => unreachable!("Node::suspend_worker() when thread is shutdown"),
            thread_state => thread_state,
        };

        let worker = thread.worker.as_ref();
        let worker_ref = WorkerRef::Thread(NonNull::from(thread));
        worker.ptr.store(worker_ref.into(), Ordering::Release);

        let mut idle_queue = self.idle_queue.load(Ordering::Relaxed);
        loop {
            let (mut idle_state, mut worker_index, aba_tag) = IdleState::decode(idle_queue);

            let old_idle_state = idle_state;
            match idle_state {
                IdleState::Shutdown => unreachable!("Node::suspend_worker() when shutdown"),
                IdleState::Notified => {}
                _ => worker_index = Some(self.worker_index_of(thread.worker)),
            }

            thread.next_index.set(worker_index);
            if old_thread_state == ThreadState::Waking {
                idle_state = IdleState::Ready;
            }

            if let Err(e) = self.idle_queue.compare_exchange_weak(
                idle_queue,
                idle_state.encode(worker_index, aba_tag.wrapping_add(1)),
                Ordering::Release,
                Ordering::Relaxed,
            ) {
                idle_queue = e;
                spin_loop_hint();
                continue;
            }

            if old_idle_state == IdleState::Notified {
                thread.state.set(old_thread_state);
            }

            if self.workers_active.fetch_sub(1, Ordering::Release) == 1 {
                if self
                    .scheduler
                    .expect("Node::suspend_worker() without a scheduler")
                    .as_ref()
                    .nodes_active
                    .fetch_sub(1, Ordering::Relaxed)
                    == 1
                {
                    struct ShutdownIter {
                        thread_iter: ThreadIter,
                        node: NonNull<Node>,
                        count: usize,
                    }

                    impl Iterator for ShutdownIter {
                        type Item = NonNull<Thread>;

                        fn next(&mut self) -> Option<Self::Item> {
                            loop {
                                if self.count == 0 {
                                    return None;
                                }

                                if let Some(thread) = self.thread_iter.next() {
                                    return Some(thread);
                                }

                                unsafe {
                                    let this_node = self.node;
                                    let new_node = this_node
                                        .as_ref()
                                        .iter_nodes()
                                        .next()
                                        .expect("Invalid node link inside ShutdownIter");
                                    self.node = NonNull::from(&*new_node);
                                    self.thread_iter = new_node.shutdown();
                                    self.count -= 1;
                                }
                            }
                        }
                    }

                    return Some(ShutdownIter {
                        thread_iter: self.shutdown(),
                        node: NonNull::from(self),
                        count: self.iter_nodes().count(),
                    });
                }
            }

            return None;
        }
    }

    unsafe fn shutdown(&self) -> ThreadIter {
        let idle_queue = IdleState::Shutdown.encode(None, 0);
        let idle_queue = self.idle_queue.swap(idle_queue, Ordering::AcqRel);
        let (idle_state, mut worker_index, _aba_tag) = IdleState::decode(idle_queue);

        assert_eq!(idle_state, IdleState::Waking);
        let mut found_workers = 0;
        let mut idle_threads = None;
        let workers = self.workers();

        while let Some(index) = worker_index {
            let worker = &workers[index.get() - 1];
            match WorkerRef::from(worker.ptr.load(Ordering::Acquire)) {
                WorkerRef::Node(_) => {
                    unreachable!(
                        "Node::shutdown() when worker {} is spawning",
                        index.get() - 1
                    );
                }
                WorkerRef::ThreadId(_) => {
                    unreachable!(
                        "Node::shutdown() when worker {} already shutdown",
                        index.get() - 1
                    );
                }
                WorkerRef::Worker(next_worker) => {
                    found_workers += 1;
                    worker_index = next_worker.map(|w| self.worker_index_of(w));
                }
                WorkerRef::Thread(thread) => {
                    found_workers += 1;
                    let thread = thread.as_ref();
                    worker_index = thread.next_index.get();

                    thread.state.set(ThreadState::Shutdown);
                    let worker_ref = WorkerRef::ThreadId(thread.id.get());
                    worker.ptr.store(worker_ref.into(), Ordering::Release);

                    thread.next_index.set(idle_threads);
                    idle_threads = NonZeroUsize::new(thread as *const _ as usize);
                }
            }
        }

        assert_eq!(found_workers, workers.len());
        ThreadIter(idle_threads)
    }

    pub(crate) fn try_acquire_polling(&self) -> Option<NodePoller<'_>> {
        match self.runq_polling.load(Ordering::Relaxed) {
            0 => self
                .runq_polling
                .compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
                .ok()
                .map(|_| NodePoller { node: self }),
            1 => None,
            _ => unreachable!("invalid runq_polling state"),
        }
    }

    pub(crate) fn push(&self, batch: Batch) {
        if batch.len() == 0 {
            return;
        }

        unsafe {
            let head = batch
                .head
                .unwrap_or_else(|| unreachable!("Node::push() with null batch head"));
            let tail = batch
                .tail
                .unwrap_or_else(|| unreachable!("Node::push() with null batch tail"));

            let prev_ptr = self
                .runq_head
                .swap(tail.as_ptr() as usize, Ordering::AcqRel);

            (prev_ptr as *const Task)
                .as_ref()
                .unwrap_or_else(|| unreachable!("Node::push() with null runq_head"))
                .next
                .store(head.as_ptr() as usize, Ordering::Release);
        }
    }
}

struct ThreadIter(Option<NonZeroUsize>);

impl Iterator for ThreadIter {
    type Item = NonNull<Thread>;

    fn next(&mut self) -> Option<Self::Item> {
        self.0
            .and_then(|index| unsafe { (index.get() as *const Thread).as_ref() })
            .map(|thread| {
                self.0 = thread.next_index.get();
                NonNull::from(thread)
            })
    }
}

pub(crate) struct NodePoller<'a> {
    node: &'a Node,
}

impl<'a> Drop for NodePoller<'a> {
    fn drop(&mut self) {
        self.node.runq_polling.store(0, Ordering::Release);
    }
}

impl<'a> Iterator for NodePoller<'a> {
    type Item = NonNull<Task>;

    fn next(&mut self) -> Option<Self::Item> {
        unsafe {
            let mut tail = self.node.runq_tail.get();
            let mut next = NonNull::new(tail.as_ref().next.load(Ordering::Acquire) as *mut Task);

            let runq_stub_ptr = self.node.runq_stub_ptr() as *mut Task;
            if tail.as_ptr().eq(&runq_stub_ptr) {
                tail = next?;
                self.node.runq_tail.set(tail);
                next = NonNull::new(tail.as_ref().next.load(Ordering::Acquire) as *mut Task);
            }

            if let Some(next) = next {
                self.node.runq_tail.set(next);
                return Some(tail);
            }

            let head = self.node.runq_head.load(Ordering::Acquire) as *mut Task;
            if !tail.as_ptr().eq(&head) {
                return None;
            }

            self.node
                .push(Batch::from(Pin::new_unchecked(&mut *runq_stub_ptr)));

            let task = tail;
            tail = NonNull::new(tail.as_ref().next.load(Ordering::Acquire) as *mut Task)?;
            self.node.runq_tail.set(tail);
            Some(task)
        }
    }
}

struct NodeIter<'a> {
    start: Option<NonNull<Node>>,
    current: Option<NonNull<Node>>,
    _lifetime: PhantomData<&'a ()>,
}

impl<'a> From<Option<NonNull<Node>>> for NodeIter<'a> {
    fn from(node: Option<NonNull<Node>>) -> Self {
        Self {
            start: node,
            current: node,
            _lifetime: PhantomData,
        }
    }
}

impl<'a> Iterator for NodeIter<'a> {
    type Item = Pin<&'a Node>;

    fn next(&mut self) -> Option<Self::Item> {
        unsafe {
            let node = Pin::new_unchecked(&*self.current?.as_ptr());
            self.current = node.next;
            if self.current == self.start {
                self.current = None;
            }
            Some(node)
        }
    }
}
