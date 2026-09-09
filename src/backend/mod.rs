pub mod iwd;
pub mod networkd;
pub mod resolved;
pub mod wpa_supplicant;

use async_trait::async_trait;
use std::{fmt::Display, net::IpAddr};
use thiserror::Error;
use tokio::{sync::mpsc, task::JoinHandle};
use zbus::{Connection, fdo::DBusProxy, names::BusName};

use crate::app::BackendEvent;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WifiNetwork {
    pub ssid: String,
    pub signal_strength: u8,
    pub security: WifiSecurity,
    pub secure: bool,
    pub connected: bool,
    pub is_known: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WifiSecurity {
    Open,
    Wep,
    Wpa,
    Rsn,
    WpaPersonal,
    WpaEnterprise,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ipv4Method {
    Dhcp,
    Static,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DnsOverTlsMode {
    Default,
    Off,
    Strict,
}

impl DnsOverTlsMode {
    pub fn resolved_mode(self) -> &'static str {
        match self {
            Self::Default => "",
            Self::Off => "no",
            Self::Strict => "yes",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WifiConfig {
    pub ipv4_method: Ipv4Method,
    pub ip_address: String,
    pub gateway: String,
    pub dns_servers: String,
    pub dns_over_tls: DnsOverTlsMode,
}

impl Default for WifiConfig {
    fn default() -> Self {
        Self {
            ipv4_method: Ipv4Method::Dhcp,
            ip_address: String::new(),
            gateway: String::new(),
            dns_servers: String::new(),
            dns_over_tls: DnsOverTlsMode::Default,
        }
    }
}

impl WifiConfig {
    pub fn dns_server_list(&self) -> Result<Vec<std::net::Ipv4Addr>, BackendError> {
        self.dns_servers
            .split(',')
            .map(str::trim)
            .filter(|server| !server.is_empty())
            .map(|server| {
                server.parse().map_err(|_| {
                    BackendError::Operation(format!("DNS server {server:?} must be valid IPv4"))
                })
            })
            .collect()
    }

    pub fn validate(&self) -> Result<(), BackendError> {
        let _ = self.dns_server_list()?;

        if self.ipv4_method == Ipv4Method::Dhcp {
            return Ok(());
        }

        let (address, prefix) = self.ip_address.split_once('/').ok_or_else(|| {
            BackendError::Operation(
                "IP address must include a CIDR prefix, for example 192.0.2.10/24".into(),
            )
        })?;
        address
            .parse::<std::net::Ipv4Addr>()
            .map_err(|_| BackendError::Operation("IP address must be valid IPv4".into()))?;
        prefix
            .parse::<u8>()
            .ok()
            .filter(|prefix| *prefix <= 32)
            .ok_or_else(|| {
                BackendError::Operation("CIDR prefix must be between 0 and 32".into())
            })?;
        self.gateway
            .parse::<std::net::Ipv4Addr>()
            .map_err(|_| BackendError::Operation("Gateway must be valid IPv4".into()))?;

        Ok(())
    }
}

impl WifiSecurity {
    pub fn is_secure(self) -> bool {
        !matches!(self, Self::Open)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkInterface {
    pub name: String,
    pub operational_state: OperationalState,
    pub carrier: bool,
    pub ip_addresses: Vec<IpAddr>,
    pub is_wired: bool,
    pub is_wireless: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationalState {
    Unknown,
    Off,
    NoCarrier,
    Dormant,
    Degraded,
    Routable,
}

#[derive(Debug, Error)]
pub enum BackendError {
    #[error("{service} is unavailable")]
    ServiceUnavailable { service: String },
    #[error("backend operation is not available: {0}")]
    Unavailable(String),
    #[error("backend operation failed: {0}")]
    Operation(String),
}

impl BackendError {
    pub fn is_service_unavailable(&self) -> bool {
        matches!(self, Self::ServiceUnavailable { .. })
    }
}

#[async_trait]
pub trait WifiBackend: Send + Sync {
    async fn scan(&self) -> Result<(), BackendError>;
    async fn get_networks(&self) -> Result<Vec<WifiNetwork>, BackendError>;
    async fn initiate_connection(&self, ssid: String, password: String)
    -> Result<(), BackendError>;
    async fn disconnect_current_network(&self) -> Result<(), BackendError>;
    async fn forget_network(&self, ssid: String) -> Result<(), BackendError>;
}

#[async_trait]
pub trait NetworkdManager: Send + Sync {
    async fn get_interfaces(&self) -> Result<Vec<NetworkInterface>, BackendError>;
}

pub enum ActiveWifiBackend {
    Iwd(iwd::IwdBackend),
    WpaSupplicant(wpa_supplicant::WpaSupplicantBackend),
}

impl ActiveWifiBackend {
    pub fn spawn_listener(&self, sender: mpsc::Sender<BackendEvent>) -> JoinHandle<()> {
        match self {
            Self::Iwd(backend) => backend.spawn_listener(sender),
            Self::WpaSupplicant(backend) => backend.spawn_listener(sender),
        }
    }
}

pub async fn initiate_connection(ssid: String, password: String) -> Result<(), BackendError> {
    let Some(backend) = detect_wifi_backend().await? else {
        return Err(BackendError::Unavailable(
            "neither iwd nor wpa_supplicant is running".into(),
        ));
    };

    backend.initiate_connection(ssid, password).await
}

pub async fn disconnect_current_network() -> Result<(), BackendError> {
    let Some(backend) = detect_wifi_backend().await? else {
        return Err(BackendError::Unavailable(
            "neither iwd nor wpa_supplicant is running".into(),
        ));
    };

    backend.disconnect_current_network().await
}

pub async fn forget_network(ssid: String) -> Result<(), BackendError> {
    let Some(backend) = detect_wifi_backend().await? else {
        return Err(BackendError::Unavailable(
            "neither iwd nor wpa_supplicant is running".into(),
        ));
    };

    backend.forget_network(ssid).await
}

#[async_trait]
impl WifiBackend for ActiveWifiBackend {
    async fn scan(&self) -> Result<(), BackendError> {
        match self {
            Self::Iwd(backend) => backend.scan().await,
            Self::WpaSupplicant(backend) => backend.scan().await,
        }
    }

    async fn get_networks(&self) -> Result<Vec<WifiNetwork>, BackendError> {
        match self {
            Self::Iwd(backend) => backend.get_networks().await,
            Self::WpaSupplicant(backend) => backend.get_networks().await,
        }
    }

    async fn initiate_connection(
        &self,
        ssid: String,
        password: String,
    ) -> Result<(), BackendError> {
        match self {
            Self::Iwd(backend) => backend.initiate_connection(ssid, password).await,
            Self::WpaSupplicant(backend) => backend.initiate_connection(ssid, password).await,
        }
    }

    async fn disconnect_current_network(&self) -> Result<(), BackendError> {
        match self {
            Self::Iwd(backend) => backend.disconnect_current_network().await,
            Self::WpaSupplicant(backend) => backend.disconnect_current_network().await,
        }
    }

    async fn forget_network(&self, ssid: String) -> Result<(), BackendError> {
        match self {
            Self::Iwd(backend) => backend.forget_network(ssid).await,
            Self::WpaSupplicant(backend) => backend.forget_network(ssid).await,
        }
    }
}

pub async fn detect_wifi_backend() -> Result<Option<ActiveWifiBackend>, BackendError> {
    let connection = Connection::system().await.map_err(dbus_error)?;
    let dbus = DBusProxy::new(&connection).await.map_err(dbus_error)?;

    if dbus
        .name_has_owner(bus_name(iwd::IWD_SERVICE)?)
        .await
        .map_err(generic_error)?
    {
        return Ok(Some(ActiveWifiBackend::Iwd(
            iwd::IwdBackend::from_connection(connection),
        )));
    }

    if dbus
        .name_has_owner(bus_name(wpa_supplicant::WPA_SUPPLICANT_SERVICE)?)
        .await
        .map_err(generic_error)?
    {
        return Ok(Some(ActiveWifiBackend::WpaSupplicant(
            wpa_supplicant::WpaSupplicantBackend::from_connection(connection),
        )));
    }

    Ok(None)
}

fn bus_name(name: &'static str) -> Result<BusName<'static>, BackendError> {
    BusName::try_from(name).map_err(generic_error)
}

pub(crate) async fn service_has_owner(
    connection: &Connection,
    service: &'static str,
) -> Result<bool, BackendError> {
    let dbus = DBusProxy::new(connection).await.map_err(dbus_error)?;
    dbus.name_has_owner(bus_name(service)?)
        .await
        .map_err(generic_error)
}

pub(crate) fn dbus_service_error(service: &'static str, error: zbus::Error) -> BackendError {
    if is_service_unavailable_error(&error) {
        return BackendError::ServiceUnavailable {
            service: service.into(),
        };
    }

    BackendError::Operation(error.to_string())
}

fn is_service_unavailable_error(error: &zbus::Error) -> bool {
    match error {
        zbus::Error::MethodError(name, _, _) => matches!(
            name.as_str(),
            "org.freedesktop.DBus.Error.ServiceUnknown"
                | "org.freedesktop.DBus.Error.NameHasNoOwner"
                | "org.freedesktop.DBus.Error.Spawn.ServiceNotFound"
        ),
        zbus::Error::FDO(error) => matches!(
            error.as_ref(),
            zbus::fdo::Error::ServiceUnknown(_)
                | zbus::fdo::Error::NameHasNoOwner(_)
                | zbus::fdo::Error::SpawnServiceNotFound(_)
        ),
        _ => false,
    }
}

fn dbus_error(error: zbus::Error) -> BackendError {
    if is_service_unavailable_error(&error) {
        return BackendError::ServiceUnavailable {
            service: "D-Bus service".into(),
        };
    }

    BackendError::Operation(error.to_string())
}

fn generic_error(error: impl Display) -> BackendError {
    BackendError::Operation(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::{BackendError, dbus_service_error};

    #[test]
    fn maps_fdo_service_absence_to_a_clean_error() {
        for error in [
            zbus::fdo::Error::ServiceUnknown("missing".into()),
            zbus::fdo::Error::NameHasNoOwner("missing".into()),
            zbus::fdo::Error::SpawnServiceNotFound("missing".into()),
        ] {
            assert!(matches!(
                dbus_service_error(
                    "org.example.MissingService",
                    zbus::Error::FDO(Box::new(error))
                ),
                BackendError::ServiceUnavailable { .. }
            ));
        }
    }
}
