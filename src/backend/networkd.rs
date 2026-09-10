use std::{net::IpAddr, time::Duration};

use async_trait::async_trait;
use futures_util::{StreamExt, stream::FuturesUnordered};
use tokio::{sync::mpsc, task::JoinHandle};
use zbus::{Connection, fdo::PropertiesProxy, proxy, zvariant::OwnedObjectPath};

use crate::app::BackendEvent;

use super::{BackendError, NetworkInterface, NetworkdManager, OperationalState};

const NETWORKD_SERVICE: &str = "org.freedesktop.network1";
const NETWORKD_MANAGER_PATH: &str = "/org/freedesktop/network1";
const LINK_REFRESH_INTERVAL: Duration = Duration::from_secs(30);
const RETRY_DELAY: Duration = Duration::from_secs(2);

#[proxy(
    interface = "org.freedesktop.network1.Manager",
    default_service = "org.freedesktop.network1",
    default_path = "/org/freedesktop/network1"
)]
trait Network1Manager {
    fn list_links(&self) -> zbus::Result<Vec<(i32, String, OwnedObjectPath)>>;
    fn describe_link(&self, ifindex: i32) -> zbus::Result<String>;
}

#[derive(Clone)]
pub struct NetworkdDbus {
    connection: Connection,
}

impl NetworkdDbus {
    pub async fn connect_system() -> Result<Self, BackendError> {
        let connection = Connection::system().await.map_err(dbus_error)?;
        Ok(Self { connection })
    }

    pub fn spawn_listener(&self, sender: mpsc::Sender<BackendEvent>) -> JoinHandle<()> {
        let manager = self.clone();

        tokio::spawn(async move {
            loop {
                match manager.get_interfaces().await {
                    Ok(interfaces) => {
                        if sender
                            .send(BackendEvent::InterfacesUpdated(interfaces))
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

                match tokio::time::timeout(LINK_REFRESH_INTERVAL, manager.wait_for_link_change())
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

    async fn manager_proxy(&self) -> Result<Network1ManagerProxy<'_>, BackendError> {
        Network1ManagerProxy::builder(&self.connection)
            .destination(NETWORKD_SERVICE)
            .map_err(dbus_error)?
            .path(NETWORKD_MANAGER_PATH)
            .map_err(dbus_error)?
            .build()
            .await
            .map_err(dbus_error)
    }

    async fn wait_for_link_change(&self) -> Result<(), BackendError> {
        let links = self
            .manager_proxy()
            .await?
            .list_links()
            .await
            .map_err(dbus_error)?;
        let mut changes = FuturesUnordered::new();

        for (_, _, path) in links {
            let properties = PropertiesProxy::builder(&self.connection)
                .destination(NETWORKD_SERVICE)
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
impl NetworkdManager for NetworkdDbus {
    async fn get_interfaces(&self) -> Result<Vec<NetworkInterface>, BackendError> {
        let manager = self.manager_proxy().await?;
        let links = manager.list_links().await.map_err(dbus_error)?;
        let mut interfaces = Vec::with_capacity(links.len());

        for (ifindex, name, _) in links {
            let description = manager.describe_link(ifindex).await.map_err(dbus_error)?;
            let description: serde_json::Value =
                serde_json::from_str(&description).map_err(json_error)?;
            let operational_state = description
                .get("OperationalState")
                .and_then(serde_json::Value::as_str)
                .map(parse_operational_state)
                .unwrap_or(OperationalState::Unknown);
            let carrier = description
                .get("CarrierState")
                .and_then(serde_json::Value::as_str)
                == Some("carrier");
            let link_type = description
                .get("Type")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            let is_loopback = name == "lo" || link_type == "loopback";
            let ip_addresses = description
                .get("Addresses")
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|address| address.get("AddressString"))
                .filter_map(serde_json::Value::as_str)
                .filter_map(|address| address.parse::<IpAddr>().ok())
                .collect();

            interfaces.push(NetworkInterface {
                name,
                operational_state,
                carrier,
                ip_addresses,
                is_wired: link_type == "ether",
                is_wireless: link_type == "wlan",
                is_loopback,
            });
        }

        Ok(interfaces)
    }
}

fn parse_operational_state(state: &str) -> OperationalState {
    match state {
        "off" => OperationalState::Off,
        "no-carrier" => OperationalState::NoCarrier,
        "dormant" => OperationalState::Dormant,
        "degraded" => OperationalState::Degraded,
        "routable" => OperationalState::Routable,
        _ => OperationalState::Unknown,
    }
}

fn dbus_error(error: zbus::Error) -> BackendError {
    BackendError::Operation(error.to_string())
}

fn json_error(error: serde_json::Error) -> BackendError {
    BackendError::Operation(format!("invalid networkd link description: {error}"))
}
