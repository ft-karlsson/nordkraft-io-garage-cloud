//! One reader per meter kind. A reader turns a device into a [`Reading`]
//! and knows nothing about nodes or NATS.

pub mod netio;
#[cfg(feature = "ble")]
pub mod shelly_ble;
pub mod shelly_http;

use crate::config::MeterConfig;
pub use crate::shelly_rpc::Reading;
use std::future::Future;
use std::pin::Pin;

pub type ReadFuture<'a> = Pin<Box<dyn Future<Output = Result<Reading, String>> + Send + 'a>>;

pub trait Meter: Send + Sync {
    /// Short tag stored with each sample: `shelly-http`, `shelly-ble`, `netio`.
    fn source(&self) -> &'static str;
    fn read(&self) -> ReadFuture<'_>;
}

pub fn build(cfg: &MeterConfig) -> Result<Box<dyn Meter>, String> {
    cfg.validate()?;
    match cfg.kind.as_str() {
        "shelly-http" => Ok(Box::new(shelly_http::ShellyHttp::new(cfg)?)),
        "netio" => Ok(Box::new(netio::Netio::new(cfg)?)),
        #[cfg(feature = "ble")]
        "shelly-ble" => Ok(Box::new(shelly_ble::ShellyBle::new(cfg)?)),
        #[cfg(not(feature = "ble"))]
        "shelly-ble" => Err("this build has no Bluetooth support (feature `ble`)".into()),
        other => Err(format!("unknown meter kind '{other}'")),
    }
}
