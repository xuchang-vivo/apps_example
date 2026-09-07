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

#![feature(cfg_boolean_literals)]

extern crate alloc;
extern crate esp_radio_sys;
extern crate libm;
extern crate librs;
extern crate rsrt;

mod agent_loop;
mod api;
mod caps;
mod client;
mod error;
mod http;
mod sse;
mod tls;
mod ui;
mod websocket_rpc;
mod wifi;

use alloc::string::ToString;
use std::{
    env,
    sync::{Arc, Mutex},
};
use tls::EmbeddedTlsTransport;

use crate::{agent_loop::AgentSession, caps::CapabilityRegistry};

struct AgentRuntime {
    client: client::Client<EmbeddedTlsTransport>,
    model: String,
    registry: CapabilityRegistry,
    session: AgentSession,
}

// BlueOS currently has no DNS resolver. Connect to the pinned IPv4 address,
// while keeping the hostname for HTTPS SNI and the HTTP Host header.
const DEEPSEEK_API_BASE: &str = "https://3.173.21.63/v1";
const DEEPSEEK_API_HOST: &str = "api.deepseek.com";

impl AgentRuntime {
    fn run_prompt(&mut self, prompt: &str) -> Result<String, String> {
        self.session
            .run_turn(&mut self.client, &self.model, &self.registry, prompt)
            .map_err(|error| error.to_string())
    }
}

fn main() -> std::io::Result<()> {
    // Establish a known off state at the earliest point so the LED does not
    // stay lit (e.g. from bootloader polarity) while WiFi and the runtime come
    // up. It stays off until the WebSocket server is ready to accept clients.
    if let Err(error) = caps::led::turn_off() {
        eprintln!("startup LED off failed: {error}");
    }

    // UI first: the face must be up before WiFi so the Connecting state is
    // visible while the radio associates. A spawn failure is non-fatal —
    // the agent loop runs headless (spec §9).
    let ui = Arc::new(Mutex::new(ui::UiState::new()));
    if let Err(error) = ui::spawn_ui_thread(ui.clone()) {
        eprintln!("ui thread spawn failed: {error}");
    }
    println!("[main] ui thread spawned, entering wifi connect");
    set_ui_state(&ui, ui::AgentState::Connecting, "connecting wifi");
    if let Err(error) = wifi::connect_wifi() {
        eprintln!("WiFi connection failed; agent server not started: {error}");
        set_ui_state(&ui, ui::AgentState::Error, "wifi failed");
        return Ok(());
    }
    main_loop(ui);
    Ok(())
}

fn set_ui_state(ui: &Mutex<ui::UiState>, agent_state: ui::AgentState, status: &str) {
    // A poisoned lock must never take startup down (spec §8/§9).
    if let Ok(mut shared) = ui.lock() {
        shared.set(agent_state, status);
    }
}

fn main_loop(ui: Arc<Mutex<ui::UiState>>) {
    // let api_key = match env::var("OPENAI_API_KEY") {
    //     Ok(value) if !value.is_empty() => value,
    //     _ => {
    //         eprintln!("OPENAI_API_KEY is not configured; agent server not started");
    //         return;
    //     }
    // };
    let api_key = String::from("sk-cbbfdb1ad3b34b6e96d177baf1fb9510");
    let endpoint = env::var("OPENAI_API_BASE").unwrap_or_else(|_| String::from(DEEPSEEK_API_BASE));
    let model = env::var("OPENAI_MODEL").unwrap_or_else(|_| String::from("deepseek-chat"));

    let registry = match CapabilityRegistry::load() {
        Ok(registry) => registry,
        Err(error) => {
            eprintln!("capability registry load failed: {error}");
            return;
        }
    };

    // Connect to the static IP while retaining DeepSeek's hostname for HTTP
    // virtual-host routing and TLS SNI.
    let client =
        match client::Client::builder(EmbeddedTlsTransport::new().with_sni(DEEPSEEK_API_HOST))
            .with_endpoint(endpoint)
            .with_host_header(DEEPSEEK_API_HOST)
            .with_api_key(api_key)
            .with_max_response_body_size(24 * 1024)
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                eprintln!("client build failed: {e}");
                return;
            }
        };

    let runtime = Arc::new(Mutex::new(AgentRuntime {
        client,
        model,
        registry,
        session: AgentSession::new(true, ui.clone()),
    }));
    set_ui_state(&ui, ui::AgentState::Idle, "ready");
    websocket_rpc::start_server(runtime);
}
