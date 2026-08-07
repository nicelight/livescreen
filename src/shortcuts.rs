use std::{sync::mpsc::Sender, thread};

use anyhow::{Context, Result};
use ashpd::{
    WindowIdentifier,
    desktop::global_shortcuts::{GlobalShortcuts, NewShortcut, Shortcut, ShortcutsChanged},
};
use futures_util::StreamExt;
use tracing::{debug, error, info, warn};

use crate::{configuration::PortalConfig, portal::portal_connection};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplicationCommand {
    ToggleRecord,
    StopRecord,
    ToggleOpacity,
    ToggleMute,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShortcutRequest {
    Configure,
}

pub struct ShortcutChannels {
    pub command_tx: Sender<ApplicationCommand>,
    pub status_tx: Sender<String>,
    pub bindings_tx: Sender<ShortcutUpdate>,
    pub configure_rx: tokio::sync::mpsc::Receiver<ShortcutRequest>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShortcutBindings {
    pub record: String,
    pub stop: String,
    pub opacity: String,
    pub mute: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShortcutUpdate {
    pub bindings: ShortcutBindings,
    pub persist: bool,
}

impl ShortcutBindings {
    fn from_shortcuts(shortcuts: &[Shortcut], fallback: &Self) -> Self {
        let mut bindings = fallback.clone();
        for shortcut in shortcuts {
            let trigger = shortcut.trigger_description();
            if trigger.trim().is_empty() {
                continue;
            }
            match shortcut.id() {
                "record" => bindings.record = trigger.to_owned(),
                "stop" => bindings.stop = trigger.to_owned(),
                "opacity" => bindings.opacity = trigger.to_owned(),
                "mute" => bindings.mute = trigger.to_owned(),
                _ => {}
            }
        }
        bindings
    }
}

pub fn start_global_shortcuts(
    channels: ShortcutChannels,
    config: PortalConfig,
    record_hotkey: String,
    stop_hotkey: String,
    mute_hotkey: String,
    identifier: WindowIdentifier,
) -> Result<()> {
    thread::Builder::new()
        .name("livescreen-shortcuts".into())
        .spawn(move || {
            let ShortcutChannels {
                command_tx: event_tx,
                status_tx,
                bindings_tx,
                mut configure_rx,
            } = channels;
            info!("Запуск потока global shortcuts");
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    error!(error = %error, "Не удалось создать runtime global shortcuts");
                    send_status(&status_tx, format!("Hotkey runtime error: {error}"));
                    return;
                }
            };
            runtime.block_on(async move {
                let result = async {
                    let connection = portal_connection().await?;
                    let portal = GlobalShortcuts::with_connection(connection).await?;
                    let session = portal.create_session(Default::default()).await?;
                    let existing = portal
                        .list_shortcuts(&session, Default::default())
                        .await?
                        .response()?;
                    let has_stale_empty_bindings = !existing.shortcuts().is_empty()
                        && existing
                            .shortcuts()
                            .iter()
                            .all(|shortcut| shortcut.trigger_description().is_empty());
                    let session = if has_stale_empty_bindings {
                        warn!(
                            shortcuts = ?existing
                                .shortcuts()
                                .iter()
                                .map(|shortcut| shortcut.id())
                                .collect::<Vec<_>>(),
                            "Обнаружены старые пустые bindings global shortcuts, выполняется повторная регистрация"
                        );
                        portal
                            .bind_shortcuts(&session, &[], Some(&identifier), Default::default())
                            .await?
                            .response()?;
                        drop(session);
                        portal.create_session(Default::default()).await?
                    } else {
                        session
                    };
                    let mut current_bindings = ShortcutBindings {
                        record: record_hotkey.to_uppercase(),
                        stop: stop_hotkey.to_uppercase(),
                        opacity: config.opacity_toggle_hotkey.to_uppercase(),
                        mute: mute_hotkey.to_uppercase(),
                    };
                    info!(
                        record_hotkey = %current_bindings.record,
                        stop_hotkey = %current_bindings.stop,
                        opacity_hotkey = %current_bindings.opacity,
                        mute_hotkey = %current_bindings.mute,
                        "Регистрация global shortcuts"
                    );
                    let bindings = [
                        NewShortcut::new("record", "Запуск и пауза записи")
                            .preferred_trigger(Some(current_bindings.record.as_str())),
                        NewShortcut::new("stop", "Остановка записи")
                            .preferred_trigger(Some(current_bindings.stop.as_str())),
                        NewShortcut::new("opacity", "Переключение прозрачности камеры")
                            .preferred_trigger(Some(current_bindings.opacity.as_str())),
                        NewShortcut::new("mute", "Включение и выключение микрофона")
                            .preferred_trigger(Some(current_bindings.mute.as_str())),
                    ];
                    let mut activated = portal.receive_activated().await?;
                    let mut changed = portal.receive_shortcuts_changed().await?;
                    let bound = portal
                        .bind_shortcuts(&session, &bindings, Some(&identifier), Default::default())
                        .await?
                        .response()?;
                    current_bindings =
                        ShortcutBindings::from_shortcuts(bound.shortcuts(), &current_bindings);
                    let bound = describe_bindings(bound.shortcuts());
                    info!(shortcuts = ?bound, "Global shortcuts приняты portal");
                    send_bindings(
                        &bindings_tx,
                        ShortcutUpdate {
                            bindings: current_bindings.clone(),
                            persist: false,
                        },
                    );
                    if bound.iter().all(|shortcut| !shortcut.ends_with('=')) {
                        info!("Global shortcuts подключены к portal");
                        send_status(&status_tx, "Глобальные горячие клавиши подключены".into());
                    } else {
                        warn!(
                            shortcuts = ?bound,
                            "Portal не назначил комбинации global shortcuts"
                        );
                        send_status(
                            &status_tx,
                            "Hotkeys не назначены: подтвердите их в окне Global Shortcuts KDE"
                                .into(),
                        );
                    }
                    loop {
                        tokio::select! {
                            Some(event) = activated.next() => {
                                debug!(shortcut_id = %event.shortcut_id(), "Получено событие global shortcut");
                                let Some(command) = command_for_shortcut_id(event.shortcut_id()) else {
                                    warn!(shortcut_id = %event.shortcut_id(), "Portal прислал неизвестный global shortcut");
                                    continue;
                                };
                                if let Err(error) = event_tx.send(command) {
                                    warn!(error = %error, "Канал команд global shortcuts закрыт");
                                    send_status(
                                        &status_tx,
                                        format!("Канал горячих клавиш закрыт: {error}"),
                                    );
                                    break;
                                }
                            }
                            Some(request) = configure_rx.recv() => {
                                match request {
                                    ShortcutRequest::Configure => {
                                        info!("Запрос на открытие стандартного окна настройки hotkeys");
                                        if let Err(error) = portal
                                            .configure_shortcuts(&session, Some(&identifier), Default::default())
                                            .await
                                        {
                                            warn!(error = %error, "Не удалось открыть окно настройки hotkeys");
                                            send_status(
                                                &status_tx,
                                                format!("Не удалось открыть настройки hotkeys: {error}"),
                                            );
                                        } else {
                                            match portal.list_shortcuts(&session, Default::default()).await {
                                                Ok(request) => match request.response() {
                                                    Ok(shortcuts) => {
                                                        current_bindings = ShortcutBindings::from_shortcuts(
                                                            shortcuts.shortcuts(),
                                                            &current_bindings,
                                                        );
                                                        send_bindings(
                                                            &bindings_tx,
                                                            ShortcutUpdate {
                                                                bindings: current_bindings.clone(),
                                                                persist: true,
                                                            },
                                                        );
                                                    }
                                                    Err(error) => {
                                                        warn!(error = %error, "Не удалось прочитать hotkeys после настройки");
                                                        send_status(
                                                            &status_tx,
                                                            format!("Не удалось прочитать настройки hotkeys: {error}"),
                                                        );
                                                    }
                                                },
                                                Err(error) => {
                                                    warn!(error = %error, "Не удалось запросить hotkeys после настройки");
                                                    send_status(
                                                        &status_tx,
                                                        format!("Не удалось обновить настройки hotkeys: {error}"),
                                                    );
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            Some(event) = changed.next() => {
                                current_bindings = update_bindings_from_change(event, &current_bindings);
                                info!(
                                    record_hotkey = %current_bindings.record,
                                    stop_hotkey = %current_bindings.stop,
                                    opacity_hotkey = %current_bindings.opacity,
                                    mute_hotkey = %current_bindings.mute,
                                    "Portal обновил global shortcuts"
                                );
                                send_bindings(
                                    &bindings_tx,
                                    ShortcutUpdate {
                                        bindings: current_bindings.clone(),
                                        persist: false,
                                    },
                                );
                            }
                            else => break,
                        }
                    }
                    Ok::<(), ashpd::Error>(())
                }
                .await;
                if let Err(error) = result {
                    error!(error = %error, "Global shortcuts завершились с ошибкой");
                    send_status(
                        &status_tx,
                        format!("Глобальные клавиши недоступны: {error}"),
                    );
                }
            });
        })
        .context("не удалось запустить поток global shortcuts")?;
    Ok(())
}

fn command_for_shortcut_id(id: &str) -> Option<ApplicationCommand> {
    match id {
        "record" => Some(ApplicationCommand::ToggleRecord),
        "stop" => Some(ApplicationCommand::StopRecord),
        "opacity" => Some(ApplicationCommand::ToggleOpacity),
        "mute" => Some(ApplicationCommand::ToggleMute),
        _ => None,
    }
}

fn update_bindings_from_change(
    event: ShortcutsChanged,
    fallback: &ShortcutBindings,
) -> ShortcutBindings {
    ShortcutBindings::from_shortcuts(event.shortcuts(), fallback)
}

fn describe_bindings(shortcuts: &[Shortcut]) -> Vec<String> {
    shortcuts
        .iter()
        .map(|shortcut| format!("{}={}", shortcut.id(), shortcut.trigger_description()))
        .collect()
}

fn send_bindings(bindings_tx: &Sender<ShortcutUpdate>, update: ShortcutUpdate) {
    if let Err(error) = bindings_tx.send(update) {
        debug!(error = %error, "Канал обновлений global shortcuts закрыт");
    }
}

fn send_status(status_tx: &Sender<String>, message: String) {
    if let Err(error) = status_tx.send(message) {
        error!(error = %error, "Не удалось передать статус global shortcuts в GTK");
    }
}

#[cfg(test)]
mod tests {
    use super::{ApplicationCommand, command_for_shortcut_id};

    #[test]
    fn shortcut_ids_map_to_distinct_application_commands() {
        assert_eq!(
            command_for_shortcut_id("record"),
            Some(ApplicationCommand::ToggleRecord)
        );
        assert_eq!(
            command_for_shortcut_id("stop"),
            Some(ApplicationCommand::StopRecord)
        );
        assert_eq!(
            command_for_shortcut_id("opacity"),
            Some(ApplicationCommand::ToggleOpacity)
        );
        assert_eq!(
            command_for_shortcut_id("mute"),
            Some(ApplicationCommand::ToggleMute)
        );
        assert_eq!(command_for_shortcut_id("unknown"), None);
    }
}
