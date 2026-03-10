use std::collections::HashSet;

use snow_cursor_capture::CursorFrameSample;

use crate::mouse::{CursorFrameRecord, CursorShapeRecord, MouseStore};

/// Processes cursor data: shape deduplication and frame recording.
///
/// Maintains a `MouseStore` that accumulates cursor shapes and frame
/// records over the lifetime of a recording. Shapes are deduplicated
/// by `shape_id` so each unique cursor image is stored exactly once.
pub(crate) struct CursorProcessor {
    mouse_store: MouseStore,
    shapes_emitted: HashSet<u64>,
    last_frame: Option<CursorFrameRecord>,
    capture_origin_x: i32,
    capture_origin_y: i32,
}

impl CursorProcessor {
    /// Create a new `CursorProcessor` with the given capture origin
    /// for coordinate translation.
    pub(crate) fn new(capture_origin_x: i32, capture_origin_y: i32) -> Self {
        Self {
            mouse_store: MouseStore::new(),
            shapes_emitted: HashSet::new(),
            last_frame: None,
            capture_origin_x,
            capture_origin_y,
        }
    }

    /// Record a cursor frame from a capture sample.
    ///
    /// If the sample carries a new cursor shape (one whose `shape_id`
    /// has not been seen before), a `CursorShapeRecord` is appended to
    /// the mouse store. Exactly one `CursorFrameRecord` is appended
    /// per call, with coordinates translated by the capture origin.
    pub(crate) fn record_frame(&mut self, timestamp_ms: u64, cursor: &CursorFrameSample) {
        if let Some(shape) = cursor.shape.as_ref() {
            if self.shapes_emitted.insert(shape.shape_id) {
                self.mouse_store
                    .cursor_shapes
                    .push(CursorShapeRecord::from_cursor_shape(shape));
            }
        }

        let frame = CursorFrameRecord::from_sample(
            cursor,
            timestamp_ms,
            self.capture_origin_x,
            self.capture_origin_y,
        );
        self.last_frame = Some(frame.clone());
        self.mouse_store.cursor_frames.push(frame);
    }

    /// Synthesize a cursor frame for a dropped video frame by cloning
    /// the last recorded frame with an updated timestamp.
    pub(crate) fn synthesize_frame_for_drop(&mut self, timestamp_ms: u64) {
        if let Some(last) = self.last_frame.as_mut() {
            last.timestamp_ms = timestamp_ms;
            self.mouse_store.cursor_frames.push(last.clone());
        }
    }

    #[cfg(test)]
    /// Return a reference to the accumulated mouse store.
    pub(crate) fn mouse_store(&self) -> &MouseStore {
        &self.mouse_store
    }

    #[cfg(test)]
    /// Return a reference to the last recorded cursor frame, if any.
    pub(crate) fn last_frame(&self) -> Option<&CursorFrameRecord> {
        self.last_frame.as_ref()
    }

    /// Consume the processor and return the accumulated mouse store.
    pub(crate) fn into_mouse_store(self) -> MouseStore {
        self.mouse_store
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use snow_cursor_capture::{CursorCompositionMode, CursorShape};
    use std::collections::HashSet;

    /// Build a `CursorFrameSample` with an attached shape for the given
    /// `shape_id`. The shape RGBA payload is deterministic but unique
    /// per id so that the processor sees genuinely different shapes.
    fn sample_with_shape(shape_id: u64) -> CursorFrameSample {
        CursorFrameSample {
            position_x: 100,
            position_y: 200,
            visible: true,
            shape_id: Some(shape_id),
            shape: Some(CursorShape {
                shape_id,
                hotspot_x: 0,
                hotspot_y: 0,
                width: 1,
                height: 1,
                composition_mode: CursorCompositionMode::AlphaBlend,
                shape_rgba: vec![shape_id as u8; 4],
            }),
        }
    }

    /// Build a `CursorFrameSample` that references a `shape_id` but
    /// carries no shape payload (the shape was already emitted).
    fn sample_without_shape(shape_id: u64) -> CursorFrameSample {
        CursorFrameSample {
            position_x: 50,
            position_y: 60,
            visible: true,
            shape_id: Some(shape_id),
            shape: None,
        }
    }

    /// Strategy that produces a sequence of `(shape_id, has_shape)`
    /// pairs. `shape_id` is drawn from a small pool so repeats are
    /// likely, exercising the deduplication logic.
    fn arb_cursor_sequence() -> impl Strategy<Value = Vec<(u64, bool)>> {
        prop::collection::vec((1u64..=8, prop::bool::ANY), 1..=30)
    }

    proptest! {
        #[test]
        fn prop_cursor_frame_coordinate_translation(
            position_x in -1_000_000i32..1_000_000,
            position_y in -1_000_000i32..1_000_000,
            origin_x in -1_000_000i32..1_000_000,
            origin_y in -1_000_000i32..1_000_000,
        ) {
            let mut proc = CursorProcessor::new(origin_x, origin_y);

            let sample = CursorFrameSample {
                position_x,
                position_y,
                visible: true,
                shape_id: Some(1),
                shape: None,
            };

            let frames_before = proc.mouse_store().cursor_frames.len();
            proc.record_frame(42, &sample);

            prop_assert_eq!(
                proc.mouse_store().cursor_frames.len(),
                frames_before + 1,
            );

            let record = proc.mouse_store().cursor_frames.last().unwrap();

            prop_assert_eq!(record.x, position_x - origin_x);
            prop_assert_eq!(record.y, position_y - origin_y);

            let last = proc.last_frame().unwrap();
            prop_assert_eq!(last.timestamp_ms, record.timestamp_ms);
            prop_assert_eq!(last.x, record.x);
            prop_assert_eq!(last.y, record.y);
            prop_assert_eq!(last.visible, record.visible);
            prop_assert_eq!(last.shape_id, record.shape_id);
        }
    }

    proptest! {
        #[test]
        fn prop_cursor_shape_deduplication(
            seq in arb_cursor_sequence(),
        ) {
            let mut proc = CursorProcessor::new(0, 0);
            let mut unique_ids: HashSet<u64> = HashSet::new();

            for (i, (shape_id, first_occurrence)) in seq.iter().enumerate() {
                let sample = if *first_occurrence && !unique_ids.contains(shape_id) {
                    sample_with_shape(*shape_id)
                } else {
                    sample_without_shape(*shape_id)
                };

                if sample.shape.is_some() {
                    unique_ids.insert(*shape_id);
                }

                proc.record_frame(i as u64, &sample);

                prop_assert_eq!(
                    proc.mouse_store().cursor_shapes.len(),
                    unique_ids.len(),
                    "shapes stored ({}) != unique ids seen ({}) after sample {}",
                    proc.mouse_store().cursor_shapes.len(),
                    unique_ids.len(),
                    i,
                );
            }

            let stored_ids: HashSet<u64> = proc
                .mouse_store()
                .cursor_shapes
                .iter()
                .map(|s| s.shape_id)
                .collect();

            for frame in &proc.mouse_store().cursor_frames {
                if let Some(fid) = frame.shape_id {
                    if unique_ids.contains(&fid) {
                        prop_assert!(
                            stored_ids.contains(&fid),
                            "frame references shape_id {} which is missing from cursor_shapes",
                            fid,
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn record_frame_appends_exactly_one_frame() {
        let mut proc = CursorProcessor::new(0, 0);
        let sample = sample_without_shape(1);
        proc.record_frame(100, &sample);
        assert_eq!(proc.mouse_store().cursor_frames.len(), 1);
        assert_eq!(proc.mouse_store().cursor_frames[0].timestamp_ms, 100);
    }

    #[test]
    fn new_shape_is_stored_on_first_occurrence() {
        let mut proc = CursorProcessor::new(0, 0);
        let sample = sample_with_shape(42);
        proc.record_frame(0, &sample);
        assert_eq!(proc.mouse_store().cursor_shapes.len(), 1);
        assert_eq!(proc.mouse_store().cursor_shapes[0].shape_id, 42);
    }

    #[test]
    fn duplicate_shape_id_is_not_stored_twice() {
        let mut proc = CursorProcessor::new(0, 0);
        proc.record_frame(0, &sample_with_shape(1));
        proc.record_frame(1, &sample_with_shape(1));
        assert_eq!(proc.mouse_store().cursor_shapes.len(), 1);
        assert_eq!(proc.mouse_store().cursor_frames.len(), 2);
    }

    #[test]
    fn synthesize_frame_for_drop_clones_last_frame() {
        let mut proc = CursorProcessor::new(10, 20);
        let sample = CursorFrameSample {
            position_x: 50,
            position_y: 60,
            visible: true,
            shape_id: Some(1),
            shape: None,
        };
        proc.record_frame(100, &sample);
        proc.synthesize_frame_for_drop(200);

        assert_eq!(proc.mouse_store().cursor_frames.len(), 2);
        let synth = &proc.mouse_store().cursor_frames[1];
        assert_eq!(synth.timestamp_ms, 200);
        assert_eq!(synth.x, 50 - 10);
        assert_eq!(synth.y, 60 - 20);
    }

    #[test]
    fn synthesize_frame_for_drop_noop_when_no_last_frame() {
        let mut proc = CursorProcessor::new(0, 0);
        proc.synthesize_frame_for_drop(100);
        assert_eq!(proc.mouse_store().cursor_frames.len(), 0);
    }

    #[test]
    fn into_mouse_store_returns_accumulated_data() {
        let mut proc = CursorProcessor::new(0, 0);
        proc.record_frame(0, &sample_with_shape(1));
        proc.record_frame(1, &sample_with_shape(2));
        proc.record_frame(2, &sample_without_shape(1));

        let store = proc.into_mouse_store();
        assert_eq!(store.cursor_shapes.len(), 2);
        assert_eq!(store.cursor_frames.len(), 3);
    }
}
