// CM supplies both the complete historical list and the currently present links.
// SetupAPI addresses the exact link independently before changing any property.
use super::{Result, check, wide};
use serde::{Deserialize, Serialize};
use std::{
    ffi::c_void,
    mem::size_of,
    ptr::{null, null_mut},
};
type H = *mut c_void;
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct Guid {
    a: u32,
    b: u16,
    c: u16,
    d: [u8; 8],
}
impl Guid {
    const fn new(a: u32, b: u16, c: u16, d: [u8; 8]) -> Self {
        Self { a, b, c, d }
    }
}
const AUDIO: Guid = Guid::new(
    0x6994ad04,
    0x93ef,
    0x11d0,
    [0xa3, 0xcc, 0, 0xa0, 0xc9, 0x22, 0x31, 0x96],
);
const RENDER: Guid = Guid::new(
    0x65e8773e,
    0x8f56,
    0x11d0,
    [0xa3, 0xb9, 0, 0xa0, 0xc9, 0x22, 0x31, 0x96],
);
#[repr(C)]
struct PropKey {
    guid: Guid,
    pid: u32,
}
const PACKET: PropKey = PropKey {
    guid: Guid::new(
        0x9404f781,
        0x7191,
        0x409b,
        [0x8b, 0x0b, 0x80, 0xbf, 0x6e, 0xc2, 0x29, 0xae],
    ),
    pid: 2,
};
const REF: PropKey = PropKey {
    guid: Guid::new(
        0x026e516e,
        0xb814,
        0x414b,
        [0x83, 0xcd, 0x85, 0x6d, 0x6f, 0xef, 0x48, 0x22],
    ),
    pid: 5,
};
const LEGACY_PACKET: PropKey = PropKey {
    guid: Guid::new(
        0x13e004d6,
        0xb066,
        0x43bd,
        [0x91, 0x3b, 0xa4, 0x15, 0xcd, 0x13, 0xda, 0x87],
    ),
    pid: 2,
};
#[repr(C)]
struct Info {
    size: u32,
    guid: Guid,
    flags_or_node: u32,
    reserved: usize,
}
impl Info {
    fn blank() -> Self {
        Self {
            size: size_of::<Self>() as u32,
            guid: AUDIO,
            flags_or_node: 0,
            reserved: 0,
        }
    }
}
#[link(name = "Setupapi")]
unsafe extern "system" {
    fn SetupDiCreateDeviceInfoList(class: *const Guid, parent: H) -> H;
    fn SetupDiDestroyDeviceInfoList(set: H) -> i32;
    fn SetupDiOpenDeviceInfoW(
        set: H,
        id: *const u16,
        parent: H,
        flags: u32,
        device: *mut Info,
    ) -> i32;
    fn SetupDiGetDeviceInstanceIdW(
        set: H,
        device: *const Info,
        buf: *mut u16,
        count: u32,
        needed: *mut u32,
    ) -> i32;
    fn SetupDiCreateDeviceInterfaceW(
        set: H,
        device: *const Info,
        class: *const Guid,
        reference: *const u16,
        flags: u32,
        iface: *mut Info,
    ) -> i32;
    fn SetupDiOpenDeviceInterfaceW(set: H, path: *const u16, flags: u32, iface: *mut Info) -> i32;
    fn SetupDiGetDeviceInterfacePropertyW(
        set: H,
        iface: *const Info,
        key: *const PropKey,
        kind: *mut u32,
        data: *mut u8,
        cap: u32,
        size: *mut u32,
        flags: u32,
    ) -> i32;
    fn SetupDiSetDeviceInterfacePropertyW(
        set: H,
        iface: *const Info,
        key: *const PropKey,
        kind: u32,
        data: *const u8,
        size: u32,
        flags: u32,
    ) -> i32;
    fn SetupDiRemoveDeviceInterface(set: H, iface: *const Info) -> i32;
}
#[link(name = "Cfgmgr32")]
unsafe extern "system" {
    fn CM_Get_Device_Interface_List_SizeW(
        size: *mut u32,
        class: *const Guid,
        dev: *const u16,
        flags: u32,
    ) -> u32;
    fn CM_Get_Device_Interface_ListW(
        class: *const Guid,
        dev: *const u16,
        buf: *mut u16,
        size: u32,
        flags: u32,
    ) -> u32;
    fn CM_Get_Device_Interface_PropertyW(
        path: *const u16,
        key: *const PropKey,
        kind: *mut u32,
        data: *mut u8,
        size: *mut u32,
        flags: u32,
    ) -> u32;
}
fn cm(op: &str, rc: u32) -> Result<()> {
    if rc == 0 {
        Ok(())
    } else {
        Err(format!("{op}：CONFIGRET 0x{rc:X}"))
    }
}
fn list(ks: &str, guid: &Guid, flags: u32) -> Result<Vec<String>> {
    let id = wide(ks);
    for _ in 0..3 {
        let mut chars = 0;
        cm("读取 KS 接口数量", unsafe {
            CM_Get_Device_Interface_List_SizeW(&mut chars, guid, id.as_ptr(), flags)
        })?;
        if chars == 0 || chars > 1_048_576 {
            return Err("设备接口列表长度异常".into());
        }
        let mut buf = vec![0u16; chars as usize];
        let rc = unsafe {
            CM_Get_Device_Interface_ListW(guid, id.as_ptr(), buf.as_mut_ptr(), chars, flags)
        };
        if rc == 0x1a {
            continue;
        }
        cm("读取 KS 接口列表", rc)?;
        return buf
            .split(|c| *c == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf16(s).map_err(|e| e.to_string()))
            .collect();
    }
    Err("设备枚举中，请重试".into())
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Classes {
    pub audio: Vec<String>,
    pub render: Vec<String>,
    pub present_audio: Vec<String>,
    pub present_render: Vec<String>,
}
impl Classes {
    pub fn load(ks: &str) -> Result<Self> {
        Ok(Self {
            audio: list(ks, &AUDIO, 1)?,
            render: list(ks, &RENDER, 1)?,
            present_audio: list(ks, &AUDIO, 0)?,
            present_render: list(ks, &RENDER, 0)?,
        })
    }
    pub fn find<'a>(links: &'a [String], r: &str) -> Result<Option<&'a str>> {
        let mut found = links.iter().filter(|p| {
            p.rsplit('\\')
                .next()
                .is_some_and(|n| n.eq_ignore_ascii_case(r))
        });
        let one = found.next().map(String::as_str);
        if found.next().is_some() {
            return Err("同名接口不唯一".into());
        }
        Ok(one)
    }
    pub fn vacant(&self, r: &str) -> Result<bool> {
        for set in [
            &self.audio,
            &self.render,
            &self.present_audio,
            &self.present_render,
        ] {
            if Self::find(set, r)?.is_some() {
                return Ok(false);
            }
        }
        Ok(true)
    }
    pub fn active(&self, r: &str) -> Result<bool> {
        Ok(Self::find(&self.present_audio, r)?.is_some()
            && Self::find(&self.present_render, r)?.is_some())
    }
}
struct Set(H);
impl Drop for Set {
    fn drop(&mut self) {
        unsafe {
            SetupDiDestroyDeviceInfoList(self.0);
        }
    }
}
impl Set {
    fn new() -> Result<Self> {
        let h = unsafe { SetupDiCreateDeviceInfoList(null(), null_mut()) };
        check(
            !h.is_null() && h as isize != -1,
            "SetupDiCreateDeviceInfoList",
        )?;
        Ok(Self(h))
    }
}
pub fn create(ks: &str, reference: &str) -> Result<()> {
    let set = Set::new()?;
    let mut dev = Info::blank();
    check(
        unsafe { SetupDiOpenDeviceInfoW(set.0, wide(ks).as_ptr(), null_mut(), 0, &mut dev) } != 0,
        "SetupDiOpenDeviceInfoW",
    )?;
    let mut id = [0u16; 512];
    check(
        unsafe {
            SetupDiGetDeviceInstanceIdW(set.0, &dev, id.as_mut_ptr(), id.len() as u32, null_mut())
        } != 0,
        "SetupDiGetDeviceInstanceIdW",
    )?;
    let actual =
        String::from_utf16_lossy(&id[..id.iter().position(|c| *c == 0).unwrap_or(id.len())]);
    if !actual.eq_ignore_ascii_case(ks) {
        return Err("KS 实例发生变化".into());
    }
    // Passing NULL for the optional SP_DEVICE_INTERFACE_DATA avoids a historical
    // ERROR_INVALID_USER_BUFFER despite the inactive registration being created.
    check(
        unsafe {
            SetupDiCreateDeviceInterfaceW(
                set.0,
                &dev,
                &AUDIO,
                wide(reference).as_ptr(),
                0,
                null_mut(),
            )
        } != 0,
        "SetupDiCreateDeviceInterfaceW",
    )
}
#[derive(Debug, PartialEq, Eq)]
struct Prop(u32, Vec<u8>);
pub struct Interface {
    set: Set,
    info: Info,
    path: Vec<u16>,
}
impl Interface {
    pub fn open(ks: &str, reference: &str, path: &str) -> Result<Self> {
        Self::open_class(ks, reference, path, "6994ad04-93ef-11d0-a3cc-00a0c9223196")
    }
    pub fn render(ks: &str, reference: &str, path: &str) -> Result<Self> {
        Self::open_class(ks, reference, path, "65e8773e-8f56-11d0-a3b9-00a0c9223196")
    }
    fn open_class(ks: &str, reference: &str, path: &str, class: &str) -> Result<Self> {
        let expected = format!(r"\\?\{}#{{{class}}}\{}", ks.replace('\\', "#"), reference);
        if !path.eq_ignore_ascii_case(&expected) {
            return Err("AUDIO 链接与 KS、类别或引用不符".into());
        }
        let path = wide(path);
        let set = Set::new()?;
        let mut info = Info::blank();
        check(
            unsafe { SetupDiOpenDeviceInterfaceW(set.0, path.as_ptr(), 0, &mut info) } != 0,
            "SetupDiOpenDeviceInterfaceW",
        )?;
        if info.flags_or_node & 4 != 0 {
            return Err("接口已标记移除".into());
        }
        let x = Self { set, info, path };
        let Some(Prop(18, b)) = x.get(&REF, false)? else {
            return Err("接口引用缺失".into());
        };
        let raw = super::registry::Raw { kind: 1, bytes: b };
        if !raw.string()?.eq_ignore_ascii_case(reference) {
            return Err("接口引用发生变化".into());
        }
        Ok(x)
    }
    fn get(&self, key: &PropKey, use_cm: bool) -> Result<Option<Prop>> {
        let mut size = 0;
        let mut kind = 0;
        let read = |p: *mut u8, cap: u32, size: &mut u32, kind: &mut u32| -> u32 {
            unsafe {
                if use_cm {
                    *size = cap;
                    CM_Get_Device_Interface_PropertyW(self.path.as_ptr(), key, kind, p, size, 0)
                } else if SetupDiGetDeviceInterfacePropertyW(
                    self.set.0, &self.info, key, kind, p, cap, size, 0,
                ) != 0
                {
                    0
                } else {
                    std::io::Error::last_os_error().raw_os_error().unwrap_or(-1) as u32
                }
            }
        };
        let e = read(null_mut(), 0, &mut size, &mut kind);
        let (missing, small) = if use_cm { (0x25, 0x1a) } else { (1168, 122) };
        if e == missing {
            return Ok(None);
        }
        if (e != 0 && e != small) || size > 1_048_576 {
            return Err(format!("读取接口属性长度失败，代码 {e}"));
        }
        let cap = size;
        let mut buf = vec![0; cap as usize];
        let e = read(buf.as_mut_ptr(), cap, &mut size, &mut kind);
        if e != 0 || size > cap {
            return Err(format!("读取接口属性失败，代码 {e}"));
        }
        buf.truncate(size as usize);
        Ok(Some(Prop(kind, buf)))
    }
    pub fn packet(&self) -> Result<Option<Vec<u8>>> {
        let a = self.get(&PACKET, false)?;
        let b = self.get(&PACKET, true)?;
        if a != b {
            return Err("SetupAPI 与 Configuration Manager 包长读回不一致".into());
        }
        match a {
            Some(Prop(0x1003, bytes)) => Ok(Some(bytes)),
            None => Ok(None),
            _ => Err("包长属性类型发生变化".into()),
        }
    }
    pub fn legacy_packet_absent(&self) -> Result<bool> {
        let a = self.get(&LEGACY_PACKET, false)?;
        super::need(a == self.get(&LEGACY_PACKET, true)?, "旧版包长双读不一致")?;
        Ok(a.is_none())
    }
    pub fn set_packet(&self, bytes: Option<&[u8]>) -> Result<()> {
        let (kind, p, size) = bytes
            .map(|d| (0x1003, d.as_ptr(), d.len() as u32))
            .unwrap_or((0, null(), 0));
        check(
            unsafe {
                SetupDiSetDeviceInterfacePropertyW(
                    self.set.0, &self.info, &PACKET, kind, p, size, 0,
                )
            } != 0,
            "SetupDiSetDeviceInterfacePropertyW",
        )?;
        if self.packet()?.as_deref() != bytes {
            return Err("包长写后双读校验失败".into());
        }
        Ok(())
    }
    pub fn remove(&self) -> Result<()> {
        if self.info.flags_or_node & 1 != 0 || self.packet()?.is_some() {
            return Err("保留仍活动或含有包长的接口".into());
        }
        check(
            unsafe { SetupDiRemoveDeviceInterface(self.set.0, &self.info) } != 0,
            "SetupDiRemoveDeviceInterface",
        )
    }
}
#[link(name = "Cfgmgr32")]
unsafe extern "system" {
    fn CM_Open_Device_Interface_KeyW(
        path: *const u16,
        access: u32,
        profile: u32,
        out: *mut H,
        flags: u32,
    ) -> u32;
}
pub fn oem(path: &str) -> Result<(Option<super::registry::Raw>, Option<super::registry::Raw>)> {
    let mut h = null_mut();
    let rc = unsafe { CM_Open_Device_Interface_KeyW(wide(path).as_ptr(), 1, 1, &mut h, 0) };
    if rc == 0x2e {
        return Ok((None, None));
    } // CR_NO_SUCH_REGISTRY_KEY
    cm("打开接口的 EP 配置", rc)?;
    super::registry::interface_oem(h)
}

#[link(name = "Cfgmgr32")]
unsafe extern "system" {
    fn CM_Locate_DevNodeW(node: *mut u32, instance: *const u16, flags: u32) -> u32;
}
pub fn physical_present(usb: &str) -> Result<bool> {
    let mut node = 0;
    let rc = unsafe { CM_Locate_DevNodeW(&mut node, wide(usb).as_ptr(), 0) };
    if rc == 13 {
        return Ok(false);
    }
    cm("确认物理 USB 在场", rc)?;
    Ok(true)
}
// Keep unmanaged interface properties and driver-owned registry data if either
// differs from the inactive registration we originally observed.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Metadata {
    properties: std::collections::BTreeMap<String, (u32, Vec<u8>)>,
    registry: std::collections::BTreeMap<String, super::registry::Raw>,
}
impl Metadata {
    pub fn clean_for_removal(&self, initial: &Self, factory: &Self) -> bool {
        let mut observed = self.clone();
        // The retail driver populates these specific defaults at first activation.
        // Only additions equal to the type and bytes on this device's factory
        // interface are attributable to the driver during cleanup.
        observed.registry.retain(|key, v| {
            let approved = match key.split_once('\0') {
                Some(("", n)) => {
                    n.eq_ignore_ascii_case("CLSID") || n.eq_ignore_ascii_case("FriendlyName")
                }
                Some((p, n)) if p.eq_ignore_ascii_case(r"\EP\0") => {
                    n.eq_ignore_ascii_case("{1da5d803-d492-4edd-8c23-e0c0ffee7f0e},2")
                        || n.eq_ignore_ascii_case("{1da5d803-d492-4edd-8c23-e0c0ffee7f0e},7")
                }
                _ => false,
            };
            !approved || initial.registry.contains_key(key) || factory.registry.get(key) != Some(v)
        });
        let name = "026e516eb814414b83cd856d6fef4822/2";
        if !initial.properties.contains_key(name)
            && factory.properties.get(name) == observed.properties.get(name)
        {
            observed.properties.remove(name);
        }
        observed == *initial
    }
}
#[link(name = "Setupapi")]
unsafe extern "system" {
    fn SetupDiGetDeviceInterfacePropertyKeys(
        set: H,
        iface: *const Info,
        keys: *mut PropKey,
        count: u32,
        needed: *mut u32,
        flags: u32,
    ) -> i32;
}
impl Interface {
    pub fn metadata(&self) -> Result<Metadata> {
        let mut count = 0;
        let rc = unsafe {
            SetupDiGetDeviceInterfacePropertyKeys(
                self.set.0,
                &self.info,
                null_mut(),
                0,
                &mut count,
                0,
            )
        };
        if rc == 0 && std::io::Error::last_os_error().raw_os_error() != Some(122) {
            check(false, "读取接口属性列表")?
        }
        if count > 512 {
            return Err("接口属性过多，保留接口".into());
        }
        let mut keys = Vec::with_capacity(count as usize);
        let mut written = 0;
        if count > 0 {
            check(
                unsafe {
                    SetupDiGetDeviceInterfacePropertyKeys(
                        self.set.0,
                        &self.info,
                        keys.as_mut_ptr(),
                        count,
                        &mut written,
                        0,
                    )
                } != 0,
                "列举接口属性",
            )?;
            if written > count {
                return Err("接口属性在读取中变化".into());
            }
            unsafe {
                keys.set_len(written as usize);
            }
        }
        let mut properties = std::collections::BTreeMap::new();
        for k in keys {
            if k.guid == PACKET.guid && k.pid == PACKET.pid {
                continue;
            }
            if let Some(Prop(kind, data)) = self.get(&k, false)? {
                properties.insert(
                    format!(
                        "{:08x}{:04x}{:04x}{:016x}/{}",
                        k.guid.a,
                        k.guid.b,
                        k.guid.c,
                        u64::from_be_bytes(k.guid.d),
                        k.pid
                    ),
                    (kind, data),
                );
            }
        }
        let mut h = null_mut();
        let rc =
            unsafe { CM_Open_Device_Interface_KeyW(self.path.as_ptr(), 0x20019, 1, &mut h, 0) };
        let registry = if rc == 0x2e {
            std::collections::BTreeMap::new()
        } else {
            cm("读取接口配置", rc)?;
            super::registry::interface_values(h)?
        };
        Ok(Metadata {
            properties,
            registry,
        })
    }
}
#[link(name = "Cfgmgr32")]
unsafe extern "system" {
    fn CM_Get_DevNode_PropertyW(
        node: u32,
        key: *const PropKey,
        kind: *mut u32,
        data: *mut u8,
        size: *mut u32,
        flags: u32,
    ) -> u32;
}
pub fn arrival(usb: &str) -> Result<u64> {
    const ARRIVAL: PropKey = PropKey {
        guid: Guid::new(
            0x83da6326,
            0x97a6,
            0x4088,
            [0x94, 0x53, 0xa1, 0x92, 0x3f, 0x57, 0x3b, 0x29],
        ),
        pid: 102,
    };
    let mut node = 0;
    cm("定位到场记录的物理设备", unsafe {
        CM_Locate_DevNodeW(&mut node, wide(usb).as_ptr(), 0)
    })?;
    let mut data = [0; 8];
    let mut size = 8;
    let mut kind = 0;
    cm("读取物理 USB 最近到场时间", unsafe {
        CM_Get_DevNode_PropertyW(node, &ARRIVAL, &mut kind, data.as_mut_ptr(), &mut size, 0)
    })?;
    super::need(kind == 16 && size == 8, "设备到场时间类型无效")?;
    Ok(u64::from_le_bytes(data))
}
