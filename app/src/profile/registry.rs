// Registry reads preserve both Windows type and original bytes. Source mutations
// and the receipt pointer share one KTM transaction, including empty-key removal.
use super::{Result, check, wide};
use serde::{Deserialize, Serialize};
use std::{
    ffi::c_void,
    ptr::{null, null_mut},
};
type H = *mut c_void;
const HKLM: H = -2147483646isize as H;
const VIEW: u32 = 0x100;
pub const READ: u32 = 0x20019;
pub const SET: u32 = 2;
fn machine_key() -> H {
    #[cfg(test)]
    if let Some(key) = scratch::machine_key() {
        return key;
    }
    HKLM
}
#[link(name = "Advapi32")]
unsafe extern "system" {
    fn RegOpenKeyExW(h: H, path: *const u16, options: u32, rights: u32, out: *mut H) -> i32;
    fn RegOpenKeyTransactedW(
        h: H,
        path: *const u16,
        options: u32,
        rights: u32,
        out: *mut H,
        tx: H,
        ext: H,
    ) -> i32;
    fn RegCreateKeyTransactedW(
        h: H,
        path: *const u16,
        res: u32,
        class: *const u16,
        options: u32,
        rights: u32,
        sa: H,
        out: *mut H,
        disposition: *mut u32,
        tx: H,
        ext: H,
    ) -> i32;
    fn RegDeleteKeyTransactedW(h: H, path: *const u16, view: u32, res: u32, tx: H, ext: H) -> i32;
    fn RegQueryValueExW(
        h: H,
        name: *const u16,
        res: H,
        kind: *mut u32,
        data: *mut u8,
        size: *mut u32,
    ) -> i32;
    fn RegSetValueExW(
        h: H,
        name: *const u16,
        res: u32,
        kind: u32,
        data: *const u8,
        size: u32,
    ) -> i32;
    fn RegDeleteValueW(h: H, name: *const u16) -> i32;
    fn RegQueryInfoKeyW(
        h: H,
        class: *mut u16,
        len: *mut u32,
        res: H,
        keys: *mut u32,
        maxkey: *mut u32,
        maxclass: *mut u32,
        values: *mut u32,
        maxname: *mut u32,
        maxdata: *mut u32,
        sec: *mut u32,
        time: H,
    ) -> i32;
    fn RegEnumValueW(
        h: H,
        index: u32,
        name: *mut u16,
        len: *mut u32,
        res: H,
        kind: *mut u32,
        data: *mut u8,
        size: *mut u32,
    ) -> i32;
    fn RegCloseKey(h: H) -> i32;
}
#[link(name = "KtmW32")]
unsafe extern "system" {
    fn CreateTransaction(
        sa: H,
        guid: H,
        options: u32,
        isolation: u32,
        flags: u32,
        timeout: u32,
        desc: *const u16,
    ) -> H;
    fn CommitTransaction(h: H) -> i32;
}
#[link(name = "Kernel32")]
unsafe extern "system" {
    fn CloseHandle(h: H) -> i32;
}
fn code(op: &str, rc: i32) -> Result<()> {
    if rc == 0 {
        Ok(())
    } else {
        Err(format!("{op}：{}", std::io::Error::from_raw_os_error(rc)))
    }
}
pub struct Tx(H);
impl Tx {
    pub fn start() -> Result<Self> {
        let h = unsafe {
            CreateTransaction(
                null_mut(),
                null_mut(),
                0,
                0,
                0,
                0,
                wide("ZH3 playback settings").as_ptr(),
            )
        };
        check(!h.is_null() && h as isize != -1, "CreateTransaction")?;
        Ok(Self(h))
    }
    pub fn commit(self) -> Result<()> {
        check(
            unsafe { CommitTransaction(self.0) } != 0,
            "CommitTransaction",
        )
    }
}
impl Drop for Tx {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}
pub struct Key(H);
impl Drop for Key {
    fn drop(&mut self) {
        unsafe {
            RegCloseKey(self.0);
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Raw {
    pub kind: u32,
    pub bytes: Vec<u8>,
}
impl Raw {
    pub fn sz(value: &str) -> Self {
        Self {
            kind: 1,
            bytes: wide(value).into_iter().flat_map(u16::to_le_bytes).collect(),
        }
    }
    pub fn dword(n: u32) -> Self {
        Self {
            kind: 4,
            bytes: n.to_le_bytes().to_vec(),
        }
    }
    pub fn binary(bytes: Vec<u8>) -> Self {
        Self { kind: 3, bytes }
    }
    pub fn string(&self) -> Result<String> {
        if self.kind != 1 || self.bytes.len() % 2 != 0 {
            return Err("选档或回执类型无效".into());
        }
        let units: Vec<_> = self
            .bytes
            .chunks_exact(2)
            .map(|w| u16::from_le_bytes([w[0], w[1]]))
            .collect();
        let s = units
            .strip_suffix(&[0])
            .filter(|s| !s.contains(&0))
            .ok_or("注册表字符串格式无效")?;
        String::from_utf16(s).map_err(|e| e.to_string())
    }
}
pub fn open(path: &str, rights: u32, tx: Option<&Tx>) -> Result<Option<Key>> {
    let mut h = null_mut();
    let p = wide(path);
    let rc = unsafe {
        match tx {
            Some(tx) => RegOpenKeyTransactedW(
                machine_key(),
                p.as_ptr(),
                0,
                rights | VIEW,
                &mut h,
                tx.0,
                null_mut(),
            ),
            None => RegOpenKeyExW(machine_key(), p.as_ptr(), 0, rights | VIEW, &mut h),
        }
    };
    if matches!(rc, 2 | 3) {
        return Ok(None);
    }
    code(&format!("读取 HKLM\\{path}"), rc)?;
    Ok(Some(Key(h)))
}
pub fn create(path: &str, rights: u32, tx: &Tx) -> Result<(Key, bool)> {
    let mut h = null_mut();
    let mut disp = 0;
    code(&format!("创建 HKLM\\{path}"), unsafe {
        RegCreateKeyTransactedW(
            machine_key(),
            wide(path).as_ptr(),
            0,
            null(),
            0,
            rights | VIEW,
            null_mut(),
            &mut h,
            &mut disp,
            tx.0,
            null_mut(),
        )
    })?;
    Ok((Key(h), disp == 1))
}
pub fn remove_empty(path: &str, tx: &Tx) -> Result<()> {
    let Some(k) = open(path, READ, Some(tx))? else {
        return Ok(());
    };
    let mut sub = 0;
    let mut vals = 0;
    code("读取子键和值计数", unsafe {
        RegQueryInfoKeyW(
            k.0,
            null_mut(),
            null_mut(),
            null_mut(),
            &mut sub,
            null_mut(),
            null_mut(),
            &mut vals,
            null_mut(),
            null_mut(),
            null_mut(),
            null_mut(),
        )
    })?;
    if sub == 0 && vals == 0 {
        drop(k);
        code(&format!("移除空键 HKLM\\{path}"), unsafe {
            RegDeleteKeyTransactedW(
                machine_key(),
                wide(path).as_ptr(),
                VIEW,
                0,
                tx.0,
                null_mut(),
            )
        })?;
    }
    Ok(())
}
impl Key {
    pub fn names(&self) -> Result<Vec<String>> {
        let mut names = vec![];
        let mut buf = vec![0u16; 16384];
        loop {
            let mut count = buf.len() as u32;
            let rc = unsafe {
                RegEnumValueW(
                    self.0,
                    names.len() as u32,
                    buf.as_mut_ptr(),
                    &mut count,
                    null_mut(),
                    null_mut(),
                    null_mut(),
                    null_mut(),
                )
            };
            if rc == 259 {
                break;
            }
            code("枚举回执锚", rc)?;
            names.push(String::from_utf16(&buf[..count as usize]).map_err(|e| e.to_string())?);
        }
        Ok(names)
    }
    pub fn get(&self, name: &str) -> Result<Option<Raw>> {
        let w = wide(name);
        let mut size = 0;
        let mut kind = 0;
        let rc = unsafe {
            RegQueryValueExW(
                self.0,
                w.as_ptr(),
                null_mut(),
                &mut kind,
                null_mut(),
                &mut size,
            )
        };
        if rc == 2 {
            return Ok(None);
        }
        code(&format!("读取 {name}"), rc)?;
        if size > 1_048_576 {
            return Err(format!("{name} 超出大小限制"));
        }
        let mut bytes = vec![0; size as usize];
        code(&format!("读取 {name} 数据"), unsafe {
            RegQueryValueExW(
                self.0,
                w.as_ptr(),
                null_mut(),
                &mut kind,
                bytes.as_mut_ptr(),
                &mut size,
            )
        })?;
        bytes.truncate(size as usize);
        Ok(Some(Raw { kind, bytes }))
    }
    pub fn put(&self, name: &str, value: &Raw) -> Result<()> {
        code(&format!("写入 {name}"), unsafe {
            RegSetValueExW(
                self.0,
                wide(name).as_ptr(),
                0,
                value.kind,
                value.bytes.as_ptr(),
                value.bytes.len() as u32,
            )
        })
    }
    pub fn erase(&self, name: &str) -> Result<()> {
        code(&format!("移除 {name}"), unsafe {
            RegDeleteValueW(self.0, wide(name).as_ptr())
        })
    }
}
pub fn read(path: &str, name: &str, tx: Option<&Tx>) -> Result<Option<Raw>> {
    match open(path, 1, tx)? {
        Some(k) => k.get(name),
        None => Ok(None),
    }
}
/// Configuration Manager returns an ordinary owned HKEY for this interface.
pub fn interface_oem(handle: *mut c_void) -> Result<(Option<Raw>, Option<Raw>)> {
    let owner = Key(handle);
    let mut sub = null_mut();
    let path = wide(r"EP\0");
    let rc = unsafe { RegOpenKeyExW(owner.0, path.as_ptr(), 0, 1 | VIEW, &mut sub) };
    if matches!(rc, 2 | 3) {
        return Ok((None, None));
    }
    code("读取接口 EP\\0", rc)?;
    let key = Key(sub);
    Ok((key.get(super::OEM)?, key.get(super::USE_DEFAULT)?))
}
#[link(name = "Advapi32")]
unsafe extern "system" {
    fn RegEnumKeyExW(
        h: H,
        index: u32,
        name: *mut u16,
        len: *mut u32,
        res: H,
        class: *mut u16,
        classlen: *mut u32,
        time: H,
    ) -> i32;
}
impl Key {
    pub fn subkeys(&self) -> Result<Vec<String>> {
        let mut names = vec![];
        let mut buf = [0u16; 256];
        loop {
            let mut len = buf.len() as u32;
            let rc = unsafe {
                RegEnumKeyExW(
                    self.0,
                    names.len() as u32,
                    buf.as_mut_ptr(),
                    &mut len,
                    null_mut(),
                    null_mut(),
                    null_mut(),
                    null_mut(),
                )
            };
            if rc == 259 {
                break;
            }
            code("枚举关联配置键", rc)?;
            names.push(String::from_utf16(&buf[..len as usize]).map_err(|e| e.to_string())?);
        }
        Ok(names)
    }
}
pub fn interface_values(handle: *mut c_void) -> Result<std::collections::BTreeMap<String, Raw>> {
    fn walk(
        key: &Key,
        prefix: &str,
        depth: u8,
        out: &mut std::collections::BTreeMap<String, Raw>,
    ) -> Result<()> {
        if depth > 12 || out.len() > 1024 {
            return Err("接口注册表内容超出范围，已保留接口".into());
        }
        if !prefix.is_empty()
            && !prefix.eq_ignore_ascii_case(r"\EP")
            && !prefix.eq_ignore_ascii_case(r"\EP\0")
        {
            out.insert(prefix.to_string(), Raw::binary(vec![]));
        }
        for name in key.names()? {
            if let Some(v) = key.get(&name)? {
                out.insert(format!("{prefix}\0{name}"), v);
            }
        }
        for name in key.subkeys()? {
            let mut h = null_mut();
            code("读取接口子键", unsafe {
                RegOpenKeyExW(key.0, wide(&name).as_ptr(), 0, READ | VIEW, &mut h)
            })?;
            walk(&Key(h), &format!(r"{prefix}\{name}"), depth + 1, out)?;
        }
        Ok(())
    }
    let owner = Key(handle);
    let mut out = std::collections::BTreeMap::new();
    walk(&owner, "", 0, &mut out)?;
    Ok(out)
}
#[cfg(test)]
pub(super) mod scratch {
    use super::*;
    use std::{cell::RefCell, fs, path::PathBuf};
    thread_local! {
        static INSTALL: RefCell<Option<(H, PathBuf)>> = const { RefCell::new(None) };
    }
    pub(super) fn machine_key() -> Option<H> {
        INSTALL.with_borrow(|v| v.as_ref().map(|(handle, _)| *handle))
    }
    pub fn fixture_exe() -> Option<PathBuf> {
        INSTALL.with_borrow(|v| v.as_ref().map(|(_, dir)| dir.join("fixture.exe")))
    }
    /// Redirect this test thread's registry operations and portable state
    /// to a fresh HKCU hive and directory while exercising the real writer.
    pub fn with_install(test: impl FnOnce() -> Result<()>) -> Result<()> {
        let hive = Hive::with_children(&[])?;
        let mut root = null_mut();
        code("打开测试安装注册表", unsafe {
            RegOpenKeyExW(
                HKCU,
                wide(&hive.path).as_ptr(),
                0,
                0xf003f | VIEW,
                &mut root,
            )
        })?;
        let root = Key(root);
        let dir = std::env::temp_dir().join(format!("zh3-install-{}", super::super::store::id()?));
        fs::create_dir(&dir).map_err(|e| e.to_string())?;
        struct Install {
            root: Key,
            dir: PathBuf,
        }
        impl Drop for Install {
            fn drop(&mut self) {
                INSTALL.with_borrow_mut(|v| *v = None);
                unsafe {
                    RegDeleteTreeW(self.root.0, null());
                }
                let _ = fs::remove_dir_all(&self.dir);
            }
        }
        let install = Install { root, dir };
        assert!(machine_key().is_none());
        INSTALL.with_borrow_mut(|v| *v = Some((install.root.0, install.dir.clone())));
        test()
    }
    const HKCU: H = -2147483647isize as H;
    #[link(name = "Advapi32")]
    unsafe extern "system" {
        fn RegCreateKeyExW(
            h: H,
            path: *const u16,
            res: u32,
            class: *const u16,
            opts: u32,
            rights: u32,
            sa: H,
            out: *mut H,
            disp: *mut u32,
        ) -> i32;
        fn RegDeleteKeyExW(h: H, path: *const u16, view: u32, res: u32) -> i32;
        fn RegDeleteTreeW(h: H, path: *const u16) -> i32;
    }
    pub struct Hive {
        path: String,
        children: Vec<&'static str>,
    }
    impl Hive {
        pub fn new() -> Result<Self> {
            Self::with_children(&["Source", "Anchor", "Reservation"])
        }
        pub fn with_children(names: &[&'static str]) -> Result<Self> {
            super::super::need(
                names
                    .iter()
                    .all(|n| !n.is_empty() && !n.contains(['\\', '\0'])),
                "无效的测试子键名",
            )?;
            let path = format!(
                r"Software\ZH3StateStorageTest-{}",
                super::super::store::id()?
            );
            let mut root = null_mut();
            let mut disp = 0;
            code("创建测试专用临时注册表根", unsafe {
                RegCreateKeyExW(
                    HKCU,
                    wide(&path).as_ptr(),
                    0,
                    null(),
                    0,
                    READ | SET | 4 | VIEW,
                    null_mut(),
                    &mut root,
                    &mut disp,
                )
            })?;
            let owned = Key(root);
            super::super::need(disp == 1, "测试键意外冲突")?;
            let mut result = Self {
                path,
                children: vec![],
            };
            for &name in names {
                let mut child = null_mut();
                code("初始化测试专用子键", unsafe {
                    RegCreateKeyExW(
                        owned.0,
                        wide(name).as_ptr(),
                        0,
                        null(),
                        0,
                        READ | SET | VIEW,
                        null_mut(),
                        &mut child,
                        &mut disp,
                    )
                })?;
                drop(Key(child));
                result.children.push(name);
            }
            Ok(result)
        }
        pub fn names(&self) -> Result<Vec<String>> {
            let mut root = null_mut();
            code("枚举隔离测试子键", unsafe {
                RegOpenKeyExW(HKCU, wide(&self.path).as_ptr(), 0, READ | VIEW, &mut root)
            })?;
            Key(root).subkeys()
        }
        pub fn key(&self, name: &str, tx: Option<&Tx>) -> Result<Key> {
            super::super::need(self.children.contains(&name), "无效的测试键名")?;
            let path = wide(&format!(r"{}\{name}", self.path));
            let mut h = null_mut();
            let rc = unsafe {
                match tx {
                    Some(tx) => RegOpenKeyTransactedW(
                        HKCU,
                        path.as_ptr(),
                        0,
                        READ | SET | VIEW,
                        &mut h,
                        tx.0,
                        null_mut(),
                    ),
                    None => RegOpenKeyExW(HKCU, path.as_ptr(), 0, READ | VIEW, &mut h),
                }
            };
            code("打开隔离测试键", rc)?;
            Ok(Key(h))
        }
        pub fn set(&self, name: &str, value: &str, tx: &Tx) -> Result<()> {
            self.key(name, Some(tx))?.put("Value", &Raw::sz(value))
        }
        pub fn get(&self, name: &str) -> Result<Option<String>> {
            self.key(name, None)?
                .get("Value")?
                .map(|v| v.string())
                .transpose()
        }
    }
    impl Drop for Hive {
        fn drop(&mut self) {
            for name in &self.children {
                unsafe {
                    RegDeleteKeyExW(
                        HKCU,
                        wide(&format!(r"{}\{name}", self.path)).as_ptr(),
                        VIEW,
                        0,
                    );
                }
            }
            unsafe {
                RegDeleteKeyExW(HKCU, wide(&self.path).as_ptr(), VIEW, 0);
            }
        }
    }
}
