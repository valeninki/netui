pub mod iwd;
pub mod networkd;
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
    #[error("backend operation is not available: {0}")]
    Unavailable(String),
    #[error("backend operation failed: {0}")]
    Operation(String),
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
    async fn set_link_up(&self, interface: &str) -> Result<(), BackendError>;
    async fn set_link_down(&self, interface: &str) -> Result<(), BackendError>;
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
        .map_err(dbus_error)?
    {
        return Ok(Some(ActiveWifiBackend::Iwd(
            iwd::IwdBackend::from_connection(connection),
        )));
    }

    if dbus
        .name_has_owner(bus_name(wpa_supplicant::WPA_SUPPLICANT_SERVICE)?)
        .await
        .map_err(dbus_error)?
    {
        return Ok(Some(ActiveWifiBackend::WpaSupplicant(
            wpa_supplicant::WpaSupplicantBackend::from_connection(connection),
        )));
    }

    Ok(None)
}

fn bus_name(name: &'static str) -> Result<BusName<'static>, BackendError> {
    BusName::try_from(name).map_err(dbus_error)
}

fn dbus_error(error: impl Display) -> BackendError {
    BackendError::Operation(error.to_string())
}
