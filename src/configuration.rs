use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tracing::{error, info, warn};

pub const DEFAULT_RECORD_HOTKEY: &str = "Ctrl+Alt+R";
pub const DEFAULT_STOP_HOTKEY: &str = "Ctrl+Alt+F";
pub const DEFAULT_OPACITY_HOTKEY: &str = "Ctrl+Alt+O";
pub const DEFAULT_MUTE_HOTKEY: &str = "Ctrl+Alt+M";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortalConfig {
    #[serde(default = "default_secondary_transparency")]
    pub secondary_transparency_percent: u8,
    #[serde(default = "default_transition_ms")]
    pub opacity_transition_ms: u64,
    #[serde(default = "default_opacity_hotkey")]
    pub opacity_toggle_hotkey: String,
}

impl Default for PortalConfig {
    fn default() -> Self {
        Self {
            secondary_transparency_percent: 100,
            opacity_transition_ms: 2_000,
            opacity_toggle_hotkey: DEFAULT_OPACITY_HOTKEY.to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserSettings {
    #[serde(default = "default_camera")]
    pub camera_device: String,
    #[serde(default = "default_mic")]
    pub microphone_source: String,
    #[serde(default = "default_system")]
    pub system_source: String,
    #[serde(default = "default_output_dir")]
    pub output_dir: String,
    #[serde(default = "default_position")]
    pub camera_position: CameraPosition,
    #[serde(default = "default_size")]
    pub camera_size_percent: u8,
    #[serde(default = "default_camera_quality")]
    pub camera_quality: CameraQuality,
    #[serde(default = "default_transparency")]
    pub camera_transparency_percent: u8,
    #[serde(default = "default_camera_border_visible")]
    pub camera_border_visible: bool,
    #[serde(default = "default_record_hotkey")]
    pub record_hotkey: String,
    #[serde(default = "default_stop_hotkey")]
    pub stop_hotkey: String,
    #[serde(default = "default_mute_hotkey")]
    pub mute_hotkey: String,
}

impl Default for UserSettings {
    fn default() -> Self {
        Self {
            camera_device: default_camera(),
            microphone_source: default_mic(),
            system_source: default_system(),
            output_dir: default_output_dir(),
            camera_position: default_position(),
            camera_size_percent: default_size(),
            camera_quality: default_camera_quality(),
            camera_transparency_percent: default_transparency(),
            camera_border_visible: default_camera_border_visible(),
            record_hotkey: default_record_hotkey(),
            stop_hotkey: default_stop_hotkey(),
            mute_hotkey: default_mute_hotkey(),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum CameraPosition {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum CameraQuality {
    Best,
    #[default]
    Optim,
    Fast,
}

impl CameraQuality {
    pub fn label(self) -> &'static str {
        match self {
            Self::Best => "Best",
            Self::Optim => "Optim",
            Self::Fast => "Fast",
        }
    }
}

fn default_secondary_transparency() -> u8 {
    100
}

fn default_transition_ms() -> u64 {
    2_000
}

fn default_opacity_hotkey() -> String {
    DEFAULT_OPACITY_HOTKEY.to_string()
}

fn default_record_hotkey() -> String {
    DEFAULT_RECORD_HOTKEY.to_string()
}

fn default_stop_hotkey() -> String {
    DEFAULT_STOP_HOTKEY.to_string()
}

fn default_mute_hotkey() -> String {
    DEFAULT_MUTE_HOTKEY.to_string()
}

fn default_camera() -> String {
    "/dev/video0".to_string()
}

fn default_mic() -> String {
    "default".to_string()
}

fn default_system() -> String {
    "@DEFAULT_MONITOR@".to_string()
}

fn default_output_dir() -> String {
    dirs::video_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."))
        .to_string_lossy()
        .into_owned()
}

fn default_position() -> CameraPosition {
    CameraPosition::TopRight
}

fn default_size() -> u8 {
    20
}

fn default_camera_quality() -> CameraQuality {
    CameraQuality::Optim
}

fn default_transparency() -> u8 {
    0
}

fn default_camera_border_visible() -> bool {
    true
}

pub trait Validatable {
    fn validate(&self) -> Result<()>;
}

impl Validatable for PortalConfig {
    fn validate(&self) -> Result<()> {
        if self.secondary_transparency_percent > 100 {
            return Err(anyhow!(
                "вторичная прозрачность должна быть в диапазоне 0..=100"
            ));
        }
        if self.opacity_transition_ms > 60_000 {
            return Err(anyhow!(
                "длительность перехода прозрачности должна быть в диапазоне 0..=60000 мс"
            ));
        }
        validate_non_empty("горячая клавиша прозрачности", &self.opacity_toggle_hotkey)
    }
}

impl Validatable for UserSettings {
    fn validate(&self) -> Result<()> {
        if !(5..=50).contains(&self.camera_size_percent) {
            return Err(anyhow!(
                "размер камеры должен быть в диапазоне 5..=50 процентов"
            ));
        }
        if self.camera_transparency_percent > 100 {
            return Err(anyhow!(
                "прозрачность камеры должна быть в диапазоне 0..=100"
            ));
        }
        validate_non_empty("горячая клавиша записи", &self.record_hotkey)?;
        validate_non_empty("горячая клавиша остановки записи", &self.stop_hotkey)?;
        validate_non_empty("горячая клавиша микрофона", &self.mute_hotkey)?;
        validate_non_empty("папка записи", &self.output_dir)
    }
}

fn validate_non_empty(field: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        return Err(anyhow!("{field} не может быть пустой"));
    }
    Ok(())
}

#[derive(Debug)]
pub struct LoadedJson<T> {
    pub value: T,
    pub warning: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Paths {
    pub portal: PathBuf,
    pub settings: PathBuf,
}

impl Paths {
    pub fn discover() -> Self {
        let config_dir = dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("livescreen");
        Self {
            portal: config_dir.join("config.json"),
            settings: config_dir.join("settings.json"),
        }
    }
}

pub fn read_json<T>(path: &Path) -> Result<LoadedJson<T>>
where
    T: DeserializeOwned + Default + Validatable,
{
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            info!(path = %path.display(), "Файл конфигурации отсутствует, используются значения по умолчанию");
            return Ok(LoadedJson {
                value: T::default(),
                warning: None,
            });
        }
        Err(error) => {
            return Err(error).with_context(|| format!("не удалось прочитать {}", path.display()));
        }
    };

    let value = match serde_json::from_str::<T>(&text) {
        Ok(value) => value,
        Err(error) => {
            warn!(path = %path.display(), error = %error, "Не удалось разобрать JSON конфигурации, используются значения по умолчанию");
            return Ok(LoadedJson {
                value: T::default(),
                warning: Some(format!(
                    "Не удалось разобрать {}: {error}; применены значения по умолчанию",
                    path.display()
                )),
            });
        }
    };
    if let Err(error) = value.validate() {
        return Ok(LoadedJson {
            value: T::default(),
            warning: Some(format!(
                "Некорректные настройки {}: {error:#}; применены значения по умолчанию",
                path.display()
            )),
        });
    }
    Ok(LoadedJson {
        value,
        warning: None,
    })
}

pub fn write_json<T>(path: &Path, value: &T) -> Result<()>
where
    T: Serialize + Validatable,
{
    value
        .validate()
        .with_context(|| format!("некорректные данные для {}", path.display()))?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("не удалось создать каталог {}", parent.display()))?;
    }
    let text = format!("{}\n", serde_json::to_string_pretty(value)?);
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow!("путь настроек не содержит имени файла: {}", path.display()))?
        .to_string_lossy();
    let temp_path = create_temp_file(parent, &file_name, text.as_bytes())?;
    let rename_result = fs::rename(&temp_path, path).with_context(|| {
        format!(
            "не удалось заменить {} временным файлом {}",
            path.display(),
            temp_path.display()
        )
    });
    if let Err(_error) = &rename_result
        && let Err(cleanup_error) = fs::remove_file(&temp_path)
    {
        error!(
            path = %temp_path.display(),
            error = %cleanup_error,
            "Не удалось удалить временный файл после ошибки записи"
        );
    }
    rename_result
}

fn create_temp_file(parent: &Path, file_name: &str, contents: &[u8]) -> Result<PathBuf> {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("системные часы показывают время до Unix epoch")?
        .as_nanos();
    for attempt in 0..100u32 {
        let candidate = parent.join(format!(
            ".{file_name}.tmp-{}-{nonce}-{attempt}",
            std::process::id()
        ));
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&candidate);
        let mut file = match file {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("не удалось создать временный файл {}", candidate.display())
                });
            }
        };
        if let Err(error) = write_and_sync(&mut file, contents) {
            if let Err(cleanup_error) = fs::remove_file(&candidate) {
                error!(
                    path = %candidate.display(),
                    error = %cleanup_error,
                    "Не удалось удалить временный файл после ошибки записи"
                );
            }
            return Err(error).with_context(|| {
                format!("не удалось записать временный файл {}", candidate.display())
            });
        }
        return Ok(candidate);
    }
    Err(anyhow!(
        "не удалось подобрать свободное имя временного файла для {}",
        parent.join(file_name).display()
    ))
}

fn write_and_sync(file: &mut File, contents: &[u8]) -> std::io::Result<()> {
    file.write_all(contents)?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn test_dir(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "livescreen-configuration-{name}-{}",
            std::process::id()
        ));
        if path.exists() {
            fs::remove_dir_all(&path).expect("test directory should be removable");
        }
        fs::create_dir_all(&path).expect("test directory should be created");
        path
    }

    #[test]
    fn missing_json_returns_defaults_without_warning() {
        let path = test_dir("missing").join("settings.json");
        let loaded = read_json::<UserSettings>(&path).expect("missing settings should load");
        assert_eq!(loaded.value.camera_size_percent, 20);
        assert_eq!(loaded.value.camera_quality, CameraQuality::Optim);
        assert!(loaded.value.camera_border_visible);
        assert_eq!(loaded.value.stop_hotkey, DEFAULT_STOP_HOTKEY);
        assert_eq!(loaded.value.mute_hotkey, DEFAULT_MUTE_HOTKEY);
        assert!(loaded.warning.is_none());
    }

    #[test]
    fn old_json_keeps_defaults_for_new_fields() {
        let path = test_dir("old").join("config.json");
        fs::write(&path, r#"{"opacity_toggle_hotkey":"Ctrl+Alt+O"}"#)
            .expect("old config should be written");
        let loaded = read_json::<PortalConfig>(&path).expect("old config should load");
        assert_eq!(loaded.value.opacity_transition_ms, 2_000);
        assert!(loaded.warning.is_none());
    }

    #[test]
    fn old_settings_keep_new_defaults() {
        let path = test_dir("old-settings").join("settings.json");
        fs::write(&path, r#"{"record_hotkey":"F3","stop_hotkey":"F4"}"#)
            .expect("old settings should be written");
        let loaded = read_json::<UserSettings>(&path).expect("old settings should load");
        assert_eq!(loaded.value.mute_hotkey, DEFAULT_MUTE_HOTKEY);
        assert!(loaded.value.camera_border_visible);
        assert!(loaded.warning.is_none());
    }

    #[test]
    fn malformed_json_returns_defaults_and_warning() {
        let path = test_dir("malformed").join("settings.json");
        fs::write(&path, "not json").expect("malformed config should be written");
        let loaded = read_json::<UserSettings>(&path).expect("malformed config should fallback");
        assert_eq!(loaded.value.camera_size_percent, 20);
        assert!(loaded.warning.is_some());
    }

    #[test]
    fn invalid_values_return_defaults_and_warning() {
        let path = test_dir("invalid").join("config.json");
        fs::write(
            &path,
            r#"{"secondary_transparency_percent":20,"opacity_transition_ms":60001,"opacity_toggle_hotkey":"Ctrl+Alt+O"}"#,
        )
        .expect("invalid config should be written");
        let loaded = read_json::<PortalConfig>(&path).expect("invalid config should fallback");
        assert_eq!(loaded.value.opacity_transition_ms, 2_000);
        assert!(loaded.warning.is_some());
    }

    #[test]
    fn invalid_user_ranges_and_empty_fields_return_defaults_and_warning() {
        let path = test_dir("invalid-user").join("settings.json");
        fs::write(
            &path,
            r#"{"output_dir":" ","camera_size_percent":4,"camera_transparency_percent":101,"record_hotkey":""}"#,
        )
        .expect("invalid settings should be written");
        let loaded = read_json::<UserSettings>(&path).expect("invalid settings should fallback");
        assert_eq!(loaded.value.camera_size_percent, 20);
        assert_eq!(loaded.value.stop_hotkey, DEFAULT_STOP_HOTKEY);
        assert_eq!(loaded.value.mute_hotkey, DEFAULT_MUTE_HOTKEY);
        assert!(loaded.warning.is_some());
    }

    #[test]
    fn atomic_write_can_be_read_back_without_leftover_temp_files() {
        let dir = test_dir("atomic");
        let path = dir.join("settings.json");
        let settings = UserSettings {
            camera_border_visible: false,
            ..UserSettings::default()
        };
        write_json(&path, &settings).expect("settings should be written");
        let loaded = read_json::<UserSettings>(&path).expect("settings should be readable");
        assert_eq!(loaded.value.output_dir, settings.output_dir);
        assert!(!loaded.value.camera_border_visible);
        let leftovers = fs::read_dir(&dir)
            .expect("test directory should be readable")
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp-"))
            .count();
        assert_eq!(leftovers, 0);
    }

    #[test]
    fn atomic_write_rejects_invalid_settings_before_replacing_file() {
        let dir = test_dir("invalid-write");
        let path = dir.join("settings.json");
        let settings = UserSettings {
            camera_size_percent: 4,
            ..UserSettings::default()
        };
        assert!(write_json(&path, &settings).is_err());
        assert!(!path.exists());
    }
}
