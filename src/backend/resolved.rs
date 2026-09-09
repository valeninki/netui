use tokio::fs;
use zbus::{Connection, proxy};

use super::{BackendError, DnsOverTlsMode, dbus_service_error, service_has_owner};

const RESOLVED_SERVICE: &str = "org.freedesktop.resolve1";
const RESOLVED_MANAGER_PATH: &str = "/org/freedesktop/resolve1";

#[proxy(
    interface = "org.freedesktop.resolve1.Manager",
    default_service = "org.freedesktop.resolve1",
    default_path = "/org/freedesktop/resolve1"
)]
trait ResolvedManager {
    fn set_link_dns_over_tls(&self, ifindex: i32, mode: &str) -> zbus::Result<()>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DnsPolicyUpdate {
    Applied,
    SkippedUnavailable,
}

pub async fn active_wifi_interface() -> Result<String, BackendError> {
    let mut entries = fs::read_dir("/sys/class/net").await.map_err(io_error)?;

    while let Some(entry) = entries.next_entry().await.map_err(io_error)? {
        let path = entry.path();
        if fs::metadata(path.join("wireless")).await.is_err() {
            continue;
        }

        let carrier = fs::read_to_string(path.join("carrier"))
            .await
            .unwrap_or_default();
        if carrier.trim() == "1" {
            return entry.file_name().into_string().map_err(|_| {
                BackendError::Operation("wireless interface name is not UTF-8".into())
            });
        }
    }

    Err(BackendError::Unavailable(
        "no active Wi-Fi interface found".into(),
    ))
}

pub async fn set_link_dns_over_tls(
    interface: &str,
    mode: DnsOverTlsMode,
) -> Result<DnsPolicyUpdate, BackendError> {
    let connection = Connection::system().await.map_err(dbus_error)?;
    if !service_has_owner(&connection, RESOLVED_SERVICE).await? {
        return Ok(DnsPolicyUpdate::SkippedUnavailable);
    }

    let ifindex = fs::read_to_string(format!("/sys/class/net/{interface}/ifindex"))
        .await
        .map_err(io_error)?
        .trim()
        .parse::<i32>()
        .map_err(|error| {
            BackendError::Operation(format!("invalid ifindex for {interface}: {error}"))
        })?;
    let manager = ResolvedManagerProxy::builder(&connection)
        .destination(RESOLVED_SERVICE)
        .map_err(resolved_error)?
        .path(RESOLVED_MANAGER_PATH)
        .map_err(resolved_error)?
        .build()
        .await
        .map_err(resolved_error)?;

    match manager
        .set_link_dns_over_tls(ifindex, mode.resolved_mode())
        .await
    {
        Ok(()) => Ok(DnsPolicyUpdate::Applied),
        Err(error) => match resolved_error(error) {
            error if error.is_service_unavailable() => Ok(DnsPolicyUpdate::SkippedUnavailable),
            error => Err(error),
        },
    }
}

fn io_error(error: std::io::Error) -> BackendError {
    BackendError::Operation(error.to_string())
}

fn dbus_error(error: zbus::Error) -> BackendError {
    dbus_service_error("D-Bus service", error)
}

fn resolved_error(error: zbus::Error) -> BackendError {
    dbus_service_error(RESOLVED_SERVICE, error)
}
