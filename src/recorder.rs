use std::{
    fs,
    future::Future,
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicU8, AtomicU64, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow};
use ashpd::{Error as AshpdError, desktop::ResponseError};
use gst::prelude::*;
use gstreamer as gst;
use tracing::{debug, error, info, warn};

use crate::{
    gst_utils::gst_quote,
    portal::{PortalStream, portal_stream},
};

static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RecordingSessionId(u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecorderState {
    Idle,
    Starting,
    Recording,
    Paused,
    Stopping,
    Error,
}

impl RecorderState {
    pub fn can_start(self) -> bool {
        matches!(self, Self::Idle | Self::Error)
    }

    pub fn can_stop(self) -> bool {
        matches!(self, Self::Starting | Self::Recording | Self::Paused)
    }

    fn can_accept(self, command: RecorderCommandKind) -> bool {
        match command {
            RecorderCommandKind::Stop => self.can_stop(),
            RecorderCommandKind::Pause => matches!(self, Self::Recording),
            RecorderCommandKind::Resume => matches!(self, Self::Paused),
        }
    }

    fn can_transition_to(self, next: Self) -> bool {
        self == next
            || matches!(
                (self, next),
                (Self::Idle, Self::Starting)
                    | (Self::Error, Self::Starting)
                    | (Self::Starting, Self::Recording)
                    | (Self::Starting, Self::Stopping)
                    | (Self::Starting, Self::Error)
                    | (Self::Recording, Self::Paused)
                    | (Self::Recording, Self::Stopping)
                    | (Self::Recording, Self::Error)
                    | (Self::Paused, Self::Recording)
                    | (Self::Paused, Self::Stopping)
                    | (Self::Paused, Self::Error)
                    | (Self::Stopping, Self::Idle)
                    | (Self::Stopping, Self::Error)
                    | (Self::Error, Self::Idle)
            )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RecorderStateMachine {
    state: RecorderState,
}

impl RecorderStateMachine {
    fn new(state: RecorderState) -> Self {
        Self { state }
    }

    fn state(self) -> RecorderState {
        self.state
    }

    fn transition(&mut self, next: RecorderState) -> bool {
        if !self.state.can_transition_to(next) {
            return false;
        }
        self.state = next;
        true
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum StartupConfirmationState {
    Pending = 0,
    Cancelled = 1,
    Committed = 2,
    Failed = 3,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StartupStopDisposition {
    CancelStartup,
    FinalizeRecording,
    Resolved,
}

#[derive(Debug)]
enum StartupDecision {
    Started,
    Cancelled,
    Error(anyhow::Error),
}

/// Арбитр результата запуска; это не вторая доменная state machine.
///
/// `RecorderController` по-прежнему единолично владеет `RecorderState`. Этот
/// объект только задаёт commit point одной операции worker: принятый до commit
/// `Stop` отменяет запуск, а после commit является обычной остановкой записи.
#[derive(Debug)]
struct StartupConfirmationArbiter {
    state: AtomicU8,
    cancel_wakeup: Condvar,
    cancel_lock: Mutex<()>,
    cancel_notify: tokio::sync::Notify,
}

impl StartupConfirmationArbiter {
    fn new() -> Self {
        Self {
            state: AtomicU8::new(StartupConfirmationState::Pending as u8),
            cancel_wakeup: Condvar::new(),
            cancel_lock: Mutex::new(()),
            cancel_notify: tokio::sync::Notify::new(),
        }
    }

    fn notify_cancellation_waiters(&self) {
        self.cancel_wakeup.notify_all();
        self.cancel_notify.notify_waiters();
    }

    fn request_stop(&self) -> StartupStopDisposition {
        let lock = match self.cancel_lock.lock() {
            Ok(lock) => lock,
            Err(poisoned) => poisoned.into_inner(),
        };
        let disposition = match self.state.compare_exchange(
            StartupConfirmationState::Pending as u8,
            StartupConfirmationState::Cancelled as u8,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => StartupStopDisposition::CancelStartup,
            Err(value) if value == StartupConfirmationState::Cancelled as u8 => {
                StartupStopDisposition::CancelStartup
            }
            Err(value) if value == StartupConfirmationState::Committed as u8 => {
                StartupStopDisposition::FinalizeRecording
            }
            Err(value) => {
                debug_assert_eq!(
                    value,
                    StartupConfirmationState::Failed as u8,
                    "неизвестное состояние арбитра запуска"
                );
                StartupStopDisposition::Resolved
            }
        };
        drop(lock);
        if matches!(disposition, StartupStopDisposition::CancelStartup) {
            self.notify_cancellation_waiters();
        }
        disposition
    }

    fn is_cancelled(&self) -> bool {
        self.state.load(Ordering::Acquire) == StartupConfirmationState::Cancelled as u8
    }

    fn commit_started(&self) -> StartupDecision {
        match self.state.compare_exchange(
            StartupConfirmationState::Pending as u8,
            StartupConfirmationState::Committed as u8,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => StartupDecision::Started,
            Err(value) if value == StartupConfirmationState::Cancelled as u8 => {
                StartupDecision::Cancelled
            }
            Err(value) if value == StartupConfirmationState::Committed as u8 => {
                StartupDecision::Started
            }
            Err(value) => StartupDecision::Error(anyhow!(
                "не удалось зафиксировать запуск recorder: состояние арбитра {value}"
            )),
        }
    }

    fn resolve_error(&self, error: anyhow::Error) -> StartupDecision {
        loop {
            let state = self.state.load(Ordering::Acquire);
            if state == StartupConfirmationState::Cancelled as u8 {
                return StartupDecision::Cancelled;
            }
            if state == StartupConfirmationState::Committed as u8
                || state == StartupConfirmationState::Failed as u8
            {
                return StartupDecision::Error(error);
            }
            debug_assert!(
                state == StartupConfirmationState::Pending as u8,
                "неизвестное состояние арбитра запуска: {state}"
            );
            if self
                .state
                .compare_exchange(
                    state,
                    StartupConfirmationState::Failed as u8,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
            {
                return StartupDecision::Error(error);
            }
        }
    }

    fn wait_for_cancellation(&self, timeout: Duration) {
        let lock = match self.cancel_lock.lock() {
            Ok(lock) => lock,
            Err(poisoned) => poisoned.into_inner(),
        };
        if self.is_cancelled() {
            return;
        }
        match self.cancel_wakeup.wait_timeout(lock, timeout) {
            Ok((_lock, _wait_result)) => {}
            Err(poisoned) => {
                let (lock, _wait_result) = poisoned.into_inner();
                drop(lock);
                warn!(
                    "Не удалось дождаться отмены startup через Condvar: mutex отравлен; продолжаю проверку состояния pipeline"
                );
            }
        }
    }

    async fn wait_until_cancelled(&self) {
        loop {
            let notified = self.cancel_notify.notified();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }
}

fn decide_startup_confirmation<F>(
    arbiter: &StartupConfirmationArbiter,
    confirmation: Result<()>,
    mut take_pipeline_error: F,
) -> StartupDecision
where
    F: FnMut() -> Option<anyhow::Error>,
{
    if let Some(error) = take_pipeline_error() {
        return arbiter.resolve_error(error);
    }

    match confirmation {
        Ok(()) => arbiter.commit_started(),
        Err(error) => arbiter.resolve_error(error),
    }
}

#[derive(Debug)]
pub enum RecorderCommand {
    BeginRecording {
        session_id: RecordingSessionId,
    },
    Stop {
        session_id: RecordingSessionId,
    },
    Pause {
        session_id: RecordingSessionId,
    },
    Resume {
        session_id: RecordingSessionId,
    },
    SetMicrophoneMuted {
        session_id: RecordingSessionId,
        muted: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecorderEvent {
    /// Controller-owned lifecycle notification for immediate UI rendering.
    StateChanged {
        session_id: RecordingSessionId,
        state: RecorderState,
    },
    /// Portal capture and pipeline are prepared; GTK may now run the countdown.
    Prepared {
        session_id: RecordingSessionId,
    },
    /// Worker confirmation that the pipeline reached Playing for the first time.
    Started {
        session_id: RecordingSessionId,
    },
    /// Worker confirmation that the pipeline reached Paused.
    Paused {
        session_id: RecordingSessionId,
    },
    /// Worker confirmation that the pipeline returned to Playing.
    Resumed {
        session_id: RecordingSessionId,
    },
    Stopped {
        session_id: RecordingSessionId,
        path: PathBuf,
    },
    Cancelled {
        session_id: RecordingSessionId,
    },
    Error {
        session_id: RecordingSessionId,
        message: String,
    },
}

impl RecorderEvent {
    fn session_id(&self) -> RecordingSessionId {
        match self {
            Self::StateChanged { session_id, .. }
            | Self::Prepared { session_id }
            | Self::Started { session_id }
            | Self::Paused { session_id }
            | Self::Resumed { session_id }
            | Self::Stopped { session_id, .. }
            | Self::Cancelled { session_id }
            | Self::Error { session_id, .. } => *session_id,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RecorderOptions {
    pub microphone: Option<String>,
    pub system_audio: Option<String>,
    pub microphone_muted: bool,
    pub output: PathBuf,
}

pub struct RecorderHandle {
    session_id: RecordingSessionId,
    command_tx: Sender<RecorderCommand>,
    thread: Option<thread::JoinHandle<()>>,
    startup_confirmation: Arc<StartupConfirmationArbiter>,
}

impl RecorderHandle {
    fn send(&self, command: RecorderCommand) -> Result<()> {
        self.command_tx
            .send(command)
            .context("не удалось отправить команду recorder")
    }

    pub fn begin_recording(&self) -> Result<()> {
        self.send(RecorderCommand::BeginRecording {
            session_id: self.session_id,
        })
    }

    pub fn stop(&self) -> Result<()> {
        self.startup_confirmation.request_stop();
        self.send(RecorderCommand::Stop {
            session_id: self.session_id,
        })
    }

    pub fn pause(&self) -> Result<()> {
        self.send(RecorderCommand::Pause {
            session_id: self.session_id,
        })
    }

    pub fn resume(&self) -> Result<()> {
        self.send(RecorderCommand::Resume {
            session_id: self.session_id,
        })
    }

    pub fn set_microphone_muted(&self, muted: bool) -> Result<()> {
        self.send(RecorderCommand::SetMicrophoneMuted {
            session_id: self.session_id,
            muted,
        })
    }

    pub fn session_id(&self) -> RecordingSessionId {
        self.session_id
    }
}

impl Drop for RecorderHandle {
    fn drop(&mut self) {
        if let Some(thread) = self.thread.take()
            && let Err(error) = thread::Builder::new()
                .name("livescreen-recorder-join".into())
                .spawn(move || {
                    if let Err(join_error) = thread.join() {
                        error!(error = ?join_error, "Поток recorder завершился с panic");
                    }
                })
        {
            error!(error = %error, "Не удалось запустить поток ожидания recorder");
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecorderCommandKind {
    Stop,
    Pause,
    Resume,
}

impl RecorderCommand {
    fn session_id(&self) -> RecordingSessionId {
        match self {
            Self::BeginRecording { session_id }
            | Self::Stop { session_id }
            | Self::Pause { session_id }
            | Self::Resume { session_id }
            | Self::SetMicrophoneMuted { session_id, .. } => *session_id,
        }
    }
}

pub struct RecorderController {
    event_tx: Sender<RecorderEvent>,
    handle: Option<RecorderHandle>,
    active_session: Option<RecordingSessionId>,
    lifecycle: RecorderStateMachine,
    pending_command: Option<RecorderCommandKind>,
}

impl RecorderController {
    pub fn new(event_tx: Sender<RecorderEvent>) -> Self {
        Self {
            event_tx,
            handle: None,
            active_session: None,
            lifecycle: RecorderStateMachine::new(RecorderState::Idle),
            pending_command: None,
        }
    }

    pub fn state(&self) -> RecorderState {
        self.lifecycle.state()
    }

    pub fn has_active_session(&self) -> bool {
        self.active_session.is_some()
    }

    pub fn is_pause_resume_pending(&self) -> bool {
        matches!(
            self.pending_command,
            Some(RecorderCommandKind::Pause | RecorderCommandKind::Resume)
        )
    }

    pub fn is_cancelling_startup(&self) -> bool {
        self.state() == RecorderState::Stopping
            && self
                .handle
                .as_ref()
                .is_some_and(|handle| handle.startup_confirmation.is_cancelled())
    }

    pub fn start(&mut self, options: RecorderOptions) -> Result<RecordingSessionId> {
        if self.active_session.is_some() || !self.state().can_start() {
            debug!(state = ?self.state(), "Запуск recorder отклонён controller");
            return Err(anyhow!(
                "нельзя начать запись из состояния {:?}",
                self.state()
            ));
        }
        info!(output = %options.output.display(), "Controller принимает запрос на новую сессию recorder");
        let handle = start_recorder(options, self.event_tx.clone())?;
        let session_id = handle.session_id();
        let transitioned = self.lifecycle.transition(RecorderState::Starting);
        debug_assert!(
            transitioned,
            "new recorder session must start from Idle/Error"
        );
        self.active_session = Some(session_id);
        self.handle = Some(handle);
        self.pending_command = None;
        info!(?session_id, "Controller создал сессию recorder");
        self.notify_state();
        Ok(session_id)
    }

    pub fn request_stop(&mut self) -> Result<()> {
        self.dispatch(RecorderCommandKind::Stop)
    }

    pub fn request_begin_recording(&self) -> Result<()> {
        if self.state() != RecorderState::Starting {
            return Ok(());
        }
        let Some(handle) = self.handle.as_ref() else {
            return Ok(());
        };
        handle.begin_recording()
    }

    pub fn request_pause_or_resume(&mut self) -> Result<()> {
        let command = match self.state() {
            RecorderState::Recording => RecorderCommandKind::Pause,
            RecorderState::Paused => RecorderCommandKind::Resume,
            _ => return Ok(()),
        };
        self.dispatch(command)
    }

    pub fn set_microphone_muted(&self, muted: bool) -> Result<()> {
        if !matches!(
            self.state(),
            RecorderState::Starting | RecorderState::Recording | RecorderState::Paused
        ) {
            return Ok(());
        }
        let Some(handle) = self.handle.as_ref() else {
            return Ok(());
        };
        handle.set_microphone_muted(muted)
    }

    fn dispatch(&mut self, command: RecorderCommandKind) -> Result<()> {
        if !self.state().can_accept(command) {
            debug!(?command, state = ?self.state(), "Команда recorder отклонена текущим состоянием");
            return Ok(());
        }
        if !matches!(command, RecorderCommandKind::Stop) && self.pending_command.is_some() {
            debug!(?command, pending = ?self.pending_command, "Команда recorder отклонена: ожидается подтверждение предыдущей команды");
            return Ok(());
        }
        let command_result = {
            let Some(handle) = self.handle.as_ref() else {
                return Ok(());
            };
            match command {
                RecorderCommandKind::Stop => handle.stop(),
                RecorderCommandKind::Pause => handle.pause(),
                RecorderCommandKind::Resume => handle.resume(),
            }
        };
        command_result?;

        if matches!(command, RecorderCommandKind::Stop) {
            self.pending_command = None;
            if !self.lifecycle.transition(RecorderState::Stopping) {
                return Err(anyhow!(
                    "недопустимый переход recorder: {:?} → Stopping",
                    self.state()
                ));
            }
            info!(?command, state = ?self.state(), "Команда recorder принята controller");
            self.notify_state();
        } else {
            self.pending_command = Some(command);
            info!(?command, state = ?self.state(), "Команда recorder принята, ожидается подтверждение worker");
        }
        Ok(())
    }

    pub fn apply_event(&mut self, event: RecorderEvent) -> Option<RecorderEvent> {
        let session_id = event.session_id();
        if self.active_session != Some(session_id) {
            debug!(?session_id, event = ?event, "Событие recorder относится к другой сессии");
            return None;
        }

        match &event {
            // StateChanged is emitted by this controller after an accepted command.
            // It is a notification for GTK, not an input that can advance lifecycle.
            RecorderEvent::StateChanged { state, .. } => {
                if self.state() != *state {
                    debug!(?session_id, event_state = ?state, current_state = ?self.state(), "StateChanged не совпадает с состоянием controller");
                    return None;
                }
            }
            RecorderEvent::Prepared { .. } => {
                if self.state() != RecorderState::Starting {
                    debug!(?session_id, state = ?self.state(), "Prepared не соответствует текущему состоянию controller");
                    return None;
                }
            }
            RecorderEvent::Started { .. } => {
                if !self.lifecycle.transition(RecorderState::Recording) {
                    warn!(?session_id, state = ?self.state(), "Started не может перевести controller в Recording");
                    return None;
                }
            }
            RecorderEvent::Paused { .. } => {
                if self.state() != RecorderState::Recording
                    || self.pending_command != Some(RecorderCommandKind::Pause)
                {
                    warn!(?session_id, state = ?self.state(), pending = ?self.pending_command, "Подтверждение Paused не соответствует ожидаемой команде");
                    return None;
                }
                self.pending_command = None;
                self.lifecycle.transition(RecorderState::Paused);
            }
            RecorderEvent::Resumed { .. } => {
                if self.state() != RecorderState::Paused
                    || self.pending_command != Some(RecorderCommandKind::Resume)
                {
                    warn!(?session_id, state = ?self.state(), pending = ?self.pending_command, "Подтверждение Resumed не соответствует ожидаемой команде");
                    return None;
                }
                self.pending_command = None;
                self.lifecycle.transition(RecorderState::Recording);
            }
            RecorderEvent::Stopped { .. } | RecorderEvent::Cancelled { .. } => {
                if self.state() != RecorderState::Stopping
                    && !self.lifecycle.transition(RecorderState::Stopping)
                {
                    return None;
                }
                info!(?session_id, event = ?event, "Controller применяет terminal event recorder");
                if !self.lifecycle.transition(RecorderState::Idle) {
                    return None;
                }
                self.active_session = None;
                self.handle.take();
                self.pending_command = None;
            }
            RecorderEvent::Error { .. } => {
                if !self.lifecycle.transition(RecorderState::Error)
                    || !self.lifecycle.transition(RecorderState::Idle)
                {
                    return None;
                }
                error!(?session_id, event = ?event, "Controller применяет error event recorder");
                self.active_session = None;
                self.handle.take();
                self.pending_command = None;
            }
        }
        Some(event)
    }

    fn notify_state(&self) {
        let Some(session_id) = self.active_session else {
            return;
        };
        send_event(
            &self.event_tx,
            RecorderEvent::StateChanged {
                session_id,
                state: self.state(),
            },
        );
        info!(?session_id, state = ?self.state(), "Изменено состояние recorder controller");
    }
}

pub fn build_pipeline(
    options: &RecorderOptions,
    portal_stream: Option<PortalStream>,
) -> Result<(gst::Pipeline, Option<PortalStream>)> {
    gst::init()?;
    // Source-provided PipeWire/Pulse clocks can stall while capture is corked and make
    // GstBaseSrc wait for seconds after Resume. All capture branches use the system clock.
    let (screen_source, screen_w, screen_h) = if let Some(stream) = portal_stream.as_ref() {
        let source = format!(
            "pipewiresrc name=screen_source fd={} path={} do-timestamp=true keepalive-time=33 provide-clock=false",
            stream.fd.as_raw_fd(),
            stream.node_id
        );
        (source, stream.width, stream.height)
    } else {
        (
            "ximagesrc name=screen_source use-damage=false show-pointer=true".to_string(),
            1920,
            1080,
        )
    };
    let target_w = screen_w.min(1920);
    let target_h =
        ((screen_h as f64 * target_w as f64 / screen_w as f64).round() as i32).max(2) & !1;
    let video = format!(
        "{screen_source} ! videoconvert ! videoscale ! videorate ! video/x-raw,width={target_w},height={target_h},framerate=30/1 ! queue ! videoconvert ! video/x-raw,format=I420 ! x264enc tune=zerolatency speed-preset=veryfast bitrate=5000 key-int-max=30 ! h264parse config-interval=1 ! queue ! mux.video_0 "
    );
    let audio = match (&options.microphone, &options.system_audio) {
        (Some(mic), Some(system)) => format!(
            "pulsesrc name=microphone_source device={} mute={} do-timestamp=true provide-clock=false ! audioconvert ! audioresample ! queue ! amix. pulsesrc device={} do-timestamp=true provide-clock=false ! audioconvert ! audioresample ! queue ! amix. audiomixer name=amix latency=100000000 ! audioconvert ! audioresample ! audio/x-raw,rate=48000,channels=2 ! voaacenc bitrate=160000 ! queue ! mux.audio_0 ",
            gst_quote(mic),
            options.microphone_muted,
            gst_quote(system)
        ),
        (Some(mic), None) => format!(
            "pulsesrc name=microphone_source device={} mute={} do-timestamp=true provide-clock=false ! audioconvert ! audioresample ! audio/x-raw,rate=48000,channels=2 ! voaacenc bitrate=160000 ! queue ! mux.audio_0 ",
            gst_quote(mic),
            options.microphone_muted
        ),
        (None, Some(system)) => format!(
            "pulsesrc device={} do-timestamp=true provide-clock=false ! audioconvert ! audioresample ! audio/x-raw,rate=48000,channels=2 ! voaacenc bitrate=160000 ! queue ! mux.audio_0 ",
            gst_quote(system)
        ),
        (None, None) => String::new(),
    };
    // A file sink does not need preroll. Keeping its asynchronous PAUSED transition enabled
    // leaves a live pipeline stuck at Paused/pending Playing after Resume.
    let description = format!(
        "mp4mux name=mux faststart=true ! filesink async=false location={} {video} {audio}",
        gst_quote(&options.output.to_string_lossy())
    );
    debug!(
        screen_source = %screen_source,
        screen_width = screen_w,
        screen_height = screen_h,
        target_width = target_w,
        target_height = target_h,
        "Собрано описание recording pipeline"
    );
    let element =
        gst::parse::launch(&description).context("не удалось собрать GStreamer pipeline")?;
    let pipeline = element
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow!("GStreamer вернул не pipeline"))?;
    if portal_stream.is_some() {
        configure_pipewire_screen_source(&pipeline)?;
    }
    Ok((pipeline, portal_stream))
}

fn configure_pipewire_screen_source(pipeline: &gst::Pipeline) -> Result<()> {
    let source = pipeline
        .by_name("screen_source")
        .context("Wayland pipeline не содержит pipewiresrc screen_source")?;
    // WirePlumber 0.5 отклоняет Stream/Input/Unknown до согласования caps.
    // Явные свойства позволяют policy связать capture stream с portal video node.
    let properties = gst::Structure::builder("props")
        .field("media.type", "Video")
        .field("media.category", "Capture")
        .field("media.role", "Screen")
        .build();
    source.set_property("stream-properties", properties);
    Ok(())
}

fn set_microphone_muted(pipeline: &gst::Pipeline, muted: bool) -> Result<()> {
    let Some(source) = pipeline.by_name("microphone_source") else {
        debug!(
            muted,
            "Microphone source отсутствует, команда Mute не требуется"
        );
        return Ok(());
    };
    if source.find_property("mute").is_none() {
        return Err(anyhow!(
            "pulsesrc microphone_source не поддерживает свойство mute"
        ));
    }
    source.set_property("mute", muted);
    Ok(())
}

enum RecordingStartGate {
    Begin,
    Cancelled,
    Error(anyhow::Error),
}

fn wait_for_recording_begin(
    pipeline: &gst::Pipeline,
    command_rx: &Receiver<RecorderCommand>,
    startup_confirmation: &StartupConfirmationArbiter,
    session_id: RecordingSessionId,
) -> RecordingStartGate {
    loop {
        if startup_confirmation.is_cancelled() {
            return RecordingStartGate::Cancelled;
        }
        match command_rx.recv_timeout(Duration::from_millis(20)) {
            Ok(command) if command.session_id() != session_id => continue,
            Ok(RecorderCommand::BeginRecording { .. }) => {
                return if startup_confirmation.is_cancelled() {
                    RecordingStartGate::Cancelled
                } else {
                    RecordingStartGate::Begin
                };
            }
            Ok(RecorderCommand::Stop { .. }) => {
                startup_confirmation.request_stop();
                return RecordingStartGate::Cancelled;
            }
            Ok(RecorderCommand::SetMicrophoneMuted { muted, .. }) => {
                if let Err(error) = set_microphone_muted(pipeline, muted) {
                    error!(?session_id, muted, error = %error, "Не удалось изменить Mute во время отсчёта");
                }
            }
            Ok(RecorderCommand::Pause { .. } | RecorderCommand::Resume { .. }) => {
                debug!(
                    ?session_id,
                    "Команда Pause/Resume проигнорирована до начала записи"
                );
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return RecordingStartGate::Error(anyhow!(
                    "канал команд recorder закрылся до начала записи"
                ));
            }
        }
    }
}

fn run_recorder(
    options: RecorderOptions,
    event_tx: Sender<RecorderEvent>,
    command_rx: Receiver<RecorderCommand>,
    startup_confirmation: Arc<StartupConfirmationArbiter>,
    session_id: RecordingSessionId,
) {
    let started_at = Instant::now();
    info!(
        ?session_id,
        output = %options.output.display(),
        session_type = ?std::env::var("XDG_SESSION_TYPE").ok(),
        "Recorder worker запущен"
    );
    let output_existed_before_start = match output_existed_before_start(&options.output) {
        Ok(existed) => {
            debug!(
                ?session_id,
                existed, "Проверен итоговый путь перед запуском"
            );
            existed
        }
        Err(error) => {
            error!(?session_id, error = %error, "Не удалось проверить итоговый путь перед запуском");
            finish_error(
                &event_tx,
                session_id,
                format!("Не удалось проверить путь записи перед запуском: {error:#}"),
            );
            return;
        }
    };

    let Some((portal_runtime, portal_stream)) =
        acquire_portal_capture(&event_tx, &startup_confirmation, session_id, started_at)
    else {
        return;
    };
    let Some((pipeline, bus, portal_stream)) = start_recording_pipeline(
        &options,
        portal_stream,
        &event_tx,
        &command_rx,
        &startup_confirmation,
        session_id,
        started_at,
        output_existed_before_start,
    ) else {
        return;
    };

    let terminal = run_recording_commands(
        &pipeline,
        &bus,
        &event_tx,
        &command_rx,
        session_id,
        started_at,
    );
    let completion = finalize_recording_pipeline(&pipeline, &bus, terminal, session_id);
    finish_recording_output(
        completion,
        &event_tx,
        session_id,
        &options.output,
        output_existed_before_start,
        started_at,
    );

    // Portal runtime, session and restricted FD must outlive every pipeline operation.
    drop(bus);
    drop(pipeline);
    drop(portal_stream);
    drop(portal_runtime);
}

fn acquire_portal_capture(
    event_tx: &Sender<RecorderEvent>,
    startup_confirmation: &Arc<StartupConfirmationArbiter>,
    session_id: RecordingSessionId,
    started_at: Instant,
) -> Option<(Option<tokio::runtime::Runtime>, Option<PortalStream>)> {
    if std::env::var("XDG_SESSION_TYPE").as_deref() != Ok("wayland") {
        info!(
            ?session_id,
            phase = "portal",
            "Wayland portal не используется для текущего типа сессии"
        );
        return Some((None, None));
    }

    info!(
        ?session_id,
        phase = "portal",
        "Ожидание Wayland ScreenCast portal"
    );
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            error!(
                ?session_id,
                phase = "portal",
                elapsed_ms = elapsed_ms(started_at),
                error = %error,
                "Не удалось создать runtime Wayland portal"
            );
            match startup_confirmation.resolve_error(error.into()) {
                StartupDecision::Cancelled => finish_cancelled(event_tx, session_id),
                StartupDecision::Error(error) => finish_error(
                    event_tx,
                    session_id,
                    format!("Не удалось запустить runtime Wayland portal: {error}"),
                ),
                StartupDecision::Started => finish_error(
                    event_tx,
                    session_id,
                    "Внутренняя ошибка арбитража запуска recorder".into(),
                ),
            }
            return None;
        }
    };

    let portal_result = runtime.block_on(select_portal_startup_result(
        portal_stream(),
        wait_for_startup_cancel(startup_confirmation.clone()),
    ));
    match portal_result {
        Ok(Some(stream)) => {
            info!(
                ?session_id,
                phase = "portal",
                node = stream.node_id,
                width = stream.width,
                height = stream.height,
                elapsed_ms = elapsed_ms(started_at),
                "Wayland portal вернул PipeWire stream и remote FD"
            );
            Some((Some(runtime), Some(stream)))
        }
        Ok(None) => {
            info!(
                ?session_id,
                phase = "portal",
                elapsed_ms = elapsed_ms(started_at),
                "Запуск отменён во время ожидания Wayland portal"
            );
            finish_cancelled(event_tx, session_id);
            None
        }
        Err(error) => {
            error!(
                ?session_id,
                phase = "portal",
                elapsed_ms = elapsed_ms(started_at),
                error = %error,
                "Wayland portal завершился ошибкой"
            );
            match portal_failure_event(session_id, error) {
                RecorderEvent::Cancelled { .. } => {
                    startup_confirmation.request_stop();
                    finish_cancelled(event_tx, session_id);
                }
                RecorderEvent::Error { message, .. } => {
                    match startup_confirmation.resolve_error(anyhow!(message)) {
                        StartupDecision::Cancelled => finish_cancelled(event_tx, session_id),
                        StartupDecision::Error(error) => {
                            finish_error(event_tx, session_id, error.to_string());
                        }
                        StartupDecision::Started => finish_error(
                            event_tx,
                            session_id,
                            "Внутренняя ошибка арбитража запуска recorder".into(),
                        ),
                    }
                }
                _ => {}
            }
            None
        }
    }
}

// Startup resources stay explicit here; a context bag would obscure their ownership and lifetime.
#[allow(clippy::too_many_arguments)]
fn start_recording_pipeline(
    options: &RecorderOptions,
    portal_stream: Option<PortalStream>,
    event_tx: &Sender<RecorderEvent>,
    command_rx: &Receiver<RecorderCommand>,
    startup_confirmation: &StartupConfirmationArbiter,
    session_id: RecordingSessionId,
    started_at: Instant,
    output_existed_before_start: bool,
) -> Option<(gst::Pipeline, gst::Bus, Option<PortalStream>)> {
    if startup_confirmation.is_cancelled() {
        info!(
            ?session_id,
            phase = "pipeline",
            elapsed_ms = elapsed_ms(started_at),
            "Запуск отменён до создания recording pipeline"
        );
        finish_cancelled(event_tx, session_id);
        return None;
    }

    info!(
        ?session_id,
        phase = "pipeline",
        elapsed_ms = elapsed_ms(started_at),
        "Сборка recording pipeline"
    );
    let (pipeline, portal_stream) = match build_pipeline(options, portal_stream) {
        Ok(value) => {
            info!(
                ?session_id,
                phase = "pipeline",
                elapsed_ms = elapsed_ms(started_at),
                "Recording pipeline собран"
            );
            value
        }
        Err(error) => {
            error!(
                ?session_id,
                phase = "pipeline",
                elapsed_ms = elapsed_ms(started_at),
                error = %error,
                "Не удалось собрать recording pipeline"
            );
            match startup_confirmation.resolve_error(error) {
                StartupDecision::Cancelled => finish_cancelled(event_tx, session_id),
                StartupDecision::Error(error) => {
                    finish_error(event_tx, session_id, format!("Ошибка GStreamer: {error:#}"))
                }
                StartupDecision::Started => finish_error(
                    event_tx,
                    session_id,
                    "Внутренняя ошибка арбитража запуска recorder".into(),
                ),
            }
            return None;
        }
    };
    if startup_confirmation.is_cancelled() {
        info!(
            ?session_id,
            phase = "pipeline",
            elapsed_ms = elapsed_ms(started_at),
            "Запуск отменён после сборки pipeline, до Playing"
        );
        cancel_pipeline_start(
            &pipeline,
            event_tx,
            session_id,
            &options.output,
            output_existed_before_start,
        );
        return None;
    }

    let Some(bus) = pipeline.bus() else {
        error!(
            ?session_id,
            phase = "pipeline",
            elapsed_ms = elapsed_ms(started_at),
            "У recording pipeline отсутствует GStreamer Bus"
        );
        let message = match startup_confirmation
            .resolve_error(anyhow!("у GStreamer pipeline отсутствует bus"))
        {
            StartupDecision::Cancelled => {
                cancel_pipeline_start(
                    &pipeline,
                    event_tx,
                    session_id,
                    &options.output,
                    output_existed_before_start,
                );
                return None;
            }
            StartupDecision::Error(error) => format!("Не удалось начать запись: {error}"),
            StartupDecision::Started => {
                "Внутренняя ошибка арбитража запуска recorder: отсутствует GStreamer bus".into()
            }
        };
        let null_message =
            pipeline_null_error_suffix(&pipeline, "; переход в Null также завершился ошибкой: ");
        finish_pipeline_error(
            event_tx,
            session_id,
            &options.output,
            output_existed_before_start,
            format!("{message}{null_message}"),
        );
        return None;
    };

    info!(
        ?session_id,
        phase = "prepared",
        elapsed_ms = elapsed_ms(started_at),
        "Recording pipeline подготовлен, ожидается экранный отсчёт"
    );
    send_event(event_tx, RecorderEvent::Prepared { session_id });
    match wait_for_recording_begin(&pipeline, command_rx, startup_confirmation, session_id) {
        RecordingStartGate::Begin => {}
        RecordingStartGate::Cancelled => {
            info!(
                ?session_id,
                phase = "prepared",
                elapsed_ms = elapsed_ms(started_at),
                "Запуск отменён во время экранного отсчёта"
            );
            cancel_pipeline_start(
                &pipeline,
                event_tx,
                session_id,
                &options.output,
                output_existed_before_start,
            );
            return None;
        }
        RecordingStartGate::Error(error) => {
            let cleanup_message =
                pipeline_null_error_suffix(&pipeline, "; также не удалось перейти в Null: ");
            finish_pipeline_error(
                event_tx,
                session_id,
                &options.output,
                output_existed_before_start,
                format!("Не удалось дождаться начала записи: {error}{cleanup_message}"),
            );
            return None;
        }
    }

    info!(
        ?session_id,
        phase = "playing",
        elapsed_ms = elapsed_ms(started_at),
        "Запуск pipeline в Playing и подтверждение состояния"
    );
    match confirm_startup_or_cancel(
        &pipeline,
        &bus,
        startup_confirmation,
        session_id,
        started_at,
    ) {
        StartupDecision::Started => {
            info!(
                ?session_id,
                phase = "playing",
                elapsed_ms = elapsed_ms(started_at),
                "Pipeline подтверждён в Playing, отправляется Started"
            );
            send_event(event_tx, RecorderEvent::Started { session_id });
            Some((pipeline, bus, portal_stream))
        }
        StartupDecision::Cancelled => {
            info!(
                ?session_id,
                phase = "playing",
                elapsed_ms = elapsed_ms(started_at),
                "Запуск отменён до commit Playing"
            );
            cancel_pipeline_start(
                &pipeline,
                event_tx,
                session_id,
                &options.output,
                output_existed_before_start,
            );
            None
        }
        StartupDecision::Error(error) => {
            error!(
                ?session_id,
                phase = "playing",
                elapsed_ms = elapsed_ms(started_at),
                error = %error,
                "Не удалось подтвердить Playing"
            );
            let cleanup_message =
                pipeline_null_error_suffix(&pipeline, "; также не удалось перейти в Null: ");
            finish_pipeline_error(
                event_tx,
                session_id,
                &options.output,
                output_existed_before_start,
                format!("Не удалось начать запись: {error}{cleanup_message}"),
            );
            None
        }
    }
}

fn pipeline_null_error_suffix(pipeline: &gst::Pipeline, prefix: &str) -> String {
    set_pipeline_null(pipeline)
        .err()
        .map(|error| format!("{prefix}{error}"))
        .unwrap_or_default()
}

fn run_recording_commands(
    pipeline: &gst::Pipeline,
    bus: &gst::Bus,
    event_tx: &Sender<RecorderEvent>,
    command_rx: &Receiver<RecorderCommand>,
    session_id: RecordingSessionId,
    started_at: Instant,
) -> Option<PipelineTerminal> {
    loop {
        if let Some(terminal) = take_available_pipeline_terminal(bus) {
            warn!(
                ?session_id,
                phase = "recording",
                terminal = ?terminal,
                elapsed_ms = elapsed_ms(started_at),
                "Recording pipeline прислал terminal message до команды Stop"
            );
            return Some(terminal);
        }

        match command_rx.recv_timeout(Duration::from_millis(20)) {
            Ok(command) if command.session_id() != session_id => continue,
            Ok(command) => match command {
                RecorderCommand::BeginRecording { .. } => {
                    debug!(
                        ?session_id,
                        "Повторная команда BeginRecording проигнорирована"
                    );
                }
                RecorderCommand::Stop { .. } => {
                    info!(
                        ?session_id,
                        phase = "stopping",
                        elapsed_ms = elapsed_ms(started_at),
                        "Worker получил Stop"
                    );
                    let terminal = take_available_pipeline_terminal(bus);
                    if let Some(terminal) = terminal.as_ref() {
                        warn!(?session_id, terminal = ?terminal, "Перед Stop обнаружено terminal message pipeline");
                    }
                    return terminal;
                }
                RecorderCommand::Pause { .. } => {
                    info!(?session_id, phase = "pausing", "Worker получил Pause");
                    if let Err(error) = set_capture_paused(pipeline, true) {
                        error!(?session_id, phase = "pausing", error = %error, "Не удалось подтвердить Paused");
                        return Some(PipelineTerminal::Error(format!(
                            "Не удалось приостановить запись: {error}"
                        )));
                    }
                    info!(
                        ?session_id,
                        phase = "paused",
                        "Recording pipeline переведён в Paused"
                    );
                    send_event(event_tx, RecorderEvent::Paused { session_id });
                }
                RecorderCommand::Resume { .. } => {
                    info!(?session_id, phase = "resuming", "Worker получил Resume");
                    if let Err(error) = set_capture_paused(pipeline, false) {
                        error!(?session_id, phase = "resuming", error = %error, "Не удалось подтвердить Playing после Resume");
                        return Some(PipelineTerminal::Error(format!(
                            "Не удалось продолжить запись: {error}"
                        )));
                    }
                    info!(
                        ?session_id,
                        phase = "recording",
                        "Recording pipeline переведён обратно в Playing"
                    );
                    send_event(event_tx, RecorderEvent::Resumed { session_id });
                }
                RecorderCommand::SetMicrophoneMuted { muted, .. } => {
                    if let Err(error) = set_microphone_muted(pipeline, muted) {
                        error!(?session_id, muted, error = %error, "Не удалось изменить Mute микрофона");
                    } else {
                        info!(?session_id, muted, "Mute микрофона изменён");
                    }
                }
            },
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                warn!(?session_id, "Канал команд recorder закрыт");
                return take_available_pipeline_terminal(bus);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
}

fn finalize_recording_pipeline(
    pipeline: &gst::Pipeline,
    bus: &gst::Bus,
    terminal: Option<PipelineTerminal>,
    session_id: RecordingSessionId,
) -> std::result::Result<(bool, bool), String> {
    let mut eos_received = false;
    let mut terminal_error = None;
    if let Some(terminal) = terminal {
        apply_pipeline_terminal(terminal, &mut eos_received, &mut terminal_error);
    }

    if !eos_received && terminal_error.is_none() {
        // EOS должен обрабатываться в Playing: при Stop из Paused сначала
        // возобновляем pipeline, чтобы muxer и live sources завершились штатно.
        if let Err(error) = set_capture_paused(pipeline, false) {
            error!(?session_id, phase = "stopping", error = %error, "Не удалось возобновить pipeline перед EOS");
            terminal_error = Some(format!(
                "Не удалось подготовить запись к сохранению: {error}"
            ));
        }
        info!(
            ?session_id,
            phase = "stopping",
            "Pipeline подготовлен к завершению, отправка EOS на mux sink pads"
        );
        if terminal_error.is_none()
            && let Err(error) = send_eos_to_mux_pads(pipeline)
        {
            error!(?session_id, phase = "stopping", error = %error, "GStreamer не принял EOS на mux sink pads");
            terminal_error = Some(error.to_string());
        } else if terminal_error.is_none() {
            debug!(
                ?session_id,
                phase = "stopping",
                "EOS отправлен, ожидание EOS на Bus"
            );
            match wait_for_eos(bus, session_id) {
                Ok(()) => {
                    eos_received = true;
                    info!(?session_id, phase = "stopping", "EOS получен на Bus");
                }
                Err(error) => {
                    error!(?session_id, phase = "stopping", error = %error, "Не удалось получить EOS");
                    terminal_error = Some(error.to_string());
                }
            }
        }
    }

    debug!(
        ?session_id,
        phase = "stopping",
        "Перевод recording pipeline в Null"
    );
    let null_succeeded = match set_pipeline_null(pipeline) {
        Ok(()) => {
            debug!(
                ?session_id,
                phase = "stopping",
                "Recording pipeline перешёл в Null"
            );
            true
        }
        Err(error) => {
            error!(?session_id, phase = "stopping", error = %error, "Не удалось перевести recording pipeline в Null");
            if let Some(existing_error) = terminal_error.as_mut() {
                existing_error.push_str(&format!(
                    "; переход recording pipeline в Null также завершился ошибкой: {error}"
                ));
            } else {
                terminal_error = Some(format!(
                    "Не удалось перевести recording pipeline в Null: {error}"
                ));
            }
            false
        }
    };
    if let Some(error) = terminal_error {
        Err(error)
    } else {
        Ok((eos_received, null_succeeded))
    }
}

fn finish_recording_output(
    completion: std::result::Result<(bool, bool), String>,
    event_tx: &Sender<RecorderEvent>,
    session_id: RecordingSessionId,
    output: &Path,
    output_existed_before_start: bool,
    started_at: Instant,
) {
    let (eos_received, null_succeeded) = match completion {
        Ok(completion) => completion,
        Err(error) => {
            error!(?session_id, elapsed_ms = elapsed_ms(started_at), error = %error, "Recorder завершился без сохранённого файла");
            finish_pipeline_error(
                event_tx,
                session_id,
                output,
                output_existed_before_start,
                format!("Запись не завершена корректно: {error}"),
            );
            return;
        }
    };
    match terminal_output(output, eos_received, null_succeeded) {
        Ok(path) => {
            info!(?session_id, path = %path.display(), elapsed_ms = elapsed_ms(started_at), "Recorder завершил запись успешно");
            send_event(event_tx, RecorderEvent::Stopped { session_id, path })
        }
        Err(error) => {
            error!(
                ?session_id,
                path = %output.display(),
                elapsed_ms = elapsed_ms(started_at),
                error = %error,
                "Итоговый MP4 не прошёл проверку"
            );
            finish_pipeline_error(
                event_tx,
                session_id,
                output,
                output_existed_before_start,
                format!("Запись не завершена корректно: {error:#}"),
            )
        }
    }
}
fn finish_cancelled(event_tx: &Sender<RecorderEvent>, session_id: RecordingSessionId) {
    info!(?session_id, "Recorder отправляет Cancelled");
    send_event(event_tx, RecorderEvent::Cancelled { session_id });
}

fn finish_error(event_tx: &Sender<RecorderEvent>, session_id: RecordingSessionId, message: String) {
    error!(?session_id, message = %message, "Recorder отправляет Error");
    send_event(
        event_tx,
        RecorderEvent::Error {
            session_id,
            message,
        },
    );
}

fn finish_pipeline_error(
    event_tx: &Sender<RecorderEvent>,
    session_id: RecordingSessionId,
    output: &Path,
    output_existed_before_start: bool,
    mut message: String,
) {
    error!(?session_id, message = %message, "Ошибка recording pipeline, выполняется очистка результата");
    if let Err(error) = cleanup_output_placeholder(output, output_existed_before_start) {
        error!(?session_id, error = %error, "Не удалось очистить placeholder MP4");
        message.push_str(&format!("; не удалось удалить placeholder MP4: {error}"));
    }
    finish_error(event_tx, session_id, message);
}

fn cancel_pipeline_start(
    pipeline: &gst::Pipeline,
    event_tx: &Sender<RecorderEvent>,
    session_id: RecordingSessionId,
    output: &Path,
    output_existed_before_start: bool,
) {
    info!(
        ?session_id,
        "Отмена запуска: перевод pipeline в Null без EOS"
    );
    let null_result = set_pipeline_null(pipeline);
    let cleanup_result = cleanup_output_placeholder(output, output_existed_before_start);
    match (null_result, cleanup_result) {
        (Ok(()), Ok(())) => {
            info!(?session_id, "Запуск отменён, pipeline очищен");
            send_event(event_tx, RecorderEvent::Cancelled { session_id });
        }
        (null_result, cleanup_result) => {
            let mut message = String::from("Не удалось отменить запуск записи");
            if let Err(error) = null_result {
                error!(?session_id, error = %error, "Не удалось перевести pipeline в Null при отмене запуска");
                message.push_str(&format!(": {error}"));
            }
            if let Err(error) = cleanup_result {
                error!(?session_id, error = %error, "Не удалось очистить placeholder при отмене запуска");
                message.push_str(&format!("; не удалось удалить placeholder MP4: {error}"));
            }
            finish_error(event_tx, session_id, message);
        }
    }
}

fn output_existed_before_start(output: &Path) -> Result<bool> {
    match fs::symlink_metadata(output) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error)
            .with_context(|| format!("не удалось проверить путь записи {}", output.display())),
    }
}

fn cleanup_output_placeholder(output: &Path, output_existed_before_start: bool) -> Result<()> {
    if output_existed_before_start {
        return Ok(());
    }

    let metadata = match fs::symlink_metadata(output) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("не удалось проверить placeholder {}", output.display()));
        }
    };
    if metadata.file_type().is_file() && metadata.len() == 0 {
        fs::remove_file(output)
            .with_context(|| format!("не удалось удалить placeholder {}", output.display()))?;
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PipelineTerminal {
    Eos,
    Error(String),
}

fn take_available_pipeline_terminal(bus: &gst::Bus) -> Option<PipelineTerminal> {
    let mut terminal = None;
    for message in bus.iter() {
        match message.view() {
            gst::MessageView::Error(error) => {
                let debug = error
                    .debug()
                    .unwrap_or_else(|| "нет debug-информации".into());
                return Some(PipelineTerminal::Error(format!(
                    "Ошибка GStreamer: {} ({debug})",
                    error.error()
                )));
            }
            // Продолжаем читать уже доступные сообщения: Error, опубликованный
            // после EOS, важнее успешной финализации.
            gst::MessageView::Eos(..) => terminal = Some(PipelineTerminal::Eos),
            _ => {}
        }
    }
    terminal
}

fn take_available_startup_error(bus: &gst::Bus) -> Option<anyhow::Error> {
    match take_available_pipeline_terminal(bus) {
        Some(PipelineTerminal::Error(message)) => Some(anyhow!(message)),
        Some(PipelineTerminal::Eos) => Some(anyhow!(
            "GStreamer pipeline завершился до подтверждения Started"
        )),
        None => None,
    }
}

fn apply_pipeline_terminal(
    terminal: PipelineTerminal,
    eos_received: &mut bool,
    terminal_error: &mut Option<String>,
) {
    match terminal {
        PipelineTerminal::Eos => *eos_received = true,
        PipelineTerminal::Error(error) => *terminal_error = Some(error),
    }
}

fn confirm_pipeline_state(
    pipeline: &gst::Pipeline,
    target: gst::State,
    context: &str,
) -> Result<()> {
    debug!(target_state = ?target, context, "Вызов pipeline.set_state");
    let state_change = pipeline
        .set_state(target)
        .with_context(|| format!("{context}: команда set_state завершилась ошибкой"))?;
    debug!(target_state = ?target, state_change = ?state_change, context, "Команда pipeline.set_state принята GStreamer");
    let (state_result, current, pending) = pipeline.state(Some(gst::ClockTime::from_seconds(5)));
    state_result.with_context(|| format!("{context}: таймаут или ошибка ожидания состояния"))?;
    debug!(target_state = ?target, current = ?current, pending = ?pending, context, "Подтверждено состояние pipeline");
    if current != target || pending != gst::State::VoidPending {
        return Err(anyhow!(
            "{context}: текущее состояние {current:?}, ожидаемое {target:?}, pending {pending:?}"
        ));
    }
    Ok(())
}

fn confirm_startup_or_cancel(
    pipeline: &gst::Pipeline,
    bus: &gst::Bus,
    startup_confirmation: &StartupConfirmationArbiter,
    session_id: RecordingSessionId,
    started_at: Instant,
) -> StartupDecision {
    let context = "не удалось подтвердить переход recording pipeline в Playing";
    debug!(
        ?session_id,
        phase = "playing",
        "Вызов pipeline.set_state(Playing)"
    );
    if let Err(error) = pipeline.set_state(gst::State::Playing) {
        let error = take_available_startup_error(bus).unwrap_or_else(|| {
            anyhow!("{context}: команда set_state завершилась ошибкой: {error}")
        });
        error!(
            ?session_id,
            phase = "playing",
            elapsed_ms = elapsed_ms(started_at),
            error = %error,
            "pipeline.set_state(Playing) вернул ошибку"
        );
        return startup_confirmation.resolve_error(error);
    }
    debug!(
        ?session_id,
        phase = "playing",
        "pipeline.set_state(Playing) принят GStreamer"
    );

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut last_observed_state = None;
    loop {
        if let Some(error) = take_available_startup_error(bus) {
            error!(
                ?session_id,
                phase = "playing",
                elapsed_ms = elapsed_ms(started_at),
                error = %error,
                "Ошибка GStreamer на Bus во время ожидания Playing"
            );
            return startup_confirmation.resolve_error(error);
        }
        if startup_confirmation.is_cancelled() {
            info!(
                ?session_id,
                phase = "playing",
                elapsed_ms = elapsed_ms(started_at),
                "Startup arbiter сообщил об отмене во время ожидания Playing"
            );
            return StartupDecision::Cancelled;
        }

        let (state_result, current, pending) =
            pipeline.state(Some(gst::ClockTime::from_nseconds(0)));
        if let Err(error) = state_result {
            let error = take_available_startup_error(bus).unwrap_or_else(|| {
                anyhow!("{context}: таймаут или ошибка ожидания состояния: {error}")
            });
            error!(
                ?session_id,
                phase = "playing",
                elapsed_ms = elapsed_ms(started_at),
                error = %error,
                "pipeline.state() вернул ошибку во время ожидания Playing"
            );
            return startup_confirmation.resolve_error(error);
        }
        let observed_state = (current, pending);
        if last_observed_state != Some(observed_state) {
            debug!(
                ?session_id,
                phase = "playing",
                elapsed_ms = elapsed_ms(started_at),
                current = ?current,
                pending = ?pending,
                "Наблюдается состояние recording pipeline"
            );
            last_observed_state = Some(observed_state);
        }
        if current == gst::State::Playing && pending == gst::State::VoidPending {
            let decision = decide_startup_confirmation(startup_confirmation, Ok(()), || {
                take_available_startup_error(bus)
            });
            info!(
                ?session_id,
                phase = "playing",
                elapsed_ms = elapsed_ms(started_at),
                decision = ?decision,
                "Подтверждение Playing прошло через startup arbiter"
            );
            return decision;
        }

        let now = std::time::Instant::now();
        if now >= deadline {
            let error = take_available_startup_error(bus).unwrap_or_else(|| {
                anyhow!("{context}: таймаут или ошибка ожидания состояния pipeline")
            });
            error!(
                ?session_id,
                phase = "playing",
                elapsed_ms = elapsed_ms(started_at),
                current = ?current,
                pending = ?pending,
                error = %error,
                "Истёк таймаут подтверждения Playing"
            );
            return startup_confirmation.resolve_error(error);
        }
        startup_confirmation.wait_for_cancellation(
            deadline
                .saturating_duration_since(now)
                .min(Duration::from_millis(50)),
        );
    }
}

fn elapsed_ms(started_at: Instant) -> u128 {
    started_at.elapsed().as_millis()
}

fn set_pipeline_null(pipeline: &gst::Pipeline) -> Result<()> {
    confirm_pipeline_state(
        pipeline,
        gst::State::Null,
        "не удалось перевести pipeline в Null",
    )
}

fn set_capture_paused(pipeline: &gst::Pipeline, paused: bool) -> Result<()> {
    let target = if paused {
        gst::State::Paused
    } else {
        gst::State::Playing
    };
    if !paused && pipeline.current_state() == gst::State::Paused {
        reset_pipewire_source_before_resume(pipeline)?;
    }
    confirm_pipeline_state(
        pipeline,
        target,
        if paused {
            "не удалось перевести recording pipeline в Paused"
        } else {
            "не удалось перевести recording pipeline в Playing"
        },
    )
}

fn reset_pipewire_source_before_resume(pipeline: &gst::Pipeline) -> Result<()> {
    let Some(source) = pipeline.by_name("screen_source") else {
        return Ok(());
    };
    let is_pipewire = source
        .factory()
        .is_some_and(|factory| factory.name() == "pipewiresrc");
    if !is_pipewire {
        return Ok(());
    }

    // PipeWire 1.6.2 can leave GstPipeWireSrc flushing after Playing -> Paused.
    // A normal BaseSrc Paused -> Ready transition runs unlock-stop before Resume.
    debug!("Сброс pipewiresrc в Ready перед Resume");
    let state_change = source
        .set_state(gst::State::Ready)
        .context("не удалось сбросить pipewiresrc в Ready перед Resume")?;
    let (state_result, current, pending) = source.state(Some(gst::ClockTime::from_seconds(5)));
    state_result.context("таймаут или ошибка ожидания pipewiresrc в Ready перед Resume")?;
    debug!(
        ?state_change,
        ?current,
        ?pending,
        "Проверен сброс pipewiresrc перед Resume"
    );
    if current != gst::State::Ready || pending != gst::State::VoidPending {
        return Err(anyhow!(
            "pipewiresrc не перешёл в Ready перед Resume: current {current:?}, pending {pending:?}"
        ));
    }
    Ok(())
}

fn send_eos_to_mux_pads(pipeline: &gst::Pipeline) -> Result<()> {
    let mux = pipeline
        .by_name("mux")
        .context("recording pipeline не содержит mp4mux")?;
    let mut pad_found = false;
    for name in ["video_0", "audio_0"] {
        let Some(pad) = mux.static_pad(name) else {
            continue;
        };
        pad_found = true;
        if !pad.send_event(gst::event::Eos::new()) {
            return Err(anyhow!("mp4mux sink pad {name} не принял EOS"));
        }
    }
    if !pad_found {
        return Err(anyhow!("mp4mux не содержит подключённых sink pads"));
    }
    Ok(())
}

async fn wait_for_startup_cancel(startup_confirmation: Arc<StartupConfirmationArbiter>) {
    startup_confirmation.wait_until_cancelled().await;
}

async fn select_portal_startup_result<T, P, C>(portal: P, startup_cancel: C) -> Result<Option<T>>
where
    P: Future<Output = Result<T>>,
    C: Future<Output = ()>,
{
    tokio::select! {
        // Если portal и Stop уже готовы одновременно, завершившийся portal
        // сохраняет свой результат: user cancel остаётся cancel, иной Err — error.
        biased;
        result = portal => result.map(Some),
        _ = startup_cancel => Ok(None),
    }
}

fn is_user_cancelled(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<AshpdError>(),
            Some(AshpdError::Response(ResponseError::Cancelled))
        )
    })
}

fn portal_failure_event(session_id: RecordingSessionId, error: anyhow::Error) -> RecorderEvent {
    if is_user_cancelled(&error) {
        RecorderEvent::Cancelled { session_id }
    } else {
        RecorderEvent::Error {
            session_id,
            message: format!("Не удалось получить экран через Wayland portal: {error:#}"),
        }
    }
}

fn wait_for_eos(bus: &gst::Bus, session_id: RecordingSessionId) -> Result<()> {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if let Some(message) = bus.timed_pop(gst::ClockTime::from_mseconds(100)) {
            match message.view() {
                gst::MessageView::Eos(..) => {
                    if let Some(PipelineTerminal::Error(error)) =
                        take_available_pipeline_terminal(bus)
                    {
                        error!(?session_id, phase = "stopping", error = %error, "На Bus обнаружена ошибка после EOS");
                        return Err(anyhow!(
                            "Ошибка GStreamer при финализации после EOS: {error}"
                        ));
                    }
                    return Ok(());
                }
                gst::MessageView::Error(error) => {
                    let debug = error
                        .debug()
                        .unwrap_or_else(|| "нет debug-информации".into());
                    let message = anyhow!(
                        "Ошибка GStreamer при финализации: {} ({debug})",
                        error.error()
                    );
                    error!(?session_id, phase = "stopping", error = %message, "На Bus обнаружена ошибка во время ожидания EOS");
                    return Err(message);
                }
                _ => {}
            }
        }
    }
    let error = anyhow!("таймаут ожидания EOS при финализации MP4");
    error!(?session_id, phase = "stopping", error = %error, "Истёк таймаут ожидания EOS");
    Err(error)
}

fn terminal_output(output: &Path, eos_received: bool, null_succeeded: bool) -> Result<PathBuf> {
    if !eos_received {
        return Err(anyhow!("EOS не получен"));
    }
    if !null_succeeded {
        return Err(anyhow!("pipeline не перешёл в Null"));
    }
    let metadata = fs::metadata(output)
        .with_context(|| format!("итоговый файл не найден: {}", output.display()))?;
    if !metadata.is_file() {
        return Err(anyhow!(
            "итоговый путь не является файлом: {}",
            output.display()
        ));
    }
    if metadata.len() == 0 {
        return Err(anyhow!("итоговый MP4 пуст: {}", output.display()));
    }
    Ok(output.to_path_buf())
}

fn send_event(event_tx: &Sender<RecorderEvent>, event: RecorderEvent) {
    if let Err(error) = event_tx.send(event) {
        error!(error = %error, "Не удалось передать событие recorder в GTK");
    }
}

pub fn start_recorder(
    options: RecorderOptions,
    event_tx: Sender<RecorderEvent>,
) -> Result<RecorderHandle> {
    let session_id = RecordingSessionId(NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed));
    info!(?session_id, output = %options.output.display(), "Создание recorder worker");
    let (command_tx, command_rx) = mpsc::channel();
    let thread_startup_confirmation = Arc::new(StartupConfirmationArbiter::new());
    let thread_startup_confirmation_for_worker = thread_startup_confirmation.clone();
    let thread = thread::Builder::new()
        .name("livescreen-recorder".into())
        .spawn(move || {
            run_recorder(
                options,
                event_tx,
                command_rx,
                thread_startup_confirmation_for_worker,
                session_id,
            )
        })
        .map_err(|error| {
            error!(?session_id, error = %error, "Не удалось запустить recorder thread");
            error
        })
        .context("не удалось запустить recorder thread")?;
    Ok(RecorderHandle {
        session_id,
        command_tx,
        thread: Some(thread),
        startup_confirmation: thread_startup_confirmation,
    })
}

#[cfg(test)]
mod tests;
