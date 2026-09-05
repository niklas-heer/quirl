//! Quirl's bounded pending Unix input; see ADR 0031.
//!
//! A complete event clears its deadline. Failure is sticky: bytes after a
//! rejected sequence must never be reinterpreted as an executable suffix.

use std::{
    fmt, io,
    ops::Deref,
    time::{Duration, Instant},
};

const ESCAPE_BYTES_MAX: usize = 4 * 1024;
const PASTE_BYTES_MAX: usize = 256 * 1024 + 12;
const PENDING_TIME_MAX: Duration = Duration::from_secs(30);

pub(crate) fn check_queue(count: usize, bytes: usize) -> io::Result<()> {
    let (resource, observed, limit, unit) = if count > 1024 {
        ("event queue", count, 1024, "events")
    } else if bytes > 1024 * 1024 {
        ("event queue", bytes, 1024 * 1024, "bytes")
    } else {
        return Ok(());
    };
    Err(InputLimit {
        resource,
        limit: limit as u64,
        observed: observed as u64,
        unit,
    }
    .error())
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct InputLimit {
    resource: &'static str,
    limit: u64,
    observed: u64,
    unit: &'static str,
}

impl fmt::Display for InputLimit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "terminal input {} limit {} {}, observed {} {}",
            self.resource, self.limit, self.unit, self.observed, self.unit
        )
    }
}

impl std::error::Error for InputLimit {}

impl InputLimit {
    pub(crate) fn error(self) -> io::Error {
        io::Error::new(io::ErrorKind::InvalidData, self)
    }
}

#[derive(Debug, Default)]
pub(crate) struct InputBuffer {
    bytes: Vec<u8>,
    started: Option<Instant>,
    failure: Option<InputLimit>,
}

impl Deref for InputBuffer {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.bytes
    }
}

impl InputBuffer {
    pub(crate) fn push(&mut self, byte: u8) -> io::Result<()> {
        self.check_deadline()?;
        let limit = if self.bytes.starts_with(b"\x1b[200~") {
            PASTE_BYTES_MAX
        } else {
            ESCAPE_BYTES_MAX
        };
        if self.bytes.len() >= limit {
            return Err(self.fail(InputLimit {
                resource: "pending sequence",
                limit: limit as u64,
                observed: self.bytes.len() as u64 + 1,
                unit: "bytes",
            }));
        }
        self.started.get_or_insert_with(Instant::now);
        self.bytes.push(byte);
        Ok(())
    }

    pub(crate) fn clear(&mut self) {
        self.bytes.clear();
        self.started = None;
    }

    pub(crate) fn check_deadline(&mut self) -> io::Result<()> {
        if let Some(failure) = self.failure {
            return Err(failure.error());
        }
        if let Some(started) = self.started {
            let elapsed = started.elapsed();
            if elapsed >= PENDING_TIME_MAX {
                return Err(self.fail(InputLimit {
                    resource: "pending sequence deadline",
                    limit: PENDING_TIME_MAX.as_millis() as u64,
                    observed: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
                    unit: "ms",
                }));
            }
        }
        Ok(())
    }

    pub(crate) fn wait_limit(&self, requested: Option<Duration>) -> Option<Duration> {
        let pending = self
            .started
            .map(|start| PENDING_TIME_MAX.saturating_sub(start.elapsed()));
        match (requested, pending) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    fn fail(&mut self, failure: InputLimit) -> io::Error {
        self.clear();
        self.failure = Some(failure);
        failure.error()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_limit_accepts_exact_boundary_and_rejects_next_byte() {
        let mut buffer = InputBuffer::default();
        for _ in 0..ESCAPE_BYTES_MAX {
            buffer.push(b'1').unwrap();
        }
        assert_eq!(buffer.len(), ESCAPE_BYTES_MAX);
        let error = buffer.push(b'1').unwrap_err();
        assert!(error.get_ref().unwrap().is::<InputLimit>());
        assert!(buffer.is_empty());
        assert!(buffer.push(b'\r').is_err());
    }

    #[test]
    fn paste_limit_is_bounded_across_chunks_and_failure_stays_sticky() {
        let mut buffer = InputBuffer::default();
        for byte in b"\x1b[200~" {
            buffer.push(*byte).unwrap();
        }
        for _ in buffer.len()..PASTE_BYTES_MAX {
            buffer.push(b'x').unwrap();
        }
        assert_eq!(buffer.len(), PASTE_BYTES_MAX);
        assert!(buffer.push(b'x').is_err());
        buffer.clear();
        assert!(buffer.push(b'\r').is_err());
    }

    #[test]
    fn completed_input_resets_the_pending_deadline() {
        let mut buffer = InputBuffer::default();
        buffer.push(b'x').unwrap();
        buffer.started = Some(Instant::now() - PENDING_TIME_MAX);
        buffer.clear();
        assert!(buffer.push(b'y').is_ok());
    }

    #[test]
    fn idle_incomplete_input_expires_without_more_bytes() {
        let mut buffer = InputBuffer::default();
        buffer.push(0x1b).unwrap();
        buffer.started = Some(Instant::now() - PENDING_TIME_MAX);
        assert_eq!(buffer.wait_limit(None), Some(Duration::ZERO));
        assert!(buffer.check_deadline().is_err());
        assert!(buffer.is_empty());
    }

    #[test]
    fn caller_poll_budget_is_never_extended() {
        let mut buffer = InputBuffer::default();
        buffer.push(0x1b).unwrap();
        assert_eq!(
            buffer.wait_limit(Some(Duration::ZERO)),
            Some(Duration::ZERO)
        );
        assert!(buffer.wait_limit(None).unwrap() <= PENDING_TIME_MAX);
    }

    #[test]
    fn retained_event_count_and_bytes_have_independent_limits() {
        assert!(check_queue(1024, 1024 * 1024).is_ok());
        assert!(check_queue(1025, 0).is_err());
        assert!(check_queue(1, 1024 * 1024 + 1).is_err());
    }
}
