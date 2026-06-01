#![cfg(target_os = "windows")]

use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tauri::{PhysicalPosition, Position, WebviewWindow, WindowEvent};
use windows::Win32::Foundation::{LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetMessageW, SetWindowsHookExW, TranslateMessage,
    UnhookWindowsHookEx, HHOOK, MSLLHOOKSTRUCT, WH_MOUSE_LL, WM_QUIT,
};

use crate::app::config::WindowConfig;

const HOT_ZONE_PX: i32 = 10;
const EXPAND_ANIMATION_MS: u64 = 200;
const SNAP_ANIMATION_MS: u64 = 150;
const MOUSE_LEAVE_DELAY_MS: u64 = 500;
const CURSOR_POLL_INTERVAL_MS: u64 = 33;
const ANIMATION_TICK_MS: u64 = 16;

static CURSOR_X: AtomicI32 = AtomicI32::new(0);
static CURSOR_Y: AtomicI32 = AtomicI32::new(0);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Edge {
    Left,
    Right,
    Top,
}

#[derive(Clone, Copy, Debug)]
enum Action {
    Expand { edge: Edge },
    Snap { edge: Edge },
}

#[derive(Clone, Copy, Debug)]
struct RectI32 {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

impl RectI32 {
    fn width(&self) -> i32 {
        self.right - self.left
    }

    fn height(&self) -> i32 {
        self.bottom - self.top
    }

    fn contains_point(&self, x: i32, y: i32) -> bool {
        x >= self.left && x <= self.right && y >= self.top && y <= self.bottom
    }
}

#[derive(Clone, Copy, Debug)]
pub struct EdgeSnapConfig {
    pub enabled: bool,
    pub threshold: i32,
    pub peek_width: i32,
    pub snap_delay_ms: u64,
}

impl EdgeSnapConfig {
    pub fn from_window_config(config: &WindowConfig) -> Self {
        Self {
            enabled: config.edge_snap_enabled,
            threshold: config.edge_snap_threshold as i32,
            peek_width: config.edge_snap_peek_width as i32,
            snap_delay_ms: config.edge_snap_delay as u64,
        }
    }
}

#[derive(Debug)]
enum Phase {
    Normal,
    Pending {
        edge: Edge,
        gen: u64,
    },
    Snapped {
        edge: Edge,
    },
    Expanded {
        edge: Edge,
        mouse_left_at: Option<Instant>,
    },
    Animating,
}

#[derive(Debug)]
struct Inner {
    phase: Phase,
}

pub struct EdgeSnapManager {
    window: WebviewWindow,
    config: EdgeSnapConfig,
    inner: Arc<Mutex<Inner>>,
    pending_gen: Arc<AtomicU64>,
    _mouse_hook: Option<MouseHook>,
}

impl EdgeSnapManager {
    pub fn new(window: WebviewWindow, config: EdgeSnapConfig) -> Self {
        let mouse_hook = if config.enabled {
            MouseHook::start()
        } else {
            None
        };

        let manager = Self {
            window,
            config,
            inner: Arc::new(Mutex::new(Inner {
                phase: Phase::Normal,
            })),
            pending_gen: Arc::new(AtomicU64::new(0)),
            _mouse_hook: mouse_hook,
        };

        manager.start_cursor_watcher();
        manager
    }

    pub fn handle_window_event(&self, event: &WindowEvent) {
        if !self.config.enabled {
            return;
        }

        match event {
            WindowEvent::Moved(_) | WindowEvent::Resized(_) => {
                self.on_window_moved_or_resized();
            }
            _ => {}
        }
    }

    fn on_window_moved_or_resized(&self) {
        let Ok((window_rect, work_area)) = self.get_window_and_work_area() else {
            return;
        };

        let edge = detect_edge(&window_rect, &work_area, self.config.threshold);

        let mut inner = self.inner.lock().expect("edge snap inner poisoned");
        match inner.phase {
            Phase::Animating => {
                return;
            }
            Phase::Snapped { .. } | Phase::Expanded { .. } => {
                if edge.is_none() {
                    inner.phase = Phase::Normal;
                }
                return;
            }
            Phase::Pending { .. } => {
                if edge.is_none() {
                    inner.phase = Phase::Normal;
                }
                return;
            }
            Phase::Normal => {}
        }
        drop(inner);

        let Some(edge) = edge else {
            return;
        };

        let gen = self.pending_gen.fetch_add(1, Ordering::Relaxed) + 1;
        {
            let mut inner = self.inner.lock().expect("edge snap inner poisoned");
            inner.phase = Phase::Pending { edge, gen };
        }

        let inner = self.inner.clone();
        let window = self.window.clone();
        let config = self.config;
        let pending_gen = self.pending_gen.clone();

        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(Duration::from_millis(config.snap_delay_ms)).await;

            let edge = {
                let mut guard = inner.lock().expect("edge snap inner poisoned");
                let Phase::Pending { edge, gen } = guard.phase else {
                    return;
                };
                if gen != pending_gen.load(Ordering::Relaxed) {
                    return;
                }

                guard.phase = Phase::Animating;
                edge
            };

            let _ = snap_to_edge(&window, config, edge).await;

            let mut guard = inner.lock().expect("edge snap inner poisoned");
            guard.phase = Phase::Snapped { edge };
        });
    }

    fn start_cursor_watcher(&self) {
        if !self.config.enabled {
            return;
        }

        let window = self.window.clone();
        let inner = self.inner.clone();
        let config = self.config;

        tauri::async_runtime::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(CURSOR_POLL_INTERVAL_MS)).await;

                let x = CURSOR_X.load(Ordering::Relaxed);
                let y = CURSOR_Y.load(Ordering::Relaxed);

                let Ok((window_rect, work_area)) = get_window_and_work_area(&window) else {
                    continue;
                };

                let action = {
                    let mut guard = inner.lock().expect("edge snap inner poisoned");
                    match guard.phase {
                        Phase::Snapped { edge } => {
                            if cursor_in_hot_zone(edge, x, y, &window_rect, &work_area) {
                                guard.phase = Phase::Animating;
                                Some(Action::Expand { edge })
                            } else {
                                None
                            }
                        }
                        Phase::Expanded {
                            edge,
                            ref mut mouse_left_at,
                        } => {
                            if window_rect.contains_point(x, y) {
                                *mouse_left_at = None;
                                None
                            } else {
                                let left_at = mouse_left_at.get_or_insert_with(Instant::now);
                                if left_at.elapsed() >= Duration::from_millis(MOUSE_LEAVE_DELAY_MS)
                                {
                                    guard.phase = Phase::Animating;
                                    Some(Action::Snap { edge })
                                } else {
                                    None
                                }
                            }
                        }
                        _ => None,
                    }
                };

                match action {
                    Some(Action::Expand { edge }) => {
                        let _ =
                            expand_from_edge(&window, config, edge, &window_rect, &work_area).await;
                        let mut guard = inner.lock().expect("edge snap inner poisoned");
                        guard.phase = Phase::Expanded {
                            edge,
                            mouse_left_at: None,
                        };
                    }
                    Some(Action::Snap { edge }) => {
                        let _ = snap_to_edge(&window, config, edge).await;
                        let mut guard = inner.lock().expect("edge snap inner poisoned");
                        guard.phase = Phase::Snapped { edge };
                    }
                    None => {}
                }
            }
        });
    }

    fn get_window_and_work_area(&self) -> Result<(RectI32, RectI32), String> {
        get_window_and_work_area(&self.window)
    }
}

fn cursor_in_hot_zone(edge: Edge, x: i32, y: i32, window_rect: &RectI32, work: &RectI32) -> bool {
    match edge {
        Edge::Left => {
            x <= work.left + HOT_ZONE_PX && y >= window_rect.top && y <= window_rect.bottom
        }
        Edge::Right => {
            x >= work.right - HOT_ZONE_PX && y >= window_rect.top && y <= window_rect.bottom
        }
        Edge::Top => x <= window_rect.right && x >= window_rect.left && y <= work.top + HOT_ZONE_PX,
    }
}

fn detect_edge(window_rect: &RectI32, work: &RectI32, threshold: i32) -> Option<Edge> {
    if window_rect.left <= work.left + threshold {
        return Some(Edge::Left);
    }
    if window_rect.right >= work.right - threshold {
        return Some(Edge::Right);
    }
    if window_rect.top <= work.top + threshold {
        return Some(Edge::Top);
    }
    None
}

fn get_window_and_work_area(window: &WebviewWindow) -> Result<(RectI32, RectI32), String> {
    let pos = window
        .outer_position()
        .map_err(|e| format!("Failed to get outer position: {}", e))?;
    let size = window
        .outer_size()
        .map_err(|e| format!("Failed to get outer size: {}", e))?;

    let window_rect = RectI32 {
        left: pos.x,
        top: pos.y,
        right: pos.x + size.width as i32,
        bottom: pos.y + size.height as i32,
    };

    let center = POINT {
        x: pos.x + (size.width as i32 / 2),
        y: pos.y + (size.height as i32 / 2),
    };

    unsafe {
        let monitor = MonitorFromPoint(center, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        GetMonitorInfoW(monitor, &mut info)
            .ok()
            .map_err(|e| format!("GetMonitorInfoW failed: {}", e))?;
        let work = RectI32 {
            left: info.rcWork.left,
            top: info.rcWork.top,
            right: info.rcWork.right,
            bottom: info.rcWork.bottom,
        };
        Ok((window_rect, work))
    }
}

async fn snap_to_edge(
    window: &WebviewWindow,
    config: EdgeSnapConfig,
    edge: Edge,
) -> Result<(), ()> {
    let Ok((window_rect, work)) = get_window_and_work_area(window) else {
        return Err(());
    };

    let (target_x, target_y) =
        snapped_target_position(&window_rect, &work, config.peek_width, edge);
    animate_window_position(
        window,
        window_rect.left,
        window_rect.top,
        target_x,
        target_y,
        SNAP_ANIMATION_MS,
        Ease::In,
    )
    .await;
    Ok(())
}

async fn expand_from_edge(
    window: &WebviewWindow,
    _config: EdgeSnapConfig,
    edge: Edge,
    window_rect: &RectI32,
    work: &RectI32,
) -> Result<(), ()> {
    let (target_x, target_y) = expanded_target_position(window_rect, work, edge);
    animate_window_position(
        window,
        window_rect.left,
        window_rect.top,
        target_x,
        target_y,
        EXPAND_ANIMATION_MS,
        Ease::Out,
    )
    .await;
    // Bring the window to the front so it appears above other windows.
    let _ = window.show();
    let _ = window.set_focus();
    Ok(())
}

fn snapped_target_position(
    window_rect: &RectI32,
    work: &RectI32,
    peek: i32,
    edge: Edge,
) -> (i32, i32) {
    match edge {
        Edge::Left => (work.left - (window_rect.width() - peek), window_rect.top),
        Edge::Right => (work.right - peek, window_rect.top),
        Edge::Top => (window_rect.left, work.top - (window_rect.height() - peek)),
    }
}

fn expanded_target_position(window_rect: &RectI32, work: &RectI32, edge: Edge) -> (i32, i32) {
    match edge {
        Edge::Left => (work.left, window_rect.top),
        Edge::Right => (work.right - window_rect.width(), window_rect.top),
        Edge::Top => (window_rect.left, work.top),
    }
}

#[derive(Clone, Copy)]
enum Ease {
    In,
    Out,
}

fn ease_value(t: f64, ease: Ease) -> f64 {
    let t = t.clamp(0.0, 1.0);
    match ease {
        Ease::In => t * t,
        Ease::Out => 1.0 - (1.0 - t) * (1.0 - t),
    }
}

async fn animate_window_position(
    window: &WebviewWindow,
    from_x: i32,
    from_y: i32,
    to_x: i32,
    to_y: i32,
    duration_ms: u64,
    ease: Ease,
) {
    if duration_ms == 0 || (from_x == to_x && from_y == to_y) {
        let _ = window.set_position(Position::Physical(PhysicalPosition { x: to_x, y: to_y }));
        return;
    }

    let start = Instant::now();
    let duration = Duration::from_millis(duration_ms);

    loop {
        let elapsed = start.elapsed();
        let done = elapsed >= duration;
        let t = if done {
            1.0
        } else {
            elapsed.as_secs_f64() / duration.as_secs_f64()
        };
        let k = ease_value(t, ease);

        let x = from_x as f64 + (to_x - from_x) as f64 * k;
        let y = from_y as f64 + (to_y - from_y) as f64 * k;
        let _ = window.set_position(Position::Physical(PhysicalPosition {
            x: x.round() as i32,
            y: y.round() as i32,
        }));

        if done {
            break;
        }

        tokio::time::sleep(Duration::from_millis(ANIMATION_TICK_MS)).await;
    }
}

struct MouseHook {
    _thread: JoinHandle<()>,
    hook: isize,
}

impl MouseHook {
    fn start() -> Option<Self> {
        let (tx, rx) = std::sync::mpsc::channel::<isize>();

        let thread = std::thread::spawn(move || unsafe {
            let hook_result = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), None, 0);

            if let Ok(hook) = hook_result {
                let _ = tx.send(hook.0 as isize);

                let mut msg = windows::Win32::UI::WindowsAndMessaging::MSG::default();
                while GetMessageW(&mut msg, None, 0, 0).into() {
                    if msg.message == WM_QUIT {
                        break;
                    }
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
                let _ = UnhookWindowsHookEx(hook);
            } else {
                let _ = tx.send(0);
            }
        });

        let hook = rx.recv().ok()?;
        if hook == 0 {
            let _ = thread.join();
            return None;
        }

        Some(Self {
            _thread: thread,
            hook,
        })
    }
}

impl Drop for MouseHook {
    fn drop(&mut self) {
        unsafe {
            let _ = UnhookWindowsHookEx(HHOOK(self.hook as *mut _));
        }
    }
}

unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        let info = *(lparam.0 as *const MSLLHOOKSTRUCT);
        CURSOR_X.store(info.pt.x, Ordering::Relaxed);
        CURSOR_Y.store(info.pt.y, Ordering::Relaxed);
    }
    CallNextHookEx(None, code, wparam, lparam)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_edge_left_right_top() {
        let work = RectI32 {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
        };
        let threshold = 20;

        let left = RectI32 {
            left: 10,
            top: 100,
            right: 510,
            bottom: 600,
        };
        assert_eq!(detect_edge(&left, &work, threshold), Some(Edge::Left));

        let right = RectI32 {
            left: 1420,
            top: 100,
            right: 1910,
            bottom: 600,
        };
        assert_eq!(detect_edge(&right, &work, threshold), Some(Edge::Right));

        let top = RectI32 {
            left: 100,
            top: 5,
            right: 600,
            bottom: 505,
        };
        assert_eq!(detect_edge(&top, &work, threshold), Some(Edge::Top));
    }

    #[test]
    fn snapped_target_respects_peek_width() {
        let work = RectI32 {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
        };
        let rect = RectI32 {
            left: 100,
            top: 200,
            right: 900,
            bottom: 700,
        };
        let peek = 5;

        let (x, y) = snapped_target_position(&rect, &work, peek, Edge::Left);
        assert_eq!(y, 200);
        assert_eq!(x, 0 - (rect.width() - peek));

        let (x, y) = snapped_target_position(&rect, &work, peek, Edge::Right);
        assert_eq!(y, 200);
        assert_eq!(x, 1920 - peek);

        let (x, y) = snapped_target_position(&rect, &work, peek, Edge::Top);
        assert_eq!(x, 100);
        assert_eq!(y, 0 - (rect.height() - peek));
    }

    #[test]
    fn ease_in_out_monotonic() {
        let a = ease_value(0.0, Ease::In);
        let b = ease_value(0.5, Ease::In);
        let c = ease_value(1.0, Ease::In);
        assert!(a <= b && b <= c);

        let a = ease_value(0.0, Ease::Out);
        let b = ease_value(0.5, Ease::Out);
        let c = ease_value(1.0, Ease::Out);
        assert!(a <= b && b <= c);
        assert_eq!(c, 1.0);
    }
}
