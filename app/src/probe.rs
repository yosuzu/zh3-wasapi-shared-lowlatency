//! Read-only ZH3 discovery. All audio queries open an explicitly identified render endpoint.
mod audio;
mod pnp;

use serde::{Deserialize, Serialize};

/// Physical USB evidence only. This type cannot be used to select a writable profile.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct ConnectedHardware {
    pub usb: String,
    pub container: String,
    pub driver: String,
    pub arrival: Option<u64>,
}

pub fn connected_hardware() -> Result<ConnectedHardware, String> {
    pnp::connected_hardware()
}

/// Resolves the verified physical Fosi KS child even when the playback endpoint is unavailable.
pub fn verified_ks(expected: &ConnectedHardware) -> Result<String, String> {
    Ok(physical_target(expected)?.ks)
}

/// A device suitable for physical PnP operations, without claiming an audio stream.
pub fn physical_target(expected: &ConnectedHardware) -> Result<Device, String> {
    pnp::physical_target(expected)
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Device {
    pub usb: String,
    pub ks: String,
    pub container: String,
    pub driver: String,
    pub endpoint_id: String,
    pub stream: String,
    pub base_ref: String,
    /// Empty on display-only snapshots and when recovery has only registered references.
    pub active_ref: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Format {
    pub rate: u32,
    pub channels: u16,
    pub valid_bits: u16,
    pub bits: u16,
    pub encoding: String,
    pub raw_hex: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Snapshot {
    pub device: Device,
    pub mix: Format,
    pub output: Option<Format>,
    pub current: Option<u32>,
    pub current_rate: Option<u32>,
    pub current_format: Option<Format>,
    pub default_frames: u32,
    pub min_frames: u32,
    pub max_frames: u32,
    pub step_frames: u32,
    pub volume: Option<u32>,
    pub muted: Option<bool>,
    #[serde(default)]
    pub endpoint_period_properties: Option<(Option<Vec<u8>>, bool)>,
}

pub fn read() -> Result<Snapshot, String> {
    snapshot_with(|| pnp::device(false))
}

/// Reads a uniquely verified active endpoint without granting a profile target.
/// All three stream/reference fields are intentionally empty on this display-only path.
pub fn readonly_snapshot() -> Result<Snapshot, String> {
    snapshot_with(pnp::readonly_device)
}

fn snapshot_with(discover: impl Fn() -> Result<Device, String>) -> Result<Snapshot, String> {
    let device = discover()?;
    let snapshot = audio::read(device)?;
    if snapshot.device != discover()? {
        return Err("读取期间 ZH3 的设备或播放接口发生了变化，请刷新。".into());
    }
    Ok(snapshot)
}

pub fn identify() -> Result<Device, String> {
    let device = pnp::device(false)?;
    audio::verify(&device)?;
    Ok(device)
}

/// Returns MMDevice OEM period bytes (PID 6) and whether use-default (PID 7) exists.
pub fn endpoint_period_properties(device: &Device) -> Result<(Option<Vec<u8>>, bool), String> {
    if pnp::device(false)? != *device {
        return Err("ZH3 身份或活动播放引用发生变化，无法读取该端点的周期属性。".into());
    }
    audio::period_properties(device)
}

/// Allows rollback while the MMDevice endpoint or audio engine is offline.
pub fn recovery_device() -> Result<Device, String> {
    let mut device = pnp::device(true)?;
    if !device.endpoint_id.is_empty() {
        match audio::verify(&device) {
            Ok(()) => {}
            Err(e) if e.starts_with("AUDIO_UNAVAILABLE:") => device.endpoint_id.clear(),
            Err(e) => return Err(e),
        }
    }
    Ok(device)
}

/// Verifies a journal's USB and KS identity without requiring a running endpoint.
pub fn verify_physical(device: &Device) -> Result<(), String> {
    pnp::verify_physical(device)
}
