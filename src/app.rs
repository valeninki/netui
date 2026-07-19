//! UI renderers must use `Color::Reset` or named ANSI 16-color variants only.
//! Do not use `Color::Rgb`, `Color::Indexed`, or hardcoded RGB/hex color values.

use ratatui::widgets::ListState;
use tokio::sync::mpsc;

use crate::backend::{NetworkInterface, WifiNetwork, WifiSecurity};

pub const BACKEND_EVENT_CHANNEL_CAPACITY: usize = 64;

#[derive(Debug, Clone)]
pub enum BackendEvent {
    WifiNetworksUpdated(Vec<WifiNetwork>),
    InterfacesUpdated(Vec<NetworkInterface>),
    ConnectionStatus(String),
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputMode {
    Normal,
    Input { ssid: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectedNetworkAction {
    Connect { ssid: String, password: String },
    Disconnect { ssid: String },
}

pub struct App {
    wifi_networks: Vec<WifiNetwork>,
    interfaces: Vec<NetworkInterface>,
    backend_event_tx: mpsc::Sender<BackendEvent>,
    backend_event_rx: mpsc::Receiver<BackendEvent>,
    wifi_list_state: ListState,
    input_mode: InputMode,
    input_buffer: String,
    should_quit: bool,
    status_message: String,
    last_error: Option<String>,
}

impl App {
    pub fn new() -> Self {
        let (backend_event_tx, backend_event_rx) = mpsc::channel(BACKEND_EVENT_CHANNEL_CAPACITY);

        Self {
            wifi_networks: Vec::new(),
            interfaces: Vec::new(),
            backend_event_tx,
            backend_event_rx,
            wifi_list_state: ListState::default(),
            input_mode: InputMode::Normal,
            input_buffer: String::new(),
            should_quit: false,
            status_message: "Waiting for backend updates".into(),
            last_error: None,
        }
    }

    pub fn backend_event_sender(&self) -> mpsc::Sender<BackendEvent> {
        self.backend_event_tx.clone()
    }

    pub async fn recv_backend_event(&mut self) -> Option<BackendEvent> {
        self.backend_event_rx.recv().await
    }

    pub fn apply_backend_event(&mut self, event: BackendEvent) {
        match event {
            BackendEvent::WifiNetworksUpdated(networks) => {
                self.wifi_networks = networks;
                self.normalize_wifi_selection();
                self.last_error = None;
            }
            BackendEvent::InterfacesUpdated(interfaces) => {
                self.interfaces = interfaces;
                self.last_error = None;
            }
            BackendEvent::ConnectionStatus(status) => {
                self.status_message = status;
                self.last_error = None;
            }
            BackendEvent::Error(error) => self.last_error = Some(format!("Error: {error}")),
        }
    }

    pub fn wifi_networks(&self) -> &[WifiNetwork] {
        &self.wifi_networks
    }

    pub fn interfaces(&self) -> &[NetworkInterface] {
        &self.interfaces
    }

    pub fn wifi_list_state(&mut self) -> &mut ListState {
        &mut self.wifi_list_state
    }

    pub fn next_wifi_network(&mut self) {
        if self.wifi_networks.is_empty() {
            self.wifi_list_state.select(None);
            return;
        }

        let next = match self.wifi_list_state.selected() {
            Some(index) => (index + 1) % self.wifi_networks.len(),
            None => 0,
        };
        self.wifi_list_state.select(Some(next));
    }

    pub fn previous_wifi_network(&mut self) {
        if self.wifi_networks.is_empty() {
            self.wifi_list_state.select(None);
            return;
        }

        let previous = match self.wifi_list_state.selected() {
            Some(0) | None => self.wifi_networks.len() - 1,
            Some(index) => index - 1,
        };
        self.wifi_list_state.select(Some(previous));
    }

    pub fn toggle_selected_network(&mut self) -> Option<SelectedNetworkAction> {
        self.last_error = None;
        let Some(network) = self
            .wifi_list_state
            .selected()
            .and_then(|index| self.wifi_networks.get(index))
            .cloned()
        else {
            self.status_message = "No Wi-Fi network selected".into();
            return None;
        };

        if network.connected {
            self.status_message = format!("Disconnecting from {}...", network.ssid);
            return Some(SelectedNetworkAction::Disconnect { ssid: network.ssid });
        }

        if !network.secure || matches!(network.security, WifiSecurity::Open) {
            self.status_message = format!("Connecting to {}...", network.ssid);
            return Some(SelectedNetworkAction::Connect {
                ssid: network.ssid,
                password: String::new(),
            });
        }

        self.input_mode = InputMode::Input {
            ssid: network.ssid.clone(),
        };
        self.input_buffer.clear();
        self.status_message = format!("Enter password for {}", network.ssid);
        None
    }

    pub fn input_mode(&self) -> &InputMode {
        &self.input_mode
    }

    pub fn input_buffer(&self) -> &str {
        &self.input_buffer
    }

    pub fn push_input_character(&mut self, character: char) {
        if matches!(self.input_mode, InputMode::Input { .. }) {
            self.input_buffer.push(character);
        }
    }

    pub fn delete_input_character(&mut self) {
        if matches!(self.input_mode, InputMode::Input { .. }) {
            self.input_buffer.pop();
        }
    }

    pub fn submit_password(&mut self) -> Option<(String, String)> {
        let InputMode::Input { ssid } = &self.input_mode else {
            return None;
        };
        let ssid = ssid.clone();
        let password = std::mem::take(&mut self.input_buffer);

        self.last_error = None;
        self.status_message = format!("Connecting to {ssid}...");
        self.input_mode = InputMode::Normal;

        Some((ssid, password))
    }

    pub fn cancel_password_input(&mut self) {
        if matches!(self.input_mode, InputMode::Input { .. }) {
            self.input_buffer.clear();
            self.input_mode = InputMode::Normal;
            self.status_message = "Connection canceled".into();
        }
    }

    pub fn forget_selected_network(&mut self) -> Option<String> {
        self.last_error = None;
        let Some(network) = self
            .wifi_list_state
            .selected()
            .and_then(|index| self.wifi_networks.get(index))
        else {
            self.status_message = "No Wi-Fi network selected".into();
            return None;
        };

        let ssid = network.ssid.clone();
        self.status_message = format!("Forgetting network {ssid}...");
        Some(ssid)
    }

    pub fn status_message(&self) -> &str {
        self.last_error.as_deref().unwrap_or(&self.status_message)
    }

    pub fn status_is_error(&self) -> bool {
        self.last_error.is_some()
    }

    pub fn should_quit(&self) -> bool {
        self.should_quit
    }

    pub fn quit(&mut self) {
        self.should_quit = true;
    }

    fn normalize_wifi_selection(&mut self) {
        if self.wifi_networks.is_empty() {
            self.wifi_list_state.select(None);
            return;
        }

        let selected = self
            .wifi_list_state
            .selected()
            .unwrap_or(0)
            .min(self.wifi_networks.len() - 1);
        self.wifi_list_state.select(Some(selected));
    }
}
