//! UI renderers must use `Color::Reset` or named ANSI 16-color variants only.
//! Do not use `Color::Rgb`, `Color::Indexed`, or hardcoded RGB/hex color values.

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
    should_quit: bool,
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
            should_quit: false,
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
            BackendEvent::WifiNetworksUpdated(networks) => self.wifi_networks = networks,
            BackendEvent::InterfacesUpdated(interfaces) => self.interfaces = interfaces,
            BackendEvent::Error(error) => self.last_error = Some(error),
        }
    }

    pub fn wifi_networks(&self) -> &[WifiNetwork] {
        &self.wifi_networks
    }

    pub fn interfaces(&self) -> &[NetworkInterface] {
        &self.interfaces
    }

    pub fn should_quit(&self) -> bool {
        self.should_quit
    }

    pub fn quit(&mut self) {
        self.should_quit = true;
    }
}
