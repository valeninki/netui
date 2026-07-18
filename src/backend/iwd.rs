use std::{collections::BTreeMap, time::Duration};

use async_trait::async_trait;
use futures_util::{StreamExt, stream::FuturesUnordered};
use tokio::{sync::mpsc, task::JoinHandle};
use zbus::{Connection, fdo::PropertiesProxy, proxy, zvariant::OwnedObjectPath};

use crate::app::BackendEvent;

use super::{BackendError, WifiBackend, WifiNetwork, WifiSecurity};

pub const IWD_SERVICE: &str = "net.connman.iwd";

const IWD_ROOT_PATH: &str = "/net/connman/iwd";
const STATION_INTERFACE: &str = "net.connman.iwd.Station";
const REFRESH_INTERVAL: Duration = Duration::from_secs(30);
const RETRY_DELAY: Duration = Duration::from_secs(2);

#[proxy(
    interface = "net.connman.iwd.Station",
    default_service = "net.connman.iwd"
)]
trait IwdStation {
    fn scan(&self) -> zbus::Result<()>;
    fn get_ordered_networks(&self) -> zbus::Result<Vec<(OwnedObjectPath, i16)>>;
}

#[proxy(
    interface = "net.connman.iwd.Network",
    default_service = "net.connman.iwd"
)]
trait IwdNetwork {
    #[zbus(property)]
    fn name(&self) -> zbus::Result<String>;

    #[zbus(property, name = "Type")]
    fn network_type(&self) -> zbus::Result<String>;

    #[zbus(property)]
    fn connected(&self) -> zbus::Result<bool>;
}

#[proxy(
    interface = "org.freedesktop.DBus.Introspectable",
    default_service = "net.connman.iwd"
)]
trait Introspectable {
    fn introspect(&self) -> zbus::Result<String>;
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

fn dbus_error(error: zbus::Error) -> BackendError {
    BackendError::Operation(error.to_string())
}
