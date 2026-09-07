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

use serde::Deserialize;
use serde_json::{Value, json};
use std::fs::OpenOptions;
use std::io::Write as _;

use crate::caps::error_result;

const LED_MATRIX_DEVICE: &str = "/dev/max7219";
const MATRIX_SIZE: usize = 8;
const DEFAULT_FRAME_MS: u64 = 200;
const MAX_FRAME_MS: u64 = 10_000;
const MAX_FRAME_COUNT: usize = 32;
const MAX_REPEAT: u32 = 16;
const MAX_ANIMATION_DURATION_MS: u64 = 30_000;

#[derive(Debug, Deserialize)]
pub struct LedMatrixFrame {
    rows: Vec<String>,
    #[serde(default)]
    hold_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LedMatrixDrawArgs {
    Image {
        rows: Vec<String>,
    },
    Animation {
        frames: Vec<LedMatrixFrame>,
        #[serde(default)]
        frame_ms: Option<u64>,
        #[serde(default)]
        repeat: Option<u32>,
    },
}

pub struct LedMatrixCaps;

impl LedMatrixCaps {
    pub fn draw(args: LedMatrixDrawArgs) -> Value {
        match args {
            LedMatrixDrawArgs::Image { rows } => Self::draw_image(rows),
            LedMatrixDrawArgs::Animation {
                frames,
                frame_ms,
                repeat,
            } => Self::play_animation(
                frames,
                frame_ms.unwrap_or(DEFAULT_FRAME_MS),
                repeat.unwrap_or(1),
            ),
        }
    }

    fn draw_image(rows: Vec<String>) -> Value {
        let raw = match encode_rows(&rows) {
            Ok(raw) => raw,
            Err(error) => return error_result("invalid_args", error),
        };
        let mut device = match open_device() {
            Ok(device) => device,
            Err(error) => return error_result("device_unavailable", error),
        };
        if let Err(error) = device.write_all(&raw) {
            return error_result(
                "io_error",
                format!("write {LED_MATRIX_DEVICE} failed: {error}"),
            );
        }

        json!({
            "ok": true,
            "action": "led_matrix_draw",
            "mode": "image",
            "width": MATRIX_SIZE,
            "height": MATRIX_SIZE,
            "rows": rows,
            "hardware": true
        })
    }

    fn play_animation(frames: Vec<LedMatrixFrame>, frame_ms: u64, repeat: u32) -> Value {
        if frames.is_empty() {
            return error_result("invalid_args", String::from("frames cannot be empty"));
        }
        if frames.len() > MAX_FRAME_COUNT {
            return error_result(
                "invalid_args",
                format!("frames cannot exceed {MAX_FRAME_COUNT}"),
            );
        }
        if frame_ms == 0 || frame_ms > MAX_FRAME_MS {
            return error_result(
                "invalid_args",
                format!("frame_ms must be 1..={MAX_FRAME_MS}"),
            );
        }
        if repeat == 0 || repeat > MAX_REPEAT {
            return error_result("invalid_args", format!("repeat must be 1..={MAX_REPEAT}"));
        }

        let mut encoded_frames = Vec::with_capacity(frames.len());
        let mut duration_per_round = 0u64;
        for (index, frame) in frames.iter().enumerate() {
            let raw = match encode_rows(&frame.rows) {
                Ok(raw) => raw,
                Err(error) => {
                    return error_result(
                        "invalid_args",
                        format!("frame {} is invalid: {error}", index + 1),
                    );
                }
            };
            let hold_ms = frame.hold_ms.unwrap_or(frame_ms);
            if hold_ms == 0 || hold_ms > MAX_FRAME_MS {
                return error_result(
                    "invalid_args",
                    format!("frame {} hold_ms must be 1..={MAX_FRAME_MS}", index + 1),
                );
            }
            duration_per_round = match duration_per_round.checked_add(hold_ms) {
                Some(duration) => duration,
                None => {
                    return error_result(
                        "invalid_args",
                        String::from("animation duration overflow"),
                    );
                }
            };
            encoded_frames.push((raw, hold_ms));
        }

        let total_duration = duration_per_round.checked_mul(repeat as u64);
        if total_duration.unwrap_or(u64::MAX) > MAX_ANIMATION_DURATION_MS {
            return error_result(
                "invalid_args",
                format!("animation duration cannot exceed {MAX_ANIMATION_DURATION_MS} ms"),
            );
        }

        let mut device = match open_device() {
            Ok(device) => device,
            Err(error) => return error_result("device_unavailable", error),
        };
        for round in 0..repeat {
            println!(
                "led_matrix> animation round={}/{} frame_count={}",
                round + 1,
                repeat,
                encoded_frames.len()
            );
            for (index, (raw, hold_ms)) in encoded_frames.iter().enumerate() {
                if let Err(error) = device.write_all(raw) {
                    return error_result(
                        "io_error",
                        format!(
                            "write {LED_MATRIX_DEVICE} failed at frame {}: {error}",
                            index + 1
                        ),
                    );
                }
                sleep_ms(*hold_ms);
            }
        }

        json!({
            "ok": true,
            "action": "led_matrix_draw",
            "mode": "animation",
            "frame_count": frames.len(),
            "repeat": repeat,
            "duration_ms": total_duration.unwrap_or(0),
            "hardware": true
        })
    }
}

fn open_device() -> Result<std::fs::File, String> {
    OpenOptions::new()
        .write(true)
        .open(LED_MATRIX_DEVICE)
        .map_err(|error| format!("open {LED_MATRIX_DEVICE} failed: {error}"))
}

/// Encode top-to-bottom rows into the MAX7219 driver's eight raw bytes.
/// The leftmost pixel maps to bit 7. `1`, `#`, `X`, and `*` are on; `0`, `.`,
/// and space are off.
fn encode_rows(rows: &[String]) -> Result<[u8; MATRIX_SIZE], String> {
    if rows.len() != MATRIX_SIZE {
        return Err(format!("rows must contain exactly {MATRIX_SIZE} strings"));
    }

    let mut raw = [0u8; MATRIX_SIZE];
    for (row_index, row) in rows.iter().enumerate() {
        if row.len() != MATRIX_SIZE {
            return Err(format!(
                "row {} must contain exactly {MATRIX_SIZE} ASCII pixels",
                row_index + 1
            ));
        }

        let mut value = 0u8;
        for (column, pixel) in row.bytes().enumerate() {
            let on = match pixel {
                b'1' | b'#' | b'X' | b'x' | b'*' => true,
                b'0' | b'.' | b' ' => false,
                _ => {
                    return Err(format!(
                        "row {} contains invalid pixel '{}' at column {}",
                        row_index + 1,
                        pixel as char,
                        column + 1
                    ));
                }
            };
            if on {
                value |= 1 << (MATRIX_SIZE - column - 1);
            }
        }
        raw[row_index] = value;
    }
    Ok(raw)
}

fn sleep_ms(ms: u64) {
    let _ = librs::time::msleep(ms as u32);
}
