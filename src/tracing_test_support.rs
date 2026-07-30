// SPDX-FileCopyrightText: 2026 conpty-oxide contributors <https://github.com/P4suta/conpty-oxide/graphs/contributors>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Test-only event capture for diagnostics that must not silently disappear.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use tracing::metadata::LevelFilter;
use tracing::span::{Attributes, Id, Record};
use tracing::subscriber::Interest;
use tracing::{Event, Metadata, Subscriber};

struct EventCounter {
    events: Arc<AtomicUsize>,
}

impl Subscriber for EventCounter {
    fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _span: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }

    fn record(&self, _span: &Id, _values: &Record<'_>) {}

    fn record_follows_from(&self, _span: &Id, _follows: &Id) {}

    fn event(&self, _event: &Event<'_>) {
        self.events.fetch_add(1, Ordering::SeqCst);
    }

    fn enter(&self, _span: &Id) {}

    fn exit(&self, _span: &Id) {}

    fn register_callsite(&self, _metadata: &'static Metadata<'static>) -> Interest {
        Interest::always()
    }

    fn max_level_hint(&self) -> Option<LevelFilter> {
        Some(LevelFilter::TRACE)
    }
}

/// Runs `operation` under a subscriber and returns the number of emitted
/// tracing events.
pub(crate) fn count_events(operation: impl FnOnce()) -> usize {
    let events = Arc::new(AtomicUsize::new(0));
    let subscriber = EventCounter {
        events: Arc::clone(&events),
    };
    tracing::subscriber::with_default(subscriber, operation);
    events.load(Ordering::SeqCst)
}
