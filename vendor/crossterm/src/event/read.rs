use std::{collections::vec_deque::VecDeque, io, time::Duration};

#[cfg(unix)]
use crate::event::source::unix::UnixInternalEventSource;
#[cfg(windows)]
use crate::event::source::windows::WindowsEventSource;
#[cfg(feature = "event-stream")]
use crate::event::sys::Waker;
use crate::event::{filter::Filter, source::EventSource, timeout::PollTimeout, InternalEvent};

/// Can be used to read `InternalEvent`s.
pub(crate) struct InternalEventReader {
    events: VecDeque<InternalEvent>,
    source: Option<Box<dyn EventSource>>,
    #[cfg(all(unix, feature = "use-dev-tty"))]
    failure: Option<super::source::unix::input_buffer::InputLimit>,
    #[cfg(not(all(unix, feature = "use-dev-tty")))]
    skipped_events: Vec<InternalEvent>,
}

impl Default for InternalEventReader {
    fn default() -> Self {
        #[cfg(windows)]
        let source = WindowsEventSource::new();
        #[cfg(unix)]
        let source = UnixInternalEventSource::new();

        let source = source.ok().map(|x| Box::new(x) as Box<dyn EventSource>);

        InternalEventReader {
            source,
            events: VecDeque::with_capacity(32),
            #[cfg(all(unix, feature = "use-dev-tty"))]
            failure: None,
            #[cfg(not(all(unix, feature = "use-dev-tty")))]
            skipped_events: Vec::with_capacity(32),
        }
    }
}

impl InternalEventReader {
    /// Returns a `Waker` allowing to wake/force the `poll` method to return `Ok(false)`.
    #[cfg(feature = "event-stream")]
    pub(crate) fn waker(&self) -> Waker {
        self.source.as_ref().expect("reader source not set").waker()
    }

    #[cfg(not(all(unix, feature = "use-dev-tty")))]
    pub(crate) fn poll<F>(&mut self, timeout: Option<Duration>, filter: &F) -> io::Result<bool>
    where
        F: Filter,
    {
        for event in &self.events {
            if filter.eval(event) {
                return Ok(true);
            }
        }

        let event_source = match self.source.as_mut() {
            Some(source) => source,
            None => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "Failed to initialize input reader",
                ))
            }
        };

        let poll_timeout = PollTimeout::new(timeout);

        loop {
            let maybe_event = match event_source.try_read(poll_timeout.leftover()) {
                Ok(None) => None,
                Ok(Some(event)) => {
                    if filter.eval(&event) {
                        Some(event)
                    } else {
                        self.skipped_events.push(event);
                        None
                    }
                }
                Err(e) => {
                    if e.kind() == io::ErrorKind::Interrupted {
                        return Ok(false);
                    }

                    return Err(e);
                }
            };

            if poll_timeout.elapsed() || maybe_event.is_some() {
                self.events.extend(self.skipped_events.drain(..));

                if let Some(event) = maybe_event {
                    self.events.push_front(event);
                    return Ok(true);
                }

                return Ok(false);
            }
        }
    }

    #[cfg(not(all(unix, feature = "use-dev-tty")))]
    pub(crate) fn read<F>(&mut self, filter: &F) -> io::Result<InternalEvent>
    where
        F: Filter,
    {
        let mut skipped_events = VecDeque::new();

        loop {
            while let Some(event) = self.events.pop_front() {
                if filter.eval(&event) {
                    while let Some(event) = skipped_events.pop_front() {
                        self.events.push_back(event);
                    }

                    return Ok(event);
                } else {
                    // We can not directly write events back to `self.events`.
                    // If we did, we would put our self's into an endless loop
                    // that would enqueue -> dequeue -> enqueue etc.
                    // This happens because `poll` in this function will always return true if there are events in it's.
                    // And because we just put the non-fulfilling event there this is going to be the case.
                    // Instead we can store them into the temporary buffer,
                    // and then when the filter is fulfilled write all events back in order.
                    skipped_events.push_back(event);
                }
            }

            let _ = self.poll(None, filter)?;
        }
    }

    #[cfg(all(unix, feature = "use-dev-tty"))]
    pub(crate) fn poll<F>(&mut self, timeout: Option<Duration>, filter: &F) -> io::Result<bool>
    where
        F: Filter,
    {
        self.check_failure()?;
        if let Err(error) = check_queue_capacity(self.events.iter()) {
            return Err(self.admission_error(error));
        }
        if self.events.iter().any(|event| filter.eval(event)) {
            return Ok(true);
        }
        let poll_timeout = PollTimeout::new(timeout);
        loop {
            let result = match self.source.as_mut() {
                Some(source) => source.try_read(poll_timeout.leftover()),
                None => {
                    return Err(io::Error::new(
                        io::ErrorKind::Other,
                        "Failed to initialize input reader",
                    ))
                }
            };
            match result {
                Ok(Some(event)) => {
                    if let Err(error) =
                        check_queue_capacity(self.events.iter().chain(std::iter::once(&event)))
                    {
                        return Err(self.admission_error(error));
                    }
                    let matches = filter.eval(&event);
                    // All retained events have one owner. Filtering does not
                    // hide another queue from admission or reorder older input.
                    self.events.push_back(event);
                    if matches {
                        return Ok(true);
                    }
                }
                Ok(None) => {}
                Err(error) if error.kind() == io::ErrorKind::Interrupted => return Ok(false),
                Err(error) => return Err(self.admission_error(error)),
            }
            if poll_timeout.elapsed() {
                return Ok(false);
            }
        }
    }

    #[cfg(all(unix, feature = "use-dev-tty"))]
    pub(crate) fn read<F>(&mut self, filter: &F) -> io::Result<InternalEvent>
    where
        F: Filter,
    {
        loop {
            self.poll(None, filter)?;
            if let Some(index) = self.events.iter().position(|event| filter.eval(event)) {
                if let Some(event) = self.events.remove(index) {
                    return Ok(event);
                }
            }
        }
    }

    #[cfg(all(unix, feature = "use-dev-tty"))]
    fn check_failure(&self) -> io::Result<()> {
        #[cfg(all(unix, feature = "use-dev-tty"))]
        if let Some(failure) = self.failure {
            return Err(failure.error());
        }
        Ok(())
    }

    /// Admission failure is terminal for this reader, even if a capability
    /// probe ignores its immediate error. Clear every pending suffix and keep
    /// the original typed error if best-effort kernel input flushing fails.
    #[cfg(all(unix, feature = "use-dev-tty"))]
    fn admission_error(&mut self, error: io::Error) -> io::Error {
        #[cfg(all(unix, feature = "use-dev-tty"))]
        if let Some(failure) = error
            .get_ref()
            .and_then(|error| error.downcast_ref::<super::source::unix::input_buffer::InputLimit>())
            .copied()
        {
            self.failure = Some(failure);
            self.events.clear();
            if let Some(source) = self.source.as_mut() {
                source.discard_input();
            }
        }
        error
    }
}

// Quirl bounds filtered terminal replies as well as parser input. Otherwise a
// stream of cursor reports can accumulate while the caller waits for a key.
#[cfg(all(unix, feature = "use-dev-tty"))]
fn check_queue_capacity<'a>(events: impl Iterator<Item = &'a InternalEvent>) -> io::Result<()> {
    #[cfg(all(unix, feature = "use-dev-tty"))]
    {
        let mut count = 0usize;
        let mut bytes = 0usize;
        for event in events {
            count = count.saturating_add(1);
            bytes = bytes.saturating_add(std::mem::size_of::<InternalEvent>());
            #[cfg(feature = "bracketed-paste")]
            if let InternalEvent::Event(crate::event::Event::Paste(text)) = event {
                bytes = bytes.saturating_add(text.len());
            }
        }
        super::source::unix::input_buffer::check_queue(count, bytes)
    }
    #[cfg(not(all(unix, feature = "use-dev-tty")))]
    {
        let _ = events;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::{collections::VecDeque, time::Duration};

    #[cfg(unix)]
    use super::super::filter::CursorPositionFilter;
    use super::{super::Event, EventSource, Filter, InternalEvent, InternalEventReader};

    #[derive(Debug, Clone)]
    pub(crate) struct InternalEventFilter;

    impl Filter for InternalEventFilter {
        fn eval(&self, _: &InternalEvent) -> bool {
            true
        }
    }

    #[test]
    fn test_poll_fails_without_event_source() {
        let mut reader = InternalEventReader {
            events: VecDeque::new(),
            source: None,
            #[cfg(all(unix, feature = "use-dev-tty"))]
            failure: None,
            #[cfg(not(all(unix, feature = "use-dev-tty")))]
            skipped_events: Vec::with_capacity(32),
        };

        assert!(reader.poll(None, &InternalEventFilter).is_err());
        assert!(reader
            .poll(Some(Duration::from_secs(0)), &InternalEventFilter)
            .is_err());
        assert!(reader
            .poll(Some(Duration::from_secs(10)), &InternalEventFilter)
            .is_err());
    }

    #[test]
    fn test_poll_returns_true_for_matching_event_in_queue_at_front() {
        let mut reader = InternalEventReader {
            events: vec![InternalEvent::Event(Event::Resize(10, 10))].into(),
            source: None,
            #[cfg(all(unix, feature = "use-dev-tty"))]
            failure: None,
            #[cfg(not(all(unix, feature = "use-dev-tty")))]
            skipped_events: Vec::with_capacity(32),
        };

        assert!(reader.poll(None, &InternalEventFilter).unwrap());
    }

    #[test]
    #[cfg(unix)]
    fn test_poll_returns_true_for_matching_event_in_queue_at_back() {
        let mut reader = InternalEventReader {
            events: vec![
                InternalEvent::Event(Event::Resize(10, 10)),
                InternalEvent::CursorPosition(10, 20),
            ]
            .into(),
            source: None,
            #[cfg(all(unix, feature = "use-dev-tty"))]
            failure: None,
            #[cfg(not(all(unix, feature = "use-dev-tty")))]
            skipped_events: Vec::with_capacity(32),
        };

        assert!(reader.poll(None, &CursorPositionFilter).unwrap());
    }

    #[test]
    fn test_read_returns_matching_event_in_queue_at_front() {
        const EVENT: InternalEvent = InternalEvent::Event(Event::Resize(10, 10));

        let mut reader = InternalEventReader {
            events: vec![EVENT].into(),
            source: None,
            #[cfg(all(unix, feature = "use-dev-tty"))]
            failure: None,
            #[cfg(not(all(unix, feature = "use-dev-tty")))]
            skipped_events: Vec::with_capacity(32),
        };

        assert_eq!(reader.read(&InternalEventFilter).unwrap(), EVENT);
    }

    #[test]
    #[cfg(unix)]
    fn test_read_returns_matching_event_in_queue_at_back() {
        const CURSOR_EVENT: InternalEvent = InternalEvent::CursorPosition(10, 20);

        let mut reader = InternalEventReader {
            events: vec![InternalEvent::Event(Event::Resize(10, 10)), CURSOR_EVENT].into(),
            source: None,
            #[cfg(all(unix, feature = "use-dev-tty"))]
            failure: None,
            #[cfg(not(all(unix, feature = "use-dev-tty")))]
            skipped_events: Vec::with_capacity(32),
        };

        assert_eq!(reader.read(&CursorPositionFilter).unwrap(), CURSOR_EVENT);
    }

    #[test]
    #[cfg(unix)]
    fn test_read_does_not_consume_skipped_event() {
        const SKIPPED_EVENT: InternalEvent = InternalEvent::Event(Event::Resize(10, 10));
        const CURSOR_EVENT: InternalEvent = InternalEvent::CursorPosition(10, 20);

        let mut reader = InternalEventReader {
            events: vec![SKIPPED_EVENT, CURSOR_EVENT].into(),
            source: None,
            #[cfg(all(unix, feature = "use-dev-tty"))]
            failure: None,
            #[cfg(not(all(unix, feature = "use-dev-tty")))]
            skipped_events: Vec::with_capacity(32),
        };

        assert_eq!(reader.read(&CursorPositionFilter).unwrap(), CURSOR_EVENT);
        assert_eq!(reader.read(&InternalEventFilter).unwrap(), SKIPPED_EVENT);
    }

    #[test]
    fn test_poll_timeouts_if_source_has_no_events() {
        let source = FakeSource::default();

        let mut reader = InternalEventReader {
            events: VecDeque::new(),
            source: Some(Box::new(source)),
            #[cfg(all(unix, feature = "use-dev-tty"))]
            failure: None,
            #[cfg(not(all(unix, feature = "use-dev-tty")))]
            skipped_events: Vec::with_capacity(32),
        };

        assert!(!reader
            .poll(Some(Duration::from_secs(0)), &InternalEventFilter)
            .unwrap());
    }

    #[test]
    fn test_poll_returns_true_if_source_has_at_least_one_event() {
        let source = FakeSource::with_events(&[InternalEvent::Event(Event::Resize(10, 10))]);

        let mut reader = InternalEventReader {
            events: VecDeque::new(),
            source: Some(Box::new(source)),
            #[cfg(all(unix, feature = "use-dev-tty"))]
            failure: None,
            #[cfg(not(all(unix, feature = "use-dev-tty")))]
            skipped_events: Vec::with_capacity(32),
        };

        assert!(reader.poll(None, &InternalEventFilter).unwrap());
        assert!(reader
            .poll(Some(Duration::from_secs(0)), &InternalEventFilter)
            .unwrap());
    }

    #[test]
    fn test_reads_returns_event_if_source_has_at_least_one_event() {
        const EVENT: InternalEvent = InternalEvent::Event(Event::Resize(10, 10));

        let source = FakeSource::with_events(&[EVENT]);

        let mut reader = InternalEventReader {
            events: VecDeque::new(),
            source: Some(Box::new(source)),
            #[cfg(all(unix, feature = "use-dev-tty"))]
            failure: None,
            #[cfg(not(all(unix, feature = "use-dev-tty")))]
            skipped_events: Vec::with_capacity(32),
        };

        assert_eq!(reader.read(&InternalEventFilter).unwrap(), EVENT);
    }

    #[test]
    fn test_read_returns_events_if_source_has_events() {
        const EVENT: InternalEvent = InternalEvent::Event(Event::Resize(10, 10));

        let source = FakeSource::with_events(&[EVENT, EVENT, EVENT]);

        let mut reader = InternalEventReader {
            events: VecDeque::new(),
            source: Some(Box::new(source)),
            #[cfg(all(unix, feature = "use-dev-tty"))]
            failure: None,
            #[cfg(not(all(unix, feature = "use-dev-tty")))]
            skipped_events: Vec::with_capacity(32),
        };

        assert_eq!(reader.read(&InternalEventFilter).unwrap(), EVENT);
        assert_eq!(reader.read(&InternalEventFilter).unwrap(), EVENT);
        assert_eq!(reader.read(&InternalEventFilter).unwrap(), EVENT);
    }

    #[test]
    fn test_poll_returns_false_after_all_source_events_are_consumed() {
        const EVENT: InternalEvent = InternalEvent::Event(Event::Resize(10, 10));

        let source = FakeSource::with_events(&[EVENT, EVENT, EVENT]);

        let mut reader = InternalEventReader {
            events: VecDeque::new(),
            source: Some(Box::new(source)),
            #[cfg(all(unix, feature = "use-dev-tty"))]
            failure: None,
            #[cfg(not(all(unix, feature = "use-dev-tty")))]
            skipped_events: Vec::with_capacity(32),
        };

        assert_eq!(reader.read(&InternalEventFilter).unwrap(), EVENT);
        assert_eq!(reader.read(&InternalEventFilter).unwrap(), EVENT);
        assert_eq!(reader.read(&InternalEventFilter).unwrap(), EVENT);
        assert!(!reader
            .poll(Some(Duration::from_secs(0)), &InternalEventFilter)
            .unwrap());
    }

    #[test]
    fn test_poll_propagates_error() {
        let mut reader = InternalEventReader {
            events: VecDeque::new(),
            source: Some(Box::new(FakeSource::new(&[]))),
            #[cfg(all(unix, feature = "use-dev-tty"))]
            failure: None,
            #[cfg(not(all(unix, feature = "use-dev-tty")))]
            skipped_events: Vec::with_capacity(32),
        };

        assert_eq!(
            reader
                .poll(Some(Duration::from_secs(0)), &InternalEventFilter)
                .err()
                .map(|e| format!("{:?}", &e.kind())),
            Some(format!("{:?}", io::ErrorKind::Other))
        );
    }

    #[test]
    fn test_read_propagates_error() {
        let mut reader = InternalEventReader {
            events: VecDeque::new(),
            source: Some(Box::new(FakeSource::new(&[]))),
            #[cfg(all(unix, feature = "use-dev-tty"))]
            failure: None,
            #[cfg(not(all(unix, feature = "use-dev-tty")))]
            skipped_events: Vec::with_capacity(32),
        };

        assert_eq!(
            reader
                .read(&InternalEventFilter)
                .err()
                .map(|e| format!("{:?}", &e.kind())),
            Some(format!("{:?}", io::ErrorKind::Other))
        );
    }

    #[test]
    fn test_poll_continues_after_error() {
        const EVENT: InternalEvent = InternalEvent::Event(Event::Resize(10, 10));

        let source = FakeSource::new(&[EVENT, EVENT]);

        let mut reader = InternalEventReader {
            events: VecDeque::new(),
            source: Some(Box::new(source)),
            #[cfg(all(unix, feature = "use-dev-tty"))]
            failure: None,
            #[cfg(not(all(unix, feature = "use-dev-tty")))]
            skipped_events: Vec::with_capacity(32),
        };

        assert_eq!(reader.read(&InternalEventFilter).unwrap(), EVENT);
        assert!(reader.read(&InternalEventFilter).is_err());
        assert!(reader
            .poll(Some(Duration::from_secs(0)), &InternalEventFilter)
            .unwrap());
    }

    #[test]
    fn test_read_continues_after_error() {
        const EVENT: InternalEvent = InternalEvent::Event(Event::Resize(10, 10));

        let source = FakeSource::new(&[EVENT, EVENT]);

        let mut reader = InternalEventReader {
            events: VecDeque::new(),
            source: Some(Box::new(source)),
            #[cfg(all(unix, feature = "use-dev-tty"))]
            failure: None,
            #[cfg(not(all(unix, feature = "use-dev-tty")))]
            skipped_events: Vec::with_capacity(32),
        };

        assert_eq!(reader.read(&InternalEventFilter).unwrap(), EVENT);
        assert!(reader.read(&InternalEventFilter).is_err());
        assert_eq!(reader.read(&InternalEventFilter).unwrap(), EVENT);
    }

    #[test]
    #[cfg(all(unix, feature = "use-dev-tty"))]
    fn filtered_read_rejects_the_first_excess_event_and_failure_stays_sticky() {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        let reads = Arc::new(AtomicUsize::new(0));
        let discards = Arc::new(AtomicUsize::new(0));
        let mut source = FakeSource::with_events(&[
            InternalEvent::Event(Event::Resize(20, 20)),
            InternalEvent::CursorPosition(1, 1),
        ]);
        source.reads = Some(Arc::clone(&reads));
        source.discards = Some(Arc::clone(&discards));
        let mut reader = InternalEventReader {
            events: vec![InternalEvent::Event(Event::Resize(10, 10)); 1024].into(),
            source: Some(Box::new(source)),
            failure: None,
            #[cfg(not(all(unix, feature = "use-dev-tty")))]
            skipped_events: Vec::with_capacity(32),
        };
        let error = reader.read(&CursorPositionFilter).unwrap_err();
        assert!(crate::event::is_input_limit_error(&error));
        assert!(error.to_string().contains("observed 1025 events"));
        assert_eq!(reads.load(Ordering::Relaxed), 1);
        assert_eq!(discards.load(Ordering::Relaxed), 1);
        assert!(reader.events.is_empty());
        let repeated = reader
            .poll(Some(Duration::ZERO), &InternalEventFilter)
            .unwrap_err();
        assert!(crate::event::is_input_limit_error(&repeated));
        assert_eq!(repeated.to_string(), error.to_string());
        assert_eq!(reads.load(Ordering::Relaxed), 1);
        assert!(reader.read(&InternalEventFilter).is_err());
        assert_eq!(discards.load(Ordering::Relaxed), 1);
    }

    #[test]
    #[cfg(all(unix, feature = "use-dev-tty"))]
    fn filtered_read_at_capacity_preserves_every_unmatched_event_in_order() {
        let events: VecDeque<_> = (0..1023)
            .map(|row| InternalEvent::Event(Event::Resize(row, 10)))
            .collect();
        let mut reader = InternalEventReader {
            events: events.clone(),
            source: Some(Box::new(FakeSource::with_events(&[
                InternalEvent::CursorPosition(1, 2),
            ]))),
            failure: None,
            #[cfg(not(all(unix, feature = "use-dev-tty")))]
            skipped_events: Vec::with_capacity(32),
        };
        assert_eq!(
            reader.read(&CursorPositionFilter).unwrap(),
            InternalEvent::CursorPosition(1, 2)
        );
        assert_eq!(reader.events, events);
    }

    #[test]
    #[cfg(all(unix, feature = "use-dev-tty", feature = "bracketed-paste"))]
    fn paste_queue_byte_limit_is_independent_of_event_count() {
        let paste = InternalEvent::Event(Event::Paste("x".repeat(256 * 1024)));
        let mut reader = InternalEventReader {
            events: vec![paste.clone(); 3].into(),
            source: Some(Box::new(FakeSource::with_events(&[paste]))),
            failure: None,
            #[cfg(not(all(unix, feature = "use-dev-tty")))]
            skipped_events: Vec::with_capacity(32),
        };
        let error = reader.poll(None, &CursorPositionFilter).unwrap_err();
        assert!(crate::event::is_input_limit_error(&error));
        assert!(error.to_string().contains("limit 1048576 bytes"));
        assert!(reader.events.is_empty());
        assert!(reader.read(&InternalEventFilter).is_err());
    }

    #[test]
    #[cfg(all(unix, feature = "use-dev-tty"))]
    fn source_admission_failure_is_sticky_but_ordinary_io_errors_remain_retryable() {
        let mut source = FakeSource::with_events(&[]);
        source.error = super::super::source::unix::input_buffer::check_queue(1025, 0).err();
        let mut reader = InternalEventReader {
            events: VecDeque::new(),
            source: Some(Box::new(source)),
            failure: None,
            #[cfg(not(all(unix, feature = "use-dev-tty")))]
            skipped_events: Vec::with_capacity(32),
        };
        let first = reader
            .poll(Some(Duration::ZERO), &InternalEventFilter)
            .unwrap_err();
        let repeated = reader
            .poll(Some(Duration::ZERO), &InternalEventFilter)
            .unwrap_err();
        assert!(crate::event::is_input_limit_error(&first));
        assert_eq!(first.to_string(), repeated.to_string());
        // Existing test_read_continues_after_error exercises ordinary I/O retry.
    }

    #[derive(Default)]
    struct FakeSource {
        events: VecDeque<InternalEvent>,
        error: Option<io::Error>,
        reads: Option<std::sync::Arc<std::sync::atomic::AtomicUsize>>,
        discards: Option<std::sync::Arc<std::sync::atomic::AtomicUsize>>,
    }

    impl FakeSource {
        fn new(events: &[InternalEvent]) -> FakeSource {
            FakeSource {
                events: events.to_vec().into(),
                error: Some(io::Error::new(io::ErrorKind::Other, "")),
                reads: None,
                discards: None,
            }
        }

        fn with_events(events: &[InternalEvent]) -> FakeSource {
            FakeSource {
                events: events.to_vec().into(),
                error: None,
                reads: None,
                discards: None,
            }
        }
    }

    impl EventSource for FakeSource {
        fn try_read(&mut self, _timeout: Option<Duration>) -> io::Result<Option<InternalEvent>> {
            if let Some(reads) = &self.reads {
                reads.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            // Return error if set in case there's just one remaining event
            if self.events.len() == 1 {
                if let Some(error) = self.error.take() {
                    return Err(error);
                }
            }

            // Return all events from the queue
            if let Some(event) = self.events.pop_front() {
                return Ok(Some(event));
            }

            // Return error if there're no more events
            if let Some(error) = self.error.take() {
                return Err(error);
            }

            // Timeout
            Ok(None)
        }

        #[cfg(all(unix, feature = "use-dev-tty"))]
        fn discard_input(&mut self) {
            self.events.clear();
            if let Some(discards) = &self.discards {
                discards.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }

        #[cfg(feature = "event-stream")]
        fn waker(&self) -> super::super::sys::Waker {
            unimplemented!();
        }
    }
}
