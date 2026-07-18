//! UI renderers must use `Color::Reset` or named ANSI 16-color variants only.
//! Do not use `Color::Rgb`, `Color::Indexed`, or hardcoded RGB/hex color values.

use ratatui::widgets::ListState;
use tokio::sync::mpsc;

use crate::backend::{NetworkInterface, WifiNetwork};

pub const BACKEND_EVENT_CHANNEL_CAPACITY: usize = 64;

#[derive(Debug, Clone)]
pub enum BackendEvent {
    WifiNetworksUpdated(Vec<WifiNetwork>),
    InterfacesUpdated(Vec<NetworkInterface>),
    Error(String),
}

pub struct App {
    wifi_networks: Vec<WifiNetwork>,
    interfaces: Vec<NetworkInterface>,
    backend_event_tx: mpsc::Sender<BackendEvent>,
    backend_event_rx: mpsc::Receiver<BackendEvent>,
    wifi_list_state: ListState,
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
            BackendEvent::Error(error) => self.last_error = Some(error),
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

    pub fn connect_selected_wifi_network(&mut self) {
        self.last_error = None;
        self.status_message = match self.wifi_list_state.selected() {
            Some(index) => match self.wifi_networks.get(index) {
                Some(network) => format!("Connecting to {}...", network.ssid),
                None => "No Wi-Fi network selected".into(),
            },
            None => "No Wi-Fi network selected".into(),
        };
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
