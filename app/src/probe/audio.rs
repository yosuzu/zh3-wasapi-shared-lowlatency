use std::{fmt::Write, ptr, slice};
use windows::Win32::{
    Devices::Properties::DEVPKEY_Device_ContainerId,
    Foundation::{PROPERTYKEY, RPC_E_CHANGED_MODE},
    Media::Audio::{Endpoints::IAudioEndpointVolume, *},
    System::{
        Com::{StructuredStorage::PropVariantToGUID, *},
        Variant::{VT_BLOB, VT_CLSID, VT_EMPTY},
    },
    UI::Shell::PropertiesSystem::IPropertyStore,
};
use windows::core::{HSTRING, Interface, Result as WinResult};

use super::{Device, Format, Snapshot};

trait WinContext<T> {
    fn context(self, action: &str) -> Result<T, String>;
}
impl<T> WinContext<T> for WinResult<T> {
    fn context(self, action: &str) -> Result<T, String> {
        self.map_err(|e| format!("{action}失败（HRESULT 0x{:08X}）。", e.code().0 as u32))
    }
}

struct Apartment(bool);
impl Apartment {
    fn enter() -> Result<Self, String> {
        let result = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if result == RPC_E_CHANGED_MODE {
            return Ok(Self(false));
        }
        result.ok().context("初始化 Windows 音频查询")?;
        Ok(Self(true))
    }
}
impl Drop for Apartment {
    fn drop(&mut self) {
        if self.0 {
            unsafe { CoUninitialize() };
        }
    }
}

struct TaskMem<T>(*mut T);
impl<T> Drop for TaskMem<T> {
    fn drop(&mut self) {
        unsafe { CoTaskMemFree(Some(self.0.cast())) };
    }
}

fn task_string(value: windows::core::PWSTR) -> Result<String, String> {
    if value.is_null() {
        return Err("Windows 未返回音频设备 ID。".into());
    }
    let _allocation = TaskMem(value.0);
    unsafe { value.to_string() }.map_err(|_| "Windows 音频设备 ID 编码无效。".into())
}

/// Direct PnP children can include an unplugged, disabled or superseded endpoint.
/// Query only those exact IDs; never follow a system-wide default or another device.
pub(super) fn active_candidate(ids: &[&str]) -> Result<Option<usize>, String> {
    if ids.is_empty() {
        return Ok(None);
    }
    let _apartment = Apartment::enter()?;
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .context("读取 ZH3 播放端点状态")?;
        unique_active(ids, |id| {
            let endpoint = match enumerator.GetDevice(&HSTRING::from(id)) {
                Ok(ep) => ep,
                Err(e) if matches!(e.code().0 as u32, 0x80070490 | 0x88890004) => return Ok(false),
                Err(e) => {
                    return Err(format!(
                        "无法核对 ZH3 播放端点状态（HRESULT 0x{:08X}）。",
                        e.code().0 as u32
                    ));
                }
            };
            Ok(endpoint.GetState().context("读取活动播放端点状态")? == DEVICE_STATE_ACTIVE)
        })
    }
}

fn unique_active(
    ids: &[&str],
    state: impl Fn(&str) -> Result<bool, String>,
) -> Result<Option<usize>, String> {
    let mut found = None;
    for (index, id) in ids.iter().enumerate() {
        if state(id)? && found.replace(index).is_some() {
            return Err("ZH3 当前有多个活动播放端点，无法唯一确定要读取的输出。".into());
        }
    }
    Ok(found)
}

pub(super) fn connected_adapter_ids(device: &Device) -> Result<Vec<String>, String> {
    let _apartment = Apartment::enter()?;
    let (endpoint, _store) = open(device)?;
    unsafe {
        let topology: IDeviceTopology = endpoint
            .Activate(CLSCTX_ALL, None)
            .context("读取 ZH3 端点连接图")?;
        let count = topology
            .GetConnectorCount()
            .context("读取 ZH3 端点连接器数")?;
        let mut adapters = Vec::new();
        for index in 0..count {
            let connector = topology.GetConnector(index).context("读取播放连接器")?;
            if connector
                .IsConnected()
                .context("核对播放连接器状态")?
                .as_bool()
            {
                let id = connector
                    .GetDeviceIdConnectedTo()
                    .context("读取播放连接器所连的适配器")?;
                adapters.push(task_string(id)?);
            }
        }
        Ok(adapters)
    }
}

fn open(device: &Device) -> Result<(IMMDevice, IPropertyStore), String> {
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .context("连接 Windows 音频设备枚举器")?;
        let ep = enumerator
            .GetDevice(&HSTRING::from(&device.endpoint_id))
            .map_err(|e| {
                let hr = e.code().0 as u32;
                if matches!(hr, 0x80070490 | 0x88890004) {
                    format!("AUDIO_UNAVAILABLE: ZH3 播放端点已离线（0x{hr:08X}）。")
                } else {
                    format!("打开已定位的 ZH3 播放端点失败（HRESULT 0x{hr:08X}）。")
                }
            })?;
        let actual = task_string(ep.GetId().context("核对 Core Audio 端点 ID")?)?;
        if !actual.eq_ignore_ascii_case(&device.endpoint_id) {
            return Err("Core Audio 的端点 ID 与 ZH3 的直接 KS 子设备不一致。".into());
        }
        let flow: IMMEndpoint = ep.cast().context("核对 ZH3 音频端点方向")?;
        if flow.GetDataFlow().context("读取 ZH3 音频端点方向")? != eRender {
            return Err("识别到的 Core Audio 端点并非播放端点。".into());
        }
        let store = ep
            .OpenPropertyStore(STGM_READ)
            .context("打开 ZH3 端点只读属性")?;
        let key = PROPERTYKEY {
            fmtid: DEVPKEY_Device_ContainerId.fmtid,
            pid: DEVPKEY_Device_ContainerId.pid,
        };
        let variant = store.GetValue(&key).context("取得播放端点的容器属性")?;
        if variant.vt() != VT_CLSID {
            return Err("Core Audio 端点没有有效的设备容器标识。".into());
        }
        let guid = PropVariantToGUID(&variant).context("解析播放端点的容器 ID")?;
        if !format!("{guid:?}").eq_ignore_ascii_case(&device.container) {
            return Err("Core Audio 播放端点与 ZH3 物理设备的容器不一致。".into());
        }
        if ep.GetState().context("读取 ZH3 播放端点状态")? != DEVICE_STATE_ACTIVE {
            return Err("AUDIO_UNAVAILABLE: ZH3 播放端点当前未处于活动状态。".into());
        }
        Ok((ep, store))
    }
}

pub(super) fn verify(device: &Device) -> Result<(), String> {
    let _apartment = Apartment::enter()?;
    open(device).map(|_| ())
}

fn period_properties_from_store(store: &IPropertyStore) -> Result<(Option<Vec<u8>>, bool), String> {
    let fmtid = windows::core::GUID::from_u128(0xe4870e26_3cc5_4cd2_ba46_ca0a9a70ed04);
    unsafe {
        let oem = store
            .GetValue(&PROPERTYKEY { fmtid, pid: 6 })
            .context("读取 ZH3 端点的 OEM 周期")?;
        let bytes = match oem.vt() {
            VT_EMPTY => None,
            VT_BLOB => {
                let blob = oem.Anonymous.Anonymous.Anonymous.blob;
                if blob.cbSize > 65536 || (blob.cbSize != 0 && blob.pBlobData.is_null()) {
                    return Err("端点 OEM 周期二进制属性格式异常。".into());
                }
                let data = if blob.cbSize == 0 {
                    vec![]
                } else {
                    slice::from_raw_parts(blob.pBlobData, blob.cbSize as usize).to_vec()
                };
                Some(data)
            }
            _ => return Err("端点 OEM 周期属性类型不是预期的二进制值。".into()),
        };
        let default = store
            .GetValue(&PROPERTYKEY { fmtid, pid: 7 })
            .context("读取 ZH3 端点的默认周期标记")?;
        Ok((bytes, default.vt() != VT_EMPTY))
    }
}

pub(super) fn period_properties(device: &Device) -> Result<(Option<Vec<u8>>, bool), String> {
    let _apartment = Apartment::enter()?;
    let (_ep, store) = open(device)?;
    period_properties_from_store(&store)
}

fn parse_wave(data: &[u8]) -> Result<Format, String> {
    if data.len() < 16 {
        return Err("Windows 返回的音频格式首部被截断。".into());
    }
    let word = |pos: usize| u16::from_le_bytes([data[pos], data[pos + 1]]);
    let tag = word(0);
    let bits = word(14);
    let channels = word(2);
    let rate = u32::from_le_bytes(data[4..8].try_into().unwrap());
    let ext = if data.len() >= 18 {
        word(16) as usize
    } else {
        0
    };
    if channels == 0
        || rate == 0
        || bits == 0
        || data.len() == 17
        || (data.len() >= 18 && 18 + ext > data.len())
    {
        return Err("Windows 返回的音频格式无效。".into());
    }
    let (valid_bits, encoding) = if tag == 0xfffe {
        if ext < 22 {
            return Err("扩展音频格式缺少有效位深或子格式。".into());
        }
        let sub = unsafe {
            data[24..40]
                .as_ptr()
                .cast::<windows::core::GUID>()
                .read_unaligned()
        };
        let pcm = windows::core::GUID::from_u128(0x00000001_0000_0010_8000_00aa00389b71);
        let float = windows::core::GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);
        let name = if sub == pcm {
            "PCM".into()
        } else if sub == float {
            "Float".into()
        } else {
            format!("{sub:?}")
        };
        (word(18), name)
    } else {
        let name = match tag {
            1 => "PCM".into(),
            3 => "Float".into(),
            _ => format!("0x{tag:04X}"),
        };
        (bits, name)
    };
    if valid_bits > bits {
        return Err("音频格式的有效位深超过了容器位深。".into());
    }
    let mut raw_hex = String::with_capacity(data.len() * 2);
    for b in data {
        write!(&mut raw_hex, "{b:02X}").unwrap();
    }
    Ok(Format {
        rate,
        channels,
        valid_bits,
        bits,
        encoding,
        raw_hex,
    })
}

impl TaskMem<WAVEFORMATEX> {
    fn format(&self) -> Result<Format, String> {
        if self.0.is_null() {
            return Err("Windows 未返回音频格式。".into());
        }
        // Both IAudioClient format methods return a CoTaskMem-allocated WAVEFORMATEX
        // with cbSize bytes of trailing format data.
        unsafe {
            let len = size_of::<WAVEFORMATEX>() + (*self.0).cbSize as usize;
            parse_wave(slice::from_raw_parts(self.0.cast(), len))
        }
    }
}

fn output_format(store: &IPropertyStore) -> Option<Format> {
    unsafe {
        let value = store.GetValue(&PKEY_AudioEngine_DeviceFormat).ok()?;
        if value.vt() != VT_BLOB {
            return None;
        }
        let blob = value.Anonymous.Anonymous.Anonymous.blob;
        if blob.pBlobData.is_null() || !(16..=65553).contains(&blob.cbSize) {
            return None;
        }
        parse_wave(slice::from_raw_parts(blob.pBlobData, blob.cbSize as usize)).ok()
    }
}

fn volume_percent(v: f32) -> Option<u32> {
    (0.0..=1.0).contains(&v).then(|| (v * 100.0).round() as u32)
}

pub(super) fn read(device: Device) -> Result<Snapshot, String> {
    let _apartment = Apartment::enter()?;
    let (ep, store) = open(&device)?;
    let engine = periods(&ep).map_err(|e| format!("AUDIO_UNAVAILABLE: {e}"))?;
    let output = output_format(&store);
    let endpoint_period_properties = period_properties_from_store(&store).ok();
    let volume_api = unsafe { ep.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None) }.ok();
    let volume = volume_api
        .as_ref()
        .and_then(|v| unsafe { v.GetMasterVolumeLevelScalar() }.ok())
        .and_then(volume_percent);
    let muted = volume_api
        .as_ref()
        .and_then(|v| unsafe { v.GetMute() }.ok())
        .map(|v| v.as_bool());
    if unsafe { ep.GetState() }.context("复核 ZH3 端点状态")? != DEVICE_STATE_ACTIVE {
        return Err("AUDIO_UNAVAILABLE: ZH3 播放端点在读取期间已离线。".into());
    }
    let (current, current_format) = match engine.running {
        Some((frames, format)) => (Some(frames), Some(format)),
        None => (None, None),
    };
    let current_rate = current_format.as_ref().map(|f| f.rate);
    Ok(Snapshot {
        device,
        mix: engine.mix,
        output,
        current,
        current_rate,
        current_format,
        default_frames: engine.default,
        step_frames: engine.step,
        min_frames: engine.min,
        max_frames: engine.max,
        volume,
        muted,
        endpoint_period_properties,
    })
}

struct EnginePeriods {
    mix: Format,
    default: u32,
    step: u32,
    min: u32,
    max: u32,
    running: Option<(u32, Format)>,
}

fn periods(ep: &IMMDevice) -> Result<EnginePeriods, String> {
    unsafe {
        let client: IAudioClient3 = ep
            .Activate(CLSCTX_ALL, None)
            .context("取得 ZH3 的 IAudioClient3")?;
        let props = AudioClientProperties {
            cbSize: size_of::<AudioClientProperties>() as u32,
            eCategory: AudioCategory_Other,
            Options: AUDCLNT_STREAMOPTIONS_NONE,
            ..Default::default()
        };
        // SetClientProperties configures only this uninitialized client.
        client
            .SetClientProperties(&props)
            .context("准备普通 Shared 查询")?;
        let format_mem = TaskMem(client.GetMixFormat().context("读取共享混音格式")?);
        let mix = format_mem.format()?;
        let (mut default, mut step, mut min, mut max) = (0, 0, 0, 0);
        client
            .GetSharedModeEnginePeriod(format_mem.0, &mut default, &mut step, &mut min, &mut max)
            .context("读取共享引擎周期范围")?;
        if step == 0 || min == 0 || !(min..=max).contains(&default) {
            return Err("共享引擎返回的周期范围无效。".into());
        }
        let mut frames = 0;
        let mut current_ptr = ptr::null_mut();
        let status = client.GetCurrentSharedModeEnginePeriod(&mut current_ptr, &mut frames);
        let memory = TaskMem(current_ptr);
        let running = if status.is_ok() && frames != 0 {
            memory.format().ok().map(|format| (frames, format))
        } else {
            None
        };
        Ok(EnginePeriods {
            mix,
            default,
            step,
            min,
            max,
            running,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn current_endpoint_survives_an_inactive_sibling_and_refuses_two_active_outputs() {
        let after_change = ["before-reenumeration", "current-output"];
        assert_eq!(
            unique_active(&after_change, |id| Ok(id == "current-output")).unwrap(),
            Some(1)
        );
        assert!(unique_active(&after_change, |_| Ok(true)).is_err());
        assert_eq!(unique_active(&after_change, |_| Ok(false)).unwrap(), None);
        assert!(unique_active(&after_change, |_| Err("状态不可读".into())).is_err());
    }

    #[test]
    fn shared_mix_preserves_the_windows_encoding() {
        let fixture: Value =
            serde_json::from_str(include_str!("../../tests/fixtures/shared-mix.json")).unwrap();
        let hex = fixture["MixFormat"]["RawHex"].as_str().unwrap();
        let bytes: Vec<_> = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect();
        let mix = parse_wave(&bytes).unwrap();
        assert_eq!(
            (mix.rate, mix.channels, mix.valid_bits, mix.bits),
            (44100, 2, 32, 32)
        );
        assert_eq!(
            (mix.encoding.as_str(), mix.raw_hex.as_str()),
            ("Float", hex)
        );
        let mut output = bytes;
        output[18..20].copy_from_slice(&24u16.to_le_bytes());
        output[24] = 1;
        let device = parse_wave(&output).unwrap();
        assert_eq!(
            (device.encoding.as_str(), device.valid_bits, device.bits),
            ("PCM", 24, 32)
        );
        assert!(parse_wave(&output[..39]).is_err());
    }

    #[test]
    fn zero_volume_is_a_valid_reading() {
        assert_eq!(volume_percent(0.0), Some(0));
        assert_eq!(volume_percent(0.517), Some(52));
        assert_eq!(volume_percent(f32::NAN), None);
    }
}
