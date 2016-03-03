#![allow(unused_mut)]

use {convert, sys, Evented, Token};
use event::{self, EventSet, Event, PollOpt};
use time::precise_time_ns;
use std::{fmt, io, mem, ptr};
use std::cell::{Cell, UnsafeCell};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, AtomicPtr, Ordering};
use std::time::Duration;

/// The `Poll` type acts as an interface allowing a program to wait on a set of
/// IO handles until one or more become "ready" to be operated on. An IO handle
/// is considered ready to operate on when the given operation can complete
/// without blocking.
///
/// To use `Poll`, an IO handle must first be registered with the `Poll`
/// instance using the `register()` handle. An `EventSet` representing the
/// program's interest in the socket is specified as well as an arbitrary
/// `Token` which is used to identify the IO handle in the future.
///
/// ## Edge-triggered and level-triggered
///
/// An IO handle registration may request edge-triggered notifications or
/// level-triggered notifications. This is done by specifying the `PollOpt`
/// argument to `register()` and `reregister()`.
///
/// ## Portability
///
/// Cross platform portability is provided for Mio's TCP & UDP implementations.
///
/// ## Examples
///
/// ```no_run
/// use mio::*;
/// use mio::tcp::*;
///
/// // Construct a new `Poll` handle
/// let mut poll = Poll::new().unwrap();
///
/// // Connect the stream
/// let stream = TcpStream::connect(&"173.194.33.80:80".parse().unwrap()).unwrap();
///
/// // Register the stream with `Poll`
/// poll.register(&stream, Token(0), EventSet::all(), PollOpt::edge()).unwrap();
///
/// // Wait for the socket to become ready
/// poll.poll(None).unwrap();
/// ```
pub struct Poll {
    // Platform specific IO selector
    selector: sys::Selector,

    // Custom readiness queue
    readiness_queue: ReadinessQueue,

    // Pending events. TODO: remove this
    events: sys::Events,
}

#[derive(Clone, Debug)]
pub struct Config {
    // Timer tick size
    timer_tick_dur: Duration,
    // Size of the timer wheel
    timer_wheel_size: usize,
}

pub struct Registration {
    node: ReadyRef,
    queue: ReadinessQueue,
}

#[derive(Clone)]
struct ReadinessQueue {
    inner: Arc<UnsafeCell<ReadinessQueueInner>>,
}

struct ReadinessQueueInner {
    // All readiness nodes are owned by the `Poll` instance and live either in
    // this linked list or in a `readiness_wheel` linked list.
    head_all_nodes: Option<Box<ReadinessNode>>,

    // linked list of nodes that are pending some processing
    head_readiness: AtomicPtr<ReadinessNode>,

    // Hashed timer wheel for delayed readiness notifications
    readiness_wheel: Vec<AtomicPtr<ReadinessNode>>,

    // Timer settings
    timer_tick_ms: u64,

    // Timer epoch
    epoch: u64,

    // Masks the target tick to get the slot in the wheel
    mask: u64,
}

struct ReadyList {
    head: ReadyRef,
}

struct ReadyRef {
    ptr: *mut ReadinessNode,
}

struct ReadinessNode {
    // ===== Fields only accessed by Poll =====
    //
    // Next node in ownership tracking queue
    next_all_nodes: Option<Box<ReadinessNode>>,

    // Previous node in the owned list
    prev_all_nodes: ReadyRef,

    // Current delay, represented in ticks. This also is used to find the
    // "head" pointer of the ownership list that this node lives in.
    delay: Option<Tick>,

    // The Token used to register the `Evented` with `Poll`. This can change,
    // but only by calling `Poll` functions, so there will be no concurrency.
    token: Token,

    // Both interest and opts can be mutated
    interest: Cell<EventSet>,

    // Poll opts
    opts: Cell<PollOpt>,

    // ===== Fields accessed by any thread ====
    //
    // Used when the node is queued in the readiness linked list. Accessing
    // this field requires winning the "queue" lock
    next_readiness: ReadyRef,

    // The set of events to include in the notification on next poll
    events: AtomicUsize,

    // Tracks if the node is queued for readiness using the MSB, the
    // rest of the usize is the readiness delay.
    queued: AtomicUsize,
}

type Tick = usize;

const NODE_QUEUED_FLAG: usize = 1;

/*
 *
 * ===== Config =====
 *
 */

impl Default for Config {
    fn default() -> Config {
        Config {
            timer_tick_dur: Duration::from_millis(100),
            timer_wheel_size: 512,
        }
    }
}

/*
 *
 * ===== Poll =====
 *
 */

impl Poll {
    pub fn new() -> io::Result<Poll> {
        // TODO: Allow config to be passed in
        let config = Config::default();

        Ok(Poll {
            selector: try!(sys::Selector::new()),
            readiness_queue: ReadinessQueue::new(&config),
            events: sys::Events::new(),
        })
    }

    pub fn register<E: ?Sized>(&mut self, io: &E, token: Token, interest: EventSet, opts: PollOpt) -> io::Result<()>
        where E: Evented
    {
        trace!("registering with poller");

        // Register interests for this socket
        try!(io.register(self, token, interest, opts));

        Ok(())
    }

    pub fn reregister<E: ?Sized>(&mut self, io: &E, token: Token, interest: EventSet, opts: PollOpt) -> io::Result<()>
        where E: Evented
    {
        trace!("registering with poller");

        // Register interests for this socket
        try!(io.reregister(self, token, interest, opts));

        Ok(())
    }

    pub fn deregister<E: ?Sized>(&mut self, io: &E) -> io::Result<()>
        where E: Evented
    {
        trace!("deregistering IO with poller");

        // Deregister interests for this socket
        try!(io.deregister(self));

        Ok(())
    }

    pub fn poll(&mut self, timeout: Option<Duration>) -> io::Result<usize> {
        let timeout = timeout.map(|to| convert::millis(to) as usize);
        try!(self.selector.select(&mut self.events, timeout));
        Ok(self.events.len())
    }

    pub fn events(&self) -> Events {
        Events {
            curr: 0,
            poll: self,
        }
    }
}

impl fmt::Debug for Poll {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        write!(fmt, "Poll")
    }
}

pub struct Events<'a> {
    curr: usize,
    poll: &'a Poll,
}

impl<'a> Events<'a> {
    pub fn get(&self, idx: usize) -> Option<Event> {
        self.poll.events.get(idx)
    }

    pub fn len(&self) -> usize {
        self.poll.events.len()
    }
}

impl<'a> Iterator for Events<'a> {
    type Item = Event;

    fn next(&mut self) -> Option<Event> {
        if self.curr == self.poll.events.len() {
            return None;
        }

        let ret = self.poll.events.get(self.curr).unwrap();
        self.curr += 1;
        Some(ret)
    }
}

// ===== Accessors for internal usage =====

pub fn selector(poll: &Poll) -> &sys::Selector {
    &poll.selector
}

pub fn selector_mut(poll: &mut Poll) -> &mut sys::Selector {
    &mut poll.selector
}

/*
 *
 * ===== Registration =====
 *
 */

impl Registration {
    pub fn new(poll: &Poll, token: Token, interest: EventSet, opts: PollOpt) -> Registration {
        let queue = poll.readiness_queue.clone();
        let node = queue.new_readiness_node(token, interest, opts);

        Registration {
            node: node,
            queue: queue,
        }
    }

    pub fn update(&self, poll: &Poll, interest: EventSet, opts: PollOpt) -> io::Result<()> {
        if !self.queue.identical(&poll.readiness_queue) {
            return Err(io::Error::new(io::ErrorKind::Other, "nope"));
        }

        self.node().interest.set(interest);
        self.node().opts.set(opts);

        // If the node is currently ready, re-queue?
        if !event::is_empty(self.readiness()) {
            self.queue_for_processing(None);
        }

        Ok(())
    }

    pub fn readiness(&self) -> EventSet {
        event::from_usize(self.node().events.load(Ordering::Relaxed))
    }

    pub fn set_readiness(&self, ready: EventSet, delay: Duration) {
        // First, process args
        let target_tick = self.queue.delay_target_tick(delay);

        // First CAS in the new readiness using relaxed.
        self.node().events.swap(event::as_usize(ready), Ordering::Relaxed);

        // Setting readiness to none doesn't require any processing by the poll
        // instance, so there is no need to enqueue the node.
        if event::is_empty(ready) {
            return;
        }

        self.queue_for_processing(target_tick);
    }

    fn queue_for_processing(&self, target_tick: Option<Tick>) {
        let mut curr;

        // Queue the node for processing.
        loop {
            curr = self.node().queued.load(Ordering::Relaxed);
            let next = (target_tick.unwrap_or(0) << 1) | NODE_QUEUED_FLAG;

            if curr == self.node().queued.compare_and_swap(curr, next, Ordering::Release) {
                break;
            }
        }

        // If the queued flag was not initially set, then the current thread
        // is assigned the responsibility of enqueuing the node for processing.
        if 0 == NODE_QUEUED_FLAG & curr {
            self.queue.prepend_readiness_node(self.node.clone());
        }
    }

    fn node(&self) -> &ReadinessNode {
        self.node.as_ref().unwrap()
    }
}

impl fmt::Debug for Registration {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.debug_struct("Registration")
            .finish()
    }
}

/*
 *
 * ===== ReadinessQueue =====
 *
 */

impl ReadinessQueue {
    fn new(config: &Config) -> ReadinessQueue {
        let timer_wheel_size = config.timer_wheel_size.next_power_of_two();

        ReadinessQueue {
            inner: Arc::new(UnsafeCell::new(ReadinessQueueInner {
                head_all_nodes: None,
                head_readiness: AtomicPtr::new(ptr::null_mut()),
                readiness_wheel: (0..timer_wheel_size).map(|_| AtomicPtr::new(ptr::null_mut())).collect(),
                timer_tick_ms: convert::millis(config.timer_tick_dur),
                mask: timer_wheel_size as u64,
                epoch: precise_time_ns(),
            }))
        }
    }

    fn poll(&self, _dst: &mut Vec<Event>) {
        let ready = self.take_ready();
        let curr_tick = self.current_tick();

        // TODO: Cap number of nodes processed
        for node in ready {
            let node_ref = node.as_ref().unwrap();
            let opts = node_ref.opts.get();

            // Atomically read queued. Use Acquire ordering to set a
            // barrier before reading events
            let mut queued = node_ref.queued.load(Ordering::Acquire);
            let mut events = node_ref.poll_events();
            let mut target_tick;

            loop {
                target_tick = queued >> 1;

                // In the following conditions, the registration is removed from
                // the readiness queue:
                //
                // - The registration is edge triggered.
                // - The event set contains no events
                // - There is a requested delay that has not already expired.
                //
                // If the drop flag is set though, the node is never queued
                // again.
                if event::is_drop(events) {
                    // dropped nodes are always processed immediately
                    target_tick = curr_tick;
                    break;
                } else if opts.is_edge() || event::is_empty(events) || curr_tick >= target_tick {
                    // Set an acquire barrier as the events field will be
                    // re-read immediately after to guard against an ABA
                    // problem
                    let next = node_ref.queued.compare_and_swap(queued, 0, Ordering::Acquire);

                    // Re-read in order to ensure we have the latest value
                    // after having marked the registration has dequeued from
                    // the readiness queue.
                    events = node_ref.poll_events();

                    if queued == next {
                        break;
                    }

                    queued = next;
                } else {
                    // The node needs to stay queued for readiness, so it gets
                    // pushed back onto the queue.
                    //
                    // TODO: It would be better to build up a batch list that
                    // requires a single CAS
                    self.prepend_readiness_node(node.clone());
                    break;
                }
            }

            // Process the node
            if curr_tick >= target_tick {
                // Process the node immediately. First, ensure that the node
                // does not currently live in the timer wheel
                self.remove_node_from_timer_wheel(node.clone());

                if event::is_drop(events) {
                    // process dropping the event
                    unimplemented!();
                } else if !events.is_none() {
                    unimplemented!();
                }
            } else {
                // Place the node timer wheel for later processing
                self.insert_node_in_timer_wheel(node.clone(), target_tick);
            }
        }
    }

    fn take_ready(&self) -> ReadyList {
        ReadyList {
            // Relaxed is acceptable here as the underlying `ReadinessNode` is
            // owned by the thread that is calling `take_ready`.
            head: ReadyRef::new(self.inner().head_readiness.swap(ptr::null_mut(), Ordering::Relaxed)),
        }
    }

    fn new_readiness_node(&self, token: Token, interest: EventSet, opts: PollOpt) -> ReadyRef {
        let mut node = Box::new(ReadinessNode::new(token, interest, opts));
        let ret = ReadyRef::new(&mut *node as *mut ReadinessNode);

        node.next_all_nodes = self.inner_mut().head_all_nodes.take();
        self.inner_mut().head_all_nodes = Some(node);

        ret
    }

    /// Prepend the given node to the head of the readiness queue. This is done
    /// with relaxed ordering.
    fn prepend_readiness_node(&self, mut node: ReadyRef) {
        loop {
            let next = self.inner().head_readiness.load(Ordering::Relaxed);

            // Update next pointer
            node.as_mut().unwrap().next_readiness = ReadyRef::new(next);

            if next == self.inner().head_readiness.compare_and_swap(next, node.ptr, Ordering::Relaxed) {
                return;
            }
        }
    }

    fn insert_node_in_timer_wheel(&self, mut node: ReadyRef, tick: Tick) {
        unimplemented!();
    }

    fn remove_node_from_timer_wheel(&self, mut node: ReadyRef) {
        unimplemented!();
    }

    fn is_empty(&self) -> bool {
        self.inner().head_readiness.load(Ordering::Relaxed).is_null()
    }

    fn current_tick(&self) -> Tick {
        let inner = self.inner();
        ((precise_time_ns() - inner.epoch) / inner.timer_tick_ms) as usize
    }

    /// Returns the tick representing the completion of the duration.
    fn delay_target_tick(&self, delay: Duration) -> Option<Tick> {
        let delay = convert::millis(delay);

        if delay == 0 {
            return None;
        }

        // TODO: safely handle handle wrapping
        Some(self.current_tick() + (delay / self.inner().timer_tick_ms) as usize)
    }

    fn identical(&self, other: &ReadinessQueue) -> bool {
        self.inner.get() == other.inner.get()
    }

    fn inner(&self) -> &ReadinessQueueInner {
        unsafe { mem::transmute(self.inner.get()) }
    }

    fn inner_mut(&self) -> &mut ReadinessQueueInner {
        unsafe { mem::transmute(self.inner.get()) }
    }
}

impl ReadinessNode {
    fn new(token: Token, interest: EventSet, opts: PollOpt) -> ReadinessNode {
        ReadinessNode {
            next_all_nodes: None,
            prev_all_nodes: ReadyRef::none(),
            token: token,
            delay: None,
            interest: Cell::new(interest),
            opts: Cell::new(opts),
            next_readiness: ReadyRef::none(),
            events: AtomicUsize::new(0),
            queued: AtomicUsize::new(0),
        }
    }

    fn poll_events(&self) -> EventSet {
        self.interest.get() & event::from_usize(self.events.load(Ordering::Relaxed))
    }
}

impl Iterator for ReadyList {
    type Item = ReadyRef;

    fn next(&mut self) -> Option<ReadyRef> {
        let mut next = self.head.take();

        if next.is_some() {
            next.as_mut().map(|n| self.head = n.next_readiness.take());
            Some(next)
        } else {
            None
        }
    }
}

impl ReadyRef {
    fn new(ptr: *mut ReadinessNode) -> ReadyRef {
        ReadyRef { ptr: ptr }
    }

    fn none() -> ReadyRef {
        ReadyRef { ptr: ptr::null_mut() }
    }

    fn take(&mut self) -> ReadyRef {
        let ret = ReadyRef { ptr: self.ptr };
        self.ptr = ptr::null_mut();
        ret
    }

    fn is_some(&self) -> bool {
        !self.ptr.is_null()
    }

    fn as_ref(&self) -> Option<&ReadinessNode> {
        if self.ptr.is_null() {
            return None;
        }

        unsafe { Some(&*self.ptr) }
    }

    fn as_mut(&mut self) -> Option<&mut ReadinessNode> {
        if self.ptr.is_null() {
            return None;
        }

        unsafe { Some(&mut *self.ptr) }
    }
}

impl Clone for ReadyRef {
    fn clone(&self) -> ReadyRef {
        ReadyRef::new(self.ptr)
    }
}
