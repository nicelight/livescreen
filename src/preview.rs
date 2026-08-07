use std::{
    sync::mpsc::{self, Receiver, Sender},
    thread,
};

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use tracing::{debug, error, info, warn};

use crate::configuration::CameraQuality;
use crate::gst_utils::gst_quote;

#[derive(Debug)]
pub struct PreviewFrame {
    pub data: Vec<u8>,
    pub width: i32,
    pub height: i32,
    pub stride: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PreviewProfile {
    capture_width: i32,
    capture_height: i32,
    output_width: i32,
    output_height: i32,
    framerate: i32,
}

#[derive(Default)]
pub struct PreviewController {
    stop_tx: Option<Sender<()>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl PreviewController {
    pub fn start(
        &mut self,
        device: Option<String>,
        quality: CameraQuality,
        frame_tx: Sender<PreviewFrame>,
    ) {
        let previous_thread = self.request_stop_and_take_thread();
        let Some(device) = device else {
            if let Some(previous_thread) = previous_thread {
                reap_preview_thread(previous_thread);
            }
            info!("Превью камеры не запускается: устройство не выбрано");
            return;
        };
        let profile = preview_profile(quality);
        info!(
            device = %device,
            quality = ?quality,
            capture = %format!("{}x{}", profile.capture_width, profile.capture_height),
            output = %format!("{}x{}", profile.output_width, profile.output_height),
            framerate = profile.framerate,
            "Запуск camera preview"
        );
        let device_for_spawn_error = device.clone();
        let (stop_tx, stop_rx) = mpsc::channel();
        let thread = match thread::Builder::new()
            .name("livescreen-camera-preview".into())
            .spawn(move || {
                wait_for_previous_preview(previous_thread);
                if stop_requested(&stop_rx) {
                    debug!("Перезапуск camera preview отменён до запуска нового pipeline");
                    return;
                }
                run_preview_pipeline(device, profile, frame_tx, &stop_rx);
            }) {
            Ok(thread) => Some(thread),
            Err(error) => {
                error!(device = %device_for_spawn_error, error = %error, "Не удалось запустить поток camera preview");
                None
            }
        };
        self.stop_tx = Some(stop_tx);
        self.thread = thread;
    }

    pub fn stop(&mut self) {
        let Some(thread) = self.request_stop_and_take_thread() else {
            return;
        };
        reap_preview_thread(thread);
    }

    fn request_stop_and_take_thread(&mut self) -> Option<thread::JoinHandle<()>> {
        if let Some(stop_tx) = self.stop_tx.take()
            && let Err(error) = stop_tx.send(())
        {
            warn!(error = %error, "Не удалось отправить stop camera preview");
        }
        self.thread.take()
    }
}

fn wait_for_previous_preview(previous_thread: Option<thread::JoinHandle<()>>) {
    if let Some(previous_thread) = previous_thread {
        debug!("Ожидание остановки предыдущего camera preview перед перезапуском");
        if let Err(error) = previous_thread.join() {
            error!(error = ?error, "Предыдущий поток camera preview завершился с panic");
        }
    }
}

fn stop_requested(stop_rx: &Receiver<()>) -> bool {
    matches!(
        stop_rx.try_recv(),
        Ok(()) | Err(mpsc::TryRecvError::Disconnected)
    )
}

fn reap_preview_thread(thread: thread::JoinHandle<()>) {
    debug!("Передача ожидания camera preview в background join thread");
    let join_result = thread::Builder::new()
        .name("livescreen-camera-preview-join".into())
        .spawn(move || {
            if let Err(error) = thread.join() {
                error!(error = ?error, "Поток camera preview завершился с panic");
            }
        });
    if let Err(error) = join_result {
        error!(
            error = %error,
            "Не удалось запустить поток ожидания camera preview; pipeline продолжит остановку самостоятельно"
        );
    }
}

fn run_preview_pipeline(
    device: String,
    profile: PreviewProfile,
    frame_tx: Sender<PreviewFrame>,
    stop_rx: &Receiver<()>,
) {
    let description = format!(
        "v4l2src device={} ! image/jpeg,width={},height={},framerate={}/1 ! jpegdec ! videoconvert ! videoscale ! videorate ! video/x-raw,format=RGBA,width={},height={},framerate={}/1 ! appsink name=preview_sink emit-signals=false sync=false max-buffers=1 drop=true",
        gst_quote(&device),
        profile.capture_width,
        profile.capture_height,
        profile.framerate,
        profile.output_width,
        profile.output_height,
        profile.framerate,
    );
    let pipeline = match gst::parse::launch(&description) {
        Ok(element) => match element.downcast::<gst::Pipeline>() {
            Ok(pipeline) => pipeline,
            Err(_) => {
                error!(device = %device, "GStreamer вернул не pipeline для camera preview");
                return;
            }
        },
        Err(error) => {
            error!(device = %device, error = %error, "Не удалось собрать pipeline camera preview");
            return;
        }
    };
    let Some(sink) = pipeline
        .by_name("preview_sink")
        .and_then(|element| element.downcast::<gst_app::AppSink>().ok())
    else {
        error!(device = %device, "В camera preview отсутствует appsink");
        return;
    };
    if let Err(error) = pipeline.set_state(gst::State::Playing) {
        error!(device = %device, error = %error, "Не удалось запустить pipeline camera preview");
        return;
    }
    info!(device = %device, "Camera preview перешёл в Playing");
    loop {
        if stop_requested(stop_rx) {
            break;
        }
        let Some(sample) = sink.try_pull_sample(gst::ClockTime::from_mseconds(100)) else {
            continue;
        };
        let Some(buffer) = sample.buffer() else {
            continue;
        };
        let Ok(map) = buffer.map_readable() else {
            continue;
        };
        if let Err(error) = frame_tx.send(PreviewFrame {
            data: map.as_slice().to_vec(),
            width: profile.output_width,
            height: profile.output_height,
            stride: profile.output_width as usize * 4,
        }) {
            debug!(error = %error, "Канал camera preview закрыт");
            break;
        }
    }
    if let Err(error) = pipeline.set_state(gst::State::Null) {
        error!(device = %device, error = %error, "Не удалось остановить pipeline camera preview");
    } else {
        info!(device = %device, "Camera preview остановлен");
    }
}

fn preview_profile(quality: CameraQuality) -> PreviewProfile {
    match quality {
        CameraQuality::Best => PreviewProfile {
            capture_width: 1280,
            capture_height: 720,
            output_width: 640,
            output_height: 360,
            framerate: 30,
        },
        CameraQuality::Optim => PreviewProfile {
            capture_width: 640,
            capture_height: 360,
            output_width: 480,
            output_height: 270,
            framerate: 30,
        },
        CameraQuality::Fast => PreviewProfile {
            capture_width: 640,
            capture_height: 360,
            output_width: 320,
            output_height: 180,
            framerate: 30,
        },
    }
}

impl Drop for PreviewController {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        time::{Duration, Instant},
    };

    #[test]
    fn stop_does_not_wait_for_preview_thread() {
        let thread = thread::spawn(|| {
            thread::sleep(Duration::from_millis(100));
        });
        let mut controller = PreviewController {
            stop_tx: None,
            thread: Some(thread),
        };
        let started = Instant::now();
        controller.stop();
        assert!(started.elapsed() < Duration::from_millis(80));
    }

    #[test]
    fn quality_profiles_scale_capture_and_output_predictably() {
        assert_eq!(
            preview_profile(CameraQuality::Best),
            PreviewProfile {
                capture_width: 1280,
                capture_height: 720,
                output_width: 640,
                output_height: 360,
                framerate: 30,
            }
        );
        assert_eq!(
            preview_profile(CameraQuality::Optim),
            PreviewProfile {
                capture_width: 640,
                capture_height: 360,
                output_width: 480,
                output_height: 270,
                framerate: 30,
            }
        );
        assert_eq!(
            preview_profile(CameraQuality::Fast),
            PreviewProfile {
                capture_width: 640,
                capture_height: 360,
                output_width: 320,
                output_height: 180,
                framerate: 30,
            }
        );
    }

    #[test]
    fn preview_restart_waits_until_previous_worker_finishes() {
        let previous_finished = Arc::new(AtomicBool::new(false));
        let previous_finished_in_worker = previous_finished.clone();
        let previous_thread = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            previous_finished_in_worker.store(true, Ordering::SeqCst);
        });

        wait_for_previous_preview(Some(previous_thread));

        assert!(previous_finished.load(Ordering::SeqCst));
    }
}
