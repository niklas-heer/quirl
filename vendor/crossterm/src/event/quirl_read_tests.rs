//! Platform-fixture tests of the actual patched reader, without upstream dev dependencies.
//!
//! Only inert event/source contracts below are substitutes. The production reader,
//! filters, deadline helper and input admission implementation are included directly.
//! These tests establish queue ownership, ordering and sticky failure; real Quirl
//! PTY tests separately exercise the assembled Unix fd/parser/terminal boundary.
#![allow(dead_code)]

#[path = "source/unix/input_buffer.rs"]
mod input_buffer;

mod event {
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub(crate) enum Event {
        Resize(u16, u16),
        Paste(String),
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) struct KeyboardEnhancementFlags;
    impl KeyboardEnhancementFlags {
        pub(crate) const DISAMBIGUATE_ESCAPE_CODES: Self = Self;
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    pub(crate) enum InternalEvent {
        Event(Event),
        CursorPosition(u16, u16),
        KeyboardEnhancementFlags(KeyboardEnhancementFlags),
        PrimaryDeviceAttributes,
    }

    pub(crate) fn is_input_limit_error(error: &std::io::Error) -> bool {
        error
            .get_ref()
            .is_some_and(|error| error.is::<crate::input_buffer::InputLimit>())
    }

    mod source {
        pub(crate) trait EventSource: Send + Sync {
            fn try_read(
                &mut self,
                timeout: Option<std::time::Duration>,
            ) -> std::io::Result<Option<super::InternalEvent>>;
            fn discard_input(&mut self) {}
        }
        pub(crate) mod unix {
            pub(crate) use crate::input_buffer;
            pub(crate) struct UnixInternalEventSource;
            impl UnixInternalEventSource {
                pub(crate) fn new() -> std::io::Result<Self> {
                    Err(std::io::Error::other("fixture cannot open a real terminal"))
                }
            }
            impl super::EventSource for UnixInternalEventSource {
                fn try_read(
                    &mut self,
                    _: Option<std::time::Duration>,
                ) -> std::io::Result<Option<super::super::InternalEvent>> {
                    Err(std::io::Error::other("fixture cannot read a real terminal"))
                }
            }
        }
    }
    mod filter {
        include!("filter.rs");
    }
    mod timeout {
        include!("timeout.rs");
    }
    mod read {
        include!("read.rs");
    }
}
