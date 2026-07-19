use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    future::Future,
    pin::Pin,
    time::{Duration, Instant},
};

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
const EMPTY_REFRESH_INTERVAL: Duration = Duration::from_secs(5);
const SCAN_REQUEST_INTERVAL: Duration = Duration::from_secs(10);
const SCAN_POLL_INTERVAL: Duration = Duration::from_secs(1);
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

    #[zbus(property, name = "Networks")]
    fn networks(&self) -> zbus::Result<Vec<OwnedObjectPath>>;

    #[zbus(property)]
    fn scanning(&self) -> zbus::Result<bool>;

    fn remove_network(&self, network: OwnedObjectPath) -> zbus::Result<()>;
    fn save_config(&self) -> zbus::Result<()>;

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

#[proxy(
    interface = "fi.w1.wpa_supplicant1.Network",
    default_service = "fi.w1.wpa_supplicant1"
)]
trait WpaNetwork {
    #[zbus(property, name = "Properties")]
    fn properties(&self) -> zbus::Result<HashMap<String, OwnedValue>>;
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
            let mut last_scan = None;

            loop {
                let mut scan_active = backend.is_scan_running().await.unwrap_or(false);
                let scan_due = last_scan
                    .map(|instant: Instant| instant.elapsed() >= SCAN_REQUEST_INTERVAL)
                    .unwrap_or(true);

                if !scan_active && scan_due {
                    if sender
                        .send(BackendEvent::WifiScanStateChanged(true))
                        .await
                        .is_err()
                    {
                        return;
                    }
                    last_scan = Some(Instant::now());

                    match backend.scan().await {
                        Ok(()) => scan_active = true,
                        Err(error) if scan_in_progress(&error) => scan_active = true,
                        Err(error) => {
                            if sender
                                .send(BackendEvent::Error(format!(
                                    "Wi-Fi scan request failed: {error}"
                                )))
                                .await
                                .is_err()
                            {
                                return;
                            }
                        }
                    }
                }

                let refresh_interval = match backend.get_networks().await {
                    Ok(networks) => {
                        let refresh_interval = if networks.is_empty() {
                            EMPTY_REFRESH_INTERVAL
                        } else {
                            REFRESH_INTERVAL
                        };
                        if sender
                            .send(BackendEvent::WifiNetworksUpdated(networks))
                            .await
                            .is_err()
                        {
                            return;
                        }
                        refresh_interval
                    }
                    Err(error) => {
                        if sender
                            .send(BackendEvent::Error(error.to_string()))
                            .await
                            .is_err()
                        {
                            return;
                        }
                        RETRY_DELAY
                    }
                };

                let scan_active = backend.is_scan_running().await.unwrap_or(scan_active);
                if sender
                    .send(BackendEvent::WifiScanStateChanged(scan_active))
                    .await
                    .is_err()
                {
                    return;
                }
                let refresh_interval = if scan_active {
                    SCAN_POLL_INTERVAL
                } else {
                    refresh_interval
                };

                match tokio::time::timeout(refresh_interval, backend.wait_for_change()).await {
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

    async fn network_proxy(
        &self,
        path: OwnedObjectPath,
    ) -> Result<WpaNetworkProxy<'_>, BackendError> {
        WpaNetworkProxy::builder(&self.connection)
            .path(path)
            .map_err(dbus_error)?
            .build()
            .await
            .map_err(dbus_error)
    }

    async fn is_scan_running(&self) -> Result<bool, BackendError> {
        let manager = self.manager_proxy().await?;

        for path in manager.interfaces().await.map_err(dbus_error)? {
            if self
                .interface_proxy(path)
                .await?
                .scanning()
                .await
                .map_err(dbus_error)?
            {
                return Ok(true);
            }
        }

        Ok(false)
    }

    async fn forget_network_profile(&self, ssid: String) -> Result<(), BackendError> {
        let manager = self.manager_proxy().await?;

        for interface_path in manager.interfaces().await.map_err(dbus_error)? {
            let interface = self.interface_proxy(interface_path).await?;
            for network_path in interface.networks().await.map_err(dbus_error)? {
                let network = self.network_proxy(network_path.clone()).await?;
                let properties = network.properties().await.map_err(dbus_error)?;
                if configured_ssid(&properties).as_deref() != Some(ssid.as_str()) {
                    continue;
                }

                interface
                    .remove_network(network_path)
                    .await
                    .map_err(dbus_error)?;
                return interface.save_config().await.map_err(dbus_error);
            }
        }

        Err(BackendError::Unavailable(format!(
            "no saved profile exists for Wi-Fi network {ssid:?}"
        )))
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
            let current_bss = interface.current_bss().await.ok();
            let mut known_ssids = BTreeSet::new();

            if let Ok(network_paths) = interface.networks().await {
                for network_path in network_paths {
                    let Ok(network) = self.network_proxy(network_path).await else {
                        continue;
                    };
                    let Ok(properties) = network.properties().await else {
                        continue;
                    };
                    if let Some(ssid) = configured_ssid(&properties) {
                        known_ssids.insert(ssid);
                    }
                }
            }

            let Ok(bss_paths) = interface.bsses().await else {
                continue;
            };
            for path in bss_paths {
                let connected = current_bss.as_ref() == Some(&path);
                let Ok(bss) = self.bss_proxy(path).await else {
                    continue;
                };
                let (Ok(wpa), Ok(rsn), Ok(privacy), Ok(ssid), Ok(signal)) = (
                    bss.wpa().await,
                    bss.rsn().await,
                    bss.privacy().await,
                    bss.ssid().await,
                    bss.signal().await,
                ) else {
                    continue;
                };
                let security = wpa_security(&wpa, &rsn, privacy);
                let ssid = String::from_utf8_lossy(&ssid).into_owned();
                let wifi_network = WifiNetwork {
                    is_known: known_ssids.contains(&ssid),
                    ssid,
                    signal_strength: dbm_to_percentage(signal),
                    security,
                    secure: security.is_secure(),
                    connected,
                };

                merge_network(&mut networks, wifi_network);
            }
        }

        Ok(networks.into_values().collect())
    }

    async fn initiate_connection(
        &self,
        _ssid: String,
        _password: String,
    ) -> Result<(), BackendError> {
        Err(BackendError::Unavailable(
            "wpa_supplicant connection requests are not implemented yet".into(),
        ))
    }

    async fn disconnect_current_network(&self) -> Result<(), BackendError> {
        Err(BackendError::Unavailable(
            "wpa_supplicant disconnection requests are not implemented yet".into(),
        ))
    }

    async fn forget_network(&self, _ssid: String) -> Result<(), BackendError> {
        self.forget_network_profile(_ssid).await
    }
}

fn configured_ssid(properties: &HashMap<String, OwnedValue>) -> Option<String> {
    let ssid = String::try_from(properties.get("ssid")?.clone()).ok()?;
    let ssid = ssid.trim();

    if let Some(ssid) = ssid
        .strip_prefix('"')
        .and_then(|ssid| ssid.strip_suffix('"'))
    {
        return unescape_wpa_string(ssid);
    }

    Some(ssid.to_owned())
}

fn unescape_wpa_string(value: &str) -> Option<String> {
    let mut output = String::new();
    let mut characters = value.chars();

    while let Some(character) = characters.next() {
        if character != '\\' {
            output.push(character);
            continue;
        }

        match characters.next()? {
            '\\' => output.push('\\'),
            '"' => output.push('"'),
            'n' => output.push('\n'),
            'r' => output.push('\r'),
            't' => output.push('\t'),
            other => output.push(other),
        }
    }

    Some(output)
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
            existing.is_known |= network.is_known;
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

fn scan_in_progress(error: &BackendError) -> bool {
    matches!(error, BackendError::Operation(message) if message.contains("InProgress"))
}
