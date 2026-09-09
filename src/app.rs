//! UI renderers must use `Color::Reset` or named ANSI 16-color variants only.
//! Do not use `Color::Rgb`, `Color::Indexed`, or hardcoded RGB/hex color values.

use std::collections::HashMap;

use ratatui::widgets::ListState;
use tokio::sync::mpsc;

use crate::backend::{Ipv4Method, NetworkInterface, WifiConfig, WifiNetwork, WifiSecurity};

pub const BACKEND_EVENT_CHANNEL_CAPACITY: usize = 64;

#[derive(Debug, Clone)]
pub enum BackendEvent {
    WifiScanStateChanged(bool),
    WifiNetworksUpdated(Vec<WifiNetwork>),
    InterfacesUpdated(Vec<NetworkInterface>),
    ActionCompleted(Result<String, String>),
    Log(String),
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputMode {
    Normal,
    Input {
        ssid: String,
    },
    Edit {
        target: ConfigTarget,
        field: ConfigField,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigField {
    Method,
    IpAddress,
    Gateway,
    Dns,
    DnsOverTls,
}

impl ConfigField {
    pub fn label(self) -> &'static str {
        match self {
            Self::Method => "Method",
            Self::IpAddress => "IP address/CIDR",
            Self::Gateway => "Gateway",
            Self::Dns => "DNS servers",
            Self::DnsOverTls => "DoT mode",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigTarget {
    Ethernet { interface: String },
    Wifi { ssid: String },
}

impl ConfigTarget {
    pub fn display_name(&self) -> &str {
        match self {
            Self::Ethernet { interface } => interface,
            Self::Wifi { ssid } => ssid,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectedNetworkAction {
    Connect { ssid: String, password: String },
    Disconnect { ssid: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivePane {
    Wifi,
    Interfaces,
}

pub struct App {
    wifi_networks: Vec<WifiNetwork>,
    interfaces: Vec<NetworkInterface>,
    backend_event_tx: mpsc::Sender<BackendEvent>,
    backend_event_rx: mpsc::Receiver<BackendEvent>,
    wifi_list_state: ListState,
    interface_list_state: ListState,
    active_pane: ActivePane,
    wifi_scanning: bool,
    is_busy: bool,
    action_in_flight: bool,
    input_mode: InputMode,
    input_buffer: String,
    edit_config: WifiConfig,
    session_profiles: HashMap<String, WifiConfig>,
    should_quit: bool,
    status_message: String,
    last_error: Option<String>,
    logs: Vec<String>,
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
            interface_list_state: ListState::default(),
            active_pane: ActivePane::Wifi,
            wifi_scanning: false,
            is_busy: false,
            action_in_flight: false,
            input_mode: InputMode::Normal,
            input_buffer: String::new(),
            edit_config: WifiConfig::default(),
            session_profiles: HashMap::new(),
            should_quit: false,
            status_message: "Waiting for backend updates".into(),
            last_error: None,
            logs: Vec::new(),
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
            BackendEvent::WifiScanStateChanged(scanning) => {
                self.wifi_scanning = scanning;
                self.update_busy_state();
            }
            BackendEvent::WifiNetworksUpdated(networks) => {
                self.wifi_networks = networks;
                self.sort_wifi_networks();
                self.normalize_wifi_selection();
            }
            BackendEvent::InterfacesUpdated(interfaces) => {
                self.interfaces = interfaces;
                self.normalize_interface_selection();
            }
            BackendEvent::ActionCompleted(result) => {
                self.action_in_flight = false;
                self.update_busy_state();

                match result {
                    Ok(status) => {
                        self.status_message = status.clone();
                        self.last_error = None;
                        self.add_log(status);
                    }
                    Err(error) => {
                        let message = format!("Error: {error}");
                        self.last_error = Some(message.clone());
                        self.add_log(message);
                    }
                }
            }
            BackendEvent::Log(message) => self.add_log(message),
            BackendEvent::Error(error) => {
                let message = format!("Error: {error}");
                self.last_error = Some(message.clone());
                self.add_log(message);
            }
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

    pub fn interface_list_state(&mut self) -> &mut ListState {
        &mut self.interface_list_state
    }

    pub fn active_pane(&self) -> ActivePane {
        self.active_pane
    }

    pub fn is_active_pane(&self, pane: ActivePane) -> bool {
        self.active_pane == pane
    }

    pub fn toggle_active_pane(&mut self) {
        self.active_pane = match self.active_pane {
            ActivePane::Wifi => ActivePane::Interfaces,
            ActivePane::Interfaces => ActivePane::Wifi,
        };
    }

    pub fn next_selection(&mut self) {
        match self.active_pane {
            ActivePane::Wifi => self.next_wifi_network(),
            ActivePane::Interfaces => self.next_interface(),
        }
    }

    pub fn previous_selection(&mut self) {
        match self.active_pane {
            ActivePane::Wifi => self.previous_wifi_network(),
            ActivePane::Interfaces => self.previous_interface(),
        }
    }

    pub fn next_wifi_network(&mut self) {
        let connection_count = self.connection_count();
        if connection_count == 0 {
            self.wifi_list_state.select(None);
            return;
        }

        let next = match self.wifi_list_state.selected() {
            Some(index) => (index + 1) % connection_count,
            None => 0,
        };
        self.wifi_list_state.select(Some(next));
    }

    pub fn previous_wifi_network(&mut self) {
        let connection_count = self.connection_count();
        if connection_count == 0 {
            self.wifi_list_state.select(None);
            return;
        }

        let previous = match self.wifi_list_state.selected() {
            Some(0) | None => connection_count - 1,
            Some(index) => index - 1,
        };
        self.wifi_list_state.select(Some(previous));
    }

    pub fn next_interface(&mut self) {
        if self.interfaces.is_empty() {
            self.interface_list_state.select(None);
            return;
        }

        let next = match self.interface_list_state.selected() {
            Some(index) => (index + 1) % self.interfaces.len(),
            None => 0,
        };
        self.interface_list_state.select(Some(next));
    }

    pub fn previous_interface(&mut self) {
        if self.interfaces.is_empty() {
            self.interface_list_state.select(None);
            return;
        }

        let previous = match self.interface_list_state.selected() {
            Some(0) | None => self.interfaces.len() - 1,
            Some(index) => index - 1,
        };
        self.interface_list_state.select(Some(previous));
    }

    pub fn toggle_selected_network(&mut self) -> Option<SelectedNetworkAction> {
        self.last_error = None;
        let Some(index) = self.wifi_list_state.selected() else {
            self.status_message = "No network connection selected".into();
            return None;
        };
        let wired_count = self.wired_interface_count();
        if index < wired_count {
            self.status_message = "Use e to configure the selected Ethernet interface".into();
            return None;
        }
        let Some(network) = self.wifi_networks.get(index - wired_count).cloned() else {
            self.status_message = "No Wi-Fi network selected".into();
            return None;
        };

        if network.connected {
            self.status_message = format!("Disconnecting from {}...", network.ssid);
            return Some(SelectedNetworkAction::Disconnect { ssid: network.ssid });
        }

        if network.is_known || !network.secure || matches!(network.security, WifiSecurity::Open) {
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

    pub fn show_wired_link_control_unavailable(&mut self) {
        if self.selected_wired_interface().is_none() {
            self.status_message = "No wired interface selected".into();
        } else {
            self.status_message = "Wired link control is not available".into();
        }
    }

    pub fn input_mode(&self) -> &InputMode {
        &self.input_mode
    }

    pub fn input_buffer(&self) -> &str {
        &self.input_buffer
    }

    pub fn push_input_character(&mut self, character: char) {
        match &self.input_mode {
            InputMode::Input { .. } => self.input_buffer.push(character),
            InputMode::Edit { field, .. } => {
                if let Some(value) = self.edit_value_mut(*field) {
                    value.push(character);
                }
            }
            InputMode::Normal => {}
        }
    }

    pub fn delete_input_character(&mut self) {
        match &self.input_mode {
            InputMode::Input { .. } => {
                self.input_buffer.pop();
            }
            InputMode::Edit { field, .. } => {
                if let Some(value) = self.edit_value_mut(*field) {
                    value.pop();
                }
            }
            InputMode::Normal => {}
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
        if matches!(&self.input_mode, InputMode::Input { .. }) {
            self.input_buffer.clear();
            self.input_mode = InputMode::Normal;
            self.status_message = "Connection canceled".into();
        }
    }

    pub fn forget_selected_network(&mut self) -> Option<String> {
        self.last_error = None;
        let Some(index) = self.wifi_list_state.selected() else {
            self.status_message = "No Wi-Fi network selected".into();
            return None;
        };
        let wired_count = self.wired_interface_count();
        if index < wired_count {
            self.status_message = "Ethernet interfaces do not have Wi-Fi profiles to forget".into();
            return None;
        }
        let Some(network) = self.wifi_networks.get(index - wired_count) else {
            self.status_message = "No Wi-Fi network selected".into();
            return None;
        };

        let ssid = network.ssid.clone();
        self.status_message = format!("Forgetting network {ssid}...");
        Some(ssid)
    }

    pub fn begin_config_edit(&mut self) -> Option<ConfigTarget> {
        if self.is_busy {
            self.show_wait_message();
            return None;
        }

        let Some(target) = self.selected_config_target() else {
            self.status_message = "Select an Ethernet or Wi-Fi connection first".into();
            return None;
        };

        self.last_error = None;
        self.edit_config = self.session_profile(&target);
        self.input_mode = InputMode::Edit {
            target: target.clone(),
            field: ConfigField::Method,
        };
        self.status_message = format!("Configure profile for {}", target.display_name());
        Some(target)
    }

    pub fn wifi_config(&self, ssid: &str) -> WifiConfig {
        self.session_profiles
            .get(&ConfigTarget::Wifi { ssid: ssid.into() }.session_key())
            .cloned()
            .unwrap_or_else(|| self.default_wifi_config(ssid))
    }

    pub fn is_editing_config(&self) -> bool {
        matches!(&self.input_mode, InputMode::Edit { .. })
    }

    pub fn edit_field(&self) -> Option<ConfigField> {
        match &self.input_mode {
            InputMode::Edit { field, .. } => Some(*field),
            _ => None,
        }
    }

    pub fn edit_target(&self) -> Option<&ConfigTarget> {
        match &self.input_mode {
            InputMode::Edit { target, .. } => Some(target),
            _ => None,
        }
    }

    pub fn edit_config(&self) -> &WifiConfig {
        &self.edit_config
    }

    pub fn toggle_edit_method(&mut self) {
        self.edit_config.ipv4_method = match self.edit_config.ipv4_method {
            Ipv4Method::Dhcp => Ipv4Method::Static,
            Ipv4Method::Static => Ipv4Method::Dhcp,
        };
    }

    pub fn cycle_edit_dot_mode(&mut self, backwards: bool) {
        use crate::backend::DnsOverTlsMode;

        self.edit_config.dns_over_tls = match (self.edit_config.dns_over_tls, backwards) {
            (DnsOverTlsMode::Default, false) | (DnsOverTlsMode::Strict, true) => {
                DnsOverTlsMode::Off
            }
            (DnsOverTlsMode::Off, false) | (DnsOverTlsMode::Default, true) => {
                DnsOverTlsMode::Strict
            }
            (DnsOverTlsMode::Strict, false) | (DnsOverTlsMode::Off, true) => {
                DnsOverTlsMode::Default
            }
        };
    }

    pub fn next_config_field(&mut self) {
        if let InputMode::Edit { field, .. } = &mut self.input_mode {
            *field = next_config_field(*field, self.edit_config.ipv4_method);
        }
    }

    pub fn previous_config_field(&mut self) {
        if let InputMode::Edit { field, .. } = &mut self.input_mode {
            *field = previous_config_field(*field, self.edit_config.ipv4_method);
        }
    }

    pub fn submit_config(&mut self) -> bool {
        let InputMode::Edit { target, .. } = &self.input_mode else {
            return false;
        };

        let target = target.clone();
        let mut config = self.edit_config.clone();
        config.ip_address = config.ip_address.trim().to_owned();
        config.gateway = config.gateway.trim().to_owned();
        config.dns_servers = config.dns_servers.trim().to_owned();

        if let Err(error) = config.validate() {
            self.last_error = Some(format!("Error: {error}"));
            return false;
        }

        self.session_profiles.insert(target.session_key(), config);
        self.last_error = None;
        self.status_message = "Session profile updated".into();
        self.input_mode = InputMode::Normal;
        self.add_log("Session profile updated".into());
        true
    }

    pub fn cancel_config_edit(&mut self) {
        if matches!(&self.input_mode, InputMode::Edit { .. }) {
            self.input_mode = InputMode::Normal;
            self.edit_config = WifiConfig::default();
            self.status_message = "Profile editing canceled".into();
        }
    }

    pub fn status_message(&self) -> &str {
        self.last_error.as_deref().unwrap_or(&self.status_message)
    }

    pub fn status_is_error(&self) -> bool {
        self.last_error.is_some()
    }

    pub fn add_log(&mut self, message: String) {
        self.logs.push(message);
        if self.logs.len() > 5 {
            self.logs.remove(0);
        }
    }

    pub fn wifi_scanning(&self) -> bool {
        self.wifi_scanning
    }

    pub fn is_busy(&self) -> bool {
        self.is_busy
    }

    pub fn begin_backend_action(&mut self) -> bool {
        if self.is_busy {
            self.show_wait_message();
            return false;
        }

        self.action_in_flight = true;
        self.update_busy_state();
        true
    }

    pub fn show_wait_message(&mut self) {
        self.status_message = "Please wait...".into();
    }

    pub fn should_quit(&self) -> bool {
        self.should_quit
    }

    pub fn quit(&mut self) {
        self.should_quit = true;
    }

    fn normalize_wifi_selection(&mut self) {
        let connection_count = self.connection_count();
        if connection_count == 0 {
            self.wifi_list_state.select(None);
            return;
        }

        let selected = self
            .wifi_list_state
            .selected()
            .unwrap_or(0)
            .min(connection_count - 1);
        self.wifi_list_state.select(Some(selected));
    }

    fn normalize_interface_selection(&mut self) {
        if self.interfaces.is_empty() {
            self.interface_list_state.select(None);
            return;
        }

        let selected = self
            .interface_list_state
            .selected()
            .unwrap_or(0)
            .min(self.interfaces.len() - 1);
        self.interface_list_state.select(Some(selected));
    }

    fn update_busy_state(&mut self) {
        self.is_busy = self.wifi_scanning || self.action_in_flight;
    }

    fn sort_wifi_networks(&mut self) {
        self.wifi_networks.sort_by(|left, right| {
            right
                .connected
                .cmp(&left.connected)
                .then_with(|| right.signal_strength.cmp(&left.signal_strength))
                .then_with(|| left.ssid.cmp(&right.ssid))
        });
    }

    fn connection_count(&self) -> usize {
        self.wired_interface_count() + self.wifi_networks.len()
    }

    fn wired_interface_count(&self) -> usize {
        self.interfaces
            .iter()
            .filter(|interface| interface.is_wired)
            .count()
    }

    fn selected_wired_interface(&self) -> Option<&NetworkInterface> {
        match self.active_pane {
            ActivePane::Wifi => self.wifi_list_state.selected().and_then(|index| {
                self.interfaces
                    .iter()
                    .filter(|interface| interface.is_wired)
                    .nth(index)
            }),
            ActivePane::Interfaces => self
                .interface_list_state
                .selected()
                .and_then(|index| self.interfaces.get(index))
                .filter(|interface| interface.is_wired),
        }
    }

    fn selected_config_target(&self) -> Option<ConfigTarget> {
        match self.active_pane {
            ActivePane::Wifi => {
                let index = self.wifi_list_state.selected()?;
                let wired_count = self.wired_interface_count();
                if index < wired_count {
                    self.interfaces
                        .iter()
                        .filter(|interface| interface.is_wired)
                        .nth(index)
                        .map(|interface| ConfigTarget::Ethernet {
                            interface: interface.name.clone(),
                        })
                } else {
                    self.wifi_networks
                        .get(index - wired_count)
                        .map(|network| ConfigTarget::Wifi {
                            ssid: network.ssid.clone(),
                        })
                }
            }
            ActivePane::Interfaces => {
                self.selected_wired_interface()
                    .map(|interface| ConfigTarget::Ethernet {
                        interface: interface.name.clone(),
                    })
            }
        }
    }

    fn session_profile(&self, target: &ConfigTarget) -> WifiConfig {
        self.session_profiles
            .get(&target.session_key())
            .cloned()
            .unwrap_or_else(|| match target {
                ConfigTarget::Wifi { ssid } => self.default_wifi_config(ssid),
                ConfigTarget::Ethernet { .. } => WifiConfig::default(),
            })
    }

    fn default_wifi_config(&self, ssid: &str) -> WifiConfig {
        if ssid.eq_ignore_ascii_case("eduroam") {
            WifiConfig {
                dns_over_tls: crate::backend::DnsOverTlsMode::Off,
                ..WifiConfig::default()
            }
        } else {
            WifiConfig::default()
        }
    }

    fn edit_value_mut(&mut self, field: ConfigField) -> Option<&mut String> {
        match field {
            ConfigField::IpAddress => Some(&mut self.edit_config.ip_address),
            ConfigField::Gateway => Some(&mut self.edit_config.gateway),
            ConfigField::Dns => Some(&mut self.edit_config.dns_servers),
            ConfigField::Method | ConfigField::DnsOverTls => None,
        }
    }
}

impl ConfigTarget {
    fn session_key(&self) -> String {
        match self {
            Self::Ethernet { interface } => format!("ethernet:{interface}"),
            Self::Wifi { ssid } => format!("wifi:{ssid}"),
        }
    }
}

fn next_config_field(field: ConfigField, method: Ipv4Method) -> ConfigField {
    match (field, method) {
        (ConfigField::Method, Ipv4Method::Static) => ConfigField::IpAddress,
        (ConfigField::Method, Ipv4Method::Dhcp) | (ConfigField::Gateway, _) => ConfigField::Dns,
        (ConfigField::IpAddress, _) => ConfigField::Gateway,
        (ConfigField::Dns, _) => ConfigField::DnsOverTls,
        (ConfigField::DnsOverTls, _) => ConfigField::Method,
    }
}

fn previous_config_field(field: ConfigField, method: Ipv4Method) -> ConfigField {
    match (field, method) {
        (ConfigField::Method, _) => ConfigField::DnsOverTls,
        (ConfigField::IpAddress, _) => ConfigField::Method,
        (ConfigField::Gateway, _) => ConfigField::IpAddress,
        (ConfigField::Dns, Ipv4Method::Static) => ConfigField::Gateway,
        (ConfigField::Dns, Ipv4Method::Dhcp) => ConfigField::Method,
        (ConfigField::DnsOverTls, _) => ConfigField::Dns,
    }
}

#[cfg(test)]
mod tests {
    use super::{App, BackendEvent, ConfigField, ConfigTarget};
    use crate::backend::{DnsOverTlsMode, Ipv4Method, WifiConfig, WifiNetwork, WifiSecurity};

    #[test]
    fn accepts_dhcp_and_static_profile_configurations() {
        assert!(WifiConfig::default().validate().is_ok());
        assert!(
            WifiConfig {
                ipv4_method: Ipv4Method::Static,
                ip_address: "192.0.2.10/24".into(),
                gateway: "192.0.2.1".into(),
                dns_servers: "1.1.1.1, 9.9.9.9".into(),
                ..WifiConfig::default()
            }
            .validate()
            .is_ok()
        );
    }

    #[test]
    fn rejects_invalid_profile_values() {
        assert!(
            WifiConfig {
                ipv4_method: Ipv4Method::Static,
                ip_address: "192.0.2.10".into(),
                gateway: "192.0.2.1".into(),
                ..WifiConfig::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            WifiConfig {
                dns_servers: "invalid".into(),
                ..WifiConfig::default()
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn unknown_wifi_profiles_open_with_dhcp_defaults() {
        let mut app = App::new();
        app.apply_backend_event(BackendEvent::WifiNetworksUpdated(vec![WifiNetwork {
            ssid: "guest".into(),
            signal_strength: 80,
            security: WifiSecurity::Open,
            secure: false,
            connected: false,
            is_known: false,
        }]));

        assert_eq!(
            app.begin_config_edit(),
            Some(ConfigTarget::Wifi {
                ssid: "guest".into()
            })
        );
        assert_eq!(app.edit_config().ipv4_method, Ipv4Method::Dhcp);
        app.next_config_field();
        assert_eq!(app.edit_field(), Some(ConfigField::Dns));
    }

    #[test]
    fn profile_edits_are_stored_in_the_session() {
        let mut app = App::new();
        app.apply_backend_event(BackendEvent::WifiNetworksUpdated(vec![WifiNetwork {
            ssid: "saved".into(),
            signal_strength: 80,
            security: WifiSecurity::Rsn,
            secure: true,
            connected: false,
            is_known: true,
        }]));

        app.begin_config_edit();
        app.toggle_edit_method();
        app.next_config_field();
        for character in "192.0.2.10/24".chars() {
            app.push_input_character(character);
        }
        app.next_config_field();
        for character in "192.0.2.1".chars() {
            app.push_input_character(character);
        }
        assert!(app.submit_config());
        assert_eq!(app.status_message(), "Session profile updated");
        assert_eq!(app.wifi_config("saved").ipv4_method, Ipv4Method::Static);
    }

    #[test]
    fn eduroam_defaults_to_dhcp_with_dot_disabled() {
        let app = App::new();
        let config = app.wifi_config("EDUROAM");

        assert_eq!(config.ipv4_method, Ipv4Method::Dhcp);
        assert_eq!(config.dns_over_tls, DnsOverTlsMode::Off);
    }
}
