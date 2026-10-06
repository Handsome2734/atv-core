//! Native Windows controller: translates Apple TV remote buttons and touch gestures
//! into Win32 keyboard/mouse input, media/volume keys, and monitor sleep/wake.
//!
//! Uses a handful of `user32` calls directly (no extra crates); no special
//! permission is required on Windows (unlike macOS Accessibility).

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use atv_core::{AtvDelegate, EventKind, TouchPhase, TrackpadMode};
use tracing::{debug, info, warn};

#[repr(C)]
#[derive(Default)]
struct Point {
    x: i32,
    y: i32,
}

#[link(name = "user32")]
extern "system" {
    fn keybd_event(vk: u8, scan: u8, flags: u32, extra: usize);
    fn mouse_event(flags: u32, dx: i32, dy: i32, data: u32, extra: usize);
    fn GetCursorPos(p: *mut Point) -> i32;
    fn SetCursorPos(x: i32, y: i32) -> i32;
    fn GetSystemMetrics(index: i32) -> i32;
    fn PostMessageW(hwnd: isize, msg: u32, wparam: usize, lparam: isize) -> i32;
    fn SetProcessDPIAware() -> i32;
}

const KEYEVENTF_EXTENDEDKEY: u32 = 0x1;
const KEYEVENTF_KEYUP: u32 = 0x2;
const MOUSEEVENTF_MOVE: u32 = 0x1;
const MOUSEEVENTF_LEFTDOWN: u32 = 0x2;
const MOUSEEVENTF_LEFTUP: u32 = 0x4;

const VK_RETURN: u8 = 0x0D;
const VK_VOLUME_MUTE: u8 = 0xAD;
const VK_VOLUME_DOWN: u8 = 0xAE;
const VK_VOLUME_UP: u8 = 0xAF;
const VK_MEDIA_NEXT: u8 = 0xB0;
const VK_MEDIA_PREV: u8 = 0xB1;
const VK_MEDIA_PLAY_PAUSE: u8 = 0xB3;

/// Windows virtual-key codes for simple remote buttons.
fn vk_for_button(name: &str) -> Option<u8> {
    Some(match name {
        "up" => 0x26,
        "down" => 0x28,
        "left" | "rewind" => 0x25,
        "right" | "fast_forward" => 0x27,
        "menu" => 0x1B,                         // Escape
        "home" => 0x24,                         // Home
        "channel_up" | "page_up" => 0x21,       // Page Up
        "channel_down" | "page_down" => 0x22,   // Page Down
        "guide" => 0x09,                        // Tab
        "play" | "pause" => VK_MEDIA_PLAY_PAUSE,
        "next" | "next_track" => VK_MEDIA_NEXT,
        "prev" | "previous" | "prev_track" | "previous_track" => VK_MEDIA_PREV,
        "volume_up" => VK_VOLUME_UP,
        "volume_down" => VK_VOLUME_DOWN,
        "mute" => VK_VOLUME_MUTE,
        _ => return None,
    })
}

fn press_key(vk: u8) {
    // Arrows, Home, PgUp/PgDn and media keys are "extended" keys; Esc/Tab/Enter are not.
    let ext = if matches!(vk, 0x09 | 0x0D | 0x1B) { 0 } else { KEYEVENTF_EXTENDEDKEY };
    unsafe {
        keybd_event(vk, 0, ext, 0);
        keybd_event(vk, 0, ext | KEYEVENTF_KEYUP, 0);
    }
}

fn cursor_pos() -> (f64, f64) {
    let mut p = Point::default();
    unsafe { GetCursorPos(&mut p) };
    (p.x as f64, p.y as f64)
}

/// Bounds of the whole virtual desktop (all monitors): (left, top, right, bottom).
fn virtual_screen() -> (f64, f64, f64, f64) {
    unsafe {
        let (x, y) = (GetSystemMetrics(76), GetSystemMetrics(77));
        let (w, h) = (GetSystemMetrics(78), GetSystemMetrics(79));
        (x as f64, y as f64, (x + w - 1) as f64, (y + h - 1) as f64)
    }
}

fn set_monitor_power(on: bool) {
    unsafe {
        if on {
            // Any user input wakes the display.
            mouse_event(MOUSEEVENTF_MOVE, 1, 0, 0, 0);
            mouse_event(MOUSEEVENTF_MOVE, -1, 0, 0, 0);
        } else {
            // HWND_BROADCAST, WM_SYSCOMMAND, SC_MONITORPOWER, 2 = off
            PostMessageW(0xFFFF, 0x0112, 0xF170, 2);
        }
    }
}

pub struct WinDelegate {
    mouse_mode: Arc<AtomicBool>,
    mouse_speed: AtomicU64,
    mouse_accel: AtomicBool,
    verbose_events: AtomicBool,
    display_asleep: AtomicBool,
    cursor: Mutex<(f64, f64)>,
    /// (last click time, click count) for play/pause multi-click detection.
    play_clicks: Arc<Mutex<(Instant, u32)>>,
    play_gen: Arc<AtomicU32>,
}

impl WinDelegate {
    pub fn new(mouse_mode: Arc<AtomicBool>, speed: f64, accel: bool, verbose: bool) -> Self {
        unsafe { SetProcessDPIAware() };
        info!("[Windows] native input controller ready");
        Self {
            mouse_mode,
            mouse_speed: AtomicU64::new(speed.clamp(0.1, 10.0).to_bits()),
            mouse_accel: AtomicBool::new(accel),
            verbose_events: AtomicBool::new(verbose),
            display_asleep: AtomicBool::new(false),
            cursor: Mutex::new(cursor_pos()),
            play_clicks: Arc::new(Mutex::new((Instant::now() - Duration::from_secs(10), 0))),
            play_gen: Arc::new(AtomicU32::new(0)),
        }
    }

    fn log(&self, msg: &str) {
        if self.verbose_events.load(Ordering::Relaxed) {
            info!("[Windows] {msg}");
        } else {
            debug!("[Windows] {msg}");
        }
    }

    fn mouse_move(&self, dx: f64, dy: f64) {
        let dist = (dx * dx + dy * dy).sqrt();
        if dist == 0.0 {
            return;
        }
        let speed = f64::from_bits(self.mouse_speed.load(Ordering::Relaxed));
        // Same ballistics curve as the macOS controller.
        let accel = if !self.mouse_accel.load(Ordering::Relaxed) {
            1.6
        } else if dist < 1.5 {
            1.15
        } else if dist < 5.0 {
            1.15 + (dist - 1.5) * 0.35
        } else {
            2.375 + (dist - 5.0).min(10.0) * 0.22
        };
        let scale = accel * 1.6 * speed;
        let (l, t, r, b) = virtual_screen();
        let mut cur = self.cursor.lock().unwrap();
        cur.0 = (cur.0 + dx * scale).clamp(l, r);
        cur.1 = (cur.1 + dy * scale).clamp(t, b);
        unsafe { SetCursorPos(cur.0.round() as i32, cur.1.round() as i32) };
    }

    fn mouse_click(&self) {
        unsafe {
            mouse_event(MOUSEEVENTF_LEFTDOWN, 0, 0, 0, 0);
            mouse_event(MOUSEEVENTF_LEFTUP, 0, 0, 0, 0);
        }
    }

    /// Single click: play/pause, double: next track, triple: previous track.
    fn handle_play_pause(&self) {
        let count = {
            let mut s = self.play_clicks.lock().unwrap();
            let now = Instant::now();
            s.1 = if now.duration_since(s.0) <= Duration::from_millis(380) { s.1 + 1 } else { 1 };
            s.0 = now;
            s.1
        };
        let gen = self.play_gen.fetch_add(1, Ordering::SeqCst) + 1;
        if count >= 3 {
            self.play_clicks.lock().unwrap().1 = 0;
            info!("[Windows] media previous track (triple-click)");
            return press_key(VK_MEDIA_PREV);
        }
        let (clicks, gen_ref) = (self.play_clicks.clone(), self.play_gen.clone());
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            if gen_ref.load(Ordering::SeqCst) != gen {
                return;
            }
            let n = std::mem::take(&mut clicks.lock().unwrap().1);
            match n {
                1 => press_key(VK_MEDIA_PLAY_PAUSE),
                2 => {
                    info!("[Windows] media next track (double-click)");
                    press_key(VK_MEDIA_NEXT)
                }
                _ => {}
            }
        });
    }
}

impl AtvDelegate for WinDelegate {
    fn on_button(&self, name: &str) {
        self.log(&format!("button: {name}"));
        if name == "power" || name == "sleep" || name == "wake" {
            let sleep = match name {
                "sleep" => true,
                "wake" => false,
                _ => !self.display_asleep.load(Ordering::SeqCst),
            };
            self.display_asleep.store(sleep, Ordering::SeqCst);
            return set_monitor_power(!sleep);
        }
        if self.display_asleep.swap(false, Ordering::SeqCst) {
            set_monitor_power(true);
        }
        match name {
            "select" if self.mouse_mode.load(Ordering::SeqCst) => self.mouse_click(),
            "select" => press_key(VK_RETURN),
            "play_pause" => self.handle_play_pause(),
            "siri" | "toggle_mode" => {
                let m = !self.mouse_mode.load(Ordering::SeqCst);
                self.mouse_mode.store(m, Ordering::SeqCst);
                info!("[Windows] input mode -> {}", if m { "mouse" } else { "direction" });
            }
            _ => match vk_for_button(name) {
                Some(vk) => press_key(vk),
                None => warn!("[Windows] no handler for button: {name}"),
            },
        }
    }

    fn on_touch(&self, dx: f64, dy: f64, phase: TouchPhase) {
        if !self.mouse_mode.load(Ordering::SeqCst) {
            return;
        }
        match phase {
            // Re-sync with the real cursor in case the user moved the physical mouse.
            TouchPhase::Began => *self.cursor.lock().unwrap() = cursor_pos(),
            TouchPhase::Moved => self.mouse_move(dx, dy),
            TouchPhase::Ended => {}
        }
    }

    fn on_trackpad_mode_changed(&self, mode: TrackpadMode) {
        self.mouse_mode.store(mode.is_mouse(), Ordering::SeqCst);
        info!("[Windows] trackpad mode -> {}", mode.as_str());
    }

    fn on_mode_changed(&self, mouse_mode: bool) {
        self.mouse_mode.store(mouse_mode, Ordering::SeqCst);
    }

    fn on_touchpad_settings_changed(&self, speed: f64, acceleration: bool, verbose_events: bool) {
        let speed = speed.clamp(0.1, 10.0);
        self.mouse_speed.store(speed.to_bits(), Ordering::Relaxed);
        self.mouse_accel.store(acceleration, Ordering::Relaxed);
        self.verbose_events.store(verbose_events, Ordering::Relaxed);
        info!("[Windows] touchpad settings: speed={speed:.2}x accel={acceleration} verbose={verbose_events}");
    }

    fn get_touchpad_settings(&self) -> (f64, bool, bool) {
        (
            f64::from_bits(self.mouse_speed.load(Ordering::Relaxed)),
            self.mouse_accel.load(Ordering::Relaxed),
            self.verbose_events.load(Ordering::Relaxed),
        )
    }

    fn get_screen_size(&self) -> Option<(f64, f64)> {
        let (l, t, r, b) = virtual_screen();
        Some((r - l, b - t))
    }

    fn on_event(&self, kind: EventKind, detail: &str) {
        match kind {
            EventKind::MouseClick => self.mouse_click(),
            EventKind::ClientConnected => info!("[Windows] iPhone connected: {detail}"),
            EventKind::ClientDisconnected => info!("[Windows] iPhone disconnected: {detail}"),
            _ => self.log(&format!("event: {kind:?} {detail}")),
        }
    }
}
