// Copyright (c) 2025 vivo Mobile Communication Co., Ltd.
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
use serde_json::{json, Value};
use std::fs::File;
use std::io::Read as _;

use crate::caps::error_result;

const BME280_DEVICE: &str = "/dev/bme2800";
const BME280_REPORT_VERSION: u8 = 1;
const BME280_REPORT_SIZE: usize = 13;

#[derive(Debug, Deserialize)]
pub struct Bme280ReadArgs {
    sensor: String,
}

/// Read one measurement from the kernel BME280 character device.
///
/// The kernel driver returns a versioned, little-endian report:
/// byte 0 is the format version, followed by temperature in milli-degrees C,
/// pressure in Pa, and relative humidity in thousandths of a percent.
pub struct Bme280Caps;

impl Bme280Caps {
    pub fn read(args: Bme280ReadArgs) -> Value {
        if args.sensor != "temperature_humidity" {
            return error_result(
                "invalid_args",
                format!(
                    "unsupported sensor '{}'; expected temperature_humidity",
                    args.sensor
                ),
            );
        }

        let mut device = match File::open(BME280_DEVICE) {
            Ok(device) => device,
            Err(error) => {
                return error_result(
                    "sensor_unavailable",
                    format!("open {BME280_DEVICE} failed: {error}"),
                );
            }
        };

        let mut report = [0u8; BME280_REPORT_SIZE];
        if let Err(error) = device.read_exact(&mut report) {
            return error_result(
                "sensor_read_failed",
                format!("read {BME280_DEVICE} failed: {error}"),
            );
        }

        if report[0] != BME280_REPORT_VERSION {
            return error_result(
                "sensor_protocol_error",
                format!("unsupported BME280 report version {}", report[0]),
            );
        }

        let temperature_milli_celsius =
            i32::from_le_bytes([report[1], report[2], report[3], report[4]]);
        let pressure_pascals = u32::from_le_bytes([report[5], report[6], report[7], report[8]]);
        let humidity_milli_percent =
            u32::from_le_bytes([report[9], report[10], report[11], report[12]]);

        json!({
            "ok": true,
            "sensor": "temperature_humidity",
            "temperature": temperature_milli_celsius as f32 / 1_000.0,
            "pressure": pressure_pascals,
            "humidity": humidity_milli_percent as f32 / 1_000.0,
            "temperature_unit": "celsius",
            "pressure_unit": "pascal",
            "humidity_unit": "percent",
        })
    }
}
