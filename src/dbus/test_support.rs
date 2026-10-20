//! Test-only NetworkManager service hosted on an isolated D-Bus daemon.

#![allow(dead_code)]

use std::collections::HashMap;
use std::error::Error;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

use zbus::connection::Builder;
use zbus::interface;
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

const NETWORK_MANAGER_NAME: &str = "org.freedesktop.NetworkManager";
const NETWORK_MANAGER_PATH: &str = "/org/freedesktop/NetworkManager";
const SETTINGS_PATH: &str = "/org/freedesktop/NetworkManager/Settings";
const WIFI_DEVICE_PATH: &str = "/org/freedesktop/NetworkManager/Devices/1";

pub(crate) type TestResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

/// Properties exposed by one fake NetworkManager access point.
#[derive(Clone, Debug)]
pub(crate) struct FakeAccessPointConfig {
    pub(crate) path: OwnedObjectPath,
    pub(crate) ssid: Vec<u8>,
    pub(crate) strength: u8,
    pub(crate) frequency: u32,
    pub(crate) flags: u32,
    pub(crate) wpa_flags: u32,
    pub(crate) rsn_flags: u32,
    pub(crate) hw_address: String,
}

impl FakeAccessPointConfig {
    pub(crate) fn new(path_suffix: u32, ssid: &str) -> Self {
        Self {
            path: OwnedObjectPath::try_from(format!(
                "/org/freedesktop/NetworkManager/AccessPoint/{path_suffix}"
            ))
            .expect("the generated access-point path is valid"),
            ssid: ssid.as_bytes().to_vec(),
            strength: 75,
            frequency: 5_180,
            flags: 0,
            wpa_flags: 0,
            rsn_flags: 0,
            hw_address: format!("02:00:00:00:00:{path_suffix:02x}"),
        }
    }
}

/// Isolated fake service plus the client connection used by `WifiManager`.
pub(crate) struct FakeNetworkManager {
    client: zbus::Connection,
    service: zbus::Connection,
    access_points: Arc<Mutex<Vec<OwnedObjectPath>>>,
    _bus: PrivateBus,
}

impl FakeNetworkManager {
    pub(crate) async fn start() -> TestResult<Self> {
        let bus = PrivateBus::start()?;
        let access_points = Arc::new(Mutex::new(Vec::new()));
        let device_path = OwnedObjectPath::try_from(WIFI_DEVICE_PATH)?;

        let service = Builder::address(bus.address.as_str())?
            .name(NETWORK_MANAGER_NAME)?
            .serve_at(NETWORK_MANAGER_PATH, FakeNetworkManagerRoot { device_path })?
            .serve_at(WIFI_DEVICE_PATH, FakeDevice)?
            .serve_at(WIFI_DEVICE_PATH, FakeWireless { access_points: Arc::clone(&access_points) })?
            .serve_at(SETTINGS_PATH, FakeSettings)?
            .build()
            .await?;

        let client = Builder::address(bus.address.as_str())?.build().await?;

        Ok(Self { client, service, access_points, _bus: bus })
    }

    pub(crate) fn client_connection(&self) -> zbus::Connection {
        self.client.clone()
    }

    pub(crate) async fn add_access_point(&self, config: FakeAccessPointConfig) -> TestResult<()> {
        let path = config.path.clone();
        self.service.object_server().at(path.clone(), FakeAccessPoint::from(config)).await?;
        self.access_points.lock().expect("fake AP list lock poisoned").push(path);
        Ok(())
    }

    pub(crate) async fn remove_access_point(&self, path: &OwnedObjectPath) -> TestResult<()> {
        self.access_points
            .lock()
            .expect("fake AP list lock poisoned")
            .retain(|candidate| candidate != path);
        self.service.object_server().remove::<FakeAccessPoint, _>(path.clone()).await?;
        Ok(())
    }
}

struct PrivateBus {
    address: String,
    child: Child,
}

impl PrivateBus {
    fn start() -> TestResult<Self> {
        let mut child = Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--nopidfile", "--print-address=1"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;

        let stdout = child.stdout.take().ok_or("dbus-daemon did not expose stdout")?;
        let mut address = String::new();
        BufReader::new(stdout).read_line(&mut address)?;
        let address = address.trim().to_owned();
        if address.is_empty() {
            return Err("dbus-daemon did not print a bus address".into());
        }

        Ok(Self { address, child })
    }
}

impl Drop for PrivateBus {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct FakeNetworkManagerRoot {
    device_path: OwnedObjectPath,
}

#[interface(name = "org.freedesktop.NetworkManager")]
impl FakeNetworkManagerRoot {
    fn get_devices(&self) -> Vec<OwnedObjectPath> {
        vec![self.device_path.clone()]
    }

    #[zbus(property)]
    fn wireless_enabled(&self) -> bool {
        true
    }
}

struct FakeDevice;

#[interface(name = "org.freedesktop.NetworkManager.Device")]
impl FakeDevice {
    #[zbus(property)]
    fn device_type(&self) -> u32 {
        2
    }

    #[zbus(property)]
    fn active_connection(&self) -> OwnedObjectPath {
        OwnedObjectPath::try_from("/").expect("root is a valid object path")
    }
}

struct FakeWireless {
    access_points: Arc<Mutex<Vec<OwnedObjectPath>>>,
}

#[interface(name = "org.freedesktop.NetworkManager.Device.Wireless")]
impl FakeWireless {
    fn request_scan(&self, _options: HashMap<String, OwnedValue>) {}

    #[zbus(property)]
    fn access_points(&self) -> Vec<OwnedObjectPath> {
        self.access_points.lock().expect("fake AP list lock poisoned").clone()
    }
}

struct FakeSettings;

#[interface(name = "org.freedesktop.NetworkManager.Settings")]
impl FakeSettings {
    fn list_connections(&self) -> Vec<OwnedObjectPath> {
        Vec::new()
    }
}

struct FakeAccessPoint {
    ssid: Vec<u8>,
    strength: u8,
    frequency: u32,
    flags: u32,
    wpa_flags: u32,
    rsn_flags: u32,
    hw_address: String,
}

impl From<FakeAccessPointConfig> for FakeAccessPoint {
    fn from(config: FakeAccessPointConfig) -> Self {
        Self {
            ssid: config.ssid,
            strength: config.strength,
            frequency: config.frequency,
            flags: config.flags,
            wpa_flags: config.wpa_flags,
            rsn_flags: config.rsn_flags,
            hw_address: config.hw_address,
        }
    }
}

#[interface(name = "org.freedesktop.NetworkManager.AccessPoint")]
impl FakeAccessPoint {
    #[zbus(property)]
    fn ssid(&self) -> Vec<u8> {
        self.ssid.clone()
    }

    #[zbus(property)]
    fn strength(&self) -> u8 {
        self.strength
    }

    #[zbus(property)]
    fn frequency(&self) -> u32 {
        self.frequency
    }

    #[zbus(property)]
    fn flags(&self) -> u32 {
        self.flags
    }

    #[zbus(property)]
    fn wpa_flags(&self) -> u32 {
        self.wpa_flags
    }

    #[zbus(property)]
    fn rsn_flags(&self) -> u32 {
        self.rsn_flags
    }

    #[zbus(property)]
    fn hw_address(&self) -> &str {
        &self.hw_address
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dbus::network_manager::WifiManager;

    #[test]
    fn serves_a_stable_network_snapshot() -> TestResult<()> {
        zbus::block_on(async {
            let fake = FakeNetworkManager::start().await?;
            let access_point = FakeAccessPointConfig::new(1, "Test Network");
            let access_point_path = access_point.path.clone();
            fake.add_access_point(access_point).await?;

            let wifi = WifiManager::from_connection(fake.client_connection()).await?;
            let networks = wifi.get_networks().await?;

            assert_eq!(networks.len(), 1);
            assert_eq!(networks[0].ssid, "Test Network");
            assert_eq!(networks[0].ap_path, access_point_path.as_str());

            fake.remove_access_point(&access_point_path).await?;
            assert!(wifi.get_networks().await?.is_empty());

            Ok(())
        })
    }
}
