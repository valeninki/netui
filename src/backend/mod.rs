use async_trait::async_trait;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WifiNetwork {
    pub ssid: String,
    pub signal_strength: u8,
    pub secure: bool,
    pub connected: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkInterface {
    pub name: String,
    pub operational_state: OperationalState,
    pub carrier: bool,
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
}

#[async_trait]
pub trait NetworkdManager: Send + Sync {
    async fn get_interfaces(&self) -> Result<Vec<NetworkInterface>, BackendError>;
    async fn set_link_up(&self, interface: &str) -> Result<(), BackendError>;
    async fn set_link_down(&self, interface: &str) -> Result<(), BackendError>;
}
