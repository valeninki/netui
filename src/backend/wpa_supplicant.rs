use std::{collections::BTreeMap, collections::HashMap, future::Future, pin::Pin, time::Duration};

use async_trait::async_trait;
use futures_util::{StreamExt, stream::FuturesUnordered};
use tokio::{sync::mpsc, task::JoinHandle};
use zbus::{
    Connection, proxy,
    zvariant::{OwnedObjectPath, OwnedValue, Value},
};

use crate::app::BackendEvent;

use super::{BackendError, WifiBackend, WifiNetwork, WifiSecurity};

pub const WPA_SUPPLICANT_SERVICE: &str = "fi.w1.wpa_supplicant1";

const WPA_SUPPLICANT_PATH: &str = "/fi/w1/wpa_supplicant1";
const REFRESH_INTERVAL: Duration = Duration::from_secs(30);
const RETRY_DELAY: Duration = Duration::from_secs(2);

#[proxy(
    interface = "fi.w1.wpa_supplicant1",
    default_service = "fi.w1.wpa_supplicant1",
    default_path = "/fi/w1/wpa_supplicant1"
)]
trait WpaSupplicant {
    #[zbus(property)]
    fn interfaces(&self) -> zbus::Result<Vec<OwnedObjectPath>>;

    #[zbus(signal)]
    fn interface_added(
        &self,
        interface: OwnedObjectPath,
        properties: HashMap<String, OwnedValue>,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    fn interface_removed(&self, interface: OwnedObjectPath) -> zbus::Result<()>;
}

#[proxy(
    interface = "fi.w1.wpa_supplicant1.Interface",
    default_service = "fi.w1.wpa_supplicant1"
)]
trait WpaInterface {
    fn scan(&self, args: HashMap<&str, Value<'_>>) -> zbus::Result<()>;

    #[zbus(property, name = "BSSs")]
    fn bsses(&self) -> zbus::Result<Vec<OwnedObjectPath>>;

    #[zbus(property, name = "CurrentBSS")]
    fn current_bss(&self) -> zbus::Result<OwnedObjectPath>;

    #[zbus(signal)]
    fn scan_done(&self, success: bool) -> zbus::Result<()>;

    #[zbus(signal)]
    fn bss_added(
        &self,
        bss: OwnedObjectPath,
        properties: HashMap<String, OwnedValue>,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    fn bss_removed(&self, bss: OwnedObjectPath) -> zbus::Result<()>;
}

#[proxy(
    interface = "fi.w1.wpa_supplicant1.BSS",
    default_service = "fi.w1.wpa_supplicant1"
)]
trait WpaBss {
    #[zbus(property, name = "SSID")]
    fn ssid(&self) -> zbus::Result<Vec<u8>>;

    #[zbus(property)]
    fn signal(&self) -> zbus::Result<i16>;

    #[zbus(property)]
    fn privacy(&self) -> zbus::Result<bool>;

    #[zbus(property, name = "WPA")]
    fn wpa(&self) -> zbus::Result<HashMap<String, OwnedValue>>;

    #[zbus(property, name = "RSN")]
    fn rsn(&self) -> zbus::Result<HashMap<String, OwnedValue>>;
}

#[derive(Clone)]
pub struct WpaSupplicantBackend {
    connection: Connection,
}

impl WpaSupplicantBackend {
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

                match tokio::time::timeout(REFRESH_INTERVAL, backend.wait_for_change()).await {
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

    async fn manager_proxy(&self) -> Result<WpaSupplicantProxy<'_>, BackendError> {
        WpaSupplicantProxy::builder(&self.connection)
            .path(WPA_SUPPLICANT_PATH)
            .map_err(dbus_error)?
            .build()
            .await
            .map_err(dbus_error)
    }

    async fn interface_proxy(
        &self,
        path: OwnedObjectPath,
    ) -> Result<WpaInterfaceProxy<'_>, BackendError> {
        WpaInterfaceProxy::builder(&self.connection)
            .path(path)
            .map_err(dbus_error)?
            .build()
            .await
            .map_err(dbus_error)
    }

    async fn bss_proxy(&self, path: OwnedObjectPath) -> Result<WpaBssProxy<'_>, BackendError> {
        WpaBssProxy::builder(&self.connection)
            .path(path)
            .map_err(dbus_error)?
            .build()
            .await
            .map_err(dbus_error)
    }

    async fn wait_for_change(&self) -> Result<(), BackendError> {
        let manager = self.manager_proxy().await?;
        let mut changes: FuturesUnordered<Pin<Box<dyn Future<Output = ()> + Send>>> =
            FuturesUnordered::new();
        let mut interface_added = manager
            .receive_interface_added()
            .await
            .map_err(dbus_error)?;
        let mut interface_removed = manager
            .receive_interface_removed()
            .await
            .map_err(dbus_error)?;

        changes.push(Box::pin(async move {
            tokio::select! {
                _ = interface_added.next() => {}
                _ = interface_removed.next() => {}
            }
        }));

        for path in manager.interfaces().await.map_err(dbus_error)? {
            let interface = self.interface_proxy(path).await?;
            let mut scan_done = interface.receive_scan_done().await.map_err(dbus_error)?;
            let mut bss_added = interface.receive_bss_added().await.map_err(dbus_error)?;
            let mut bss_removed = interface.receive_bss_removed().await.map_err(dbus_error)?;

            changes.push(Box::pin(async move {
                tokio::select! {
                    _ = scan_done.next() => {}
                    _ = bss_added.next() => {}
                    _ = bss_removed.next() => {}
                }
            }));
        }

        let _ = changes.next().await;
        Ok(())
    }
}

#[async_trait]
impl WifiBackend for WpaSupplicantBackend {
    async fn scan(&self) -> Result<(), BackendError> {
        let manager = self.manager_proxy().await?;
        let interfaces = manager.interfaces().await.map_err(dbus_error)?;
        if interfaces.is_empty() {
            return Err(BackendError::Unavailable(
                "wpa_supplicant is not managing a wireless interface".into(),
            ));
        }

        for path in interfaces {
            let mut args = HashMap::new();
            args.insert("Type", Value::from("active"));
            self.interface_proxy(path)
                .await?
                .scan(args)
                .await
                .map_err(dbus_error)?;
        }

        Ok(())
    }

    async fn get_networks(&self) -> Result<Vec<WifiNetwork>, BackendError> {
        let manager = self.manager_proxy().await?;
        let mut networks = BTreeMap::new();

        for interface_path in manager.interfaces().await.map_err(dbus_error)? {
            let interface = self.interface_proxy(interface_path).await?;
            let current_bss = interface.current_bss().await.map_err(dbus_error)?;

            for path in interface.bsses().await.map_err(dbus_error)? {
                let connected = path == current_bss;
                let bss = self.bss_proxy(path).await?;
                let security = wpa_security(
                    &bss.wpa().await.map_err(dbus_error)?,
                    &bss.rsn().await.map_err(dbus_error)?,
                    bss.privacy().await.map_err(dbus_error)?,
                );
                let ssid = String::from_utf8_lossy(&bss.ssid().await.map_err(dbus_error)?).into();
                let wifi_network = WifiNetwork {
                    ssid,
                    signal_strength: dbm_to_percentage(bss.signal().await.map_err(dbus_error)?),
                    security,
                    secure: security.is_secure(),
                    connected,
                };

                merge_network(&mut networks, wifi_network);
            }
        }

        Ok(networks.into_values().collect())
    }
}

fn dbm_to_percentage(dbm: i16) -> u8 {
    let percentage = (i32::from(dbm) + 100) * 100 / 60;
    percentage.clamp(0, 100) as u8
}

fn wpa_security(
    wpa: &HashMap<String, OwnedValue>,
    rsn: &HashMap<String, OwnedValue>,
    privacy: bool,
) -> WifiSecurity {
    if !rsn.is_empty() {
        WifiSecurity::Rsn
    } else if !wpa.is_empty() {
        WifiSecurity::Wpa
    } else if privacy {
        WifiSecurity::Wep
    } else {
        WifiSecurity::Open
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
