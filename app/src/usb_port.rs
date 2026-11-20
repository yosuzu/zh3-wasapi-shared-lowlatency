//! Reconnect only the physical ZH3 attached to a verified downstream USB port.
//! A successful cycle request still requires fresh PnP and audio readback.

use crate::probe::{self, Device};
use serde::Serialize;
use std::mem::{offset_of, size_of};
use windows::Win32::{
    Devices::{DeviceAndDriverInstallation::*, Properties::*, Usb::*},
    Foundation::{CloseHandle, DEVPROPKEY, GENERIC_WRITE, HANDLE},
    Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    },
    System::IO::DeviceIoControl,
};
use windows::core::HSTRING;

type Result<T> = std::result::Result<T, String>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PortTarget {
    pub port: u32,
    pub usb: String,
    pub ks: String,
    pub container: String,
    pub driver: String,
    pub driver_key: String,
    pub hub_instance: String,
    pub hub_link: String,
    /// A replug into the same port can preserve the instance ID and driver key.
    pub usb_arrival: u64,
}

fn cm(code: CONFIGRET, label: &str) -> Result<()> {
    if code == CR_SUCCESS {
        Ok(())
    } else {
        Err(format!(
            "{label}失败（Configuration Manager 0x{:02X}）。",
            code.0
        ))
    }
}

fn property(node: u32, key: &DEVPROPKEY, ty: DEVPROPTYPE) -> Result<Vec<u8>> {
    let mut kind = DEVPROPTYPE::default();
    let mut size = 0u32;
    let rc = unsafe { CM_Get_DevNode_PropertyW(node, key, &mut kind, None, &mut size, 0) };
    if rc != CR_BUFFER_SMALL {
        cm(rc, "查询 ZH3 USB 端口属性大小")?;
    }
    if !(1..=65536).contains(&size) {
        return Err("ZH3 USB 端口属性长度无效。".into());
    }
    let mut buf = vec![0u8; size as usize];
    cm(
        unsafe {
            CM_Get_DevNode_PropertyW(node, key, &mut kind, Some(buf.as_mut_ptr()), &mut size, 0)
        },
        "读取 ZH3 USB 端口属性",
    )?;
    if kind != ty || size as usize > buf.len() {
        return Err("ZH3 USB 端口属性的类型或长度发生变化。".into());
    }
    buf.truncate(size as usize);
    Ok(buf)
}

fn terminated_string(bytes: &[u8]) -> Result<String> {
    if bytes.len() % 2 != 0 {
        return Err("USB 驱动键名被截断。".into());
    }
    let units: Vec<_> = bytes
        .chunks_exact(2)
        .map(|v| u16::from_le_bytes([v[0], v[1]]))
        .collect();
    let end = units
        .iter()
        .position(|&v| v == 0)
        .ok_or("USB 驱动键名缺少结束符。")?;
    if end == 0 || units[end..].iter().any(|&v| v != 0) {
        return Err("USB 驱动键名为空或包含多余数据。".into());
    }
    String::from_utf16(&units[..end]).map_err(|_| "USB 驱动键名不是有效的 UTF-16。".into())
}

fn hub_link(parent: &str) -> Result<String> {
    let id = HSTRING::from(parent);
    let mut length = 0u32;
    let flags = CM_GET_DEVICE_INTERFACE_LIST_PRESENT;
    cm(
        unsafe {
            CM_Get_Device_Interface_List_SizeW(&mut length, &GUID_DEVINTERFACE_USB_HUB, &id, flags)
        },
        "定位 ZH3 的父 USB Hub",
    )?;
    if !(1..=65536).contains(&length) {
        return Err("父 USB Hub 接口清单长度无效。".into());
    }
    let mut buf = vec![0u16; length as usize];
    cm(
        unsafe { CM_Get_Device_Interface_ListW(&GUID_DEVINTERFACE_USB_HUB, &id, &mut buf, flags) },
        "读取父 USB Hub 接口",
    )?;
    let mut links = buf.split(|&v| v == 0).take_while(|s| !s.is_empty());
    let link = links
        .next()
        .ok_or("ZH3 的直接父设备没有在场 USB Hub 接口。")?;
    if links.next().is_some() {
        return Err("ZH3 的直接父设备有多个 USB Hub 接口。".into());
    }
    String::from_utf16(link).map_err(|_| "父 USB Hub 接口名无效。".into())
}

fn attachment(d: &Device, confirmed_port: Option<u32>, access: u32) -> Result<(PortTarget, Hub)> {
    // This checks the single present physical VID/PID, both driver services and
    // versions, the direct KS child, and their common nonempty container.
    probe::verify_physical(d)?;
    let mut node = 0;
    cm(
        unsafe { CM_Locate_DevNodeW(&mut node, &HSTRING::from(&d.usb), CM_LOCATE_DEVNODE_NORMAL) },
        "定位 ZH3 USB 实例",
    )?;
    let mut parent = 0;
    cm(
        unsafe { CM_Get_Parent(&mut parent, node, 0) },
        "定位 ZH3 的直接父设备",
    )?;
    let mut id = [0u16; MAX_DEVICE_ID_LEN as usize];
    cm(
        unsafe { CM_Get_Device_IDW(parent, &mut id, 0) },
        "读取父 USB Hub 实例 ID",
    )?;
    let end = id
        .iter()
        .position(|&v| v == 0)
        .ok_or("父 Hub 实例 ID 被截断。")?;
    let hub_instance = String::from_utf16(&id[..end]).map_err(|_| "父 Hub 实例 ID 无效。")?;
    let hub_link = hub_link(&hub_instance)?;
    let usb_arrival = u64::from_le_bytes(
        property(node, &DEVPKEY_Device_LastArrivalDate, DEVPROP_TYPE_FILETIME)?
            .try_into()
            .map_err(|_| "USB 设备到达时间格式错误。")?,
    );
    if usb_arrival == 0 {
        return Err("缺少 ZH3 本次 USB 连接的到达时间。".into());
    }
    let driver_key = terminated_string(&property(
        node,
        &DEVPKEY_Device_Driver,
        DEVPROP_TYPE_STRING,
    )?)?;
    let hub = Hub::open(&hub_link, access)?;
    let port = match confirmed_port {
        Some(port) => {
            if !hub.matches(port, &driver_key)? {
                return Err("已确认的 USB 端口现在没有连接同一台 ZH3。".into());
            }
            port
        }
        None => {
            // DEVPKEY_Device_Address is bus-specific. It is only a fast hint,
            // never evidence that the port belongs to this physical instance.
            let hint = property(node, &DEVPKEY_Device_Address, DEVPROP_TYPE_UINT32)
                .ok()
                .and_then(|bytes| bytes.try_into().ok())
                .map(u32::from_le_bytes);
            find_port(
                hint,
                || hub.highest_port(),
                |port| hub.matches(port, &driver_key),
            )?
        }
    };
    Ok((
        PortTarget {
            port,
            usb: d.usb.to_ascii_uppercase(),
            ks: d.ks.to_ascii_uppercase(),
            container: d.container.to_ascii_lowercase(),
            driver: d.driver.clone(),
            driver_key: driver_key.to_ascii_lowercase(),
            hub_instance: hub_instance.to_ascii_uppercase(),
            hub_link: hub_link.to_ascii_lowercase(),
            usb_arrival,
        },
        hub,
    ))
}

fn find_port(
    hint: Option<u32>,
    highest: impl FnOnce() -> Result<u32>,
    mut matches: impl FnMut(u32) -> Result<bool>,
) -> Result<u32> {
    if let Some(port @ 1..=255) = hint {
        if matches(port).unwrap_or(false) {
            return Ok(port);
        }
    }
    let last = highest()?;
    if !(1..=255).contains(&last) {
        return Err("父 USB Hub 未返回有效的下游端口数。".into());
    }
    let mut found = None;
    for port in 1..=last {
        if matches(port)? && found.replace(port).is_some() {
            return Err("同一父 USB Hub 上有多个端口符合目标，无法安全重新连接。".into());
        }
    }
    found.ok_or_else(|| "父 USB Hub 上没有找到与 ZH3 的驱动键和设备描述均符合的端口。".into())
}

struct Hub(HANDLE);

impl Drop for Hub {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

fn win_error(label: &str, err: windows::core::Error) -> String {
    format!(
        "{label}失败（HRESULT 0x{:08X}，{}）。",
        err.code().0 as u32,
        err.message()
    )
}

impl Hub {
    fn open(path: &str, desired_access: u32) -> Result<Self> {
        unsafe {
            CreateFileW(
                &HSTRING::from(path),
                desired_access,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                None,
            )
            .map(Self)
            .map_err(|e| win_error("打开 ZH3 的父 USB Hub 接口", e))
        }
    }

    // These two METHOD_BUFFERED queries both take ConnectionIndex as the
    // first field of the full, variably sized structure. USBView uses the
    // same input/output buffer. No write operation shares this entry point.
    fn query(&self, port: u32, what: ReadQuery) -> Result<Vec<u8>> {
        let (code, capacity) = match what {
            ReadQuery::Connection => (
                IOCTL_USB_GET_NODE_CONNECTION_INFORMATION_EX,
                size_of::<USB_NODE_CONNECTION_INFORMATION_EX>() + 255 * size_of::<USB_PIPE_INFO>(),
            ),
            ReadQuery::DriverKey => (IOCTL_USB_GET_NODE_CONNECTION_DRIVERKEY_NAME, 65536),
        };
        let mut data = vec![0u8; capacity];
        data[..4].copy_from_slice(&port.to_le_bytes());
        let mut returned = 0u32;
        let pointer = data.as_mut_ptr().cast();
        unsafe {
            DeviceIoControl(
                self.0,
                code,
                Some(pointer),
                capacity as u32,
                Some(pointer),
                capacity as u32,
                Some(&mut returned),
                None,
            )
            .map_err(|e| win_error("读取 ZH3 所在 USB 端口", e))?;
        }
        if returned as usize > capacity {
            return Err("USB Hub 返回的端口信息超过接收缓冲区。".into());
        }
        data.truncate(returned as usize);
        Ok(data)
    }

    fn highest_port(&self) -> Result<u32> {
        let mut info = USB_HUB_INFORMATION_EX::default();
        let mut returned = 0;
        let buffer = (&raw mut info).cast();
        let size = size_of::<USB_HUB_INFORMATION_EX>() as u32;
        unsafe {
            DeviceIoControl(
                self.0,
                IOCTL_USB_GET_HUB_INFORMATION_EX,
                Some(buffer),
                size,
                Some(buffer),
                size,
                Some(&mut returned),
                None,
            )
            .map_err(|e| win_error("读取直接父 Hub 的端口数", e))?;
        }
        if (returned as usize)
            < offset_of!(USB_HUB_INFORMATION_EX, HighestPortNumber) + size_of::<u16>()
        {
            return Err("USB Hub 返回了不完整的端口数量信息。".into());
        }
        Ok(u32::from(info.HighestPortNumber))
    }

    fn matches(&self, port: u32, driver_key: &str) -> Result<bool> {
        if !is_zh3_connection(&self.query(port, ReadQuery::Connection)?, port)? {
            return Ok(false);
        }
        let key = self.query(port, ReadQuery::DriverKey)?;
        Ok(port_driver_key(&key, port)?.eq_ignore_ascii_case(driver_key))
    }

    fn cycle(&self, port: u32) -> Result<()> {
        let request = USB_CYCLE_PORT_PARAMS {
            ConnectionIndex: port,
            StatusReturned: 0,
        };
        let mut reply = USB_CYCLE_PORT_PARAMS::default();
        let mut returned = 0;
        let size = size_of::<USB_CYCLE_PORT_PARAMS>() as u32;
        // Some hub stacks return the USBD status in the same-sized structure.
        // The IOCTL contract also permits no output and reports status through
        // DeviceIoControl. Send exactly once for either contract.
        unsafe {
            DeviceIoControl(
                self.0,
                IOCTL_USB_HUB_CYCLE_PORT,
                Some((&raw const request).cast()),
                size,
                Some((&raw mut reply).cast()),
                size,
                Some(&mut returned),
                None,
            )
            .map_err(|e| win_error("请求重新连接 ZH3 USB 端口", e))?;
        }
        cycle_response(port, returned, reply)
    }
}

enum ReadQuery {
    Connection,
    DriverKey,
}

fn port_driver_key(data: &[u8], port: u32) -> Result<String> {
    let start = offset_of!(USB_NODE_CONNECTION_DRIVERKEY_NAME, DriverKeyName);
    if data.len() < start || data[..4] != port.to_le_bytes() {
        return Err("USB Hub 返回了其他端口或不完整的驱动键。".into());
    }
    let actual = u32::from_le_bytes(data[4..8].try_into().unwrap()) as usize;
    if !(start..=data.len()).contains(&actual) {
        return Err("USB Hub 返回的端口驱动键长度无效。".into());
    }
    terminated_string(&data[start..actual])
}

fn is_zh3_connection(data: &[u8], port: u32) -> Result<bool> {
    type Info = USB_NODE_CONNECTION_INFORMATION_EX;
    if data.len() < offset_of!(Info, PipeList) {
        return Err("USB Hub 返回了不完整的设备描述。".into());
    }
    let descriptor = offset_of!(Info, DeviceDescriptor);
    let word = |offset| u16::from_le_bytes([data[offset], data[offset + 1]]);
    let status = offset_of!(Info, ConnectionStatus);
    let state = i32::from_le_bytes(data[status..status + 4].try_into().unwrap());
    if state != DeviceConnected.0 {
        // USBHUB3 can return a zero index and no descriptor for an empty port.
        return Ok(false);
    }
    if data[..4] != port.to_le_bytes() {
        return Err("USB Hub 返回的已连接设备属于其他端口。".into());
    }
    if data[descriptor + offset_of!(USB_DEVICE_DESCRIPTOR, bLength)] as usize
        != size_of::<USB_DEVICE_DESCRIPTOR>()
        || data[descriptor + offset_of!(USB_DEVICE_DESCRIPTOR, bDescriptorType)] as u32
            != USB_DEVICE_DESCRIPTOR_TYPE
        || data[offset_of!(Info, DeviceIsHub)] != 0
        || data[descriptor + offset_of!(USB_DEVICE_DESCRIPTOR, bDeviceClass)] as u32
            == USB_DEVICE_CLASS_HUB
        || word(descriptor + offset_of!(USB_DEVICE_DESCRIPTOR, idVendor)) != 0x152a
        || word(descriptor + offset_of!(USB_DEVICE_DESCRIPTOR, idProduct)) != 0x889b
    {
        return Ok(false);
    }
    Ok(true)
}

fn cycle_response(port: u32, returned: u32, reply: USB_CYCLE_PORT_PARAMS) -> Result<()> {
    if returned == 0 {
        return Ok(());
    }
    if returned != size_of::<USB_CYCLE_PORT_PARAMS>() as u32 || reply.ConnectionIndex != port {
        return Err("USB 端口重连已请求，Hub 返回了无法核对的回执，请等待端点读回。".into());
    }
    let status = reply.StatusReturned;
    if (status as i32) < 0 {
        return Err(format!(
            "USB Hub 返回 USBD 状态 0x{status:08X}，请等待目标端点读回。"
        ));
    }
    Ok(())
}

fn require_same(expected: &PortTarget, current: &PortTarget) -> Result<()> {
    if current != expected {
        return Err("确认后 ZH3、USB 端口或本次连接已发生变化，请重新检测。".into());
    }
    Ok(())
}

/// Produces the native confirmation evidence without requesting hub write access.
pub fn target(d: &Device) -> Result<PortTarget> {
    let (target, _) = attachment(d, None, 0)?;
    Ok(target)
}

/// The caller owns the user's confirmation and journal. No automatic retry.
pub fn reconnect(d: &Device, expected: &PortTarget) -> Result<()> {
    let (observed, _) = attachment(d, Some(expected.port), 0)?;
    require_same(expected, &observed)?;
    let (current, hub) = attachment(d, Some(expected.port), GENERIC_WRITE.0)?;
    require_same(expected, &current)?;
    // Keep the same handle open and check the confirmed connection again
    // immediately before dispatch. Never search for a replacement here.
    if !hub.matches(current.port, &current.driver_key)? {
        return Err("已确认的 ZH3 USB 端口在执行前发生变化。".into());
    }
    hub.cycle(current.port)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Independent packed USB header layout: ConnectionIndex, 18-byte device
    // descriptor, configuration, speed, IsHub, address, pipe count and state.
    fn zh3_port() -> Vec<u8> {
        let mut data = vec![0u8; 35];
        data[..4].copy_from_slice(&18u32.to_le_bytes());
        data[4..6].copy_from_slice(&[18, 1]);
        data[12..16].copy_from_slice(&[0x2a, 0x15, 0x9b, 0x88]);
        data[31..35].copy_from_slice(&1u32.to_le_bytes());
        data
    }

    #[test]
    fn connection_must_be_a_connected_nonhub_zh3_on_the_confirmed_port() {
        let sample = zh3_port();
        assert!(is_zh3_connection(&sample, 18).unwrap());
        assert!(is_zh3_connection(&sample, 17).is_err());
        assert!(is_zh3_connection(&sample[..34], 18).is_err());
        assert!(!is_zh3_connection(&[0u8; 35], 3).unwrap());
        for (offset, replacement) in [(4, 8), (5, 2), (8, 9), (12, 0), (14, 0), (24, 1), (31, 0)] {
            let mut other = sample.clone();
            other[offset] = replacement;
            assert!(
                !is_zh3_connection(&other, 18).unwrap(),
                "accepted modified byte {offset}"
            );
        }
    }

    #[test]
    fn driver_key_requires_the_matching_port_and_a_complete_terminated_name() {
        let mut data = Vec::from(7u32.to_le_bytes());
        data.extend(12u32.to_le_bytes());
        data.extend([b'K', 0, 0, 0]);
        assert_eq!(port_driver_key(&data, 7).unwrap(), "K");
        assert!(port_driver_key(&data, 18).is_err());
        assert!(port_driver_key(&data[..11], 7).is_err());
        data[10] = b'J';
        assert!(port_driver_key(&data, 7).is_err());
    }

    #[test]
    fn replaced_device_port_hub_or_usb_arrival_invalidates_confirmation() {
        let original = PortTarget {
            port: 18,
            usb: "usb-a".into(),
            ks: "ks-a".into(),
            container: "container-a".into(),
            driver: "driver-a".into(),
            driver_key: "key-a".into(),
            hub_instance: "hub-a".into(),
            hub_link: "link-a".into(),
            usb_arrival: 100,
        };
        assert!(require_same(&original, &original).is_ok());
        for other in [
            PortTarget {
                port: 19,
                ..original.clone()
            },
            PortTarget {
                usb: "usb-b".into(),
                ..original.clone()
            },
            PortTarget {
                ks: "ks-b".into(),
                ..original.clone()
            },
            PortTarget {
                container: "container-b".into(),
                ..original.clone()
            },
            PortTarget {
                driver_key: "key-b".into(),
                ..original.clone()
            },
            PortTarget {
                hub_instance: "hub-b".into(),
                ..original.clone()
            },
            PortTarget {
                hub_link: "link-b".into(),
                ..original.clone()
            },
            PortTarget {
                usb_arrival: 101,
                ..original.clone()
            },
        ] {
            assert!(require_same(&original, &other).is_err());
        }
    }

    #[test]
    fn discovery_uses_each_hosts_hub_connection_data_instead_of_a_fixed_address() {
        let root = PortTarget {
            port: 18,
            usb: "USB-A".into(),
            ks: "KS-A".into(),
            container: "CONTAINER-A".into(),
            driver: "6.16".into(),
            driver_key: "CLASS/0000".into(),
            hub_instance: "ROOT-HUB-A".into(),
            hub_link: "LINK-A".into(),
            usb_arrival: 173,
        };
        let external = PortTarget {
            port: 3,
            usb: "USB-B".into(),
            ks: "KS-B".into(),
            container: "CONTAINER-B".into(),
            driver_key: "CLASS/0007".into(),
            hub_instance: "EXTERNAL-HUB-B".into(),
            hub_link: "LINK-B".into(),
            usb_arrival: 896,
            ..root.clone()
        };
        for (local, address_hint, highest_port) in [
            (&root, Some(18), 21),
            (&external, Some(7), 7),
            (&external, None, 7),
        ] {
            let discovered = find_port(
                address_hint,
                || Ok(highest_port),
                |p| {
                    let mut descriptor = zh3_port();
                    descriptor[..4].copy_from_slice(&p.to_le_bytes());
                    let seen_key = if p == local.port {
                        local.driver_key.as_str()
                    } else {
                        "another-device-key"
                    };
                    Ok(is_zh3_connection(&descriptor, p)? && seen_key == local.driver_key)
                },
            )
            .unwrap();
            assert_eq!(discovered, local.port);
            assert!(require_same(local, local).is_ok());
        }
        assert!(require_same(&root, &external).is_err());
        assert!(find_port(None, || Ok(4), |p| Ok(p == 2 || p == 3)).is_err());
        assert!(find_port(Some(44), || Ok(7), |_| Ok(false)).is_err());
    }

    #[test]
    fn cycle_reply_uses_the_usbd_result_only_when_the_driver_returned_it() {
        let reply = |port, status| USB_CYCLE_PORT_PARAMS {
            ConnectionIndex: port,
            StatusReturned: status,
        };
        assert!(cycle_response(18, 0, reply(0, 0)).is_ok());
        assert!(cycle_response(18, 8, reply(18, 0)).is_ok());
        assert!(cycle_response(18, 8, reply(18, 0xc0000001)).is_err());
        assert!(cycle_response(18, 4, reply(18, 0)).is_err());
        assert!(cycle_response(18, 8, reply(17, 0)).is_err());
    }

    #[test]
    #[ignore = "read-only USB hub preflight; requires a connected ZH3 and hub query access"]
    fn attached_target_matches_the_direct_hub_port_without_reconnecting() {
        let device = probe::identify().unwrap();
        let port = target(&device).unwrap();
        assert!(port.usb.eq_ignore_ascii_case(&device.usb));
        assert!(port.ks.eq_ignore_ascii_case(&device.ks));
        assert!(port.container.eq_ignore_ascii_case(&device.container));
        assert_ne!(port.usb_arrival, 0);
        let hub = Hub::open(&port.hub_link, 0).unwrap();
        let discovered = find_port(
            None,
            || hub.highest_port(),
            |p| hub.matches(p, &port.driver_key),
        )
        .unwrap();
        assert_eq!(discovered, port.port);
        println!(
            "ZH3 port preflight: {}",
            serde_json::to_string(&port).unwrap()
        );
    }
}
