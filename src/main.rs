//! Composition root LiveScreen.
//!
//! Здесь остаются bootstrap процесса, GTK application shell/lifecycle и связывание вертикальных срезов через callbacks, timers и каналы.
//! Slice-specific логика должна жить в соответствующем модуле; одноразовое GTK-связывание не выносится, если это только добавляет аргументы, `clone()` или общий контейнер состояния.
//!
//! При изменениях сохраняй глобальные runtime-инварианты:
//! - GTK и Layer Shell objects создаются и изменяются только в GTK main thread;
//! - GTK main loop не ждёт синхронно portal, GStreamer, устройства или `JoinHandle`;
//! - callbacks и hotkeys отправляют доменные команды, а UI применяет подтверждённые worker events через каналы и не создаёт параллельный lifecycle;
//! - каждый срез остаётся владельцем своего ресурса и состояния; этот файл только связывает владельцев и не дублирует их логику;
//! - camera overlay остаётся отдельным desktop-окном вне recording pipeline, а Wayland screen capture проходит только через xdg-desktop-portal/PipeWire.

mod configuration;
mod devices;
mod gst_utils;
mod logging;
mod overlay;
mod portal;
mod preview;
mod recorder;
mod shortcuts;

use std::{
    cell::{Cell, RefCell},
    fs,
    path::PathBuf,
    rc::Rc,
    sync::mpsc,
    time::Duration,
};

use anyhow::{Context, Result};
use ashpd::WindowIdentifier;
use chrono::Local;
use gstreamer as gst;
use gtk::{gio, glib, prelude::*};
use gtk4 as gtk;
use tracing::{debug, error, info, warn};

use crate::{
    configuration::{CameraQuality, Paths, PortalConfig, UserSettings, read_json, write_json},
    devices::{audio_sources, camera_devices},
    overlay::{
        RecordingIndicatorOverlay, create_camera_overlay, create_recording_indicator_overlay,
    },
    portal::{APP_ID, ensure_desktop_entry, portal_stream},
    preview::{PreviewController, PreviewFrame},
    recorder::{RecorderController, RecorderEvent, RecorderOptions, RecorderState},
    shortcuts::{
        ApplicationCommand, ShortcutBindings, ShortcutChannels, ShortcutRequest, ShortcutUpdate,
        start_global_shortcuts,
    },
};

#[allow(deprecated)]
fn combo_with_items(items: &[(String, String)], selected: &str) -> gtk::ComboBoxText {
    let combo = gtk::ComboBoxText::new();
    for (id, label) in items {
        combo.append(Some(id), label);
    }
    if combo.active_id().is_none() {
        if items.iter().any(|(id, _)| id == selected) {
            combo.set_active_id(Some(selected));
        } else if let Some((id, _)) = items.first() {
            combo.set_active_id(Some(id));
        }
    }
    combo
}

#[allow(deprecated)]
fn combo_active_id(combo: &gtk::ComboBoxText) -> Option<String> {
    combo.active_id().map(|id| id.to_string())
}

#[allow(deprecated)]
fn connect_combo_changed<F>(combo: &gtk::ComboBoxText, callback: F)
where
    F: Fn(&gtk::ComboBoxText) + 'static,
{
    combo.connect_changed(callback);
}

#[allow(deprecated)]
fn choose_output_directory(parent: &gtk::ApplicationWindow, output_entry: &gtk::Entry) {
    let dialog = gtk::FileChooserNative::builder()
        .title("Папка записи")
        .action(gtk::FileChooserAction::SelectFolder)
        .transient_for(parent)
        .build();
    let output_entry = output_entry.clone();
    dialog.connect_response(move |dialog, response| {
        if response == gtk::ResponseType::Accept
            && let Some(file) = dialog.file().and_then(|file| file.path())
        {
            output_entry.set_text(&file.to_string_lossy());
        }
        dialog.destroy();
    });
    dialog.set_visible(true);
}

fn selected_camera_device(active_id: Option<&str>) -> String {
    active_id.unwrap_or("none").to_owned()
}

fn install_ui_css() {
    let Some(display) = gtk::gdk::Display::default() else {
        warn!("GTK display недоступен для установки UI styles");
        return;
    };
    let provider = gtk::CssProvider::new();
    provider.load_from_string(
        ".hotkey-value { min-height: 18px; padding: 0 4px; margin: 0; }\
         button.selfie-control { border-width: 0.5px; border-color: alpha(currentColor, 0.22); box-shadow: none; }\
         .recording-indicator-overlay, .recording-indicator-root { background-color: transparent; }\
         .recording-indicator-text { font-size: 112px; font-weight: 700; }\
         .recording-indicator-white { color: #ffffff; }\
         .recording-indicator-red { color: #ef3e3e; }\
         .recording-indicator-green { color: #37c86f; }\
         .recording-indicator-lime { color: #d8f05a; }",
    );
    gtk::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}

fn hotkey_button(value: &str) -> gtk::Button {
    let button = gtk::Button::with_label(value);
    button.set_halign(gtk::Align::Start);
    button.set_hexpand(true);
    button.add_css_class("flat");
    button.add_css_class("hotkey-value");
    button.set_tooltip_text(Some("Нажмите, чтобы изменить hotkeys"));
    button
}

fn round_control_button(label: &str) -> gtk::Button {
    let button = gtk::Button::with_label(label);
    button.add_css_class("circular");
    button.set_size_request(104, 104);
    button.set_halign(gtk::Align::Center);
    button.set_valign(gtk::Align::Center);
    button
}

fn render_mute_button(button: &gtk::ToggleButton) {
    if button.is_active() {
        button.set_label("MUTED");
        button.set_tooltip_text(Some("Микрофон выключен"));
        button.add_css_class("destructive-action");
    } else {
        button.set_label("MUTE");
        button.set_tooltip_text(Some("Выключить микрофон"));
        button.remove_css_class("destructive-action");
    }
}

fn connect_hotkey_button(
    button: &gtk::Button,
    configure_tx: &tokio::sync::mpsc::Sender<ShortcutRequest>,
) {
    let configure_tx = configure_tx.clone();
    button.connect_clicked(move |_| {
        if let Err(error) = configure_tx.try_send(ShortcutRequest::Configure) {
            debug!(error = ?error, "Запрос настройки hotkeys уже ожидает обработки");
        }
    });
}

fn selected_camera_quality(active_id: Option<&str>) -> CameraQuality {
    match active_id {
        Some("best") => CameraQuality::Best,
        Some("fast") => CameraQuality::Fast,
        _ => CameraQuality::Optim,
    }
}

fn render_recorder_controls(
    state: RecorderState,
    pause_resume_pending: bool,
    record_button: &gtk::Button,
    stop_button: &gtk::Button,
) {
    record_button.set_label(recorder_control_label(state));
    match state {
        RecorderState::Idle | RecorderState::Error => {
            record_button.set_sensitive(true);
            stop_button.set_sensitive(false);
        }
        RecorderState::Stopping => {
            record_button.set_sensitive(false);
            stop_button.set_sensitive(false);
        }
        RecorderState::Paused => {
            record_button.set_sensitive(!pause_resume_pending);
            stop_button.set_sensitive(true);
        }
        RecorderState::Recording => {
            record_button.set_sensitive(!pause_resume_pending);
            stop_button.set_sensitive(true);
        }
        RecorderState::Starting => {
            record_button.set_sensitive(false);
            stop_button.set_sensitive(true);
        }
    }
}

fn recorder_control_label(state: RecorderState) -> &'static str {
    match state {
        RecorderState::Idle | RecorderState::Error => "START",
        RecorderState::Starting => "START…",
        RecorderState::Recording => "PAUSE",
        RecorderState::Paused => "RESUME",
        RecorderState::Stopping => "SAVING…",
    }
}

fn recorder_state_status(state: RecorderState) -> &'static str {
    match state {
        RecorderState::Idle => "Готово к записи",
        RecorderState::Starting => "Открывается выбор экрана…",
        RecorderState::Recording => "● Запись идет",
        RecorderState::Paused => "Ⅱ Запись на паузе",
        RecorderState::Stopping => "сохранение видео, ждите..",
        RecorderState::Error => "Ошибка записи",
    }
}

fn recorder_status(recorder: &RecorderController) -> &'static str {
    if recorder.is_cancelling_startup() {
        "отмена запуска…"
    } else {
        recorder_state_status(recorder.state())
    }
}

fn can_apply_auxiliary_status(state: RecorderState) -> bool {
    state != RecorderState::Stopping
}

fn render_recorder_ui(
    recorder: &RecorderController,
    record_button: &gtk::Button,
    stop_button: &gtk::Button,
    status: &gtk::Label,
) {
    let state = recorder.state();
    render_recorder_controls(
        state,
        recorder.is_pause_resume_pending(),
        record_button,
        stop_button,
    );
    status.set_text(recorder_status(recorder));
}

fn close_recording_indicator(indicator: &Rc<RefCell<Option<Rc<RecordingIndicatorOverlay>>>>) {
    if let Some(indicator) = indicator.borrow_mut().take() {
        indicator.close();
    }
}

fn show_stop_indicator(indicator_slot: &Rc<RefCell<Option<Rc<RecordingIndicatorOverlay>>>>) {
    let Some(indicator) = indicator_slot.borrow().as_ref().cloned() else {
        return;
    };
    let weak_indicator = Rc::downgrade(&indicator);
    let indicator_slot = indicator_slot.clone();
    indicator.show_stopped(move || {
        let Some(indicator) = weak_indicator.upgrade() else {
            return;
        };
        indicator.close();
        let mut slot = indicator_slot.borrow_mut();
        if slot
            .as_ref()
            .is_some_and(|current| Rc::ptr_eq(current, &indicator))
        {
            slot.take();
        }
    });
}

fn close_recording_indicator_unless_stop(
    indicator: &Rc<RefCell<Option<Rc<RecordingIndicatorOverlay>>>>,
) {
    let should_close = indicator
        .borrow()
        .as_ref()
        .is_some_and(|indicator| !indicator.is_showing_stop());
    if should_close {
        close_recording_indicator(indicator);
    }
}

fn persist_shortcut_bindings(paths: &Paths, bindings: &ShortcutBindings) -> Result<()> {
    let mut settings = read_json::<UserSettings>(&paths.settings)?.value;
    settings.record_hotkey = bindings.record.clone();
    settings.stop_hotkey = bindings.stop.clone();
    settings.mute_hotkey = bindings.mute.clone();
    write_json(&paths.settings, &settings).with_context(|| {
        format!(
            "не удалось сохранить hotkeys в {}",
            paths.settings.display()
        )
    })?;

    let mut config = read_json::<PortalConfig>(&paths.portal)?.value;
    config.opacity_toggle_hotkey = bindings.opacity.clone();
    write_json(&paths.portal, &config).with_context(|| {
        format!(
            "не удалось сохранить hotkey прозрачности в {}",
            paths.portal.display()
        )
    })?;
    Ok(())
}

fn main() -> Result<()> {
    let _logging_guard = logging::init()?;
    info!(
        version = env!("CARGO_PKG_VERSION"),
        "LiveScreen запускается"
    );
    gst::init().context("не удалось инициализировать GStreamer")?;
    info!("GStreamer инициализирован");
    ensure_desktop_entry().context("не удалось зарегистрировать desktop entry")?;
    info!("Desktop entry зарегистрирован");
    if std::env::args().any(|arg| arg == "--diagnose") {
        info!("Запущен режим диагностики");
        return diagnose();
    }
    if std::env::args().any(|arg| arg == "--portal-test") {
        info!("Запущен режим проверки Wayland portal");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("не удалось запустить runtime для проверки portal")?;
        let stream = runtime.block_on(portal_stream())?;
        println!(
            "Portal screen stream: node={}, size={}x{}",
            stream.node_id, stream.width, stream.height
        );
        return Ok(());
    }

    let paths = Paths::discover();
    info!(
        portal_config = %paths.portal.display(),
        settings = %paths.settings.display(),
        session_type = ?std::env::var("XDG_SESSION_TYPE").ok(),
        "Загрузка конфигурации"
    );
    let portal_loaded = read_json::<PortalConfig>(&paths.portal)?;
    let mut startup_warnings = Vec::new();
    if let Some(warning) = portal_loaded.warning {
        startup_warnings.push(warning);
    }
    if !paths.portal.exists()
        && let Err(error) = write_json(&paths.portal, &portal_loaded.value)
    {
        startup_warnings.push(format!(
            "Не удалось сохранить начальные настройки {}: {error:#}",
            paths.portal.display()
        ));
    }
    let settings_loaded = read_json::<UserSettings>(&paths.settings)?;
    if let Some(warning) = settings_loaded.warning {
        startup_warnings.push(warning);
    }
    let saved_settings = settings_loaded.value;
    let startup_warning = startup_warnings.join("\n");
    if startup_warning.is_empty() {
        info!("Конфигурация загружена без предупреждений");
    } else {
        warn!(warning = %startup_warning, "Конфигурация загружена с предупреждениями");
    }

    let (shortcut_command_tx, shortcut_command_rx) = mpsc::channel::<ApplicationCommand>();
    let (shortcut_status_tx, shortcut_status_rx) = mpsc::channel::<String>();
    let (shortcut_bindings_tx, shortcut_bindings_rx) = mpsc::channel::<ShortcutUpdate>();
    let (shortcut_configure_tx, shortcut_configure_rx) = tokio::sync::mpsc::channel(1);
    let shortcut_command_rx = Rc::new(RefCell::new(shortcut_command_rx));
    let shortcut_status_rx = Rc::new(RefCell::new(shortcut_status_rx));
    let shortcut_bindings_rx = Rc::new(RefCell::new(shortcut_bindings_rx));
    let shortcut_started = Rc::new(Cell::new(false));

    let app = gtk::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::HANDLES_OPEN)
        .build();
    let paths_for_app = paths.clone();
    let shortcut_started_for_app = shortcut_started.clone();
    let shortcut_command_tx_for_app = shortcut_command_tx.clone();
    let shortcut_status_tx_for_app = shortcut_status_tx.clone();
    let shortcut_bindings_tx_for_app = shortcut_bindings_tx.clone();
    let shortcut_configure_tx_for_app = shortcut_configure_tx.clone();
    let shortcut_configure_rx_for_app = Rc::new(RefCell::new(Some(shortcut_configure_rx)));
    let saved_settings_for_app = saved_settings.clone();
    let portal_for_app = portal_loaded.value;
    let startup_warning_for_app = startup_warning.clone();
    app.connect_activate(move |app| {
        install_ui_css();
        let cameras = camera_devices();
        let audio = audio_sources();
        info!(cameras = cameras.len(), audio_sources = audio.len(), "GTK application активирована");
        let saved = saved_settings_for_app.clone();
        let shortcut_bindings = Rc::new(RefCell::new(ShortcutBindings {
            record: saved.record_hotkey.clone(),
            stop: saved.stop_hotkey.clone(),
            opacity: portal_for_app.opacity_toggle_hotkey.clone(),
            mute: saved.mute_hotkey.clone(),
        }));
        let window = gtk::ApplicationWindow::builder()
            .application(app)
            .title("LiveScreen — запись экрана")
            .default_width(620)
            .resizable(false)
            .build();
        let root = gtk::Box::new(gtk::Orientation::Vertical, 4);
        root.set_margin_top(16);
        root.set_margin_bottom(16);
        root.set_margin_start(20);
        root.set_margin_end(20);
        let status = gtk::Label::new(Some("Готово к записи"));
        status.set_xalign(0.0);
        status.set_wrap(true);
        status.set_max_width_chars(52);
        status.set_margin_top(10);
        status.set_margin_bottom(10);
        status.set_margin_start(12);
        status.set_margin_end(92);
        status.add_css_class("dim-label");
        let form = gtk::Grid::new();
        form.set_row_spacing(10);
        form.set_column_spacing(12);
        let label = |text: &str| {
            let label = gtk::Label::new(Some(text));
            label.set_xalign(0.0);
            label
        };
        form.attach(&label("Камера"), 0, 0, 1, 1);
        let camera_items = if cameras.is_empty() {
            vec![("none".into(), "Камера не найдена".into())]
        } else {
            cameras
                .iter()
                .map(|device| (device.id.clone(), device.label.clone()))
                .collect()
        };
        let camera_combo = combo_with_items(&camera_items, &saved.camera_device);
        form.attach(&camera_combo, 1, 0, 2, 1);
        let (preview_tx, preview_rx) = mpsc::channel::<PreviewFrame>();
        let preview_controller = Rc::new(RefCell::new(PreviewController::default()));
        form.attach(&label("Микрофон"), 0, 1, 1, 1);
        let mic_items = audio
            .iter()
            .filter(|source| !source.monitor)
            .map(|source| (source.id.clone(), source.label.clone()))
            .collect::<Vec<_>>();
        let mic_combo = combo_with_items(&mic_items, &saved.microphone_source);
        form.attach(&mic_combo, 1, 1, 2, 1);
        form.attach(&label("Звук системы"), 0, 2, 1, 1);
        let system_items = audio
            .iter()
            .filter(|source| source.monitor)
            .map(|source| (source.id.clone(), source.label.clone()))
            .collect::<Vec<_>>();
        let system_combo = combo_with_items(&system_items, &saved.system_source);
        form.attach(&system_combo, 1, 2, 2, 1);
        form.attach(&label("Экран"), 0, 3, 1, 1);
        let monitor_label =
            gtk::Label::new(Some("Выбор монитора появится в системном диалоге Wayland"));
        monitor_label.set_xalign(0.0);
        monitor_label.add_css_class("dim-label");
        form.attach(&monitor_label, 1, 3, 2, 1);
        form.attach(&label("Размер камеры"), 0, 4, 1, 1);
        let size_scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, 5.0, 50.0, 1.0);
        size_scale.set_value(saved.camera_size_percent as f64);
        size_scale.set_hexpand(true);
        size_scale.set_draw_value(true);
        size_scale.set_value_pos(gtk::PositionType::Right);
        form.attach(&size_scale, 1, 4, 2, 1);
        form.attach(&label("Качество Selfie"), 0, 5, 1, 1);
        let quality_items = [
            ("best".to_string(), CameraQuality::Best.label().to_string()),
            ("optim".to_string(), CameraQuality::Optim.label().to_string()),
            ("fast".to_string(), CameraQuality::Fast.label().to_string()),
        ];
        let quality_combo = combo_with_items(
            &quality_items,
            match saved.camera_quality {
                CameraQuality::Best => "best",
                CameraQuality::Optim => "optim",
                CameraQuality::Fast => "fast",
            },
        );
        form.attach(&quality_combo, 1, 5, 1, 1);
        let border_check = gtk::CheckButton::with_label("Рамка");
        border_check.set_active(saved.camera_border_visible);
        border_check.set_tooltip_text(Some("Показывать тонкую рамку вокруг Selfie"));
        form.attach(&border_check, 2, 5, 1, 1);
        form.attach(&label("Selfie ON visible"), 0, 6, 1, 1);
        let opacity_scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 100.0, 1.0);
        opacity_scale.set_value(saved.camera_transparency_percent as f64);
        opacity_scale.set_hexpand(true);
        opacity_scale.set_value_pos(gtk::PositionType::Right);
        form.attach(&opacity_scale, 1, 6, 2, 1);
        form.attach(&label("selfie OFF visible"), 0, 7, 1, 1);
        let secondary_opacity_scale =
            gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 100.0, 1.0);
        secondary_opacity_scale.set_value(portal_for_app.secondary_transparency_percent as f64);
        secondary_opacity_scale.set_hexpand(true);
        secondary_opacity_scale.set_value_pos(gtk::PositionType::Right);
        form.attach(&secondary_opacity_scale, 1, 7, 2, 1);
        let hotkey_form = gtk::Grid::new();
        hotkey_form.set_row_spacing(0);
        hotkey_form.set_column_spacing(10);
        hotkey_form.attach(&label("START / PAUSE"), 0, 0, 1, 1);
        let record_hotkey_label = hotkey_button(&saved.record_hotkey);
        hotkey_form.attach(&record_hotkey_label, 1, 0, 1, 1);
        hotkey_form.attach(&label("STOP"), 2, 0, 1, 1);
        let stop_hotkey_label = hotkey_button(&saved.stop_hotkey);
        hotkey_form.attach(&stop_hotkey_label, 3, 0, 1, 1);
        hotkey_form.attach(&label("SELFIE"), 0, 1, 1, 1);
        let opacity_hotkey_label = hotkey_button(&portal_for_app.opacity_toggle_hotkey);
        hotkey_form.attach(&opacity_hotkey_label, 1, 1, 1, 1);
        hotkey_form.attach(&label("MUTE"), 2, 1, 1, 1);
        let mute_hotkey_label = hotkey_button(&saved.mute_hotkey);
        hotkey_form.attach(&mute_hotkey_label, 3, 1, 1, 1);
        form.attach(&label("Папка записи"), 0, 8, 1, 1);
        let output_entry = gtk::Entry::new();
        output_entry.set_text(&saved.output_dir);
        output_entry.set_hexpand(true);
        form.attach(&output_entry, 1, 8, 1, 1);
        let choose_output = gtk::Button::with_label("Выбрать…");
        form.attach(&choose_output, 2, 8, 1, 1);
        connect_hotkey_button(&record_hotkey_label, &shortcut_configure_tx_for_app);
        connect_hotkey_button(&stop_hotkey_label, &shortcut_configure_tx_for_app);
        connect_hotkey_button(&opacity_hotkey_label, &shortcut_configure_tx_for_app);
        connect_hotkey_button(&mute_hotkey_label, &shortcut_configure_tx_for_app);
        let shared_frame = Rc::new(RefCell::new(None::<PreviewFrame>));
        let camera_overlay = create_camera_overlay(
            shared_frame.clone(),
            saved.camera_position,
            saved.camera_size_percent,
            saved.camera_transparency_percent,
            Duration::from_millis(portal_for_app.opacity_transition_ms),
            portal_for_app.secondary_transparency_percent,
            saved.camera_border_visible,
        );
        if !startup_warning_for_app.is_empty() {
            status.set_text(&format!("Предупреждение: {startup_warning_for_app}"));
        } else if std::env::var("XDG_SESSION_TYPE").as_deref() == Ok("wayland")
            && !camera_overlay.layer_shell
        {
            status.set_text("Overlay камеры недоступен: compositor не поддержал Layer Shell");
        }
        preview_controller.borrow_mut().start(
            combo_active_id(&camera_combo).filter(|id| id != "none"),
            saved.camera_quality,
            preview_tx.clone(),
        );
        {
            let preview_controller = preview_controller.clone();
            let preview_tx = preview_tx.clone();
            let quality_combo = quality_combo.clone();
            connect_combo_changed(&camera_combo, move |combo| {
                preview_controller.borrow_mut().start(
                    combo_active_id(combo).filter(|id| id != "none"),
                    selected_camera_quality(combo_active_id(&quality_combo).as_deref()),
                    preview_tx.clone(),
                );
            });
        }
        {
            let preview_controller = preview_controller.clone();
            let preview_tx = preview_tx.clone();
            let camera_combo = camera_combo.clone();
            connect_combo_changed(&quality_combo, move |combo| {
                preview_controller.borrow_mut().start(
                    combo_active_id(&camera_combo).filter(|id| id != "none"),
                    selected_camera_quality(combo_active_id(combo).as_deref()),
                    preview_tx.clone(),
                );
            });
        }
        {
            let shared_frame = shared_frame.clone();
            let camera_overlay = camera_overlay.clone();
            glib::timeout_add_local(Duration::from_millis(50), move || {
                let mut latest = None;
                while let Ok(frame) = preview_rx.try_recv() {
                    latest = Some(frame);
                }
                if let Some(frame) = latest {
                    *shared_frame.borrow_mut() = Some(frame);
                    camera_overlay.area.queue_draw();
                }
                glib::ControlFlow::Continue
            });
        }
        {
            let camera_overlay = camera_overlay.clone();
            glib::timeout_add_local(Duration::from_millis(16), move || {
                camera_overlay.tick_opacity();
                glib::ControlFlow::Continue
            });
        }
        {
            let camera_overlay = camera_overlay.clone();
            size_scale.connect_value_changed(move |scale| {
                let size = scale.value().round() as u8;
                camera_overlay.resize(size);
            });
        }
        {
            let camera_overlay = camera_overlay.clone();
            opacity_scale.connect_value_changed(move |scale| {
                let transparency = scale.value().round() as u8;
                camera_overlay.set_primary_opacity(transparency);
            });
        }
        {
            let camera_overlay = camera_overlay.clone();
            secondary_opacity_scale.connect_value_changed(move |scale| {
                let transparency = scale.value().round() as u8;
                camera_overlay.set_secondary_opacity(transparency);
            });
        }
        {
            let camera_overlay = camera_overlay.clone();
            border_check.connect_toggled(move |check| {
                camera_overlay.set_border_visible(check.is_active());
            });
        }
        {
            let output_entry = output_entry.clone();
            let parent = window.clone();
            choose_output.connect_clicked(move |_| {
                choose_output_directory(&parent, &output_entry);
            });
        }
        let action_panel = gtk::Box::new(gtk::Orientation::Vertical, 4);
        let action_overlay = gtk::Overlay::new();
        action_overlay.set_child(Some(&action_panel));
        root.append(&action_overlay);
        let controls = gtk::Box::new(gtk::Orientation::Horizontal, 16);
        controls.set_halign(gtk::Align::Center);
        action_panel.append(&controls);
        let record_button = round_control_button("START");
        record_button.add_css_class("suggested-action");
        controls.append(&record_button);
        let stop_button = round_control_button("STOP");
        stop_button.add_css_class("destructive-action");
        stop_button.set_sensitive(false);
        controls.append(&stop_button);
        let opacity_button = round_control_button("SELFIE");
        opacity_button.add_css_class("selfie-control");
        controls.append(&opacity_button);
        let status_frame = gtk::Frame::new(None);
        status_frame.set_child(Some(&status));
        action_panel.append(&status_frame);
        let mute_button = gtk::ToggleButton::with_label("MUTE");
        mute_button.set_size_request(68, 34);
        mute_button.set_halign(gtk::Align::End);
        mute_button.set_valign(gtk::Align::End);
        mute_button.set_margin_end(8);
        mute_button.set_margin_bottom(5);
        render_mute_button(&mute_button);
        action_overlay.add_overlay(&mute_button);

        let settings_content = gtk::Box::new(gtk::Orientation::Vertical, 12);
        settings_content.set_margin_top(12);
        settings_content.set_margin_bottom(12);
        settings_content.set_margin_start(12);
        settings_content.set_margin_end(12);
        let hotkey_title = gtk::Label::new(Some("Горячие клавиши"));
        hotkey_title.set_xalign(0.0);
        hotkey_title.add_css_class("heading");
        settings_content.append(&hotkey_title);
        settings_content.append(&hotkey_form);
        settings_content.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        settings_content.append(&form);

        let settings_label = gtk::Label::new(Some("Настройки"));
        settings_label.set_hexpand(true);
        settings_label.set_xalign(0.5);
        settings_label.add_css_class("dim-label");
        settings_label.set_margin_top(0);
        settings_label.set_margin_bottom(0);

        let settings_expander = gtk::Expander::new(None);
        settings_expander.set_label_widget(Some(&settings_label));
        settings_expander.set_expanded(false);
        settings_expander.set_resize_toplevel(true);
        settings_expander.set_hexpand(true);
        settings_expander.set_child(Some(&settings_content));
        root.append(&settings_expander);
        window.set_child(Some(&root));

        let (event_tx, event_rx) = mpsc::channel::<RecorderEvent>();
        let recorder = Rc::new(RefCell::new(RecorderController::new(event_tx.clone())));
        let recording_indicator = Rc::new(RefCell::new(
            None::<Rc<RecordingIndicatorOverlay>>,
        ));
        {
            let recorder = recorder.clone();
            let status = status.clone();
            mute_button.connect_toggled(move |button| {
                let muted = button.is_active();
                render_mute_button(button);
                if let Err(error) = recorder.borrow().set_microphone_muted(muted) {
                    error!(muted, error = %error, "Не удалось отправить команду Mute");
                    status.set_text(&format!("Ошибка Mute микрофона: {error:#}"));
                }
            });
        }
        {
            let recorder = recorder.clone();
            let status = status.clone();
            let record_button = record_button.clone();
            let stop_button_for_render = stop_button.clone();
            let recording_indicator = recording_indicator.clone();
            stop_button.connect_clicked(move |_| {
                let state = recorder.borrow().state();
                if !state.can_stop() {
                    return;
                }
                info!(?state, "Пользователь запросил остановку записи");
                show_stop_indicator(&recording_indicator);
                let stop_result = recorder.borrow_mut().request_stop();
                match stop_result {
                    Ok(()) => render_recorder_ui(
                        &recorder.borrow(),
                        &record_button,
                        &stop_button_for_render,
                        &status,
                    ),
                    Err(error) => {
                        error!(error = %error, "Не удалось отправить команду остановки записи");
                        status.set_text(&format!("Ошибка остановки записи: {error:#}"));
                    }
                }
            });
        }
        {
            let camera_overlay = camera_overlay.clone();
            opacity_button.connect_clicked(move |_| {
                camera_overlay.toggle_opacity();
            });
        }
        {
            let recorder = recorder.clone();
            let status = status.clone();
            let mic_combo = mic_combo.clone();
            let system_combo = system_combo.clone();
            let output_entry = output_entry.clone();
            let camera_position = saved.camera_position;
            let size_scale = size_scale.clone();
            let opacity_scale = opacity_scale.clone();
            let secondary_opacity_scale = secondary_opacity_scale.clone();
            let quality_combo = quality_combo.clone();
            let border_check = border_check.clone();
            let mute_button = mute_button.clone();
            let paths = paths_for_app.clone();
            let portal_config = portal_for_app.clone();
            let shortcut_bindings = shortcut_bindings.clone();
            record_button.connect_clicked(move |_| {
                let state = recorder.borrow().state();
                if matches!(state, RecorderState::Recording | RecorderState::Paused) {
                    if let Err(error) = recorder.borrow_mut().request_pause_or_resume() {
                        status.set_text(&format!("Ошибка команды паузы: {error:#}"));
                    }
                    return;
                }
                if !state.can_start() {
                    return;
                }
                let mic = combo_active_id(&mic_combo);
                let system = combo_active_id(&system_combo);
                let dir = PathBuf::from(output_entry.text().as_str());
                if let Err(error) = fs::create_dir_all(&dir) {
                    status.set_text(&format!("Ошибка папки записи: {error}"));
                    return;
                }
                let output = dir.join(format!(
                    "screen-{}.mp4",
                    Local::now().format("%Y%m%d-%H%M%S")
                ));
                let user = UserSettings {
                    camera_device: selected_camera_device(
                        combo_active_id(&camera_combo).as_deref(),
                    ),
                    microphone_source: mic.clone().unwrap_or_else(|| "default".into()),
                    system_source: system.clone().unwrap_or_else(|| "@DEFAULT_MONITOR@".into()),
                    output_dir: dir.to_string_lossy().into_owned(),
                    camera_position,
                    camera_size_percent: size_scale.value().round() as u8,
                    camera_quality: selected_camera_quality(
                        combo_active_id(&quality_combo).as_deref(),
                    ),
                    camera_transparency_percent: opacity_scale.value().round() as u8,
                    camera_border_visible: border_check.is_active(),
                    record_hotkey: shortcut_bindings.borrow().record.clone(),
                    stop_hotkey: shortcut_bindings.borrow().stop.clone(),
                    mute_hotkey: shortcut_bindings.borrow().mute.clone(),
                };
                if let Err(error) = write_json(&paths.settings, &user) {
                    status.set_text(&format!(
                        "Не удалось сохранить настройки перед записью: {error:#}"
                    ));
                    return;
                }
                let portal_config = PortalConfig {
                    secondary_transparency_percent: secondary_opacity_scale.value().round() as u8,
                    opacity_toggle_hotkey: shortcut_bindings.borrow().opacity.clone(),
                    ..portal_config.clone()
                };
                if let Err(error) = write_json(&paths.portal, &portal_config) {
                    status.set_text(&format!(
                        "Не удалось сохранить настройки прозрачности: {error:#}"
                    ));
                    return;
                }
                let options = RecorderOptions {
                    microphone: mic,
                    system_audio: system,
                    microphone_muted: mute_button.is_active(),
                    output,
                };
                info!(output = %options.output.display(), "Пользователь запросил запуск записи");
                match recorder.borrow_mut().start(options) {
                    Ok(session_id) => info!(?session_id, "Сессия записи принята, ожидается подтверждение Playing"),
                    Err(error) => {
                        error!(error = %error, "Не удалось создать сессию записи");
                        status.set_text(&format!("Не удалось запустить запись: {error:#}"));
                    }
                }
            });
        }
        {
            let record_button = record_button.clone();
            camera_overlay.set_record_toggle_action(move || {
                if record_button.is_sensitive() {
                    record_button.emit_clicked();
                }
            });
        }
        let event_recorder = recorder.clone();
        let record_button_events = record_button.clone();
        let stop_button_events = stop_button.clone();
        let status_events = status.clone();
        let window_events = window.clone();
        let recording_indicator_events = recording_indicator.clone();
        let shutdown_requested = Rc::new(Cell::new(false));
        let shutdown_ready = Rc::new(Cell::new(false));
        let close_timeout_started = Rc::new(Cell::new(false));
        let shutdown_requested_events = shutdown_requested.clone();
        let shutdown_ready_events = shutdown_ready.clone();
        glib::timeout_add_local(Duration::from_millis(50), move || {
            while let Ok(event) = event_rx.try_recv() {
                debug!(event = ?event, "GTK event timer получил событие recorder");
                let Some(event) = event_recorder.borrow_mut().apply_event(event) else {
                    debug!("GTK event timer проигнорировал запоздалое или недопустимое событие recorder");
                    continue;
                };
                debug!(event = ?event, "Получено событие recorder");
                match event {
                    RecorderEvent::StateChanged { .. } | RecorderEvent::Started { .. } => {
                        render_recorder_ui(
                            &event_recorder.borrow(),
                            &record_button_events,
                            &stop_button_events,
                            &status_events,
                        );
                    }
                    RecorderEvent::Prepared { .. } => {
                        close_recording_indicator(&recording_indicator_events);
                        let indicator = create_recording_indicator_overlay();
                        if std::env::var("XDG_SESSION_TYPE").as_deref() == Ok("wayland")
                            && !indicator.layer_shell
                        {
                            warn!("Layer Shell недоступен для экранного индикатора записи");
                        }
                        *recording_indicator_events.borrow_mut() = Some(indicator.clone());
                        status_events.set_text("Запись начнется через 2 секунды…");
                        let recorder = event_recorder.clone();
                        let status = status_events.clone();
                        indicator.play_countdown(move || {
                            status.set_text("Запуск записи…");
                            if let Err(error) = recorder.borrow().request_begin_recording() {
                                error!(error = %error, "Не удалось продолжить запуск после отсчёта");
                                status.set_text(&format!(
                                    "Не удалось начать запись после отсчёта: {error:#}"
                                ));
                            }
                        });
                    }
                    RecorderEvent::Paused { .. } => {
                        render_recorder_ui(
                            &event_recorder.borrow(),
                            &record_button_events,
                            &stop_button_events,
                            &status_events,
                        );
                        if let Some(indicator) = recording_indicator_events.borrow().as_ref() {
                            indicator.show_paused();
                        }
                    }
                    RecorderEvent::Resumed { .. } => {
                        render_recorder_ui(
                            &event_recorder.borrow(),
                            &record_button_events,
                            &stop_button_events,
                            &status_events,
                        );
                        if let Some(indicator) = recording_indicator_events.borrow().as_ref() {
                            indicator.show_resumed();
                        }
                    }
                    RecorderEvent::Error { message, .. } => {
                        close_recording_indicator_unless_stop(&recording_indicator_events);
                        error!(message = %message, "Recorder завершился с ошибкой");
                        render_recorder_ui(
                            &event_recorder.borrow(),
                            &record_button_events,
                            &stop_button_events,
                            &status_events,
                        );
                        status_events.set_text(&message);
                    }
                    RecorderEvent::Cancelled { .. } => {
                        close_recording_indicator_unless_stop(&recording_indicator_events);
                        info!("Запуск или запись отменены");
                        render_recorder_ui(
                            &event_recorder.borrow(),
                            &record_button_events,
                            &stop_button_events,
                            &status_events,
                        );
                        status_events.set_text("Запись отменена");
                    }
                    RecorderEvent::Stopped { path, .. } => {
                        close_recording_indicator_unless_stop(&recording_indicator_events);
                        info!(path = %path.display(), "Запись успешно сохранена");
                        render_recorder_ui(
                            &event_recorder.borrow(),
                            &record_button_events,
                            &stop_button_events,
                            &status_events,
                        );
                        status_events.set_text(&format!("Файл сохранен: {}", path.display()));
                    }
                }
                if shutdown_requested_events.get() && !event_recorder.borrow().has_active_session()
                {
                    shutdown_ready_events.set(true);
                    window_events.close();
                }
            }
            glib::ControlFlow::Continue
        });
        {
            let shortcut_command_rx = shortcut_command_rx.clone();
            let record_button = record_button.clone();
            let stop_button = stop_button.clone();
            let camera_overlay = camera_overlay.clone();
            let mute_button = mute_button.clone();
            glib::timeout_add_local(Duration::from_millis(100), move || {
                loop {
                    let command = shortcut_command_rx.borrow_mut().try_recv();
                    let Ok(command) = command else { break };
                    match command {
                        ApplicationCommand::ToggleRecord => record_button.emit_clicked(),
                        ApplicationCommand::StopRecord => stop_button.emit_clicked(),
                        ApplicationCommand::ToggleOpacity => camera_overlay.toggle_opacity(),
                        ApplicationCommand::ToggleMute => {
                            mute_button.set_active(!mute_button.is_active());
                        }
                    }
                }
                glib::ControlFlow::Continue
            });
        }
        {
            let recorder = recorder.clone();
            let status = status.clone();
            let shortcut_status_rx = shortcut_status_rx.clone();
            glib::timeout_add_local(Duration::from_millis(200), move || {
                loop {
                    let message = shortcut_status_rx.borrow_mut().try_recv();
                    let Ok(message) = message else { break };
                    if can_apply_auxiliary_status(recorder.borrow().state()) {
                        status.set_text(&message);
                    }
                }
                glib::ControlFlow::Continue
            });
        }
        {
            let shortcut_bindings_rx = shortcut_bindings_rx.clone();
            let record_hotkey_label = record_hotkey_label.clone();
            let stop_hotkey_label = stop_hotkey_label.clone();
            let opacity_hotkey_label = opacity_hotkey_label.clone();
            let mute_hotkey_label = mute_hotkey_label.clone();
            let shortcut_bindings = shortcut_bindings.clone();
            let paths = paths_for_app.clone();
            glib::timeout_add_local(Duration::from_millis(200), move || {
                while let Ok(update) = shortcut_bindings_rx.borrow_mut().try_recv() {
                    let bindings = update.bindings;
                    record_hotkey_label.set_label(&bindings.record);
                    stop_hotkey_label.set_label(&bindings.stop);
                    opacity_hotkey_label.set_label(&bindings.opacity);
                    mute_hotkey_label.set_label(&bindings.mute);
                    *shortcut_bindings.borrow_mut() = bindings.clone();
                    if update.persist
                        && let Err(error) = persist_shortcut_bindings(&paths, &bindings)
                    {
                        error!(error = %error, "Не удалось сохранить выбранные hotkeys");
                    }
                }
                glib::ControlFlow::Continue
            });
        }
        {
            let recorder = recorder.clone();
            let preview_controller = preview_controller.clone();
            let status = status.clone();
            let record_button = record_button.clone();
            let stop_button = stop_button.clone();
            let shutdown_requested = shutdown_requested.clone();
            let shutdown_ready = shutdown_ready.clone();
            let close_timeout_started = close_timeout_started.clone();
            let close_window = window.clone();
            let recording_indicator = recording_indicator.clone();
            window.connect_close_request(move |_| {
                close_recording_indicator(&recording_indicator);
                if shutdown_ready.get() {
                    return glib::Propagation::Proceed;
                }
                if recorder.borrow().has_active_session() {
                    shutdown_requested.set(true);
                    let stop_result = recorder.borrow_mut().request_stop();
                    match stop_result {
                        Ok(()) => render_recorder_ui(
                            &recorder.borrow(),
                            &record_button,
                            &stop_button,
                            &status,
                        ),
                        Err(error) => {
                            error!(error = %error, "Не удалось остановить recorder при закрытии окна");
                            status.set_text(&format!("Ошибка остановки записи: {error:#}"));
                        }
                    }
                    preview_controller.borrow_mut().stop();
                    if !close_timeout_started.replace(true) {
                        let timeout_window = close_window.clone();
                        let timeout_status = status.clone();
                        let timeout_recorder = recorder.clone();
                        let timeout_ready = shutdown_ready.clone();
                        glib::timeout_add_local(Duration::from_secs(10), move || {
                            if timeout_ready.get() || !timeout_recorder.borrow().has_active_session()
                            {
                                return glib::ControlFlow::Break;
                            }
                            error!(
                                "Recorder не завершился за 10 секунд при закрытии окна; выполняется принудительное закрытие"
                            );
                            timeout_status.set_text(
                                "Не удалось завершить запись за 10 секунд; окно закрывается, файл может быть поврежден",
                            );
                            timeout_ready.set(true);
                            timeout_window.close();
                            glib::ControlFlow::Break
                        });
                    }
                    return glib::Propagation::Stop;
                }
                preview_controller.borrow_mut().stop();
                glib::Propagation::Proceed
            });
        }
        window.present();
        if !shortcut_started_for_app.replace(true) {
            let configure_rx = shortcut_configure_rx_for_app.borrow_mut().take();
            if let Some(configure_rx) = configure_rx {
                let window_for_shortcuts = window.clone();
                let command_tx = shortcut_command_tx_for_app.clone();
                let status_tx = shortcut_status_tx_for_app.clone();
                let bindings_tx = shortcut_bindings_tx_for_app.clone();
                let config = portal_for_app.clone();
                let bindings = shortcut_bindings.borrow().clone();
                glib::MainContext::default().spawn_local(async move {
                    let Some(identifier) = WindowIdentifier::from_native(&window_for_shortcuts).await
                    else {
                        warn!("Не удалось получить идентификатор окна для global shortcuts");
                        if let Err(error) = status_tx.send(
                            "Глобальные клавиши недоступны: не удалось получить идентификатор окна"
                                .into(),
                        ) {
                            error!(error = %error, "Не удалось передать статус shortcuts в GTK");
                        }
                        return;
                    };
                    if let Err(error) = start_global_shortcuts(
                        ShortcutChannels {
                            command_tx,
                            status_tx: status_tx.clone(),
                            bindings_tx,
                            configure_rx,
                        },
                        config,
                        bindings.record,
                        bindings.stop,
                        bindings.mute,
                        identifier,
                    ) && let Err(send_error) =
                        status_tx.send(format!("Глобальные клавиши недоступны: {error:#}"))
                    {
                        error!(error = %send_error, "Не удалось передать статус shortcuts в GTK");
                    }
                });
            }
        }
    });
    app.run();
    Ok(())
}

fn diagnose() -> Result<()> {
    println!("LiveScreen diagnostics");
    println!(
        "  XDG_SESSION_TYPE: {}",
        std::env::var("XDG_SESSION_TYPE").unwrap_or_else(|_| "unknown".into())
    );
    println!(
        "  WAYLAND_DISPLAY: {}",
        std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "not set".into())
    );
    println!(
        "  DISPLAY: {}",
        std::env::var("DISPLAY").unwrap_or_else(|_| "not set".into())
    );
    println!("  cameras:");
    for camera in camera_devices() {
        println!("    {} — {}", camera.id, camera.label);
    }
    println!("  audio sources:");
    for source in audio_sources() {
        println!("    {} — {}", source.id, source.label);
    }
    println!("  gstreamer elements:");
    for name in [
        "pipewiresrc",
        "ximagesrc",
        "v4l2src",
        "jpegdec",
        "appsink",
        "capsfilter",
        "pulsesrc",
        "videoconvert",
        "videoscale",
        "videorate",
        "audioconvert",
        "audioresample",
        "audiomixer",
        "cairooverlay",
        "compositor",
        "x264enc",
        "h264parse",
        "voaacenc",
        "mp4mux",
    ] {
        println!("    {name}: {}", gst::ElementFactory::find(name).is_some());
    }
    println!("  portal config: {}", Paths::discover().portal.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        RecorderState, can_apply_auxiliary_status, recorder_control_label, recorder_state_status,
        selected_camera_device,
    };

    #[test]
    fn stopping_status_keeps_the_saving_contract() {
        assert_eq!(
            recorder_state_status(RecorderState::Stopping),
            "сохранение видео, ждите.."
        );
    }

    #[test]
    fn auxiliary_status_cannot_override_saving_status() {
        assert!(!can_apply_auxiliary_status(RecorderState::Stopping));
        assert!(can_apply_auxiliary_status(RecorderState::Idle));
        assert!(can_apply_auxiliary_status(RecorderState::Recording));
    }

    #[test]
    fn recorder_control_label_tracks_the_confirmed_lifecycle() {
        assert_eq!(recorder_control_label(RecorderState::Idle), "START");
        assert_eq!(recorder_control_label(RecorderState::Starting), "START…");
        assert_eq!(recorder_control_label(RecorderState::Recording), "PAUSE");
        assert_eq!(recorder_control_label(RecorderState::Paused), "RESUME");
        assert_eq!(recorder_control_label(RecorderState::Stopping), "SAVING…");
        assert_eq!(recorder_control_label(RecorderState::Error), "START");
    }

    #[test]
    fn selected_camera_device_uses_current_active_id() {
        assert_eq!(selected_camera_device(Some("/dev/video2")), "/dev/video2");
    }

    #[test]
    fn selected_camera_device_preserves_missing_camera_entry() {
        assert_eq!(selected_camera_device(Some("none")), "none");
        assert_eq!(selected_camera_device(None), "none");
    }
}
