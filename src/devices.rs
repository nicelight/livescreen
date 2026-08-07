use std::{fs, process::Command};

#[derive(Debug, Clone)]
pub struct CameraDevice {
    pub id: String,
    pub label: String,
}

#[derive(Debug, Clone)]
pub struct AudioSource {
    pub id: String,
    pub label: String,
    pub monitor: bool,
}

pub fn camera_devices() -> Vec<CameraDevice> {
    let mut devices = fs::read_dir("/dev")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            name.strip_prefix("video")
                .and_then(|number| number.parse::<u32>().ok())
                .map(|_| entry.path())
        })
        .filter(|path| {
            let result = Command::new("v4l2-ctl")
                .arg(format!("--device={}", path.display()))
                .arg("--list-formats-ext")
                .output();
            match result {
                Ok(output) => {
                    output.status.success()
                        && String::from_utf8_lossy(&output.stdout).contains("Size")
                }
                Err(_) => true,
            }
        })
        .map(|path| {
            let id = path.to_string_lossy().into_owned();
            let sys_name = path
                .file_name()
                .map(|name| format!("/sys/class/video4linux/{}/name", name.to_string_lossy()));
            let label = sys_name
                .and_then(|path| fs::read_to_string(path).ok())
                .map(|name| name.trim().to_string())
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| id.clone());
            CameraDevice { id, label }
        })
        .collect::<Vec<_>>();
    devices.sort_by(|left, right| left.id.cmp(&right.id));
    devices
}

pub fn audio_sources() -> Vec<AudioSource> {
    let mut result = vec![
        AudioSource {
            id: "default".into(),
            label: "Микрофон по умолчанию".into(),
            monitor: false,
        },
        AudioSource {
            id: "@DEFAULT_MONITOR@".into(),
            label: "Звук системы по умолчанию".into(),
            monitor: true,
        },
    ];
    let output = Command::new("pactl")
        .args(["list", "short", "sources"])
        .output();
    if let Ok(output) = output {
        let text = String::from_utf8_lossy(&output.stdout);
        for line in text.lines() {
            let fields = line.split('\t').collect::<Vec<_>>();
            if fields.len() < 2 {
                continue;
            }
            let id = fields[1].trim();
            if id.is_empty() || result.iter().any(|source| source.id == id) {
                continue;
            }
            let monitor = id.ends_with(".monitor");
            result.push(AudioSource {
                id: id.to_string(),
                label: if monitor {
                    format!("Система: {id}")
                } else {
                    format!("Микрофон: {id}")
                },
                monitor,
            });
        }
    }
    result
}
