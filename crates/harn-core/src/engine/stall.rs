//! Stall detection: the agent looks busy, but only timers and spinners change on screen.

use std::hash::{DefaultHasher, Hash, Hasher};

/// Glyphs that animate on a busy screen without meaning progress.
const SPINNER_GLYPHS: &str = "·✢✳✶✻✽*⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏";

#[derive(Debug, Default)]
pub(super) struct StallWatch {
    window: Option<Window>,
}

#[derive(Debug)]
struct Window {
    hash: u64,
    since_ms: u64,
    reported: bool,
}

impl StallWatch {
    /// Tracks a working screen; any change besides timers and spinners starts a new window.
    pub(super) fn observe(&mut self, text: &str, now_ms: u64) {
        let hash = normalized_hash(text);
        if self.window.as_ref().is_none_or(|w| w.hash != hash) {
            self.window = Some(Window {
                hash,
                since_ms: now_ms,
                reported: false,
            });
        }
    }

    pub(super) fn reset(&mut self) {
        self.window = None;
    }

    /// When the current window counts as a stall, unless it was already reported.
    pub(super) fn stalled_at_ms(&self, stall_after_ms: u64) -> Option<u64> {
        self.window
            .as_ref()
            .filter(|w| !w.reported)
            .map(|w| w.since_ms.saturating_add(stall_after_ms))
    }

    pub(super) fn mark_reported(&mut self) {
        if let Some(window) = &mut self.window {
            window.reported = true;
        }
    }
}

/// The screen without digits, spinner glyphs and whitespace: timers and animation change those
/// even when nothing progresses.
fn normalized_hash(text: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    text.chars()
        .filter(|c| !(c.is_whitespace() || c.is_ascii_digit() || SPINNER_GLYPHS.contains(*c)))
        .for_each(|c| c.hash(&mut hasher));
    hasher.finish()
}
