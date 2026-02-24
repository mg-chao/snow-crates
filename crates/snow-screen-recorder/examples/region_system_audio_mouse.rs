use std::thread;
use std::time::Duration;

use snow_capture::CaptureRegion;
use snow_screen_recorder::{
    EditConfig, EditingSession, ExportConfig, ExportFormat, MouseEditConfig, RecordingAudioConfig,
    RecordingConfig, RecordingSession, RecordingTarget, VideoEncodeConfig, VideoEncodingSpeed,
};

const REGION_X: i32 = 0;
const REGION_Y: i32 = 0;
const REGION_WIDTH: u32 = 2000;
const REGION_HEIGHT: u32 = 2000;
const TARGET_FPS: u32 = 60;
const RECORD_SECONDS: u64 = 5;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let output_dir = std::env::current_dir()?.join("recordings");
    let export_path = output_dir.join("region_0_0_2000_2000_fps24.mp4");

    let region = CaptureRegion::new(REGION_X, REGION_Y, REGION_WIDTH, REGION_HEIGHT)?;
    let recording_config = RecordingConfig {
        target: RecordingTarget::Region(region),
        output_dir: output_dir.clone(),
        fps: TARGET_FPS,
        video: VideoEncodeConfig {
            quality: 100,
            speed: VideoEncodingSpeed::UltraFast,
        },
        audio: RecordingAudioConfig {
            microphone_enabled: false,
            system_audio_enabled: true,
            ..RecordingAudioConfig::default()
        },
        keep_temp_files: true,
        ..RecordingConfig::default()
    };

    let mut recording = RecordingSession::create(recording_config)?;
    recording.start()?;

    println!(
        "Recording region ({}, {})-({}, {}) at {} FPS for {} seconds...",
        REGION_X,
        REGION_Y,
        REGION_X + REGION_WIDTH as i32,
        REGION_Y + REGION_HEIGHT as i32,
        TARGET_FPS,
        RECORD_SECONDS
    );
    thread::sleep(Duration::from_secs(RECORD_SECONDS));

    let stop_duration_start = std::time::Instant::now();
    let artifact = recording.stop()?;
    println!("Recording stopped in {} ms", stop_duration_start.elapsed().as_millis());

    let mut editing = EditingSession::open(artifact)?;
    let mut edit_config = EditConfig::default();
    edit_config.microphone_audio.enabled = false;
    edit_config.system_audio.enabled = true;
    edit_config.mouse = MouseEditConfig {
        visible: true,
        trail_enabled: true,
        click_enabled: true,
    };
    edit_config.export = ExportConfig {
        format: ExportFormat::Mp4,
        output_path: export_path.clone(),
        video: VideoEncodeConfig {
            quality: 100,
            speed: VideoEncodingSpeed::UltraFast,
        },
    };
    editing.set_config(edit_config)?;

    let start_ts = std::time::Instant::now();
    let result = editing.export()?;
    println!(
        "Export completed in {} seconds.",
        start_ts.elapsed().as_secs_f64()
    );

    println!(
        "Export finished: {} (duration: {} ms)",
        result.output_path.display(),
        result.duration_ms
    );

    Ok(())
}
