use std::{fs, os::fd::OwnedFd, path::PathBuf};

use anyhow::{Context, Result, anyhow};
use ashpd::desktop::{
    Session,
    screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType},
};
use tracing::{debug, info};

pub const APP_ID: &str = "com.serg.livescreen";
const DESKTOP_ENTRY: &str = include_str!("../data/com.serg.livescreen.desktop");

pub struct PortalStream {
    pub node_id: u32,
    pub fd: OwnedFd,
    pub width: i32,
    pub height: i32,
    _session: Session<Screencast>,
}

pub fn ensure_desktop_entry() -> Result<()> {
    let Some(data_dir) = dirs::data_dir() else {
        debug!("Каталог данных пользователя недоступен, desktop entry не создаётся");
        return Ok(());
    };
    let path = data_dir
        .join("applications")
        .join("com.serg.livescreen.desktop");
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("не удалось определить каталог desktop entry"))?;
    fs::create_dir_all(parent)
        .with_context(|| format!("не удалось создать каталог {}", parent.display()))?;
    let executable = if let Some(appimage) = std::env::var_os("APPIMAGE") {
        PathBuf::from(appimage)
    } else {
        std::env::current_exe().context("не удалось определить executable LiveScreen")?
    };
    let executable = executable
        .to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    let desktop_entry = DESKTOP_ENTRY
        .replace("Exec=livescreen", &format!("Exec={executable}"))
        .replace(
            "StartupNotify=true",
            &format!("TryExec={executable}\nStartupNotify=true"),
        );
    fs::write(&path, desktop_entry)
        .with_context(|| format!("не удалось записать desktop entry {}", path.display()))?;
    info!(path = %path.display(), "Desktop entry записан");
    Ok(())
}

pub async fn portal_stream() -> Result<PortalStream> {
    info!("Начало Wayland ScreenCast portal startup");
    let connection = portal_connection().await?;
    let proxy = Screencast::with_connection(connection)
        .await
        .context("не удалось подключить ScreenCast portal")?;
    debug!("Подключение к Wayland ScreenCast portal установлено");
    let session = proxy
        .create_session(Default::default())
        .await
        .context("не удалось создать portal session")?;
    debug!("Portal session создана");
    let select_request = proxy
        .select_sources(
            &session,
            SelectSourcesOptions::default()
                .set_cursor_mode(CursorMode::Embedded)
                .set_sources(SourceType::Monitor | SourceType::Window | SourceType::Virtual)
                .set_multiple(false)
                .set_persist_mode(ashpd::desktop::PersistMode::DoNot),
        )
        .await
        .context("не удалось выбрать источник экрана")?;
    info!("Диалог выбора источника экрана открыт");
    select_request
        .response()
        .context("не удалось подтвердить выбор источника экрана")?;
    info!("Источник экрана выбран в Wayland portal");
    info!("Запрос запуска Wayland ScreenCast session отправлен");
    let start_request = proxy
        .start(&session, None, Default::default())
        .await
        .context("не удалось запустить Wayland ScreenCast session")?;
    debug!("Ожидание ответа запуска Wayland ScreenCast session");
    let streams = start_request
        .response()
        .context("не удалось получить результат запуска Wayland ScreenCast session")?;
    info!("Wayland ScreenCast session запущена, открывается PipeWire remote FD");
    let stream = streams
        .streams()
        .first()
        .cloned()
        .ok_or_else(|| anyhow!("portal не вернул монитор"))?;
    let (width, height) = stream.size().unwrap_or((1920, 1080));
    let fd = proxy
        .open_pipe_wire_remote(&session, Default::default())
        .await
        .context("не удалось открыть PipeWire remote FD")?;
    debug!("PipeWire remote FD открыт");
    info!(
        node = stream.pipe_wire_node_id(),
        width, height, "Wayland portal вернул PipeWire stream"
    );
    Ok(PortalStream {
        node_id: stream.pipe_wire_node_id(),
        fd,
        width: width.max(320),
        height: height.max(240),
        _session: session,
    })
}

pub async fn portal_connection() -> std::result::Result<zbus::Connection, ashpd::Error> {
    let connection = zbus::Connection::session().await?;
    ashpd::register_host_app_with_connection(connection.clone(), APP_ID.parse()?).await?;
    Ok(connection)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_id_matches_desktop_entry_contract() {
        assert!(DESKTOP_ENTRY.contains(APP_ID));
    }
}
