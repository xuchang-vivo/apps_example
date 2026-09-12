// Copyright (c) 2026 vivo Mobile Communication Co., Ltd.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//       http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! On-device Slint UI for the agent loop: a cartoon face on /dev/fb0 that
//! mirrors the agent's progress. See
//! docs/superpowers/specs/2026-09-07-agent-loop-slint-ui-design.md.
//!
//! Slint is built `unsafe-single-threaded`, so every Slint call happens on
//! the one UI thread spawned by `spawn_ui_thread`.

mod app_window {
    include!(env!("SLINT_UI_GENERATED"));
}
mod fb_backend;
mod math;

use std::cell::RefCell;
use std::io::{Error, ErrorKind, Result as IoResult};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::thread;

use slint::ComponentHandle;

use crate::agent_loop::MAX_TOOL_ROUNDS;
use crate::ui::app_window::MainWindow;

/// 8 KB: 64 KB exhausted the shared kernel heap together with the Slint
/// allocations (MainWindow::new() died silently); 8 KB leaves room for them
/// and still covers the UI call depth (spec §11).
const UI_THREAD_STACK_SIZE: usize = 16 * 1024;

/// How long a Done/Error face is shown before the UI reverts to Idle
/// (UI-side timer; the agent-side state is untouched, spec §3).
const DONE_ERROR_HOLD_MS: u128 = 3000;

/// Idle blink: one blink every 3 s, ~150 ms closed (spec §3).
const BLINK_PERIOD_MS: u128 = 3000;
const BLINK_CLOSED_MS: u128 = 150;

/// Spinner orbit period, milliseconds per revolution.
const SPINNER_PERIOD_MS: u128 = 1500;

/// Bottom-bar status text cap (spec §3 says 24 chars).
const MAX_STATUS_CHARS: usize = 24;

/// Agent loop states mirrored on the face. Values 0-8 are pushed to the
/// Slint `state` property as i32.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AgentState {
    Idle = 0,
    Connecting = 1,
    Thinking = 2,
    Streaming = 3,
    Reasoning = 4,
    ToolRunning = 5,
    YieldingWifi = 6,
    Done = 7,
    Error = 8,
}

impl AgentState {
    pub fn as_int(self) -> i32 {
        self as i32
    }
}

/// Copy of the shared state taken under the mutex (spec §8: lock + copy).
struct UiSnapshot {
    state: AgentState,
    round: u8,
    status: String,
}

/// Shared state written by the agent thread (`set`/`set_round`, each a
/// microsecond mutex hold, never locked across a model call) and read by
/// the UI thread (`snapshot`).
///
/// Deviation from the spec sketch: `status` is a heap `String` truncated to
/// 24 chars instead of `ArrayString<24>` — the app already links `alloc`.
/// The spec's `turn_ms` field is omitted: the UI thread owns the whole turn
/// clock (spec §3) and detects turn starts by observing Thinking with
/// round == 1.
pub struct UiState {
    state: AgentState,
    round: u8,
    status: String,
}

impl UiState {
    pub fn new() -> Self {
        Self {
            state: AgentState::Idle,
            round: 0,
            status: String::new(),
        }
    }

    /// Agent-side write. Truncates `status` on a char boundary so the
    /// bottom-bar Text stays inside its box.
    pub fn set(&mut self, state: AgentState, status: &str) {
        self.state = state;
        let mut end = status.len().min(MAX_STATUS_CHARS);
        while !status.is_char_boundary(end) {
            end -= 1;
        }
        self.status.clear();
        self.status.push_str(&status[..end]);
    }

    pub fn set_round(&mut self, round: u8) {
        self.round = round;
    }

    /// Module-internal: only `UiRefresher::update` calls this, so a private
    /// return type is fine (a `pub fn` returning the private `UiSnapshot`
    /// would trip the `private_interfaces` lint).
    fn snapshot(&self) -> UiSnapshot {
        UiSnapshot {
            state: self.state,
            round: self.round,
            status: self.status.clone(),
        }
    }
}

/// Bridges the shared `UiState` into the `MainWindow` properties, once per
/// frame, before `update_timers_and_animations`. The infinite pulse/z-lift
/// animations keep the window dirty, so the loop repaints at ~12 fps
/// regardless; the conditional pushes below exist to skip redundant
/// property writes and per-frame `format!`/`String` allocations.
struct UiRefresher {
    ui: Option<slint::Weak<MainWindow>>,
    shared: Arc<Mutex<UiState>>,
    /// Last agent-side state observed (drives the transition timers).
    agent_state: AgentState,
    /// Deadline after which a Done/Error face reverts to Idle.
    revert_at: Option<u128>,
    /// When the current turn was first observed (UI-side turn clock).
    turn_started_at: Option<u128>,
    /// Last values pushed to Slint (skip redundant property writes).
    display_round: u8,
    display_status: String,
    display_turn_text: String,
    display_blink: f32,
    display_round_frac: f32,
}

impl UiRefresher {
    fn new(shared: Arc<Mutex<UiState>>) -> Self {
        Self {
            ui: None,
            shared,
            agent_state: AgentState::Idle,
            revert_at: None,
            turn_started_at: None,
            display_round: u8::MAX,
            display_status: String::new(),
            display_turn_text: String::new(),
            display_blink: 0.0,
            display_round_frac: f32::NAN,
        }
    }

    fn attach_ui(&mut self, ui: slint::Weak<MainWindow>) {
        self.ui = Some(ui);
    }

    fn update(&mut self, now_ms: u128) {
        let ui = match self.ui.as_ref().and_then(|ui| ui.upgrade()) {
            Some(ui) => ui,
            None => return,
        };

        let snapshot = match self.shared.lock() {
            Ok(state) => state.snapshot(),
            // Poisoned: the writer side is gone; keep the last face.
            Err(_) => return,
        };

        if snapshot.state != self.agent_state {
            self.agent_state = snapshot.state;
            self.revert_at = match snapshot.state {
                AgentState::Done | AgentState::Error => Some(now_ms + DONE_ERROR_HOLD_MS),
                _ => None,
            };
            // A new turn always begins as Thinking with round 1: the round
            // counter is written at the top of the model-round loop, before
            // the Thinking state. This always lasts the whole first HTTP
            // call, so an 80 ms poll cannot miss it.
            if snapshot.state == AgentState::Thinking && snapshot.round == 1 {
                self.turn_started_at = Some(now_ms);
            }
        }

        // Display state: Done/Error hold for 3 s, then Idle.
        let display_state = match self.revert_at {
            Some(deadline) if now_ms >= deadline => AgentState::Idle,
            _ => self.agent_state,
        };
        ui.set_state(display_state.as_int());

        if snapshot.round != self.display_round {
            self.display_round = snapshot.round;
            ui.set_round_text(format!("R{}/{}", snapshot.round, MAX_TOOL_ROUNDS).into());
            let frac = snapshot.round as f32 / MAX_TOOL_ROUNDS as f32;
            if frac != self.display_round_frac {
                self.display_round_frac = frac;
                ui.set_round_frac(frac);
            }
        }

        if snapshot.status != self.display_status {
            self.display_status = snapshot.status;
            ui.set_status(self.display_status.clone().into());
        }

        // UI-side turn clock (spec §3): the agent is blocked inside
        // chat_completion while a turn runs, so it cannot tick this.
        // Seconds with one decimal: the dialog TIME row reads "t 12.4s".
        let turn_text = match self.turn_started_at {
            Some(start) if display_state != AgentState::Idle => {
                let secs = (now_ms.saturating_sub(start)) as f32 / 1000.0;
                format!("t{:.1}s", secs)
            }
            _ => String::from("t0.0s"),
        };
        if turn_text != self.display_turn_text {
            self.display_turn_text = turn_text;
            ui.set_turn_text(self.display_turn_text.clone().into());
        }

        // Idle blink: two-level, pushed only on change.
        let blink = if display_state == AgentState::Idle {
            if now_ms % BLINK_PERIOD_MS < BLINK_CLOSED_MS {
                0.1
            } else {
                1.0
            }
        } else {
            1.0
        };
        if blink != self.display_blink {
            self.display_blink = blink;
            ui.set_blink(blink);
        }

        // Spinner orbit (Thinking only; the dot is invisible otherwise).
        if display_state == AgentState::Thinking {
            let phase = (now_ms % SPINNER_PERIOD_MS) as f32 / SPINNER_PERIOD_MS as f32;
            let angle = phase * 2.0 * core::f32::consts::PI;
            ui.set_spin_x(libm::cosf(angle));
            ui.set_spin_y(libm::sinf(angle));
        }
    }
}

/// Minimal Slint platform: one software-rendered window on /dev/fb0.
struct BluekernelBackend {
    window: RefCell<Option<Rc<slint::platform::software_renderer::MinimalSoftwareWindow>>>,
    refresher: Rc<RefCell<UiRefresher>>,
}

impl BluekernelBackend {
    fn new(refresher: Rc<RefCell<UiRefresher>>) -> Self {
        Self {
            window: RefCell::new(None),
            refresher,
        }
    }
}

impl slint::platform::Platform for BluekernelBackend {
    fn create_window_adapter(
        &self,
    ) -> Result<Rc<dyn slint::platform::WindowAdapter>, slint::PlatformError> {
        let window = slint::platform::software_renderer::MinimalSoftwareWindow::new(
            slint::platform::software_renderer::RepaintBufferType::ReusedBuffer,
        );
        window.set_size(slint::PhysicalSize::new(
            fb_backend::LCD_H_RES as u32,
            fb_backend::LCD_V_RES as u32,
        ));
        self.window.replace(Some(window.clone()));
        Ok(window)
    }

    fn duration_since_start(&self) -> std::time::Duration {
        std::time::Duration::from_millis(fb_backend::uptime_millis() as u64)
    }

    fn run_event_loop(&self) -> Result<(), slint::PlatformError> {
        // Spec §9: if /dev/fb0 cannot be opened, exit the thread; the agent
        // loop keeps running headless.
        let mut fb = fb_backend::FbFile::open()
            .map_err(|err| slint::PlatformError::Other(err.to_string()))?;

        loop {
            // Push the shared state into the window before advancing the
            // animation clock so this frame reflects the latest snapshot.
            self.refresher
                .borrow_mut()
                .update(fb_backend::uptime_millis());

            slint::platform::update_timers_and_animations();

            if let Some(window) = self.window.borrow().clone() {
                window.request_redraw();
                let mut draw_result = Ok(());
                window.draw_if_needed(|renderer| {
                    // Render line-by-line to avoid a full-frame RGB565
                    // allocation (~300 KB); the trade-off is no Slint
                    // Path item support.
                    renderer
                        .render_by_line(fb_backend::FbLineBuffer::new(&mut fb, &mut draw_result));
                });
                // Spec §9: a mid-frame write failure skips the damaged
                // frame and retries on the next one.
                if let Err(error) = draw_result {
                    eprintln!("[ui] frame write failed: {error}");
                }
            }

            let _ = librs::time::msleep(fb_backend::FRAME_DELAY_MS);
        }
    }
}

fn ui_thread_main(shared: Arc<Mutex<UiState>>) -> IoResult<()> {
    println!("[ui] starting agent_loop slint ui");

    let refresher = Rc::new(RefCell::new(UiRefresher::new(shared)));
    let backend = BluekernelBackend::new(refresher.clone());

    slint::platform::set_platform(Box::new(backend))
        .map_err(|err| Error::new(ErrorKind::Other, err.to_string()))?;

    let ui = MainWindow::new().map_err(|err| Error::new(ErrorKind::Other, err.to_string()))?;

    refresher.borrow_mut().attach_ui(ui.as_weak());

    slint::run_event_loop().map_err(|err| Error::new(ErrorKind::Other, err.to_string()))
}

/// Spawns the UI thread. The JoinHandle is dropped on purpose: the thread
/// runs until process exit and is never joined (the agent loop must not
/// wait on the UI).
pub fn spawn_ui_thread(shared: Arc<Mutex<UiState>>) -> IoResult<()> {
    let handle = thread::Builder::new()
        .name(String::from("slint-ui"))
        .stack_size(UI_THREAD_STACK_SIZE)
        .spawn(move || {
            if let Err(error) = ui_thread_main(shared) {
                // Spec §9: a UI failure never takes the agent loop down.
                eprintln!("[ui] thread exiting: {error}");
            }
        })?;
    drop(handle);
    Ok(())
}
