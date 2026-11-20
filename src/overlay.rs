use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::{Duration, Instant},
};

use gtk::{glib, prelude::*};
use gtk4 as gtk;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};

use crate::{configuration::CameraPosition, preview::PreviewFrame};

struct Animation {
    from: f64,
    to: f64,
    started: Instant,
    duration: Duration,
}

fn smoothstep(fraction: f64) -> f64 {
    let fraction = fraction.clamp(0.0, 1.0);
    fraction * fraction * (3.0 - 2.0 * fraction)
}

impl Animation {
    fn value(&self, now: Instant) -> (f64, bool) {
        let elapsed = now.saturating_duration_since(self.started);
        if self.duration.is_zero() {
            return (self.to, true);
        }
        let fraction = (elapsed.as_secs_f64() / self.duration.as_secs_f64()).clamp(0.0, 1.0);
        let eased = smoothstep(fraction);
        (self.from + (self.to - self.from) * eased, fraction >= 1.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IndicatorColor {
    White,
    Red,
    Green,
    Lime,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct IndicatorLayer {
    text: &'static str,
    color: IndicatorColor,
    opacity: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct IndicatorFrame {
    primary: Option<IndicatorLayer>,
    secondary: Option<IndicatorLayer>,
    complete: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IndicatorAnimation {
    Countdown,
    Paused,
    Resumed,
    Stopped,
}

impl IndicatorAnimation {
    fn frame(self, elapsed: Duration) -> IndicatorFrame {
        match self {
            Self::Countdown => countdown_frame(elapsed),
            Self::Paused => single_label_frame(
                "Paused",
                IndicatorColor::Lime,
                elapsed,
                Duration::from_millis(600),
                true,
            ),
            Self::Resumed => single_label_frame(
                "Rec",
                IndicatorColor::Red,
                elapsed,
                Duration::from_millis(200),
                false,
            ),
            Self::Stopped => single_label_frame(
                "STOP",
                IndicatorColor::Green,
                elapsed,
                Duration::from_millis(1_000),
                true,
            ),
        }
    }
}

fn countdown_frame(elapsed: Duration) -> IndicatorFrame {
    if elapsed < Duration::from_millis(1_000) {
        let progress = smoothstep(elapsed.as_secs_f64());
        return IndicatorFrame {
            primary: Some(IndicatorLayer {
                text: "2",
                color: IndicatorColor::White,
                opacity: 1.0 - progress,
            }),
            secondary: Some(IndicatorLayer {
                text: "1",
                color: IndicatorColor::White,
                opacity: progress,
            }),
            complete: false,
        };
    }
    if elapsed < Duration::from_millis(1_800) {
        let phase = elapsed - Duration::from_millis(1_000);
        let progress = smoothstep(phase.as_secs_f64() / 0.8);
        return IndicatorFrame {
            primary: Some(IndicatorLayer {
                text: "1",
                color: IndicatorColor::White,
                opacity: 1.0 - progress,
            }),
            secondary: Some(IndicatorLayer {
                text: "Rec",
                color: IndicatorColor::White,
                opacity: progress,
            }),
            complete: false,
        };
    }
    if elapsed < Duration::from_millis(2_000) {
        return IndicatorFrame {
            primary: Some(IndicatorLayer {
                text: "Rec",
                color: IndicatorColor::Red,
                opacity: 1.0,
            }),
            secondary: None,
            complete: false,
        };
    }
    IndicatorFrame {
        primary: None,
        secondary: None,
        complete: true,
    }
}

fn single_label_frame(
    text: &'static str,
    color: IndicatorColor,
    elapsed: Duration,
    duration: Duration,
    fade: bool,
) -> IndicatorFrame {
    if elapsed >= duration {
        return IndicatorFrame {
            primary: None,
            secondary: None,
            complete: true,
        };
    }
    let progress = smoothstep(elapsed.as_secs_f64() / duration.as_secs_f64());
    IndicatorFrame {
        primary: Some(IndicatorLayer {
            text,
            color,
            opacity: if fade { 1.0 - progress } else { 1.0 },
        }),
        secondary: None,
        complete: false,
    }
}

pub struct RecordingIndicatorOverlay {
    window: gtk::Window,
    animation_widget: gtk::Overlay,
    primary: gtk::Label,
    secondary: gtk::Label,
    generation: Cell<u64>,
    closed: Cell<bool>,
    showing_stop: Cell<bool>,
    pub layer_shell: bool,
}

impl RecordingIndicatorOverlay {
    pub fn play_countdown<F>(self: &Rc<Self>, complete: F)
    where
        F: FnOnce() + 'static,
    {
        self.play(IndicatorAnimation::Countdown, Some(Box::new(complete)));
    }

    pub fn show_paused(self: &Rc<Self>) {
        self.play(IndicatorAnimation::Paused, None);
    }

    pub fn show_resumed(self: &Rc<Self>) {
        self.play(IndicatorAnimation::Resumed, None);
    }

    pub fn show_stopped<F>(self: &Rc<Self>, complete: F)
    where
        F: FnOnce() + 'static,
    {
        self.play(IndicatorAnimation::Stopped, Some(Box::new(complete)));
    }

    pub fn is_showing_stop(&self) -> bool {
        self.showing_stop.get()
    }

    pub fn close(&self) {
        if self.closed.replace(true) {
            return;
        }
        self.generation.set(self.generation.get().wrapping_add(1));
        self.showing_stop.set(false);
        clear_indicator_label(&self.primary);
        clear_indicator_label(&self.secondary);
        self.window.close();
    }

    fn play(self: &Rc<Self>, animation: IndicatorAnimation, complete: Option<Box<dyn FnOnce()>>) {
        if self.closed.get() {
            return;
        }
        let generation = self.generation.get().wrapping_add(1);
        self.generation.set(generation);
        self.showing_stop
            .set(animation == IndicatorAnimation::Stopped);
        let started = Instant::now();
        let complete = RefCell::new(complete);
        self.render(animation.frame(Duration::ZERO));
        let weak = Rc::downgrade(self);
        self.animation_widget.add_tick_callback(move |_, _| {
            let Some(indicator) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            if indicator.closed.get() || indicator.generation.get() != generation {
                return glib::ControlFlow::Break;
            }
            let frame = animation.frame(Instant::now().saturating_duration_since(started));
            indicator.render(frame);
            if frame.complete {
                indicator.showing_stop.set(false);
                if let Some(complete) = complete.borrow_mut().take() {
                    complete();
                }
                glib::ControlFlow::Break
            } else {
                glib::ControlFlow::Continue
            }
        });
    }

    fn render(&self, frame: IndicatorFrame) {
        render_indicator_label(&self.primary, frame.primary);
        render_indicator_label(&self.secondary, frame.secondary);
    }
}

fn render_indicator_label(label: &gtk::Label, layer: Option<IndicatorLayer>) {
    let Some(layer) = layer else {
        clear_indicator_label(label);
        return;
    };
    label.set_label(layer.text);
    label.set_opacity(layer.opacity);
    label.set_visible(layer.opacity > 0.001);
    for class in [
        "recording-indicator-white",
        "recording-indicator-red",
        "recording-indicator-green",
        "recording-indicator-lime",
    ] {
        label.remove_css_class(class);
    }
    label.add_css_class(match layer.color {
        IndicatorColor::White => "recording-indicator-white",
        IndicatorColor::Red => "recording-indicator-red",
        IndicatorColor::Green => "recording-indicator-green",
        IndicatorColor::Lime => "recording-indicator-lime",
    });
}

fn clear_indicator_label(label: &gtk::Label) {
    label.set_opacity(0.0);
    label.set_visible(false);
}

struct OverlayOpacityState {
    primary_alpha: f64,
    secondary_alpha: f64,
    current_alpha: f64,
    target_alpha: f64,
    animation: Option<Animation>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct OverlayGeometry {
    x: i32,
    y: i32,
    diameter: i32,
    monitor_width: i32,
    monitor_height: i32,
}

pub struct CameraOverlay {
    window: gtk::Window,
    pub area: gtk::DrawingArea,
    opacity: Rc<RefCell<OverlayOpacityState>>,
    pub layer_shell: bool,
    monitor: Option<gtk::gdk::Monitor>,
    transition: Duration,
    geometry: Rc<Cell<OverlayGeometry>>,
    border_visible: Rc<Cell<bool>>,
    drag_origin: Cell<(i32, i32)>,
    interactive: Cell<bool>,
    dragging: Cell<bool>,
    record_toggle_action: RefCell<Option<Rc<dyn Fn()>>>,
}

impl CameraOverlay {
    pub fn set_primary_opacity(&self, transparency_percent: u8) {
        let alpha = 1.0 - transparency_percent.min(100) as f64 / 100.0;
        let mut opacity = self.opacity.borrow_mut();
        opacity.primary_alpha = alpha;
        if opacity.animation.is_none()
            && (opacity.target_alpha - opacity.secondary_alpha).abs() > 0.001
        {
            opacity.current_alpha = alpha;
            opacity.target_alpha = alpha;
            self.area.set_opacity(alpha);
        }
    }

    pub fn set_secondary_opacity(&self, transparency_percent: u8) {
        let alpha = 1.0 - transparency_percent.min(100) as f64 / 100.0;
        let mut opacity = self.opacity.borrow_mut();
        opacity.secondary_alpha = alpha;
        if opacity.animation.is_none()
            && (opacity.target_alpha - opacity.primary_alpha).abs() > 0.001
        {
            opacity.current_alpha = alpha;
            opacity.target_alpha = alpha;
            self.area.set_opacity(alpha);
        }
    }

    pub fn toggle_opacity(&self) {
        let now = Instant::now();
        let mut opacity = self.opacity.borrow_mut();
        let current = opacity
            .animation
            .as_ref()
            .map(|animation| animation.value(now).0)
            .unwrap_or(opacity.current_alpha);
        let next = if (opacity.target_alpha - opacity.secondary_alpha).abs() < 0.001 {
            opacity.primary_alpha
        } else {
            opacity.secondary_alpha
        };
        self.set_interactive(false);
        opacity.current_alpha = current;
        opacity.target_alpha = next;
        opacity.animation = Some(Animation {
            from: current,
            to: next,
            started: now,
            duration: self.transition,
        });
    }

    pub fn tick_opacity(&self) {
        let mut opacity = self.opacity.borrow_mut();
        let Some(animation) = opacity.animation.as_ref() else {
            return;
        };
        let (value, complete) = animation.value(Instant::now());
        opacity.current_alpha = value;
        self.area.set_opacity(value);
        if complete {
            let selfie_on_visible = (opacity.target_alpha - opacity.primary_alpha).abs() < 0.001;
            opacity.animation = None;
            drop(opacity);
            self.set_interactive(selfie_on_visible);
        }
    }

    pub fn configure(&self, position: CameraPosition, size_percent: u8) {
        let Some(monitor) = self.monitor.as_ref() else {
            return;
        };
        let geometry = monitor.geometry();
        let diameter =
            ((geometry.width() as f64 * size_percent.clamp(5, 50) as f64 / 100.0).round() as i32)
                .max(64);
        let margin = (geometry.width() / 50).max(12);
        let location = match position {
            CameraPosition::TopLeft => (margin, margin),
            CameraPosition::TopRight => (geometry.width() - diameter - margin, margin),
            CameraPosition::BottomLeft => (margin, geometry.height() - diameter - margin),
            CameraPosition::BottomRight => (
                geometry.width() - diameter - margin,
                geometry.height() - diameter - margin,
            ),
        };
        self.set_geometry(diameter, location, geometry.width(), geometry.height());
    }

    pub fn resize(&self, size_percent: u8) {
        let Some(monitor) = self.monitor.as_ref() else {
            return;
        };
        let geometry = monitor.geometry();
        let diameter =
            ((geometry.width() as f64 * size_percent.clamp(5, 50) as f64 / 100.0).round() as i32)
                .max(64);
        self.set_geometry(
            diameter,
            (self.geometry.get().x, self.geometry.get().y),
            geometry.width(),
            geometry.height(),
        );
    }

    pub fn set_border_visible(&self, visible: bool) {
        self.border_visible.set(visible);
        self.area.queue_draw();
    }

    pub fn set_record_toggle_action(&self, action: impl Fn() + 'static) {
        self.record_toggle_action.replace(Some(Rc::new(action)));
    }

    pub fn set_interactive(&self, interactive: bool) {
        self.interactive.set(interactive);
        self.area.set_can_target(interactive);
        self.apply_input_region();
    }

    fn set_geometry(&self, diameter: i32, location: (i32, i32), width: i32, height: i32) {
        // Обычное окно перемещает оконный менеджер, а круг остаётся в локальном начале.
        let (width, height) = if self.layer_shell {
            (width, height)
        } else {
            (diameter, diameter)
        };
        let location = clamp_overlay_location(location, diameter, width, height);
        self.geometry.set(OverlayGeometry {
            x: location.0,
            y: location.1,
            diameter,
            monitor_width: width,
            monitor_height: height,
        });
        self.area.queue_draw();
        self.area.set_content_width(width);
        self.area.set_content_height(height);
        self.window.set_default_size(width, height);
        self.apply_input_region();
        if !self.layer_shell {
            return;
        }
        for edge in [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom] {
            self.window.set_anchor(edge, true);
            self.window.set_margin(edge, 0);
        }
    }

    fn set_location(&self, location: (i32, i32), width: i32, height: i32) {
        let mut geometry = self.geometry.get();
        let location = clamp_overlay_location(location, geometry.diameter, width, height);
        geometry.x = location.0;
        geometry.y = location.1;
        geometry.monitor_width = width;
        geometry.monitor_height = height;
        self.geometry.set(geometry);
        self.area.queue_draw();
    }

    fn begin_drag(&self) {
        if self.interactive.get() {
            let geometry = self.geometry.get();
            self.drag_origin.set((geometry.x, geometry.y));
            self.dragging.set(true);
            self.apply_input_region();
        }
    }

    fn end_drag(&self) {
        self.dragging.set(false);
        self.apply_input_region();
    }

    fn update_drag(&self, offset_x: f64, offset_y: f64) {
        if !self.interactive.get() {
            return;
        }
        let Some(monitor) = self.monitor.as_ref() else {
            return;
        };
        let geometry = monitor.geometry();
        let origin = self.drag_origin.get();
        self.set_location(
            (
                origin.0 + offset_x.round() as i32,
                origin.1 + offset_y.round() as i32,
            ),
            geometry.width(),
            geometry.height(),
        );
    }

    fn apply_input_region(&self) {
        let Some(surface) = self.window.surface() else {
            return;
        };
        if !self.interactive.get() {
            let input_region = cairo::Region::create();
            surface.set_input_region(Some(&input_region));
        } else if self.dragging.get() {
            surface.set_input_region(None);
        } else {
            let input_region = circle_input_region(self.geometry.get());
            surface.set_input_region(Some(&input_region));
        }
    }
}

fn clamp_overlay_location(
    location: (i32, i32),
    diameter: i32,
    monitor_width: i32,
    monitor_height: i32,
) -> (i32, i32) {
    (
        location.0.clamp(0, (monitor_width - diameter).max(0)),
        location.1.clamp(0, (monitor_height - diameter).max(0)),
    )
}

fn circle_input_region(geometry: OverlayGeometry) -> cairo::Region {
    let visible_diameter = (geometry.diameter - 14).max(24);
    let origin_x = geometry.x + 7;
    let origin_y = geometry.y + 7;
    let radius = visible_diameter as f64 / 2.0;
    let mut rectangles = Vec::with_capacity(visible_diameter as usize);
    for row in 0..visible_diameter {
        let dy = row as f64 + 0.5 - radius;
        let half_width = (radius * radius - dy * dy).max(0.0).sqrt();
        let left = (radius - half_width).floor() as i32;
        let right = (radius + half_width).ceil() as i32;
        rectangles.push(cairo::RectangleInt::new(
            origin_x + left,
            origin_y + row,
            (right - left).max(1),
            1,
        ));
    }
    cairo::Region::create_rectangles(&rectangles)
}

pub fn create_recording_indicator_overlay() -> Rc<RecordingIndicatorOverlay> {
    let window = gtk::Window::new();
    window.set_decorated(false);
    window.set_resizable(false);
    window.set_focusable(false);
    window.set_can_target(false);
    window.set_title(Some("LiveScreen — индикатор записи"));
    window.add_css_class("recording-indicator-overlay");

    let animation_widget = gtk::Overlay::new();
    animation_widget.set_can_target(false);
    animation_widget.set_hexpand(true);
    animation_widget.set_vexpand(true);
    animation_widget.add_css_class("recording-indicator-root");
    let transparent_base = gtk::Box::new(gtk::Orientation::Vertical, 0);
    transparent_base.set_can_target(false);
    transparent_base.set_hexpand(true);
    transparent_base.set_vexpand(true);
    animation_widget.set_child(Some(&transparent_base));

    let indicator_label = || {
        let label = gtk::Label::new(None);
        label.set_halign(gtk::Align::Center);
        label.set_valign(gtk::Align::Center);
        label.set_can_target(false);
        label.set_size_request(720, 180);
        label.set_visible(false);
        label.add_css_class("recording-indicator-text");
        label
    };
    let primary = indicator_label();
    let secondary = indicator_label();
    animation_widget.add_overlay(&primary);
    animation_widget.add_overlay(&secondary);
    window.set_child(Some(&animation_widget));

    let layer_shell = std::env::var("XDG_SESSION_TYPE").as_deref() == Ok("wayland")
        && gtk4_layer_shell::is_supported();
    let monitor = gtk::gdk::Display::default().and_then(|display| {
        display
            .monitors()
            .item(0)
            .and_downcast::<gtk::gdk::Monitor>()
    });
    if let Some(monitor) = monitor.as_ref() {
        let geometry = monitor.geometry();
        window.set_default_size(geometry.width(), geometry.height());
        animation_widget.set_size_request(geometry.width(), geometry.height());
    }
    if layer_shell {
        window.init_layer_shell();
        window.set_layer(Layer::Overlay);
        window.set_keyboard_mode(KeyboardMode::None);
        window.set_exclusive_zone(0);
        window.set_namespace(Some("livescreen-recording-indicator"));
        window.set_monitor(monitor.as_ref());
        for edge in [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom] {
            window.set_anchor(edge, true);
            window.set_margin(edge, 0);
        }
    } else {
        window.fullscreen();
    }
    window.connect_realize(|window| {
        if let Some(surface) = window.surface() {
            let input_region = cairo::Region::create();
            surface.set_input_region(Some(&input_region));
        }
    });

    let indicator = Rc::new(RecordingIndicatorOverlay {
        window,
        animation_widget,
        primary,
        secondary,
        generation: Cell::new(0),
        closed: Cell::new(false),
        showing_stop: Cell::new(false),
        layer_shell,
    });
    indicator.window.present();
    indicator
}

pub fn create_camera_overlay(
    frame: Rc<RefCell<Option<PreviewFrame>>>,
    position: CameraPosition,
    size_percent: u8,
    transparency_percent: u8,
    transition: Duration,
    secondary_transparency_percent: u8,
    border_visible: bool,
) -> Rc<CameraOverlay> {
    let window = gtk::Window::new();
    window.set_decorated(false);
    window.set_resizable(false);
    window.set_title(Some("LiveScreen — камера"));
    window.add_css_class("camera-overlay");
    if let Some(display) = gtk::gdk::Display::default() {
        let css = gtk::CssProvider::new();
        css.load_from_string(
            ".camera-overlay { background-color: transparent; }\n.camera-overlay > * { background-color: transparent; }",
        );
        gtk::style_context_add_provider_for_display(
            &display,
            &css,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
    let area = gtk::DrawingArea::new();
    area.set_content_width(160);
    area.set_content_height(160);
    let frame_for_draw = frame;
    let geometry = Rc::new(Cell::new(OverlayGeometry::default()));
    let geometry_for_draw = geometry.clone();
    let border_visible = Rc::new(Cell::new(border_visible));
    let border_visible_for_draw = border_visible.clone();
    area.set_draw_func(move |_, cr, width, height| {
        draw_overlay_camera(
            cr,
            width,
            height,
            frame_for_draw.borrow().as_ref(),
            geometry_for_draw.get(),
            border_visible_for_draw.get(),
        );
    });
    window.set_child(Some(&area));

    let layer_shell = std::env::var("XDG_SESSION_TYPE").as_deref() == Ok("wayland")
        && gtk4_layer_shell::is_supported();
    let monitor = gtk::gdk::Display::default().and_then(|display| {
        display
            .monitors()
            .item(0)
            .and_downcast::<gtk::gdk::Monitor>()
    });
    if layer_shell {
        window.init_layer_shell();
        window.set_layer(Layer::Overlay);
        window.set_keyboard_mode(KeyboardMode::None);
        window.set_exclusive_zone(0);
        window.set_namespace(Some("livescreen-camera"));
        window.set_monitor(monitor.as_ref());
    }
    let primary_alpha = 1.0 - transparency_percent.min(100) as f64 / 100.0;
    let overlay = Rc::new(CameraOverlay {
        window,
        area,
        opacity: Rc::new(RefCell::new(OverlayOpacityState {
            primary_alpha,
            secondary_alpha: 1.0 - secondary_transparency_percent.min(100) as f64 / 100.0,
            current_alpha: primary_alpha,
            target_alpha: primary_alpha,
            animation: None,
        })),
        layer_shell,
        monitor,
        transition,
        geometry,
        border_visible,
        drag_origin: Cell::new((0, 0)),
        interactive: Cell::new(false),
        dragging: Cell::new(false),
        record_toggle_action: RefCell::new(None),
    });
    overlay.area.set_opacity(primary_alpha);
    overlay.configure(position, size_percent);
    let dragged = Rc::new(Cell::new(false));
    let drag = gtk::GestureDrag::new();
    drag.set_button(1);
    {
        let overlay = Rc::downgrade(&overlay);
        let dragged = dragged.clone();
        drag.connect_drag_begin(move |_, _, _| {
            dragged.set(false);
            if let Some(overlay) = overlay.upgrade() {
                overlay.begin_drag();
            }
        });
    }
    {
        let overlay = Rc::downgrade(&overlay);
        let dragged = dragged.clone();
        drag.connect_drag_update(move |gesture, offset_x, offset_y| {
            let Some(overlay) = overlay.upgrade() else {
                return;
            };
            if !overlay.interactive.get() {
                return;
            }
            if (offset_x.abs() > 3.0 || offset_y.abs() > 3.0)
                && !dragged.replace(true)
                && !overlay.layer_shell
                && let Some(toplevel) = overlay
                    .window
                    .surface()
                    .and_downcast::<gtk::gdk::Toplevel>()
                && let Some(event) = gesture.current_event()
                && let Some(device) = event.device()
                && let Some((x, y)) = event.position()
            {
                toplevel.begin_move(&device, 1, x, y, event.time());
            }
            if overlay.layer_shell {
                overlay.update_drag(offset_x, offset_y);
            }
        });
    }
    {
        let overlay = Rc::downgrade(&overlay);
        drag.connect_drag_end(move |_, _, _| {
            if let Some(overlay) = overlay.upgrade() {
                overlay.end_drag();
            }
        });
    }
    overlay.area.add_controller(drag);

    let click = gtk::GestureClick::new();
    click.set_button(1);
    {
        let overlay = Rc::downgrade(&overlay);
        let dragged = dragged.clone();
        click.connect_released(move |_, _, _, _| {
            if dragged.replace(false) {
                return;
            }
            if let Some(overlay) = overlay.upgrade()
                && overlay.interactive.get()
            {
                overlay.toggle_opacity();
            }
        });
    }
    overlay.area.add_controller(click);

    let right_click = gtk::GestureClick::new();
    right_click.set_button(3);
    {
        let overlay = Rc::downgrade(&overlay);
        right_click.connect_released(move |_, _, _, _| {
            if let Some(overlay) = overlay.upgrade()
                && overlay.interactive.get()
                && let Some(action) = overlay.record_toggle_action.borrow().as_ref()
            {
                action();
            }
        });
    }
    overlay.area.add_controller(right_click);
    overlay.set_interactive(true);
    overlay.window.present();
    let overlay_for_input_region = Rc::downgrade(&overlay);
    glib::idle_add_local_once(move || {
        if let Some(overlay) = overlay_for_input_region.upgrade() {
            overlay.apply_input_region();
        }
    });
    overlay
}

fn draw_overlay_camera(
    cr: &cairo::Context,
    width: i32,
    height: i32,
    frame: Option<&PreviewFrame>,
    geometry: OverlayGeometry,
    border_visible: bool,
) {
    if geometry.diameter > 0 {
        draw_camera_frame(
            cr,
            frame,
            geometry.x as f64 + 7.0,
            geometry.y as f64 + 7.0,
            (geometry.diameter - 14).max(24) as f64,
            border_visible,
        );
    } else {
        let diameter = (width.min(height) - 14).max(24) as f64;
        draw_camera_frame(
            cr,
            frame,
            (width as f64 - diameter) / 2.0,
            (height as f64 - diameter) / 2.0,
            diameter,
            border_visible,
        );
    }
}

fn draw_camera_frame(
    cr: &cairo::Context,
    frame: Option<&PreviewFrame>,
    x: f64,
    y: f64,
    diameter: f64,
    border_visible: bool,
) {
    let center_x = x + diameter / 2.0;
    let center_y = y + diameter / 2.0;

    let _ = cr.save();
    cr.arc(
        center_x,
        center_y,
        diameter / 2.0,
        0.0,
        std::f64::consts::TAU,
    );
    cr.clip();
    if frame.is_none() {
        cr.set_source_rgb(0.18, 0.2, 0.24);
        let _ = cr.paint();
    }

    if let Some(frame) = frame {
        let mut data = frame.data.clone();
        for pixel in data.chunks_exact_mut(4) {
            pixel.swap(0, 2);
        }
        if let Ok(surface) = cairo::ImageSurface::create_for_data(
            data,
            cairo::Format::ARgb32,
            frame.width,
            frame.height,
            frame.stride as i32,
        ) {
            let scale = (diameter / frame.width as f64).max(diameter / frame.height as f64);
            let draw_width = frame.width as f64 * scale;
            let draw_height = frame.height as f64 * scale;
            let _ = cr.save();
            cr.translate(center_x - draw_width / 2.0, center_y - draw_height / 2.0);
            cr.scale(scale, scale);
            let _ = cr.set_source_surface(&surface, 0.0, 0.0);
            let _ = cr.paint();
            let _ = cr.restore();
        }
    }
    let _ = cr.restore();

    if border_visible {
        cr.set_source_rgba(1.0, 1.0, 1.0, 0.8);
        cr.set_line_width(2.0);
        cr.arc(
            center_x,
            center_y,
            diameter / 2.0 - 1.0,
            0.0,
            std::f64::consts::TAU,
        );
        let _ = cr.stroke();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn animation_reaches_target_and_is_reversible_from_current_value() {
        let start = Instant::now();
        let animation = Animation {
            from: 1.0,
            to: 0.0,
            started: start,
            duration: Duration::from_millis(2_000),
        };
        let (midpoint, complete) = animation.value(start + Duration::from_millis(1_000));
        assert!(!complete);
        assert!(midpoint < 1.0 && midpoint > 0.0);
        let (end, complete) = animation.value(start + Duration::from_millis(2_100));
        assert!(complete);
        assert!((end - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    #[ignore = "требует отдельного X11 display"]
    fn x11_camera_resize_keeps_circle_inside_window() {
        gtk::init().expect("тест требует GTK display");
        let camera = create_camera_overlay(
            Rc::new(RefCell::new(None)),
            CameraPosition::BottomRight,
            20,
            0,
            Duration::ZERO,
            100,
            true,
        );
        assert!(!camera.layer_shell);
        let width = camera
            .monitor
            .as_ref()
            .expect("тест требует монитор")
            .geometry()
            .width();
        for percent in [20, 30] {
            camera.resize(percent);
            let diameter = ((width as f64 * f64::from(percent) / 100.0).round() as i32).max(64);
            let geometry = camera.geometry.get();
            assert_eq!(
                (geometry.x, geometry.y, geometry.diameter),
                (0, 0, diameter)
            );
            assert_eq!(camera.window.default_size(), (diameter, diameter));
            let region = circle_input_region(geometry);
            assert!(region.contains_point(diameter / 2, diameter / 2));
            assert!(!region.contains_point(0, 0));
        }
        camera.window.close();
    }

    #[test]
    fn dragged_overlay_location_stays_inside_monitor() {
        assert_eq!(
            clamp_overlay_location((-40, 700), 200, 1_000, 800),
            (0, 600)
        );
        assert_eq!(
            clamp_overlay_location((900, -20), 200, 1_000, 800),
            (800, 0)
        );
    }

    #[test]
    fn recording_countdown_crossfades_and_finishes_at_two_seconds() {
        let start = countdown_frame(Duration::ZERO);
        assert_eq!(start.primary.map(|layer| layer.text), Some("2"));
        assert_eq!(start.secondary.map(|layer| layer.text), Some("1"));
        assert!(!start.complete);

        let midpoint = countdown_frame(Duration::from_millis(500));
        let primary = midpoint.primary.expect("2 should still be fading");
        let secondary = midpoint.secondary.expect("1 should be fading in");
        assert!((primary.opacity - 0.5).abs() < 0.001);
        assert!((secondary.opacity - 0.5).abs() < 0.001);

        let rec_fade = countdown_frame(Duration::from_millis(1_400));
        assert_eq!(rec_fade.primary.map(|layer| layer.text), Some("1"));
        assert_eq!(rec_fade.secondary.map(|layer| layer.text), Some("Rec"));

        let red_rec = countdown_frame(Duration::from_millis(1_800));
        assert_eq!(
            red_rec.primary.map(|layer| (layer.text, layer.color)),
            Some(("Rec", IndicatorColor::Red))
        );
        assert!(!red_rec.complete);

        let complete = countdown_frame(Duration::from_millis(2_000));
        assert!(complete.complete);
        assert!(complete.primary.is_none());
        assert!(complete.secondary.is_none());
    }

    #[test]
    fn paused_and_stop_fade_but_resumed_rec_stays_solid() {
        let paused = IndicatorAnimation::Paused.frame(Duration::from_millis(300));
        let paused_layer = paused.primary.expect("Paused should be visible");
        assert_eq!(paused_layer.color, IndicatorColor::Lime);
        assert!((paused_layer.opacity - 0.5).abs() < 0.001);

        let resumed = IndicatorAnimation::Resumed.frame(Duration::from_millis(199));
        let resumed_layer = resumed.primary.expect("Rec should be visible");
        assert_eq!(resumed_layer.color, IndicatorColor::Red);
        assert!((resumed_layer.opacity - 1.0).abs() < f64::EPSILON);
        assert!(
            IndicatorAnimation::Resumed
                .frame(Duration::from_millis(200))
                .complete
        );

        let stopped = IndicatorAnimation::Stopped.frame(Duration::from_millis(500));
        let stopped_layer = stopped.primary.expect("STOP should be visible");
        assert_eq!(stopped_layer.text, "STOP");
        assert_eq!(stopped_layer.color, IndicatorColor::Green);
        assert!((stopped_layer.opacity - 0.5).abs() < 0.001);
        assert!(
            IndicatorAnimation::Stopped
                .frame(Duration::from_millis(1_000))
                .complete
        );
    }
}
