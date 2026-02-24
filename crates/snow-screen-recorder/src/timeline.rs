use std::time::{Duration, Instant};

use crate::artifact::PauseInterval;

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

    pub fn mark_pause(&mut self, at: Instant) {
        if self.pause_started_at.is_none() {
            self.pause_started_at = Some(at);
        }
    }

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
