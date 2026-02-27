use std::time::{Duration, Instant};

use crate::artifact::PauseInterval;

/// Single source of truth for active (non-paused) elapsed time during a recording.
///
/// `PauseTimeline` is mutated exclusively by video-capture backend timestamps:
/// - [`mark_pause`](Self::mark_pause) — called with the `at` field from
///   `CaptureEvent::Paused { at }`
/// - [`mark_resume`](Self::mark_resume) — called with the `at` field from
///   `CaptureEvent::Resumed { at, .. }`
///
/// Audio lifecycle events (`AudioEvent::Paused`, `AudioEvent::Resumed`,
/// etc.) must **not** directly modify timeline state. Audio processors may *read*
/// the timeline for timestamp alignment, but never write to it.
#[derive(Debug)]
pub struct PauseTimeline {
    started_at: Instant,
    pause_started_at: Option<Instant>,
    intervals: Vec<PauseInterval>,
    total_paused: Duration,
}

impl PauseTimeline {
    pub fn new(started_at: Instant) -> Self {
        Self {
            started_at,
            pause_started_at: None,
            intervals: Vec::new(),
            total_paused: Duration::ZERO,
        }
    }

    /// Record the start of a pause interval.
    ///
    /// Must be called with a backend-provided `Instant` from
    /// `CaptureEvent::Paused { at }`, **not** `Instant::now()`.
    /// Using the backend timestamp ensures the pause point reflects the
    /// actual capture-pipeline pause, not the coordinator's processing
    /// latency.
    pub fn mark_pause(&mut self, at: Instant) {
        if self.pause_started_at.is_none() {
            self.pause_started_at = Some(at);
        }
    }

    /// Record the end of a pause interval and accumulate paused duration.
    ///
    /// Must be called with a backend-provided `Instant` from
    /// `CaptureEvent::Resumed { at, .. }`, **not** `Instant::now()`.
    /// The interval `[pause_start, at)` is appended to the timeline and
    /// its duration is added to `total_paused`.
    pub fn mark_resume(&mut self, at: Instant) {
        if let Some(start) = self.pause_started_at.take() {
            let start_ms = start.saturating_duration_since(self.started_at).as_millis() as u64;
            let end_ms = at.saturating_duration_since(self.started_at).as_millis() as u64;
            self.total_paused += at.saturating_duration_since(start);
            self.intervals.push(PauseInterval { start_ms, end_ms });
        }
    }

    pub fn finalize(&mut self, at: Instant) {
        if self.pause_started_at.is_some() {
            self.mark_resume(at);
        }
    }

    pub fn intervals(&self) -> &[PauseInterval] {
        &self.intervals
    }

    pub fn active_elapsed_duration(&self, at: Instant) -> Duration {
        let elapsed = at.saturating_duration_since(self.started_at);
        let mut paused = self.total_paused;
        if let Some(paused_from) = self.pause_started_at {
            paused += at.saturating_duration_since(paused_from);
        }
        elapsed.saturating_sub(paused)
    }

    pub fn active_elapsed_ms(&self, at: Instant) -> u64 {
        self.active_elapsed_duration(at).as_millis() as u64
    }

    pub fn active_elapsed_from_stream_offset(&self, offset: Duration) -> Duration {
        let at = self
            .started_at
            .checked_add(offset)
            .unwrap_or(self.started_at);
        self.active_elapsed_duration(at)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// Strategy that generates a sorted sequence of pause/resume offset pairs (in ms).
    ///
    /// Each pair `(pause_offset, resume_offset)` satisfies
    /// `pause_offset < resume_offset`, and pairs are non-overlapping and
    /// strictly ordered so that the timeline sees a valid alternating
    /// pause/resume sequence.
    fn arb_pause_resume_pairs() -> impl Strategy<Value = Vec<(u64, u64)>> {
        prop::collection::vec((1u64..500, 1u64..500), 1..=8).prop_map(|gaps| {
            let mut cursor: u64 = 0;
            let mut pairs = Vec::new();
            for (gap_before, pause_dur) in gaps {
                cursor += gap_before; // active time before this pause
                let pause_at = cursor;
                cursor += pause_dur; // paused duration
                let resume_at = cursor;
                pairs.push((pause_at, resume_at));
            }
            pairs
        })
    }

    proptest! {
        #[test]
        fn prop_pause_timeline_uses_backend_timestamps(
            pairs in arb_pause_resume_pairs(),
            extra_active_ms in 0u64..500,
        ) {
            let started_at = Instant::now();
            let mut timeline = PauseTimeline::new(started_at);

            let mut total_paused_ms: u64 = 0;

            for &(pause_ms, resume_ms) in &pairs {
                let pause_at = started_at + Duration::from_millis(pause_ms);
                let resume_at = started_at + Duration::from_millis(resume_ms);

                timeline.mark_pause(pause_at);
                timeline.mark_resume(resume_at);

                total_paused_ms += resume_ms - pause_ms;
            }

            let intervals = timeline.intervals();
            prop_assert_eq!(
                intervals.len(),
                pairs.len(),
                "expected {} intervals, got {}",
                pairs.len(),
                intervals.len(),
            );

            for (i, (&(pause_ms, resume_ms), interval)) in
                pairs.iter().zip(intervals.iter()).enumerate()
            {
                prop_assert_eq!(
                    interval.start_ms, pause_ms,
                    "interval[{}].start_ms: expected {} (backend), got {}",
                    i, pause_ms, interval.start_ms,
                );
                prop_assert_eq!(
                    interval.end_ms, resume_ms,
                    "interval[{}].end_ms: expected {} (backend), got {}",
                    i, resume_ms, interval.end_ms,
                );
            }

            let last_resume_ms = pairs.last().map(|&(_, r)| r).unwrap_or(0);
            let query_ms = last_resume_ms + extra_active_ms;
            let query_at = started_at + Duration::from_millis(query_ms);

            let active_ms = timeline.active_elapsed_ms(query_at);
            let expected_active_ms = query_ms - total_paused_ms;

            prop_assert_eq!(
                active_ms, expected_active_ms,
                "active_elapsed_ms: expected {} (total {} - paused {}), got {}",
                expected_active_ms, query_ms, total_paused_ms, active_ms,
            );
        }
    }
}
