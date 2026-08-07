use super::*;
use std::fs;

#[test]
fn pipeline_description_parses_without_display_or_devices() {
    let options = RecorderOptions {
        microphone: None,
        system_audio: None,
        microphone_muted: false,
        output: PathBuf::from("/tmp/livescreen-test.mp4"),
    };
    let (pipeline, _) = build_pipeline(&options, None).expect("pipeline should parse");
    assert!(pipeline.by_name("comp").is_none());
    assert!(pipeline.by_name("mux").is_some());
    assert!(pipeline.by_name("screen_pause").is_none());
}

#[test]
fn audio_pipeline_parses_without_camera_overlay_branch() {
    let options = RecorderOptions {
        microphone: Some("default".into()),
        system_audio: Some("@DEFAULT_MONITOR@".into()),
        microphone_muted: false,
        output: PathBuf::from("/tmp/livescreen-test-camera.mp4"),
    };
    let (pipeline, _) = build_pipeline(&options, None).expect("full pipeline should parse");
    assert!(pipeline.by_name("mux").is_some());
    assert!(pipeline.by_name("comp").is_none());
    assert!(pipeline.by_name("camera_circle").is_none());
    assert!(pipeline.by_name("audio_pause").is_none());
}

#[test]
fn microphone_mute_targets_only_the_microphone_source() {
    let options = RecorderOptions {
        microphone: Some("default".into()),
        system_audio: Some("@DEFAULT_MONITOR@".into()),
        microphone_muted: true,
        output: PathBuf::from("/tmp/livescreen-test-muted.mp4"),
    };
    let (pipeline, _) = build_pipeline(&options, None).expect("audio pipeline should parse");
    let microphone = pipeline
        .by_name("microphone_source")
        .expect("microphone branch should have a named source");
    assert!(microphone.property::<bool>("mute"));
    set_microphone_muted(&pipeline, false).expect("microphone should unmute");
    assert!(!microphone.property::<bool>("mute"));

    let system_only = RecorderOptions {
        microphone: None,
        system_audio: Some("@DEFAULT_MONITOR@".into()),
        microphone_muted: true,
        output: PathBuf::from("/tmp/livescreen-test-system-only.mp4"),
    };
    let (pipeline, _) =
        build_pipeline(&system_only, None).expect("system-only pipeline should parse");
    assert!(pipeline.by_name("microphone_source").is_none());
}

#[test]
fn pipewire_screen_source_is_classified_as_video_capture() {
    gst::init().expect("GStreamer should initialize for the test");
    let element = gst::parse::launch("pipewiresrc name=screen_source ! fakesink")
        .expect("test pipeline should parse");
    let pipeline = element
        .downcast::<gst::Pipeline>()
        .expect("test pipeline should be a pipeline");

    configure_pipewire_screen_source(&pipeline).expect("PipeWire source should be configured");

    let source = pipeline
        .by_name("screen_source")
        .expect("test pipeline should contain screen_source");
    let properties = source.property::<gst::Structure>("stream-properties");
    assert_eq!(
        properties.get::<String>("media.type").as_deref(),
        Ok("Video")
    );
    assert_eq!(
        properties.get::<String>("media.category").as_deref(),
        Ok("Capture")
    );
    assert_eq!(
        properties.get::<String>("media.role").as_deref(),
        Ok("Screen")
    );
}

#[test]
fn terminal_output_requires_eos_null_and_existing_file() {
    let directory = std::env::temp_dir().join(format!(
        "livescreen-recorder-terminal-{}",
        std::process::id()
    ));
    if directory.exists() {
        fs::remove_dir_all(&directory).expect("test directory should be removable");
    }
    fs::create_dir_all(&directory).expect("test directory should be created");
    let output = directory.join("recording.mp4");

    assert!(terminal_output(&output, false, true).is_err());
    assert!(terminal_output(&output, true, false).is_err());
    assert!(terminal_output(&output, true, true).is_err());
    fs::write(&output, b"").expect("empty test output should be written");
    assert!(
        terminal_output(&output, true, true)
            .expect_err("zero-byte output should be rejected")
            .to_string()
            .contains("пуст")
    );
    fs::write(&output, b"mp4").expect("test output should be written");
    assert_eq!(
        terminal_output(&output, true, true).expect("terminal output should be accepted"),
        output
    );
    fs::remove_dir_all(&directory).expect("test directory should be removable");
}

#[test]
fn cleanup_removes_only_new_zero_byte_placeholder() {
    let directory = std::env::temp_dir().join(format!(
        "livescreen-recorder-cleanup-{}",
        std::process::id()
    ));
    if directory.exists() {
        fs::remove_dir_all(&directory).expect("test directory should be removable");
    }
    fs::create_dir_all(&directory).expect("test directory should be created");

    let new_output = directory.join("new.mp4");
    fs::write(&new_output, b"").expect("new placeholder should be written");
    cleanup_output_placeholder(&new_output, false).expect("new placeholder should be removed");
    assert!(!new_output.exists());

    let existing_output = directory.join("existing.mp4");
    fs::write(&existing_output, b"").expect("existing file should be written");
    cleanup_output_placeholder(&existing_output, true)
        .expect("pre-existing file should be left alone");
    assert!(existing_output.exists());

    let non_empty_output = directory.join("non-empty.mp4");
    fs::write(&non_empty_output, b"not empty").expect("non-empty file should be written");
    cleanup_output_placeholder(&non_empty_output, false)
        .expect("non-empty file should be left alone");
    assert!(non_empty_output.exists());

    fs::remove_dir_all(&directory).expect("test directory should be removable");
}

#[test]
fn portal_response_cancel_is_cancelled_but_other_response_is_error() {
    let session_id = RecordingSessionId(7);
    let cancelled = anyhow::Error::new(AshpdError::Response(ResponseError::Cancelled));
    assert!(matches!(
        portal_failure_event(session_id, cancelled),
        RecorderEvent::Cancelled { session_id: actual } if actual == session_id
    ));

    let other = anyhow::Error::new(AshpdError::Response(ResponseError::Other));
    assert!(matches!(
        portal_failure_event(session_id, other),
        RecorderEvent::Error { session_id: actual, message }
            if actual == session_id && message.contains("portal")
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn startup_cancellation_wins_over_late_portal_error() {
    let session_id = RecordingSessionId(14);
    let startup_confirmation = Arc::new(StartupConfirmationArbiter::new());
    assert_eq!(
        startup_confirmation.request_stop(),
        StartupStopDisposition::CancelStartup
    );
    let portal_error = anyhow::Error::new(AshpdError::Response(ResponseError::Other));
    let result = select_portal_startup_result(
        std::future::ready(Err::<(), anyhow::Error>(portal_error)),
        wait_for_startup_cancel(startup_confirmation.clone()),
    )
    .await;

    let error = result.expect_err("portal result should still be delivered");
    let RecorderEvent::Error { message, .. } = portal_failure_event(session_id, error) else {
        panic!("non-cancellation portal result should initially be an error");
    };
    assert!(matches!(
        startup_confirmation.resolve_error(anyhow!(message)),
        StartupDecision::Cancelled
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn portal_user_cancel_and_startup_cancel_remain_cancelled() {
    let session_id = RecordingSessionId(15);
    let startup_confirmation = Arc::new(StartupConfirmationArbiter::new());
    assert_eq!(
        startup_confirmation.request_stop(),
        StartupStopDisposition::CancelStartup
    );
    let portal_cancel = anyhow::Error::new(AshpdError::Response(ResponseError::Cancelled));
    let portal_result = select_portal_startup_result(
        std::future::ready(Err::<(), anyhow::Error>(portal_cancel)),
        wait_for_startup_cancel(startup_confirmation),
    )
    .await;
    let error = portal_result.expect_err("portal user cancellation must be preserved");
    assert!(matches!(
        portal_failure_event(session_id, error),
        RecorderEvent::Cancelled { session_id: actual } if actual == session_id
    ));

    let startup_confirmation = Arc::new(StartupConfirmationArbiter::new());
    assert_eq!(
        startup_confirmation.request_stop(),
        StartupStopDisposition::CancelStartup
    );
    let startup_result = select_portal_startup_result(
        std::future::pending::<Result<()>>(),
        wait_for_startup_cancel(startup_confirmation),
    )
    .await;
    assert!(matches!(startup_result, Ok(None)));
}

#[test]
fn controller_accepts_pause_and_resume_only_after_worker_confirmation() {
    let session_id = RecordingSessionId(8);
    let (mut controller, _event_rx, command_rx, _startup_confirmation) =
        controller_with_handle(session_id, RecorderState::Recording);

    controller
        .request_pause_or_resume()
        .expect("pause should be accepted");
    assert_eq!(controller.state(), RecorderState::Recording);
    assert!(controller.is_pause_resume_pending());
    assert!(matches!(
        command_rx.recv().expect("pause command should be sent"),
        RecorderCommand::Pause { session_id: actual } if actual == session_id
    ));
    assert!(
        controller
            .apply_event(RecorderEvent::Paused { session_id })
            .is_some()
    );
    assert_eq!(controller.state(), RecorderState::Paused);
    assert!(!controller.is_pause_resume_pending());

    controller
        .request_pause_or_resume()
        .expect("resume should be accepted");
    assert_eq!(controller.state(), RecorderState::Paused);
    assert!(controller.is_pause_resume_pending());
    assert!(matches!(
        command_rx.recv().expect("resume command should be sent"),
        RecorderCommand::Resume { session_id: actual } if actual == session_id
    ));
    assert!(
        controller
            .apply_event(RecorderEvent::Resumed { session_id })
            .is_some()
    );
    assert_eq!(controller.state(), RecorderState::Recording);
    assert!(!controller.is_pause_resume_pending());
}

#[test]
fn controller_forwards_microphone_mute_without_changing_lifecycle() {
    let session_id = RecordingSessionId(20);
    let (controller, _event_rx, command_rx, _startup_confirmation) =
        controller_with_handle(session_id, RecorderState::Recording);

    controller
        .set_microphone_muted(true)
        .expect("mute should be forwarded");
    assert_eq!(controller.state(), RecorderState::Recording);
    assert!(matches!(
        command_rx.recv().expect("mute command should be sent"),
        RecorderCommand::SetMicrophoneMuted {
            session_id: actual,
            muted: true
        } if actual == session_id
    ));
}

#[test]
fn prepared_start_waits_for_explicit_begin_without_changing_lifecycle() {
    let session_id = RecordingSessionId(21);
    let (mut controller, _event_rx, command_rx, _startup_confirmation) =
        controller_with_handle(session_id, RecorderState::Starting);

    assert!(
        controller
            .apply_event(RecorderEvent::Prepared { session_id })
            .is_some()
    );
    assert_eq!(controller.state(), RecorderState::Starting);
    controller
        .request_begin_recording()
        .expect("prepared recording should accept BeginRecording");
    assert!(matches!(
        command_rx.recv().expect("begin command should be sent"),
        RecorderCommand::BeginRecording { session_id: actual } if actual == session_id
    ));
    assert_eq!(controller.state(), RecorderState::Starting);
}

#[test]
fn recording_start_gate_accepts_begin_and_honors_startup_cancellation() {
    gst::init().expect("GStreamer should initialize for the test");
    let element = gst::parse::launch("fakesrc ! fakesink").expect("test pipeline should parse");
    let pipeline = element
        .downcast::<gst::Pipeline>()
        .expect("test element should be a pipeline");
    let session_id = RecordingSessionId(22);

    let (command_tx, command_rx) = mpsc::channel();
    command_tx
        .send(RecorderCommand::BeginRecording { session_id })
        .expect("begin command should be queued");
    assert!(matches!(
        wait_for_recording_begin(
            &pipeline,
            &command_rx,
            &StartupConfirmationArbiter::new(),
            session_id,
        ),
        RecordingStartGate::Begin
    ));

    let startup_confirmation = StartupConfirmationArbiter::new();
    startup_confirmation.request_stop();
    let (_command_tx, command_rx) = mpsc::channel();
    assert!(matches!(
        wait_for_recording_begin(&pipeline, &command_rx, &startup_confirmation, session_id,),
        RecordingStartGate::Cancelled
    ));
}

#[test]
fn controller_enters_stopping_immediately_and_sends_stop_once() {
    let session_id = RecordingSessionId(9);
    let (mut controller, event_rx, command_rx, startup_confirmation) =
        controller_with_handle(session_id, RecorderState::Recording);

    controller
        .request_stop()
        .expect("first stop should be accepted");
    assert_eq!(controller.state(), RecorderState::Stopping);
    assert_eq!(
        startup_confirmation.request_stop(),
        StartupStopDisposition::FinalizeRecording
    );
    assert!(matches!(
        command_rx.recv().expect("stop command should be sent"),
        RecorderCommand::Stop { session_id: actual } if actual == session_id
    ));
    assert!(command_rx.try_recv().is_err());
    assert!(matches!(
        event_rx.try_iter().find(|event| matches!(
            event,
            RecorderEvent::StateChanged { session_id: actual, state: RecorderState::Stopping }
                if *actual == session_id
        )),
        Some(RecorderEvent::StateChanged { .. })
    ));

    controller
        .request_stop()
        .expect("repeated stop in Stopping should be ignored");
    assert!(command_rx.try_recv().is_err());
}

#[test]
fn controller_stops_from_paused() {
    let session_id = RecordingSessionId(18);
    let (mut controller, event_rx, command_rx, _startup_confirmation) =
        controller_with_handle(session_id, RecorderState::Paused);

    controller
        .request_stop()
        .expect("stop from paused should be accepted");
    assert_eq!(controller.state(), RecorderState::Stopping);
    assert!(!controller.is_pause_resume_pending());
    assert!(matches!(
        command_rx.recv().expect("stop command should be sent"),
        RecorderCommand::Stop { session_id: actual } if actual == session_id
    ));
    assert!(event_rx.try_iter().any(|event| matches!(
        event,
        RecorderEvent::StateChanged {
            state: RecorderState::Stopping,
            ..
        }
    )));
}

#[test]
fn stop_before_startup_commit_is_cancelled_without_finalization() {
    let session_id = RecordingSessionId(13);
    let (mut controller, event_rx, command_rx, startup_confirmation) =
        controller_with_handle(session_id, RecorderState::Starting);

    controller
        .request_stop()
        .expect("stop during startup should be accepted");
    assert_eq!(controller.state(), RecorderState::Stopping);
    assert!(matches!(
        command_rx.recv().expect("stop command should be sent"),
        RecorderCommand::Stop { session_id: actual } if actual == session_id
    ));

    assert!(matches!(
        decide_startup_confirmation(&startup_confirmation, Ok(()), || None),
        StartupDecision::Cancelled
    ));
    assert!(event_rx.try_iter().all(|event| !matches!(
        event,
        RecorderEvent::Stopped { .. } | RecorderEvent::Error { .. }
    )));
}

#[test]
fn startup_commit_before_stop_requires_recording_finalization() {
    let startup_confirmation = StartupConfirmationArbiter::new();

    assert!(matches!(
        decide_startup_confirmation(&startup_confirmation, Ok(()), || None),
        StartupDecision::Started
    ));
    assert_eq!(
        startup_confirmation.request_stop(),
        StartupStopDisposition::FinalizeRecording
    );
}

#[test]
fn cancelled_startup_transitions_to_null_without_eos_or_stopped() {
    gst::init().expect("GStreamer should initialize for the test");
    let element =
        gst::parse::launch("fakesrc is-live=true ! fakesink").expect("test pipeline should parse");
    let pipeline = element
        .downcast::<gst::Pipeline>()
        .expect("test pipeline should be a pipeline");
    pipeline
        .set_state(gst::State::Playing)
        .expect("test pipeline should enter Playing");
    let bus = pipeline.bus().expect("test pipeline should have a bus");
    let output = std::env::temp_dir().join(format!(
        "livescreen-cancelled-startup-{}.mp4",
        std::process::id()
    ));
    let (event_tx, event_rx) = mpsc::channel();

    cancel_pipeline_start(&pipeline, &event_tx, RecordingSessionId(16), &output, false);

    assert!(matches!(
        event_rx.recv().expect("cancelled event should be sent"),
        RecorderEvent::Cancelled { .. }
    ));
    assert!(
        !bus.iter()
            .any(|message| matches!(message.view(), gst::MessageView::Eos(..)))
    );
    assert!(!output.exists());
}

#[test]
fn startup_cancellation_cannot_be_overwritten_by_late_pipeline_error() {
    let startup_confirmation = StartupConfirmationArbiter::new();
    assert_eq!(
        startup_confirmation.request_stop(),
        StartupStopDisposition::CancelStartup
    );
    let mut bus_checks = 0;
    let decision = decide_startup_confirmation(&startup_confirmation, Ok(()), || {
        bus_checks += 1;
        (bus_checks == 2).then(|| anyhow!("PipeWire link failed"))
    });

    assert!(matches!(decision, StartupDecision::Cancelled));
    assert_eq!(bus_checks, 1);
}

#[test]
fn startup_pipeline_error_first_wins_over_later_stop() {
    let startup_confirmation = StartupConfirmationArbiter::new();
    let decision = decide_startup_confirmation(&startup_confirmation, Ok(()), || {
        Some(anyhow!("PipeWire link failed"))
    });

    match decision {
        StartupDecision::Error(error) => {
            assert!(error.to_string().contains("PipeWire link failed"));
        }
        StartupDecision::Started | StartupDecision::Cancelled => {
            panic!("pipeline error observed first must remain an error")
        }
    }
    assert_eq!(
        startup_confirmation.request_stop(),
        StartupStopDisposition::Resolved
    );
}

#[test]
fn available_bus_error_wins_over_eos_when_stop_checks_terminal() {
    gst::init().expect("GStreamer should initialize for the test");
    let bus = gst::Bus::new();
    bus.post(gst::message::Eos::new())
        .expect("EOS should be posted to the test bus");
    bus.post(gst::message::Error::new(
        gst::CoreError::Failed,
        "PipeWire link failed",
    ))
    .expect("error should be posted to the test bus");

    match take_available_pipeline_terminal(&bus) {
        Some(PipelineTerminal::Error(message)) => {
            assert!(message.contains("PipeWire link failed"));
        }
        Some(PipelineTerminal::Eos) | None => {
            panic!("available bus error must win over stop finalization")
        }
    }
}

#[test]
fn wait_for_eos_rejects_available_error_queued_after_eos() {
    gst::init().expect("GStreamer should initialize for the test");
    let bus = gst::Bus::new();
    bus.post(gst::message::Eos::new())
        .expect("EOS should be posted to the test bus");
    bus.post(gst::message::Error::new(
        gst::CoreError::Failed,
        "muxer finalization failed",
    ))
    .expect("error should be posted to the test bus");

    let error = wait_for_eos(&bus, RecordingSessionId(17))
        .expect_err("available error after EOS must not be masked");
    assert!(error.to_string().contains("muxer finalization failed"));
}

#[test]
fn state_machine_covers_controller_transition_commands() {
    let mut lifecycle = RecorderStateMachine::new(RecorderState::Idle);
    assert!(lifecycle.state().can_start());
    assert!(!lifecycle.state().can_accept(RecorderCommandKind::Pause));
    assert!(lifecycle.transition(RecorderState::Starting));
    assert!(lifecycle.state().can_stop());
    assert!(!lifecycle.state().can_accept(RecorderCommandKind::Pause));
    assert!(lifecycle.transition(RecorderState::Recording));
    assert!(lifecycle.state().can_accept(RecorderCommandKind::Pause));
    assert!(lifecycle.transition(RecorderState::Paused));
    assert!(lifecycle.state().can_accept(RecorderCommandKind::Resume));
    assert!(lifecycle.state().can_stop());
    assert!(lifecycle.transition(RecorderState::Recording));
    assert!(lifecycle.state().can_accept(RecorderCommandKind::Pause));
    assert!(lifecycle.transition(RecorderState::Stopping));
    assert!(!lifecycle.state().can_start());
    assert!(!lifecycle.state().can_stop());
    assert!(lifecycle.transition(RecorderState::Error));
    assert!(lifecycle.transition(RecorderState::Idle));
    assert!(lifecycle.state().can_start());
}

fn controller_with_handle(
    session_id: RecordingSessionId,
    state: RecorderState,
) -> (
    RecorderController,
    Receiver<RecorderEvent>,
    Receiver<RecorderCommand>,
    Arc<StartupConfirmationArbiter>,
) {
    let (event_tx, event_rx) = mpsc::channel();
    let (command_tx, command_rx) = mpsc::channel();
    let startup_confirmation = Arc::new(StartupConfirmationArbiter::new());
    if state != RecorderState::Starting {
        assert!(matches!(
            startup_confirmation.commit_started(),
            StartupDecision::Started
        ));
    }
    let handle = RecorderHandle {
        session_id,
        command_tx,
        thread: None,
        startup_confirmation: startup_confirmation.clone(),
    };
    let controller = RecorderController {
        event_tx,
        handle: Some(handle),
        active_session: Some(session_id),
        lifecycle: RecorderStateMachine::new(state),
        pending_command: None,
    };
    (controller, event_rx, command_rx, startup_confirmation)
}

#[test]
fn stale_session_events_do_not_change_current_recorder_state() {
    let (event_tx, _event_rx) = mpsc::channel();
    let current = RecordingSessionId(11);
    let stale = RecordingSessionId(10);
    let mut controller = RecorderController {
        event_tx,
        handle: None,
        active_session: Some(current),
        lifecycle: RecorderStateMachine::new(RecorderState::Starting),
        pending_command: None,
    };

    assert!(
        controller
            .apply_event(RecorderEvent::Started { session_id: stale })
            .is_none()
    );
    assert_eq!(controller.state(), RecorderState::Starting);

    assert!(
        controller
            .apply_event(RecorderEvent::Started {
                session_id: current,
            })
            .is_some()
    );
    assert_eq!(controller.state(), RecorderState::Recording);

    assert!(
        controller
            .apply_event(RecorderEvent::Error {
                session_id: stale,
                message: "старое событие".into(),
            })
            .is_none()
    );
    assert_eq!(controller.state(), RecorderState::Recording);

    assert!(
        controller
            .apply_event(RecorderEvent::Cancelled {
                session_id: current,
            })
            .is_some()
    );
    assert_eq!(controller.state(), RecorderState::Idle);
    assert!(!controller.has_active_session());
}

#[test]
fn successful_terminal_event_returns_controller_to_idle() {
    let (event_tx, _event_rx) = mpsc::channel();
    let session_id = RecordingSessionId(12);
    let output = PathBuf::from("/tmp/livescreen-terminal.mp4");
    let mut controller = RecorderController {
        event_tx,
        handle: None,
        active_session: Some(session_id),
        lifecycle: RecorderStateMachine::new(RecorderState::Stopping),
        pending_command: None,
    };

    assert!(
        controller
            .apply_event(RecorderEvent::Stopped {
                session_id,
                path: output.clone(),
            })
            .is_some()
    );
    assert_eq!(controller.state(), RecorderState::Idle);
    assert!(!controller.has_active_session());

    assert!(
        controller
            .apply_event(RecorderEvent::Error {
                session_id,
                message: "запоздалое событие".into(),
            })
            .is_none()
    );
    assert_eq!(controller.state(), RecorderState::Idle);
}

#[test]
fn pipeline_state_pause_allows_eos_after_pause_and_resume() {
    gst::init().expect("GStreamer should initialize for the test");
    let element = gst::parse::launch(
        "mp4mux name=mux ! fakesink sync=false \
         videotestsrc is-live=true ! videoconvert ! video/x-raw,width=320,height=240,framerate=30/1 ! \
         x264enc tune=zerolatency ! h264parse ! queue ! mux.video_0 \
         audiotestsrc is-live=true ! audioconvert ! audioresample ! \
         voaacenc ! queue ! mux.audio_0",
    )
    .expect("test pipeline should parse");
    let pipeline = element
        .downcast::<gst::Pipeline>()
        .expect("test pipeline should be a pipeline");
    pipeline
        .set_state(gst::State::Playing)
        .expect("test pipeline should enter Playing");
    std::thread::sleep(Duration::from_millis(300));
    let pause_result = pipeline
        .set_state(gst::State::Paused)
        .expect("live pipeline should accept Paused");
    assert!(matches!(
        pause_result,
        gst::StateChangeSuccess::Success | gst::StateChangeSuccess::NoPreroll
    ));
    set_capture_paused(&pipeline, true).expect("pipeline should remain paused");
    std::thread::sleep(Duration::from_millis(300));
    let resume_result = pipeline
        .set_state(gst::State::Playing)
        .expect("live pipeline should accept Playing");
    assert!(matches!(
        resume_result,
        gst::StateChangeSuccess::Success | gst::StateChangeSuccess::Async
    ));
    set_capture_paused(&pipeline, false).expect("pipeline should remain playing");
    std::thread::sleep(Duration::from_millis(300));
    let bus = pipeline.bus().expect("test pipeline should have a bus");
    assert_eq!(
        finalize_recording_pipeline(&pipeline, &bus, None, RecordingSessionId(19))
            .expect("pipeline should finalize after capture pause and resume"),
        (true, true)
    );
}

#[test]
fn pipewire_resume_reset_skips_other_screen_sources() {
    gst::init().expect("GStreamer should initialize for the test");
    let element = gst::parse::launch("fakesrc name=screen_source ! fakesink")
        .expect("test pipeline should parse");
    let pipeline = element
        .downcast::<gst::Pipeline>()
        .expect("test element should be a pipeline");

    reset_pipewire_source_before_resume(&pipeline)
        .expect("non-PipeWire source should not require a compatibility reset");
    assert_eq!(
        pipeline
            .by_name("screen_source")
            .expect("screen source should exist")
            .current_state(),
        gst::State::Null
    );
}
