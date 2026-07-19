use std::{collections::BTreeMap, fmt::Display, time::Duration};

use async_trait::async_trait;
use futures_util::{StreamExt, stream::FuturesUnordered};
use tokio::{sync::mpsc, task::JoinHandle};
use zbus::{Connection, fdo::PropertiesProxy, interface, proxy, zvariant::OwnedObjectPath};

use crate::app::BackendEvent;

use super::{BackendError, WifiBackend, WifiNetwork, WifiSecurity};

pub const IWD_SERVICE: &str = "net.connman.iwd";

const IWD_ROOT_PATH: &str = "/net/connman/iwd";
const PASSWORD_AGENT_PATH: &str = "/org/netui/IwdPasswordAgent";
const STATION_INTERFACE: &str = "net.connman.iwd.Station";
const REFRESH_INTERVAL: Duration = Duration::from_secs(30);
const RETRY_DELAY: Duration = Duration::from_secs(2);

#[proxy(
    interface = "net.connman.iwd.Station",
    default_service = "net.connman.iwd"
)]
trait IwdStation {
    fn scan(&self) -> zbus::Result<()>;
    fn disconnect(&self) -> zbus::Result<()>;
    fn get_ordered_networks(&self) -> zbus::Result<Vec<(OwnedObjectPath, i16)>>;

    #[zbus(property)]
    fn state(&self) -> zbus::Result<String>;
}

#[proxy(
    interface = "net.connman.iwd.Network",
    default_service = "net.connman.iwd"
)]
trait IwdNetwork {
    fn connect(&self) -> zbus::Result<()>;

    #[zbus(property)]
    fn name(&self) -> zbus::Result<String>;

    #[zbus(property, name = "Type")]
    fn network_type(&self) -> zbus::Result<String>;

    #[zbus(property)]
    fn connected(&self) -> zbus::Result<bool>;

    #[zbus(property, name = "KnownNetwork")]
    fn known_network(&self) -> zbus::Result<OwnedObjectPath>;
}

#[proxy(
    interface = "net.connman.iwd.KnownNetwork",
    default_service = "net.connman.iwd"
)]
trait IwdKnownNetwork {
    fn forget(&self) -> zbus::Result<()>;

    #[zbus(property)]
    fn name(&self) -> zbus::Result<String>;
}

#[proxy(
    interface = "net.connman.iwd.AgentManager",
    default_service = "net.connman.iwd"
)]
trait IwdAgentManager {
    fn register_agent(&self, path: OwnedObjectPath) -> zbus::Result<()>;
    fn unregister_agent(&self, path: OwnedObjectPath) -> zbus::Result<()>;
}

#[proxy(
    interface = "org.freedesktop.DBus.Introspectable",
    default_service = "net.connman.iwd"
)]
trait Introspectable {
    fn introspect(&self) -> zbus::Result<String>;
}

struct PasswordAgent {
    password: String,
}

#[interface(name = "net.connman.iwd.Agent")]
impl PasswordAgent {
    async fn request_passphrase(&self, _network: OwnedObjectPath) -> String {
        self.password.clone()
    }
}

#[derive(Clone)]
pub struct IwdBackend {
    connection: Connection,
}

impl IwdBackend {
    pub async fn connect_system() -> Result<Self, BackendError> {
        let connection = Connection::system().await.map_err(dbus_error)?;
        Ok(Self::from_connection(connection))
    }

    pub(crate) fn from_connection(connection: Connection) -> Self {
        Self { connection }
    }

    pub fn spawn_listener(&self, sender: mpsc::Sender<BackendEvent>) -> JoinHandle<()> {
        let backend = self.clone();

        tokio::spawn(async move {
            loop {
                match backend.get_networks().await {
                    Ok(networks) => {
                        if sender
                            .send(BackendEvent::WifiNetworksUpdated(networks))
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                    Err(error) => {
                        if sender
                            .send(BackendEvent::Error(error.to_string()))
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                }

                match tokio::time::timeout(REFRESH_INTERVAL, backend.wait_for_station_change())
                    .await
                {
                    Ok(Ok(())) | Err(_) => {}
                    Ok(Err(error)) => {
                        if sender
                            .send(BackendEvent::Error(error.to_string()))
                            .await
                            .is_err()
                        {
                            return;
                        }
                        tokio::time::sleep(RETRY_DELAY).await;
                    }
                }
            }
        })
    }

    async fn station_paths(&self) -> Result<Vec<String>, BackendError> {
        let root = self.introspect(IWD_ROOT_PATH).await?;
        let mut stations = Vec::new();

        // iwd does not provide ObjectManager, so walk root -> adapter -> device.
        for adapter in child_nodes(&root) {
            let adapter_path = format!("{IWD_ROOT_PATH}/{adapter}");
            let Ok(adapter_xml) = self.introspect(&adapter_path).await else {
                continue;
            };

            if exposes_interface(&adapter_xml, STATION_INTERFACE) {
                stations.push(adapter_path);
                continue;
            }

            for device in child_nodes(&adapter_xml) {
                let device_path = format!("{adapter_path}/{device}");
                let Ok(device_xml) = self.introspect(&device_path).await else {
                    continue;
                };

                if exposes_interface(&device_xml, STATION_INTERFACE) {
                    stations.push(device_path);
                }
            }
        }

        Ok(stations)
    }

    async fn introspect(&self, path: &str) -> Result<String, BackendError> {
        IntrospectableProxy::builder(&self.connection)
            .path(path)
            .map_err(dbus_error)?
            .build()
            .await
            .map_err(dbus_error)?
            .introspect()
            .await
            .map_err(dbus_error)
    }

    async fn station_proxy<'a>(
        &'a self,
        path: &'a str,
    ) -> Result<IwdStationProxy<'a>, BackendError> {
        IwdStationProxy::builder(&self.connection)
            .path(path)
            .map_err(dbus_error)?
            .build()
            .await
            .map_err(dbus_error)
    }

    async fn network_proxy(
        &self,
        path: OwnedObjectPath,
    ) -> Result<IwdNetworkProxy<'_>, BackendError> {
        IwdNetworkProxy::builder(&self.connection)
            .path(path)
            .map_err(dbus_error)?
            .build()
            .await
            .map_err(dbus_error)
    }

    async fn agent_manager_proxy(&self) -> Result<IwdAgentManagerProxy<'_>, BackendError> {
        IwdAgentManagerProxy::builder(&self.connection)
            .path(IWD_ROOT_PATH)
            .map_err(dbus_error)?
            .build()
            .await
            .map_err(dbus_error)
    }

    async fn known_network_proxy(
        &self,
        path: OwnedObjectPath,
    ) -> Result<IwdKnownNetworkProxy<'_>, BackendError> {
        IwdKnownNetworkProxy::builder(&self.connection)
            .path(path)
            .map_err(dbus_error)?
            .build()
            .await
            .map_err(dbus_error)
    }

    async fn connect_network(&self, ssid: String, password: String) -> Result<(), BackendError> {
        let mut target_network = None;

        for station_path in self.station_paths().await? {
            let station = self.station_proxy(&station_path).await?;
            for (path, _) in station.get_ordered_networks().await.map_err(dbus_error)? {
                let network = self.network_proxy(path.clone()).await?;
                if network.name().await.map_err(dbus_error)? == ssid {
                    target_network = Some(network);
                    break;
                }
            }

            if target_network.is_some() {
                break;
            }
        }

        let Some(network) = target_network else {
            return Err(BackendError::Unavailable(format!(
                "Wi-Fi network {ssid:?} is no longer available"
            )));
        };

        if network.network_type().await.map_err(dbus_error)? == "8021x" {
            return Err(BackendError::Unavailable(
                "enterprise Wi-Fi authentication is not implemented".into(),
            ));
        }

        let agent_path = OwnedObjectPath::try_from(PASSWORD_AGENT_PATH).map_err(dbus_error)?;
        let agent_manager = self.agent_manager_proxy().await?;
        self.connection
            .object_server()
            .at(PASSWORD_AGENT_PATH, PasswordAgent { password })
            .await
            .map_err(dbus_error)?;

        if let Err(error) = agent_manager.register_agent(agent_path.clone()).await {
            let _ = self
                .connection
                .object_server()
                .remove::<PasswordAgent, _>(PASSWORD_AGENT_PATH)
                .await;
            return Err(dbus_error(error));
        }

        let connect_result = network.connect().await.map_err(dbus_error);
        let unregister_result = agent_manager
            .unregister_agent(agent_path)
            .await
            .map_err(dbus_error);
        let remove_result = self
            .connection
            .object_server()
            .remove::<PasswordAgent, _>(PASSWORD_AGENT_PATH)
            .await
            .map(|_| ())
            .map_err(dbus_error);

        connect_result?;
        unregister_result?;
        remove_result
    }

    async fn disconnect_connected_network(&self) -> Result<(), BackendError> {
        for station_path in self.station_paths().await? {
            let station = self.station_proxy(&station_path).await?;
            let state = station.state().await.map_err(dbus_error)?;
            if matches!(state.as_str(), "connected" | "connecting") {
                return station.disconnect().await.map_err(dbus_error);
            }
        }

        Err(BackendError::Unavailable(
            "iwd has no connected Wi-Fi network".into(),
        ))
    }

    async fn forget_network_profile(&self, ssid: String) -> Result<(), BackendError> {
        for station_path in self.station_paths().await? {
            let station = self.station_proxy(&station_path).await?;
            for (path, _) in station.get_ordered_networks().await.map_err(dbus_error)? {
                let network = self.network_proxy(path).await?;
                if network.name().await.map_err(dbus_error)? != ssid {
                    continue;
                }

                let known_path = network.known_network().await.map_err(dbus_error)?;
                if known_path.as_str() != "/" {
                    return self
                        .known_network_proxy(known_path)
                        .await?
                        .forget()
                        .await
                        .map_err(dbus_error);
                }
            }
        }

        let root = self.introspect(IWD_ROOT_PATH).await?;
        for node in child_nodes(&root) {
            let path = format!("{IWD_ROOT_PATH}/{node}");
            let Ok(xml) = self.introspect(&path).await else {
                continue;
            };
            if !exposes_interface(&xml, "net.connman.iwd.KnownNetwork") {
                continue;
            }

            let path = OwnedObjectPath::try_from(path).map_err(dbus_error)?;
            let network = self.known_network_proxy(path).await?;
            if network.name().await.map_err(dbus_error)? == ssid {
                return network.forget().await.map_err(dbus_error);
            }
        }

        Err(BackendError::Unavailable(format!(
            "no saved profile exists for Wi-Fi network {ssid:?}"
        )))
    }

    async fn wait_for_station_change(&self) -> Result<(), BackendError> {
        let mut changes = FuturesUnordered::new();

        for path in self.station_paths().await? {
            let properties = PropertiesProxy::builder(&self.connection)
                .destination(IWD_SERVICE)
                .map_err(dbus_error)?
                .path(path)
                .map_err(dbus_error)?
                .build()
                .await
                .map_err(dbus_error)?;
            let mut changed = properties
                .receive_properties_changed()
                .await
                .map_err(dbus_error)?;

            changes.push(async move { changed.next().await });
        }

        if changes.is_empty() {
            tokio::time::sleep(RETRY_DELAY).await;
        } else {
            let _ = changes.next().await;
        }

        Ok(())
    }
}

#[async_trait]
impl WifiBackend for IwdBackend {
    async fn scan(&self) -> Result<(), BackendError> {
        let station_paths = self.station_paths().await?;
        if station_paths.is_empty() {
            return Err(BackendError::Unavailable(
                "iwd is not managing a station interface".into(),
            ));
        }

        for path in station_paths {
            self.station_proxy(&path)
                .await?
                .scan()
                .await
                .map_err(dbus_error)?;
        }

        Ok(())
    }

    async fn get_networks(&self) -> Result<Vec<WifiNetwork>, BackendError> {
        let mut networks = BTreeMap::new();

        for station_path in self.station_paths().await? {
            let station = self.station_proxy(&station_path).await?;
            for (path, signal) in station.get_ordered_networks().await.map_err(dbus_error)? {
                let network = self.network_proxy(path).await?;
                let security = iwd_security(&network.network_type().await.map_err(dbus_error)?);
                let wifi_network = WifiNetwork {
                    ssid: network.name().await.map_err(dbus_error)?,
                    signal_strength: iwd_signal_to_percentage(signal),
                    security,
                    secure: security.is_secure(),
                    connected: network.connected().await.map_err(dbus_error)?,
                };

                merge_network(&mut networks, wifi_network);
            }
        }

        Ok(networks.into_values().collect())
    }

    async fn initiate_connection(
        &self,
        ssid: String,
        password: String,
    ) -> Result<(), BackendError> {
        self.connect_network(ssid, password).await
    }

    async fn disconnect_current_network(&self) -> Result<(), BackendError> {
        self.disconnect_connected_network().await
    }

    async fn forget_network(&self, ssid: String) -> Result<(), BackendError> {
        self.forget_network_profile(ssid).await
    }
}

fn child_nodes(xml: &str) -> Vec<&str> {
    const PREFIX: &str = "<node name=\"";

    xml.match_indices(PREFIX)
        .filter_map(|(offset, _)| {
            let remainder = &xml[offset + PREFIX.len()..];
            remainder.find('"').map(|end| &remainder[..end])
        })
        .collect()
}

fn exposes_interface(xml: &str, interface: &str) -> bool {
    xml.contains(&format!("<interface name=\"{interface}\""))
}

fn iwd_signal_to_percentage(signal: i16) -> u8 {
    let percentage = (i32::from(signal) + 10_000) * 100 / 6_000;
    percentage.clamp(0, 100) as u8
}

fn iwd_security(network_type: &str) -> WifiSecurity {
    match network_type {
        "open" => WifiSecurity::Open,
        "wep" => WifiSecurity::Wep,
        "psk" => WifiSecurity::WpaPersonal,
        "8021x" => WifiSecurity::WpaEnterprise,
        _ => WifiSecurity::Unknown,
    }
}

fn merge_network(networks: &mut BTreeMap<String, WifiNetwork>, network: WifiNetwork) {
    networks
        .entry(network.ssid.clone())
        .and_modify(|existing| {
            existing.signal_strength = existing.signal_strength.max(network.signal_strength);
            existing.connected |= network.connected;
            if network.connected || !existing.secure {
                existing.security = network.security;
                existing.secure = network.secure;
            }
        })
        .or_insert(network);
}

fn dbus_error(error: impl Display) -> BackendError {
    BackendError::Operation(error.to_string())
}
