//! Injected input goes out chunk by chunk with pauses in between (Ink-based TUIs treat text and
//! Enter in one write as a paste), without ever blocking the session loop.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

#[derive(Debug, Default)]
pub(crate) struct InputQueue {
    pending: VecDeque<(Instant, Vec<u8>)>,
}

impl InputQueue {
    /// Schedules `chunks` `gap` apart, the first one now or, if earlier input is still pending,
    /// one `gap` after it.
    pub(crate) fn push(
        &mut self,
        chunks: impl IntoIterator<Item = Vec<u8>>,
        gap: Duration,
        now: Instant,
    ) {
        let mut due = self
            .pending
            .back()
            .map_or(now, |(last, _)| (*last + gap).max(now));
        for chunk in chunks {
            self.pending.push_back((due, chunk));
            due += gap;
        }
    }

    pub(crate) fn pop_due(&mut self, now: Instant) -> Option<Vec<u8>> {
        let (due, _) = self.pending.front()?;
        if *due > now {
            return None;
        }
        self.pending.pop_front().map(|(_, chunk)| chunk)
    }

    pub(crate) fn next_due(&self) -> Option<Instant> {
        self.pending.front().map(|(due, _)| *due)
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    pub(crate) fn clear(&mut self) {
        self.pending.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GAP: Duration = Duration::from_millis(150);

    fn chunks(parts: &[&str]) -> Vec<Vec<u8>> {
        parts.iter().map(|p| p.as_bytes().to_vec()).collect()
    }

    #[test]
    fn chunks_go_out_one_gap_apart() {
        let t0 = Instant::now();
        let mut queue = InputQueue::default();
        queue.push(chunks(&["continue", "\r"]), GAP, t0);

        assert_eq!(queue.pop_due(t0), Some(b"continue".to_vec()));
        assert_eq!(queue.pop_due(t0), None);
        assert_eq!(queue.next_due(), Some(t0 + GAP));
        assert_eq!(queue.pop_due(t0 + GAP - Duration::from_millis(1)), None);
        assert_eq!(queue.pop_due(t0 + GAP), Some(b"\r".to_vec()));
        assert_eq!(queue.next_due(), None);
    }
}
