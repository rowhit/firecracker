// Copyright 2019 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;
use std::fmt::Formatter;
use std::io;
use std::ops::{Deref, DerefMut};
use std::os::unix::io::{AsRawFd, RawFd};
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};

use utils::eventfd::EventFd;

use epoll;
use pollable::{EventRegistrationData, Pollable, PollableOp, PollableOpBuilder};

const EVENT_BUFFER_SIZE: usize = 128;
const DEFAULT_EPOLL_TIMEOUT: i32 = 250;

pub type Result<T> = std::result::Result<T, Error>;
pub type WrappedHandler = Arc<Mutex<dyn EventHandler>>;

/// Errors associated with epoll events handling.
pub enum Error {
    /// Cannot create epoll fd.
    EpollCreate(io::Error),
    /// Polling I/O error.
    Poll(io::Error),
    /// The specified pollable already registered.
    AlreadyExists(Pollable),
    /// The specified pollable is not registered.
    NotFound(Pollable),
    /// Error while writing the channel eventfd.
    ChannelFd(io::Error),
    /// Channel disconnected.
    ChannelDisconnect,
    /// Error while cloning tx channel.
    ChannelClone(io::Error),
}

impl std::fmt::Debug for Error {
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        use self::Error::*;

        match self {
            EpollCreate(err) => write!(f, "Unable to create epoll fd: {}", err),
            Poll(err) => write!(f, "Error during epoll call: {}", err),
            AlreadyExists(pollable) => write!(
                f,
                "A handler for the specified pollable {} already exists.",
                pollable
            ),
            ChannelFd(err) => write!(f, "Error while writing channel event fd: {}", err),
            ChannelDisconnect => write!(f, "Error while reading from a disconnected channel"),
            ChannelClone(err) => write!(f, "Error while cloning tx channel: {}", err),
            NotFound(pollable) => write!(
                f,
                "A handler for the specified pollable {} was not found.",
                pollable
            ),
        }
    }
}

struct EventHandlerData {
    data: EventRegistrationData,
    handler: WrappedHandler,
}

impl EventHandlerData {
    fn new(data: EventRegistrationData, handler: WrappedHandler) -> EventHandlerData {
        EventHandlerData { data, handler }
    }
}

/// A trait to express the ability to respond to I/O event readiness
/// using callbacks.
pub trait EventHandler: Send {
    /// Handle a read event (EPOLLIN).
    fn handle_read(&mut self, _source: Pollable) -> Vec<PollableOp> {
        vec![]
    }
    /// Handle a write event (EPOLLOUT).
    fn handle_write(&mut self, _source: Pollable) -> Vec<PollableOp> {
        vec![]
    }
    /// Handle a close event (EPOLLRDHUP).
    fn handle_close(&mut self, _source: Pollable) -> Vec<PollableOp> {
        vec![]
    }
    /// Handle an error event (EPOLLERR).
    fn handle_error(&mut self, _source: Pollable) -> Vec<PollableOp> {
        vec![]
    }

    /// Initial registration of pollable objects.
    /// Use the PollableOpBuilder to build the vector of PollableOps.
    fn init(&self) -> Vec<PollableOp>;
}

/// Wraps a HashMap of fd to EventHandlerData.
struct HandlerMap {
    handlers: HashMap<i32, EventHandlerData>,
}

impl HandlerMap {
    pub fn new() -> HandlerMap {
        HandlerMap {
            handlers: HashMap::new(),
        }
    }

    // Returns a copy of EventHandlerData instead of mut ref.
    pub fn get(&mut self, id: i32) -> Option<EventHandlerData> {
        match self.handlers.get(&id) {
            Some(handler) => Some(EventHandlerData::new(
                (handler.data.0, handler.data.1),
                handler.handler.clone(),
            )),
            None => None,
        }
    }
}

impl Deref for HandlerMap {
    type Target = HashMap<i32, EventHandlerData>;
    fn deref(&self) -> &Self::Target {
        &self.handlers
    }
}

impl DerefMut for HandlerMap {
    fn deref_mut(&mut self) -> &mut HashMap<i32, EventHandlerData> {
        &mut self.handlers
    }
}

// The tx side of a generic API channel.
// Sending a T message over this channel will also write the eventFd.
// The rx side of the channel polls the eventfd for incoming messages.
pub struct GenericChannel<T> {
    channel: Sender<T>,
    fd: EventFd,
}

impl<T> GenericChannel<T> {
    pub fn new(channel: Sender<T>) -> Result<GenericChannel<T>> {
        Ok(GenericChannel {
            channel,
            fd: EventFd::new(libc::EFD_NONBLOCK).map_err(Error::ChannelFd)?,
        })
    }

    /// Send a message of type T and notify rx side by writing
    /// the eventfd.
    pub fn send(&mut self, msg: T) -> Result<()> {
        self.fd.write(1).map_err(Error::ChannelFd)?;
        // This send can fail only if the channel is disconnected.
        self.channel.send(msg).map_err(|_| Error::ChannelDisconnect)
    }

    /// Reads eventfd event count.
    pub fn read_event(&mut self) -> u64 {
        self.fd.read().unwrap_or(0)
    }

    /// Try to clone the channel and fd.
    /// Might fail in fd.try_clone().
    fn try_clone(&self) -> Result<Self> {
        Ok(GenericChannel {
            channel: self.channel.clone(),
            fd: self.fd.try_clone().map_err(Error::ChannelClone)?,
        })
    }
}

impl<T> AsRawFd for GenericChannel<T> {
    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

pub type ChannelMessage = (WrappedHandler, Vec<PollableOp>);
pub type Channel = GenericChannel<ChannelMessage>;

/// Manages I/O notifications using epoll mechanism.
pub struct EventManager {
    epoll: epoll::Epoll,
    handlers: HandlerMap,
    ready_events: Vec<epoll::Event>,
    channel_rx: Receiver<ChannelMessage>,
    channel_tx: Channel,
}

impl AsRawFd for EventManager {
    fn as_raw_fd(&self) -> RawFd {
        self.epoll.as_raw_fd()
    }
}

impl EventManager {
    /// Create a new EventManager.
    pub fn new() -> Result<EventManager> {
        let epoll_fd = epoll::Epoll::new().map_err(Error::EpollCreate)?;
        let (tx, rx) = channel();

        Ok(EventManager {
            epoll: epoll_fd,
            handlers: HandlerMap::new(),
            // This buffer is used for storing the events returned by `epoll_wait()`.
            // We preallocate memory for this buffer in order to not repeat this
            // operation every time `run()` loop is executed.
            ready_events: vec![epoll::Event::default(); EVENT_BUFFER_SIZE],
            channel_rx: rx,
            channel_tx: Channel::new(tx)?,
        })
    }

    #[inline(always)]
    pub fn get_channel(&self) -> Result<Channel> {
        self.channel_tx.try_clone()
    }

    // Register a new event handler for the pollable and mask specified
    // in event_data.
    fn register_handler(
        &mut self,
        event_data: EventRegistrationData,
        wrapped_handler: WrappedHandler,
    ) -> Result<()> {
        let (pollable, event_type) = event_data;

        if self.handlers.get(pollable).is_some() {
            return Err(Error::AlreadyExists(pollable));
        };

        self.epoll
            .ctl(
                epoll::ControlOperation::Add,
                pollable,
                epoll::Event::new(
                    event_type.into(),
                    // Use the fd for event source identification in handlers.
                    pollable as u64,
                ),
            )
            .map_err(Error::Poll)?;

        self.handlers.insert(
            pollable,
            EventHandlerData::new((pollable, event_type), wrapped_handler.clone()),
        );
        Ok(())
    }

    /// Process register/unregister/update requests received by API channel.
    ///
    pub fn process_ops(&mut self) -> Result<u64> {
        let event_count = self.channel_tx.read_event();
        for _ in 0..event_count {
            match self.channel_rx.try_recv() {
                Ok((wrapped_handler, ops)) => {
                    self.update(wrapped_handler, ops)?;
                    Ok(())
                }
                // We expect to try reading until TryRecvError::Empty.
                Err(err) => match err {
                    TryRecvError::Empty => Ok(()),
                    TryRecvError::Disconnected => Err(Error::ChannelDisconnect),
                },
            }?
        }
        Ok(event_count)
    }

    /// Update an event handler pollables and event sets.
    /// Use the PollableOpBuilder to build the vector of PollableOps.
    ///
    pub fn update(
        &mut self,
        wrapped_handler: WrappedHandler,
        pollable_ops: Vec<PollableOp>,
    ) -> Result<()> {
        for op in pollable_ops {
            match op {
                PollableOp::Register(data) => {
                    self.register_handler(data, wrapped_handler.clone())?
                }
                PollableOp::Unregister(pollable) => self.unregister(pollable)?,
                PollableOp::Update(data) => self.update_event(data)?,
            }
        }
        Ok(())
    }

    /// Register a new event handler.
    /// handler.init() will specify the pollable and event set.
    ///
    pub fn register<T: EventHandler + 'static>(&mut self, handler: T) -> Result<Arc<Mutex<T>>> {
        let pollable_ops = handler.init();
        let wrapped_type = Arc::new(Mutex::new(handler));
        let wrapped_handler: Arc<Mutex<dyn EventHandler>> = wrapped_type.clone();

        self.update(wrapped_handler, pollable_ops)?;

        Ok(wrapped_type)
    }

    fn update_event(&mut self, event: EventRegistrationData) -> Result<()> {
        if let Some(handler_data) = self.handlers.get_mut(&event.0) {
            self.epoll
                .ctl(
                    epoll::ControlOperation::Modify,
                    event.0,
                    epoll::Event::new(event.1.into(), event.0 as u64),
                )
                .map_err(Error::Poll)?;
            handler_data.data = event;
        } else {
            return Err(Error::NotFound(event.0));
        }

        Ok(())
    }

    /// Unregister the event handler for the specified pollable.
    ///
    pub fn unregister(&mut self, pollable: Pollable) -> Result<()> {
        match self.handlers.remove(&pollable) {
            Some(_) => {
                self.epoll
                    .ctl(
                        epoll::ControlOperation::Delete,
                        pollable,
                        epoll::Event::default(),
                    )
                    .map_err(Error::Poll)?;
            }
            None => {
                return Err(Error::NotFound(pollable));
            }
        }
        Ok(())
    }

    // Dispatch an epoll event set for a handler.
    #[inline(always)]
    fn dispatch_event(
        &mut self,
        source: Pollable,
        evset: epoll::EventType,
        wrapped_handler: WrappedHandler,
    ) -> Result<()> {
        let mut all_ops = Vec::new();

        let mut handler = wrapped_handler.lock().expect("Handler lock is poisoned");

        // If an error occurs on a fd then only dispatch the error callback,
        // ignoring other flags.
        if evset.contains(epoll::EventType::ERROR) {
            all_ops.append(&mut handler.handle_error(source));
        } else {
            // We expect EventHandler implementors to be prepared to
            // handle multiple events for a pollable in this order:
            // READ, WRITE, CLOSE.
            if evset.contains(epoll::EventType::IN) {
                all_ops.append(&mut handler.handle_read(source));
            }
            if evset.contains(epoll::EventType::OUT) {
                all_ops.append(&mut handler.handle_write(source));
            }
            if evset.contains(epoll::EventType::READ_HANG_UP) {
                all_ops.append(&mut handler.handle_close(source));
            }
        }

        self.update(wrapped_handler.clone(), all_ops)?;
        Ok(())
    }

    // Process/dispatch buffered epoll events.
    fn process_events(&mut self, event_count: usize) -> Result<()> {
        for idx in 0..event_count {
            let event = self.ready_events[idx];
            let event_mask = event.events();
            let event_data = event.data();
            let evset = match epoll::EventType::from_bits(event_mask) {
                Some(evset) => evset,
                None => {
                    // Ignore unknown bits in event mask.
                    // TODO: We might want to log this as a warning but that requires a logger
                    // reference and a crate dependency.
                    continue;
                }
            };

            if let Some(event_handler_data) = self.handlers.get(event_data as i32) {
                if let Ok(()) = self.dispatch_event(
                    event_handler_data.data.0,
                    evset,
                    event_handler_data.handler,
                ) {
                    continue;
                } else {
                    // We might get errors related to PollableOps.
                    // Need to decide what to do with them.
                    // Options:
                    // 1. Break loop and throw them to the caller
                    // 2. Invoke a TBD event handler error callback.
                }
            } else {
                // There is no handler registered for this event set/pollable.
            }
        }

        Ok(())
    }

    // Wait for events, then dispatch to registered event handlers.
    pub fn run(&mut self) -> Result<usize> {
        self.run_timeout(-1)
    }

    // Wait for events or a timeout, then dispatch to registered event handlers.
    pub fn run_timeout(&mut self, milliseconds: i32) -> Result<usize> {
        let event_count = self
            .epoll
            .wait(EVENT_BUFFER_SIZE, milliseconds, &mut self.ready_events[..])
            .map_err(Error::Poll)?;
        self.process_events(event_count)?;

        self.process_ops()?;

        Ok(event_count)
    }
}

// Cascaded epoll support.
impl EventHandler for EventManager {
    fn handle_read(&mut self, _source: Pollable) -> Vec<PollableOp> {
        match self.run_timeout(DEFAULT_EPOLL_TIMEOUT) {
            Ok(_) => vec![],
            Err(_) => vec![],
        }
    }

    // Returns the epoll fd to the parent EventManager.
    fn init(&self) -> Vec<PollableOp> {
        vec![PollableOpBuilder::new(self.epoll.as_raw_fd())
            .readable()
            .register()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pollable::EventSet;

    struct DummyEventConsumer {
        pollable: Pollable,
        event_fd: EventFd,
        read: bool,
        write: bool,
        error: bool,
        close: bool,
    }

    impl DummyEventConsumer {
        pub fn new() -> Self {
            let event_fd = EventFd::new(libc::EFD_NONBLOCK).unwrap();
            DummyEventConsumer {
                pollable: event_fd.as_raw_fd(),
                event_fd,
                read: false,
                write: false,
                error: false,
                close: false,
            }
        }
    }

    impl EventHandler for DummyEventConsumer {
        /// Handle a read event (EPOLLIN).
        fn handle_read(&mut self, source: Pollable) -> Vec<PollableOp> {
            if source == self.event_fd.as_raw_fd() {
                self.read = true;
            }
            vec![]
        }
        /// Handle a write event (EPOLLOUT).
        fn handle_write(&mut self, source: Pollable) -> Vec<PollableOp> {
            if source == self.event_fd.as_raw_fd() {
                self.write = true;
            }
            vec![]
        }
        /// Handle a close event (EPOLLRDHUP).
        fn handle_close(&mut self, source: Pollable) -> Vec<PollableOp> {
            if source == self.event_fd.as_raw_fd() {
                self.close = true;
            }
            vec![]
        }
        /// Handle an error event (EPOLLERR).assert_ne!
        fn handle_error(&mut self, source: Pollable) -> Vec<PollableOp> {
            if source == self.event_fd.as_raw_fd() {
                self.error = true;
            }
            vec![]
        }

        /// Initial registration of pollable objects.
        /// Use the PollableOpBuilder to build the vector of PollableOps.
        fn init(&self) -> Vec<PollableOp> {
            vec![PollableOpBuilder::new(self.pollable)
                .readable()
                .writeable()
                .closeable()
                .register()]
        }
    }

    #[test]
    fn test_event_type_to_epoll_mask() {
        let mask: epoll::EventType = (EventSet::READ | EventSet::WRITE | EventSet::CLOSE).into();
        let epoll_mask =
            epoll::EventType::IN | epoll::EventType::OUT | epoll::EventType::READ_HANG_UP;

        assert_eq!(mask, epoll_mask);
    }

    #[test]
    fn test_channel() {
        let mut em = EventManager::new().unwrap();
        let mut channel = em.get_channel().unwrap();
        let dummy = DummyEventConsumer::new();
        let pollable = dummy.pollable;
        let event_fd = EventFd::new(libc::EFD_NONBLOCK).unwrap();
        let pollable2 = event_fd.as_raw_fd();

        let handler = Arc::new(Mutex::new(dummy));
        let mut ops = vec![PollableOpBuilder::new(pollable).readable().register()];
        channel.send((handler.clone(), ops)).unwrap();
        ops = vec![PollableOpBuilder::new(pollable2).readable().register()];
        channel.send((handler.clone(), ops)).unwrap();

        assert_eq!(em.process_ops().unwrap(), 2);
    }

    #[test]
    fn test_channel_api_register() {
        let mut em = EventManager::new().unwrap();
        let mut channel = em.get_channel().unwrap();
        let dummy = DummyEventConsumer::new();
        let pollable = dummy.pollable;
        let handler = Arc::new(Mutex::new(dummy));

        // Negative test: register the same pollable/handler twice.
        let mut ops = vec![PollableOpBuilder::new(pollable).readable().register()];
        channel.send((handler.clone(), ops)).unwrap();
        ops = vec![PollableOpBuilder::new(pollable).readable().register()];
        channel.send((handler.clone(), ops)).unwrap();

        assert!(em.process_ops().is_err());

        // Validate the handler is registered.
        let event_fd = EventFd::new(libc::EFD_NONBLOCK).unwrap();
        let pollable2 = event_fd.as_raw_fd();
        ops = vec![PollableOpBuilder::new(pollable2).writeable().register()];
        channel.send((handler.clone(), ops)).unwrap();

        assert_eq!(em.process_ops().unwrap(), 1);
        assert!(em.handlers.get(pollable).is_some());
    }

    #[test]
    fn test_callback_api_register() {
        // Test registration via register()/init() callback api.
        let mut em = EventManager::new().unwrap();

        let handler = em.register(DummyEventConsumer::new()).unwrap();
        let pollable = handler.lock().expect("Unlock failed.").pollable;

        assert!(em.handlers.get(pollable).is_some());
    }

    #[test]
    fn test_update_api_register() {
        // Test registration via update() api.
        let mut em = EventManager::new().unwrap();

        let handler = Arc::new(Mutex::new(DummyEventConsumer::new()));
        let pollable = handler.lock().expect("Unlock failed.").pollable;
        let ops = handler.lock().expect("Unlock failed.").init();
        em.update(handler, ops).unwrap();

        let handler_data = em.handlers.get(pollable);
        assert!(handler_data.is_some());
        let reg_data = handler_data.unwrap().data;
        assert_eq!(
            reg_data,
            (pollable, EventSet::READ | EventSet::WRITE | EventSet::CLOSE)
        );
    }

    #[test]
    fn test_update_api_unregister() {
        // Test unregistration via update() api.
        let mut em = EventManager::new().unwrap();

        let handler = Arc::new(Mutex::new(DummyEventConsumer::new()));
        let pollable = handler.lock().expect("Unlock failed.").pollable;
        let ops = handler.lock().expect("Unlock failed.").init();
        em.update(handler.clone(), ops).unwrap();

        let mut handler_data = em.handlers.get(pollable);
        assert!(handler_data.is_some());

        assert!(em
            .update(
                handler.clone(),
                vec![PollableOpBuilder::new(pollable).unregister()]
            )
            .is_ok());

        handler_data = em.handlers.get(pollable);
        assert!(handler_data.is_none());
    }

    #[test]
    fn test_channel_api_unregister() {
        // Test unregistration via channel api.
        let mut em = EventManager::new().unwrap();
        let mut channel = em.get_channel().unwrap();

        let handler = em.register(DummyEventConsumer::new()).unwrap();
        let pollable = handler.lock().expect("Unlock failed.").pollable;

        let mut handler_data = em.handlers.get(pollable);
        assert!(handler_data.is_some());

        channel
            .send((
                handler.clone(),
                vec![PollableOpBuilder::new(pollable).unregister()],
            ))
            .unwrap();

        assert_eq!(em.process_ops().unwrap(), 1);

        handler_data = em.handlers.get(pollable);
        assert!(handler_data.is_none());
    }

    #[test]
    fn test_unregister_fail() {
        // Test unregistration failure.
        let mut em = EventManager::new().unwrap();

        let handler = Arc::new(Mutex::new(DummyEventConsumer::new()));
        let pollable = handler.lock().expect("Unlock failed").pollable;

        let mut handler_data = em.handlers.get(pollable);
        assert!(handler_data.is_none());

        let err = em.unregister(pollable).unwrap_err();
        assert_eq!(
            format!("{:?}", err),
            format!(
                "A handler for the specified pollable {} was not found.",
                pollable
            )
        );

        handler_data = em.handlers.get(pollable);
        assert!(handler_data.is_none());
    }

    #[test]
    fn test_register_fail() {
        // Test registration failure.
        let mut em = EventManager::new().unwrap();

        let dummy1 = DummyEventConsumer::new();
        let mut dummy2 = DummyEventConsumer::new();
        let mut dummy3 = DummyEventConsumer::new();
        dummy2.pollable = dummy1.pollable;
        dummy3.pollable = dummy1.pollable;
        let pollable = dummy1.pollable;

        em.register(dummy1).unwrap();
        assert!(em.register(dummy2).is_err());
        match em.register(dummy3) {
            Err(err) => assert_eq!(
                format!("{:?}", err),
                format!(
                    "A handler for the specified pollable {} already exists.",
                    pollable
                )
            ),
            Ok(_) => panic!("Registration should fail for duplicate fds."),
        }
    }

    #[test]
    fn test_channel_api_update() {
        // Test update pollable eventset via channel api.
        let mut em = EventManager::new().unwrap();
        let mut channel = em.get_channel().unwrap();

        let handler = Arc::new(Mutex::new(DummyEventConsumer::new()));
        let pollable = handler.lock().expect("Unlock failed.").pollable;
        let ops = handler.lock().expect("Unlock failed.").init();
        // register via update
        em.update(handler.clone(), ops).unwrap();

        let mut handler_data = em.handlers.get(pollable);
        assert!(handler_data.is_some());

        channel
            .send((
                handler.clone(),
                vec![PollableOpBuilder::new(pollable).writeable().update()],
            ))
            .unwrap();

        assert_eq!(em.process_ops().unwrap(), 1);

        handler_data = em.handlers.get(pollable);
        assert!(handler_data.is_some());
        let reg_data = handler_data.unwrap().data;
        assert_eq!(reg_data, (pollable, EventSet::WRITE));
    }

    #[test]
    fn test_read_event() {
        let mut em = EventManager::new().unwrap();
        let handler = em.register(DummyEventConsumer::new()).unwrap();

        handler
            .lock()
            .expect("Unlock failed")
            .event_fd
            .write(1)
            .unwrap();

        em.run_timeout(1).unwrap();
        assert!(handler.lock().expect("Unlock failed").read);
    }

    #[test]
    fn test_write_event() {
        let mut em = EventManager::new().unwrap();
        let handler = em.register(DummyEventConsumer::new()).unwrap();

        em.run().unwrap();
        assert!(handler.lock().expect("Unlock failed").write);
    }

    #[test]
    fn test_cascaded_events() {
        let mut root_em = EventManager::new().unwrap();
        let mut em = EventManager::new().unwrap();
        let handler = em.register(DummyEventConsumer::new()).unwrap();
        handler
            .lock()
            .expect("Unlock failed")
            .event_fd
            .write(1)
            .unwrap();

        root_em.register(em).unwrap();

        root_em.run().unwrap();
        assert!(handler.lock().expect("Unlock failed").read);
        assert!(handler.lock().expect("Unlock failed").write);
    }
}