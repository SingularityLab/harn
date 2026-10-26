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

    #[test]
    fn late_polling_releases_everything_due_in_order() {
        let t0 = Instant::now();
        let mut queue = InputQueue::default();
        queue.push(chunks(&["\x1b", "continue", "\r"]), GAP, t0);
        let late = t0 + GAP * 5;
        let released: Vec<_> = std::iter::from_fn(|| queue.pop_due(late)).collect();
        assert_eq!(released, chunks(&["\x1b", "continue", "\r"]));
    }

    #[test]
    fn a_second_action_queues_behind_the_first() {
        let t0 = Instant::now();
        let mut queue = InputQueue::default();
        queue.push(chunks(&["a", "b"]), GAP, t0);
        queue.push(chunks(&["c"]), GAP, t0);
        assert_eq!(queue.pop_due(t0), Some(b"a".to_vec()));
        assert_eq!(queue.pop_due(t0 + GAP), Some(b"b".to_vec()));
        assert_eq!(queue.next_due(), Some(t0 + GAP * 2));
    }

    #[test]
    fn clear_drops_pending_input() {
        let t0 = Instant::now();
        let mut queue = InputQueue::default();
        queue.push(chunks(&["continue", "\r"]), GAP, t0);
        queue.clear();
        assert_eq!(queue.pop_due(t0 + GAP), None);
        assert_eq!(queue.next_due(), None);
    }
}
