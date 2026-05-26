use crate::{
    App, MutableState, NewStudioRecordingAdded, RecordingState, audio::AppSounds,
    recording_settings::RecordingSettingsStore,
};
use cap_project::{
    Cursors, MultipleSegment, MultipleSegments, Platform, ProjectConfiguration, RecordingMeta,
    RecordingMetaInner, StudioRecordingMeta, StudioRecordingStatus, TimelineConfiguration,
    TimelineSegment, VideoMeta,
};
use cap_recording::{
    feeds::microphone,
    instant_recording,
    sources::screen_capture::{CaptureWindow, ScreenCaptureTarget},
};
use cap_utils::ensure_dir;
use kameo::actor::ActorRef;
use relative_path::RelativePathBuf;
use serde::{Deserialize, Serialize};
use specta::Type;
use std::{
    collections::HashSet,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use tauri::{AppHandle, Manager, Wry};
use tauri_plugin_store::StoreExt;
use tauri_specta::Event;
use tracing::{info, warn};

const SETTINGS_KEY: &str = "replay_buffer_settings";
const POLL_INTERVAL: Duration = Duration::from_secs(2);
const CLIP_SEGMENT_WAIT: Duration = Duration::from_millis(2200);
const RETENTION_SAFETY_SECS: f64 = 60.0;
const SEGMENT_MARGIN_SECS: f64 = 4.0;

#[derive(Debug, Clone, Serialize, Deserialize, Type, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ReplayWindowRule {
    pub owner_name: String,
    pub window_title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executable_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase", default)]
pub struct ReplayBufferSettings {
    pub enabled: bool,
    pub clip_duration_secs: u32,
    pub targets: Vec<ReplayWindowRule>,
}

impl Default for ReplayBufferSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            clip_duration_secs: 30,
            targets: Vec::new(),
        }
    }
}

impl ReplayBufferSettings {
    pub fn get(app: &AppHandle<Wry>) -> Result<Self, String> {
        match app.store("store").map(|s| s.get(SETTINGS_KEY)) {
            Ok(Some(store)) => serde_json::from_value(store)
                .map_err(|e| format!("Failed to deserialize replay buffer settings: {e}")),
            _ => Ok(Self::default()),
        }
    }
}

#[derive(Default)]
pub struct ReplayBufferState {
    session: tokio::sync::Mutex<Option<ReplaySession>>,
}

struct ReplaySession {
    key: ReplayTargetKey,
    handle: instant_recording::ActorHandle,
    dir: PathBuf,
    tracker: Arc<Mutex<SegmentTracker>>,
    bridge: Option<std::thread::JoinHandle<()>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReplayTargetKey {
    executable_path: Option<String>,
    owner_name: String,
    window_title: String,
}

#[derive(Debug, Clone)]
struct SegmentRecord {
    path: PathBuf,
    index: u32,
    start: f64,
    end: f64,
}

#[derive(Debug, Clone)]
struct SegmentSnapshot {
    video_init: Option<PathBuf>,
    audio_init: Option<PathBuf>,
    video_segments: Vec<SegmentRecord>,
    audio_segments: Vec<SegmentRecord>,
}

#[derive(Default, Debug)]
struct SegmentTracker {
    video_init: Option<PathBuf>,
    audio_init: Option<PathBuf>,
    video_segments: Vec<SegmentRecord>,
    audio_segments: Vec<SegmentRecord>,
    protected_paths: HashSet<PathBuf>,
}

impl SegmentTracker {
    fn record(&mut self, event: cap_enc_ffmpeg::segmented_stream::SegmentCompletedEvent) {
        use cap_enc_ffmpeg::segmented_stream::SegmentMediaType;

        if event.is_init {
            match event.media_type {
                SegmentMediaType::Video => self.video_init = Some(event.path),
                SegmentMediaType::Audio => self.audio_init = Some(event.path),
            }
            return;
        }

        let segments = match event.media_type {
            SegmentMediaType::Video => &mut self.video_segments,
            SegmentMediaType::Audio => &mut self.audio_segments,
        };
        let start = segments.last().map(|s| s.end).unwrap_or(0.0);
        let end = start + event.duration.max(0.0);
        segments.push(SegmentRecord {
            path: event.path,
            index: event.index,
            start,
            end,
        });
    }

    fn snapshot(&self) -> SegmentSnapshot {
        SegmentSnapshot {
            video_init: self.video_init.clone(),
            audio_init: self.audio_init.clone(),
            video_segments: self.video_segments.clone(),
            audio_segments: self.audio_segments.clone(),
        }
    }

    fn protect(&mut self, paths: &[PathBuf]) {
        self.protected_paths.extend(paths.iter().cloned());
    }

    fn unprotect(&mut self, paths: &[PathBuf]) {
        for path in paths {
            self.protected_paths.remove(path);
        }
    }

    fn prune(&mut self, retain_secs: f64) {
        let latest = self.video_segments.last().map(|s| s.end).unwrap_or(0.0);
        let cutoff = (latest - retain_secs).max(0.0);
        prune_segments(&mut self.video_segments, cutoff, &self.protected_paths);
        prune_segments(&mut self.audio_segments, cutoff, &self.protected_paths);
    }
}

fn prune_segments(segments: &mut Vec<SegmentRecord>, cutoff: f64, protected: &HashSet<PathBuf>) {
    let split = segments
        .iter()
        .position(|segment| segment.end >= cutoff || protected.contains(&segment.path))
        .unwrap_or(segments.len());
    let removed = segments.drain(..split).collect::<Vec<_>>();
    for segment in removed {
        if let Err(err) = std::fs::remove_file(&segment.path) {
            warn!(path = %segment.path.display(), error = %err, "Failed to prune replay segment");
        }
    }
}

fn normalize_executable_path(path: &str) -> String {
    path.trim().replace('/', "\\").to_ascii_lowercase()
}

fn target_key(window: &CaptureWindow) -> ReplayTargetKey {
    ReplayTargetKey {
        executable_path: window
            .executable_path
            .as_deref()
            .map(normalize_executable_path),
        owner_name: window.owner_name.trim().to_ascii_lowercase(),
        window_title: window.name.trim().to_ascii_lowercase(),
    }
}

fn rule_matches(rule: &ReplayWindowRule, window: &CaptureWindow) -> bool {
    if let Some(rule_path) = rule
        .executable_path
        .as_deref()
        .map(normalize_executable_path)
        && let Some(window_path) = window
            .executable_path
            .as_deref()
            .map(normalize_executable_path)
    {
        return rule_path == window_path;
    }

    rule.owner_name
        .trim()
        .eq_ignore_ascii_case(&window.owner_name)
        && rule.window_title.trim().eq_ignore_ascii_case(&window.name)
}

pub fn start_manager(app: AppHandle, state: Arc<tokio::sync::RwLock<App>>) {
    tokio::spawn(async move {
        loop {
            if let Err(err) = tick_manager(&app, &state).await {
                warn!("Replay buffer manager tick failed: {err}");
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    });
}

async fn tick_manager(
    app: &AppHandle,
    state: &Arc<tokio::sync::RwLock<App>>,
) -> Result<(), String> {
    let settings = ReplayBufferSettings::get(app)?;
    let replay_state = app.state::<ReplayBufferState>();

    if !settings.enabled || settings.targets.is_empty() || normal_recording_active_arc(state).await
    {
        stop_session(&replay_state).await;
        return Ok(());
    }

    let Some(window) = find_target_window(&settings) else {
        stop_session(&replay_state).await;
        return Ok(());
    };

    let key = target_key(&window);
    let target = ScreenCaptureTarget::Window { id: window.id };

    let mut guard = replay_state.session.lock().await;
    if guard.as_ref().is_some_and(|session| session.key == key) {
        if let Some(session) = guard.as_ref() {
            let retain_secs =
                settings.clip_duration_secs as f64 + RETENTION_SAFETY_SECS + SEGMENT_MARGIN_SECS;
            if let Ok(mut tracker) = session.tracker.lock() {
                tracker.prune(retain_secs);
            }
        }
        return Ok(());
    }

    if let Some(session) = guard.take() {
        stop_replay_session(session).await;
    }

    match start_session(app, state, key, target).await {
        Ok(session) => {
            info!("Replay buffer session started");
            *guard = Some(session);
        }
        Err(err) => warn!("Failed to start replay buffer session: {err:#}"),
    }

    Ok(())
}

async fn normal_recording_active(state: &MutableState<'_, App>) -> bool {
    !matches!(state.read().await.recording_state, RecordingState::None)
}

async fn normal_recording_active_arc(state: &Arc<tokio::sync::RwLock<App>>) -> bool {
    !matches!(state.read().await.recording_state, RecordingState::None)
}

fn find_target_window(settings: &ReplayBufferSettings) -> Option<CaptureWindow> {
    cap_recording::sources::screen_capture::list_windows()
        .into_iter()
        .map(|(window, _)| window)
        .find(|window| {
            settings
                .targets
                .iter()
                .any(|rule| rule_matches(rule, window))
        })
}

async fn start_session(
    app: &AppHandle,
    state: &Arc<tokio::sync::RwLock<App>>,
    key: ReplayTargetKey,
    target: ScreenCaptureTarget,
) -> anyhow::Result<ReplaySession> {
    let base_dir = app.path().app_cache_dir()?.join("replay-buffer");
    let dir = ensure_dir(&base_dir)?.join(uuid::Uuid::new_v4().to_string());
    ensure_dir(&dir)?;

    let settings = RecordingSettingsStore::get(app)
        .ok()
        .flatten()
        .unwrap_or_default();
    let (mic_feed, selected_label, selected_settings) = {
        let app_state = state.read().await;
        (
            app_state.mic_feed.clone(),
            app_state.selected_mic_label.clone(),
            selected_label_settings(&app_state),
        )
    };
    let mic_feed = lock_selected_microphone(&mic_feed, selected_label, selected_settings).await?;

    let mut builder = instant_recording::Actor::builder(dir.clone(), target)
        .with_system_audio(settings.system_audio)
        .with_max_output_size(1920);

    if let Some(mic_feed) = mic_feed {
        builder = builder.with_mic_feed(mic_feed);
    }

    let handle = builder
        .build(
            #[cfg(target_os = "macos")]
            None,
        )
        .await?;

    let tracker = Arc::new(Mutex::new(SegmentTracker::default()));
    let segment_rx = handle.take_segment_rx();
    let bridge = segment_rx.map(|rx| {
        let tracker = tracker.clone();
        std::thread::Builder::new()
            .name("replay-buffer-segments".to_string())
            .spawn(move || {
                while let Ok(event) = rx.recv() {
                    if let Ok(mut tracker) = tracker.lock() {
                        tracker.record(event);
                    }
                }
            })
            .expect("failed to spawn replay buffer segment bridge")
    });

    Ok(ReplaySession {
        key,
        handle,
        dir,
        tracker,
        bridge,
    })
}

fn selected_label_settings(app_state: &App) -> Option<microphone::MicrophoneDeviceSettings> {
    app_state
        .selected_mic_label
        .as_deref()
        .and_then(|label| app_state.microphone_settings_for_label(label))
}

async fn lock_selected_microphone(
    mic_feed: &ActorRef<microphone::MicrophoneFeed>,
    selected_label: Option<String>,
    selected_settings: Option<microphone::MicrophoneDeviceSettings>,
) -> anyhow::Result<Option<Arc<microphone::MicrophoneFeedLock>>> {
    let Some(label) = selected_label else {
        return Ok(None);
    };

    if let Ok(lock) = mic_feed.ask(microphone::Lock).await
        && lock.device_name() == label
    {
        return Ok(Some(Arc::new(lock)));
    }

    let ready = mic_feed
        .ask(microphone::SetInput {
            label: label.clone(),
            settings: selected_settings,
        })
        .await
        .map_err(|err| {
            anyhow::anyhow!("Failed to initialize replay microphone '{label}': {err}")
        })?;
    ready.await?;
    let lock = mic_feed
        .ask(microphone::Lock)
        .await
        .map_err(|err| anyhow::anyhow!("Failed to lock replay microphone '{label}': {err}"))?;
    Ok(Some(Arc::new(lock)))
}

async fn stop_session(replay_state: &ReplayBufferState) {
    let mut guard = replay_state.session.lock().await;
    if let Some(session) = guard.take() {
        stop_replay_session(session).await;
    }
}

async fn stop_replay_session(mut session: ReplaySession) {
    if let Err(err) = session.handle.cancel().await {
        warn!("Failed to stop replay buffer session: {err:#}");
    }
    if let Some(bridge) = session.bridge.take()
        && bridge.join().is_err()
    {
        warn!("Replay segment bridge thread panicked");
    }
    if let Err(err) = tokio::fs::remove_dir_all(&session.dir).await {
        warn!(path = %session.dir.display(), error = %err, "Failed to remove replay buffer session dir");
    }
}

#[tauri::command(async)]
#[specta::specta]
pub async fn clip_replay_buffer(
    app: AppHandle,
    state: MutableState<'_, App>,
) -> Result<PathBuf, String> {
    if normal_recording_active(&state).await {
        return Err("Replay buffer is paused while another recording is active".to_string());
    }

    let settings = ReplayBufferSettings::get(&app)?;
    let replay_state = app.state::<ReplayBufferState>();
    let tracker = {
        let guard = replay_state.session.lock().await;
        guard
            .as_ref()
            .map(|session| session.tracker.clone())
            .ok_or_else(|| "Replay buffer is not active".to_string())?
    };

    let snapshot = snapshot_after_segment_wait(&tracker).await?;

    let selection = select_clip_segments(&snapshot, settings.clip_duration_secs as f64)
        .ok_or_else(|| "Not enough replay buffer data to save a clip".to_string())?;

    {
        let mut tracker = tracker
            .lock()
            .map_err(|_| "Replay buffer tracker lock poisoned".to_string())?;
        tracker.protect(&selection.protected_paths);
    }

    let protected_paths = selection.protected_paths.clone();
    let result = materialize_clip(&app, selection).await;

    {
        let mut tracker = tracker
            .lock()
            .map_err(|_| "Replay buffer tracker lock poisoned".to_string())?;
        if let Ok(path) = &result {
            info!(path = %path.display(), "Replay clip saved");
        }
        tracker.unprotect(&protected_paths);
    }

    let path = result?;
    AppSounds::Notification.play();
    NewStudioRecordingAdded { path: path.clone() }
        .emit(&app)
        .ok();
    Ok(path)
}

async fn snapshot_after_segment_wait(
    tracker: &Arc<Mutex<SegmentTracker>>,
) -> Result<SegmentSnapshot, String> {
    let initial = tracker
        .lock()
        .map_err(|_| "Replay buffer tracker lock poisoned".to_string())?
        .snapshot();
    let initial_latest = initial.video_segments.last().map(|s| s.end);

    let deadline = tokio::time::Instant::now() + CLIP_SEGMENT_WAIT;
    loop {
        let snapshot = tracker
            .lock()
            .map_err(|_| "Replay buffer tracker lock poisoned".to_string())?
            .snapshot();
        let latest = snapshot.video_segments.last().map(|s| s.end);
        if latest.is_some() && latest != initial_latest {
            return Ok(snapshot);
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(snapshot);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

struct ClipSelection {
    video_init: PathBuf,
    audio_init: Option<PathBuf>,
    video_segments: Vec<SegmentRecord>,
    audio_segments: Vec<SegmentRecord>,
    duration: f64,
    protected_paths: Vec<PathBuf>,
}

fn select_clip_segments(snapshot: &SegmentSnapshot, requested_secs: f64) -> Option<ClipSelection> {
    let latest = snapshot.video_segments.last()?.end;
    let start = (latest - requested_secs).max(0.0);
    let video_segments = snapshot
        .video_segments
        .iter()
        .filter(|segment| segment.end > start)
        .cloned()
        .collect::<Vec<_>>();
    if video_segments.is_empty() {
        return None;
    }

    let first_start = video_segments.first().map(|s| s.start).unwrap_or(0.0);
    let duration = latest - first_start;
    let indices = video_segments
        .iter()
        .map(|s| s.index)
        .collect::<HashSet<_>>();
    let audio_segments = snapshot
        .audio_segments
        .iter()
        .filter(|segment| indices.contains(&segment.index))
        .cloned()
        .collect::<Vec<_>>();

    let video_init = snapshot.video_init.clone()?;
    let audio_init = (!audio_segments.is_empty())
        .then(|| snapshot.audio_init.clone())
        .flatten();
    let mut protected_paths = Vec::with_capacity(
        1 + usize::from(audio_init.is_some()) + video_segments.len() + audio_segments.len(),
    );
    protected_paths.push(video_init.clone());
    if let Some(path) = &audio_init {
        protected_paths.push(path.clone());
    }
    protected_paths.extend(video_segments.iter().map(|s| s.path.clone()));
    protected_paths.extend(audio_segments.iter().map(|s| s.path.clone()));

    Some(ClipSelection {
        video_init,
        audio_init,
        video_segments,
        audio_segments,
        duration,
        protected_paths,
    })
}

async fn materialize_clip(app: &AppHandle, selection: ClipSelection) -> Result<PathBuf, String> {
    let recordings_base_dir = app.path().app_data_dir().unwrap().join("recordings");
    ensure_dir(&recordings_base_dir)
        .map_err(|e| format!("Failed to create recordings directory: {e}"))?;
    let filename = cap_utils::ensure_unique_filename("Replay Clip.cap", &recordings_base_dir)
        .map_err(|e| format!("Failed to choose replay clip filename: {e}"))?;
    let recording_dir = recordings_base_dir.join(filename);
    let content_dir = ensure_dir(&recording_dir.join("content"))
        .map_err(|e| format!("Failed to create replay clip content directory: {e}"))?;
    let segment_dir = ensure_dir(&content_dir.join("segments").join("segment-0"))
        .map_err(|e| format!("Failed to create replay clip segment directory: {e}"))?;
    let screenshots_dir = ensure_dir(&recording_dir.join("screenshots"))
        .map_err(|e| format!("Failed to create replay clip screenshots directory: {e}"))?;

    let output_path = content_dir.join("output.mp4");
    let display_path = segment_dir.join("display.mp4");
    let video_segments = selection
        .video_segments
        .iter()
        .map(|s| s.path.clone())
        .collect::<Vec<_>>();
    let video_temp = content_dir.join("video.mp4");

    tokio::task::spawn_blocking({
        let video_init = selection.video_init.clone();
        let video_segments = video_segments.clone();
        let video_temp = video_temp.clone();
        move || {
            cap_enc_ffmpeg::remux::concatenate_m4s_segments_with_init(
                &video_init,
                &video_segments,
                &video_temp,
            )
        }
    })
    .await
    .map_err(|e| format!("Replay clip video remux task failed: {e}"))?
    .map_err(|e| format!("Failed to remux replay video: {e}"))?;

    let audio_path = if let Some(audio_init) = selection.audio_init.clone() {
        let audio_segments = selection
            .audio_segments
            .iter()
            .map(|s| s.path.clone())
            .collect::<Vec<_>>();
        if !audio_segments.is_empty() {
            let audio_temp = content_dir.join("audio.mp4");
            tokio::task::spawn_blocking({
                let audio_temp = audio_temp.clone();
                move || {
                    cap_enc_ffmpeg::remux::concatenate_m4s_segments_with_init(
                        &audio_init,
                        &audio_segments,
                        &audio_temp,
                    )
                }
            })
            .await
            .map_err(|e| format!("Replay clip audio remux task failed: {e}"))?
            .map_err(|e| format!("Failed to remux replay audio: {e}"))?;
            Some(audio_temp)
        } else {
            None
        }
    } else {
        None
    };

    if let Some(audio_path) = audio_path {
        tokio::task::spawn_blocking({
            let video_temp = video_temp.clone();
            let display_path = display_path.clone();
            move || {
                cap_enc_ffmpeg::remux::merge_video_audio(&video_temp, &audio_path, &display_path)
            }
        })
        .await
        .map_err(|e| format!("Replay clip merge task failed: {e}"))?
        .map_err(|e| format!("Failed to merge replay clip audio: {e}"))?;
    } else {
        tokio::fs::rename(&video_temp, &display_path)
            .await
            .map_err(|e| format!("Failed to move replay video output: {e}"))?;
    }

    if let Err(err) = tokio::fs::copy(&display_path, &output_path).await {
        warn!("Failed to copy replay clip compatibility output: {err}");
    }
    tokio::fs::write(recording_dir.join(".force-ffmpeg-decoder"), b"")
        .await
        .map_err(|e| format!("Failed to write replay clip decoder marker: {e}"))?;

    let display_screenshot = screenshots_dir.join("display.jpg");
    if let Err(err) = crate::create_screenshot(display_path.clone(), display_screenshot, None).await
    {
        warn!("Failed to create replay clip thumbnail: {err}");
    }

    let relative_display = RelativePathBuf::from("content/segments/segment-0/display.mp4");
    let meta = RecordingMeta {
        platform: Some(Platform::default()),
        project_path: recording_dir.clone(),
        pretty_name: "Replay Clip".to_string(),
        sharing: None,
        inner: RecordingMetaInner::Studio(Box::new(StudioRecordingMeta::MultipleSegments {
            inner: MultipleSegments {
                segments: vec![MultipleSegment {
                    display: VideoMeta {
                        path: relative_display,
                        fps: 30,
                        start_time: Some(0.0),
                        device_id: None,
                    },
                    camera: None,
                    mic: None,
                    system_audio: None,
                    cursor: None,
                    keyboard: None,
                }],
                cursors: Cursors::default(),
                status: Some(StudioRecordingStatus::Complete),
            },
        })),
        upload: None,
    };
    meta.save_for_project()
        .map_err(|e| format!("Failed to save replay clip metadata: {e}"))?;

    let mut config = ProjectConfiguration::default();
    config.timeline = Some(TimelineConfiguration {
        segments: vec![TimelineSegment {
            recording_clip: 0,
            timescale: 1.0,
            start: 0.0,
            end: selection.duration,
        }],
        zoom_segments: Vec::new(),
        scene_segments: Vec::new(),
        mask_segments: Vec::new(),
        text_segments: Vec::new(),
        caption_segments: Vec::new(),
        keyboard_segments: Vec::new(),
    });
    config
        .write(&recording_dir)
        .map_err(|e| format!("Failed to save replay clip config: {e}"))?;

    Ok(recording_dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segment(index: u32, start: f64, end: f64) -> SegmentRecord {
        SegmentRecord {
            path: PathBuf::from(format!("segment_{index:03}.m4s")),
            index,
            start,
            end,
        }
    }

    #[test]
    fn target_matching_prefers_executable_path() {
        let rule = ReplayWindowRule {
            owner_name: "Wrong".to_string(),
            window_title: "Wrong".to_string(),
            executable_path: Some("C:/Games/Game/game.exe".to_string()),
        };
        let window = CaptureWindow {
            id: "1".parse().unwrap(),
            owner_name: "Game".to_string(),
            name: "Game Window".to_string(),
            bounds: scap_targets::bounds::LogicalBounds::new(
                scap_targets::bounds::LogicalPosition::new(0.0, 0.0),
                scap_targets::bounds::LogicalSize::new(100.0, 100.0),
            ),
            refresh_rate: 60,
            bundle_identifier: None,
            executable_path: Some("c:\\games\\game\\GAME.exe".to_string()),
        };

        assert!(rule_matches(&rule, &window));
    }

    #[test]
    fn target_matching_falls_back_to_owner_and_title() {
        let rule = ReplayWindowRule {
            owner_name: "Game".to_string(),
            window_title: "Game Window".to_string(),
            executable_path: None,
        };
        let window = CaptureWindow {
            id: "1".parse().unwrap(),
            owner_name: "game".to_string(),
            name: "game window".to_string(),
            bounds: scap_targets::bounds::LogicalBounds::new(
                scap_targets::bounds::LogicalPosition::new(0.0, 0.0),
                scap_targets::bounds::LogicalSize::new(100.0, 100.0),
            ),
            refresh_rate: 60,
            bundle_identifier: None,
            executable_path: None,
        };

        assert!(rule_matches(&rule, &window));
    }

    #[test]
    fn select_clip_rounds_outward_to_segment_boundaries() {
        let snapshot = SegmentSnapshot {
            video_init: Some(PathBuf::from("init.mp4")),
            audio_init: None,
            video_segments: vec![
                segment(1, 0.0, 2.0),
                segment(2, 2.0, 4.0),
                segment(3, 4.0, 6.0),
                segment(4, 6.0, 8.0),
            ],
            audio_segments: Vec::new(),
        };

        let selected = select_clip_segments(&snapshot, 3.0).unwrap();

        assert_eq!(selected.video_segments.first().unwrap().index, 3);
        assert!(selected.duration >= 3.0);
    }

    #[test]
    fn prune_keeps_protected_segments() {
        let mut tracker = SegmentTracker {
            video_segments: vec![segment(1, 0.0, 2.0), segment(2, 2.0, 4.0)],
            ..Default::default()
        };
        let protected = tracker.video_segments[0].path.clone();
        tracker.protect(&[protected]);
        tracker.prune(1.0);

        assert_eq!(tracker.video_segments.len(), 2);
    }

    #[test]
    fn prune_retains_requested_window_and_removes_older_segments() {
        let mut tracker = SegmentTracker {
            video_segments: vec![
                segment(1, 0.0, 2.0),
                segment(2, 2.0, 4.0),
                segment(3, 4.0, 6.0),
                segment(4, 6.0, 8.0),
                segment(5, 8.0, 10.0),
            ],
            ..Default::default()
        };

        tracker.prune(5.0);

        assert_eq!(tracker.video_segments.first().unwrap().index, 3);
        assert!(
            tracker.video_segments.last().unwrap().end - tracker.video_segments[0].start >= 5.0
        );
    }
}
