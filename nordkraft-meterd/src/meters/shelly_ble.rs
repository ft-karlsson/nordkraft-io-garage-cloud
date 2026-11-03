//! Shelly Gen2/Gen3 over Bluetooth Low Energy: the same JSON-RPC, carried
//! over the device's GATT RPC service (the Mongoose OS one Shelly builds on).
//!
//! ```text
//! service   5f6d4f53-5f52-5043-5f53-56435f49445f   "_mOS_RPC_SVC_ID_"
//! data      5f6d4f53-5f52-5043-5f64-6174615f5f5f   "_mOS_RPC_data___"  read/write
//! tx_ctl    5f6d4f53-5f52-5043-5f74-785f63746c5f   "_mOS_RPC_tx_ctl_"  write: frame length (u32 BE)
//! rx_ctl    5f6d4f53-5f52-5043-5f72-785f63746c5f   "_mOS_RPC_rx_ctl_"  read: reply length (u32 BE)
//! ```
//!
//! Write the length, write the frame in chunks, read the reply length, read
//! the reply in chunks. The device must have `BLE.rpc.enable` on and, once
//! provisioning is complete, be bonded to this host (`bluetoothctl pair`
//! during a `BLE.StartPairing` window). No IP network is involved: the
//! plug reaches nothing, and nothing reaches the plug but this host.
//!
//! The connection is made per reading and dropped again; once a minute
//! that is nothing, and a dropped link heals itself on the next tick.

use super::{Meter, ReadFuture, Reading};
use crate::config::MeterConfig;
use crate::shelly_rpc::{self, Component};
use btleplug::api::{
    Central, CentralEvent, Characteristic, Manager as _, Peripheral as _, ScanFilter, WriteType,
};
use btleplug::platform::{Adapter, Manager, Peripheral};
use futures::StreamExt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::time::timeout;
use uuid::Uuid;

pub const SVC: Uuid = Uuid::from_u128(0x5f6d4f53_5f52_5043_5f53_56435f49445f);
pub const DATA: Uuid = Uuid::from_u128(0x5f6d4f53_5f52_5043_5f64_6174615f5f5f);
pub const TX_CTL: Uuid = Uuid::from_u128(0x5f6d4f53_5f52_5043_5f74_785f63746c5f);
pub const RX_CTL: Uuid = Uuid::from_u128(0x5f6d4f53_5f52_5043_5f72_785f63746c5f);

/// Conservative write size: fits the default ATT MTU, so no negotiation
/// is needed. A status request is a few writes.
const CHUNK: usize = 20;
const SCAN_FOR: Duration = Duration::from_secs(12);

pub struct ShellyBle {
    address: Option<String>,
    name: Option<String>,
    component: Component,
    component_id: u32,
    seq: AtomicU64,
}

impl ShellyBle {
    pub fn new(cfg: &MeterConfig) -> Result<Self, String> {
        Ok(Self {
            address: cfg.address.as_ref().map(|a| a.to_uppercase()),
            name: cfg.name.clone(),
            component: Component::parse(&cfg.component).ok_or("bad component")?,
            component_id: cfg.component_id,
            seq: AtomicU64::new(1),
        })
    }

    fn matches(&self, address: &str, local_name: Option<&str>) -> bool {
        if let Some(a) = &self.address {
            if address.eq_ignore_ascii_case(a) {
                return true;
            }
        }
        if let (Some(want), Some(got)) = (&self.name, local_name) {
            if want.eq_ignore_ascii_case(got) {
                return true;
            }
        }
        false
    }
}

async fn adapter() -> Result<Adapter, String> {
    let manager = Manager::new()
        .await
        .map_err(|e| format!("bluetooth: {e}"))?;
    manager
        .adapters()
        .await
        .map_err(|e| format!("bluetooth: {e}"))?
        .into_iter()
        .next()
        .ok_or_else(|| "no bluetooth adapter (is bluetoothd running?)".to_string())
}

/// Scan until the configured device shows up, or give up.
async fn find(me: &ShellyBle, central: &Adapter) -> Result<Peripheral, String> {
    // Already known to the adapter from an earlier scan?
    for p in central.peripherals().await.map_err(|e| e.to_string())? {
        let name = p
            .properties()
            .await
            .ok()
            .flatten()
            .and_then(|pr| pr.local_name);
        if me.matches(&p.address().to_string(), name.as_deref()) {
            return Ok(p);
        }
    }
    let mut events = central.events().await.map_err(|e| e.to_string())?;
    central
        .start_scan(ScanFilter::default())
        .await
        .map_err(|e| format!("scan: {e}"))?;
    let found = timeout(SCAN_FOR, async {
        while let Some(ev) = events.next().await {
            let id = match ev {
                CentralEvent::DeviceDiscovered(id) | CentralEvent::DeviceUpdated(id) => id,
                _ => continue,
            };
            let Ok(p) = central.peripheral(&id).await else {
                continue;
            };
            let name = p
                .properties()
                .await
                .ok()
                .flatten()
                .and_then(|pr| pr.local_name);
            if me.matches(&p.address().to_string(), name.as_deref()) {
                return Some(p);
            }
        }
        None
    })
    .await;
    let _ = central.stop_scan().await;
    match found {
        Ok(Some(p)) => Ok(p),
        _ => Err(format!(
            "device not seen in {}s ({})",
            SCAN_FOR.as_secs(),
            me.address
                .clone()
                .or_else(|| me.name.clone())
                .unwrap_or_default()
        )),
    }
}

fn chr(p: &Peripheral, uuid: Uuid) -> Result<Characteristic, String> {
    p.characteristics()
        .into_iter()
        .find(|c| c.uuid == uuid)
        .ok_or_else(|| format!("characteristic {uuid} missing (BLE RPC not enabled?)"))
}

async fn rpc(p: &Peripheral, frame: &[u8]) -> Result<Vec<u8>, String> {
    let data = chr(p, DATA)?;
    let tx = chr(p, TX_CTL)?;
    let rx = chr(p, RX_CTL)?;
    p.write(
        &tx,
        &(frame.len() as u32).to_be_bytes(),
        WriteType::WithResponse,
    )
    .await
    .map_err(|e| format!("tx_ctl: {e}"))?;
    for chunk in frame.chunks(CHUNK) {
        p.write(&data, chunk, WriteType::WithResponse)
            .await
            .map_err(|e| format!("data write: {e}"))?;
    }
    let len = p.read(&rx).await.map_err(|e| format!("rx_ctl: {e}"))?;
    if len.len() < 4 {
        return Err("short rx_ctl".into());
    }
    let want = u32::from_be_bytes([len[0], len[1], len[2], len[3]]) as usize;
    if want == 0 || want > 64 * 1024 {
        return Err(format!("implausible reply length {want}"));
    }
    let mut out = Vec::with_capacity(want);
    while out.len() < want {
        let part = p.read(&data).await.map_err(|e| format!("data read: {e}"))?;
        if part.is_empty() {
            return Err("reply ended early".into());
        }
        out.extend_from_slice(&part);
    }
    out.truncate(want);
    Ok(out)
}

impl Meter for ShellyBle {
    fn source(&self) -> &'static str {
        "shelly-ble"
    }

    fn read(&self) -> ReadFuture<'_> {
        Box::pin(async move {
            let central = adapter().await?;
            let p = find(self, &central).await?;
            if !p.is_connected().await.unwrap_or(false) {
                timeout(Duration::from_secs(15), p.connect())
                    .await
                    .map_err(|_| "connect timed out".to_string())?
                    .map_err(|e| format!("connect: {e} (paired?)"))?;
            }
            let result = async {
                p.discover_services()
                    .await
                    .map_err(|e| format!("discover: {e}"))?;
                let id = self.seq.fetch_add(1, Ordering::Relaxed);
                let frame =
                    shelly_rpc::request(id, self.component.status_method(), self.component_id);
                let body = rpc(&p, &frame).await?;
                let mut reading = shelly_rpc::parse_status(self.component, &body)?;
                if self.component == Component::Em1 {
                    let frame = shelly_rpc::request(id + 1, "EM1Data.GetStatus", self.component_id);
                    if let Ok(data) = rpc(&p, &frame).await {
                        reading.energy_wh = shelly_rpc::em1_energy(&data);
                    }
                }
                Ok::<Reading, String>(reading)
            }
            .await;
            let _ = p.disconnect().await;
            result
        })
    }
}

/// One device seen by `nordkraft-meterd scan`.
pub struct Seen {
    pub address: String,
    pub name: Option<String>,
    pub rssi: i16,
    /// Advertised the Shelly RPC service, or carries a Shelly name.
    pub shelly: bool,
}

/// `nordkraft-meterd scan`: list everything nearby that has a name, Shelly
/// devices first. The scan is unfiltered on purpose: a plug does not always
/// put the RPC service UUID in its advertisement, so filtering on it can
/// hide the very device being looked for.
pub async fn scan(seconds: u64) -> Result<Vec<Seen>, String> {
    let central = adapter().await?;
    central
        .start_scan(ScanFilter::default())
        .await
        .map_err(|e| format!("scan: {e}"))?;
    tokio::time::sleep(Duration::from_secs(seconds)).await;
    let _ = central.stop_scan().await;
    let mut out = Vec::new();
    for p in central.peripherals().await.map_err(|e| e.to_string())? {
        let Some(props) = p.properties().await.ok().flatten() else {
            continue;
        };
        let name = props.local_name.clone();
        let shelly = props.services.contains(&SVC)
            || name
                .as_deref()
                .is_some_and(|n| n.to_ascii_lowercase().starts_with("shelly"));
        if name.is_none() && !shelly {
            continue;
        }
        out.push(Seen {
            address: p.address().to_string(),
            name,
            rssi: props.rssi.unwrap_or(0),
            shelly,
        });
    }
    out.sort_by_key(|s| (!s.shelly, -(s.rssi as i32)));
    Ok(out)
}
