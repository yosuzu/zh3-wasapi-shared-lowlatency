// The portable file is untrusted. HKLM authenticates a slot and its
// SHA-256 digest. Each state revision includes a fresh nonce.
mod slots;
use super::registry::{self, Raw, Tx};
use super::{Device, Receipt, Result, check, wide};
use std::{
    ffi::c_void,
    fs::{self, File, OpenOptions},
    io::Read,
    os::windows::fs::{MetadataExt, OpenOptionsExt},
    path::{Component, PathBuf, Prefix},
    ptr::{null, null_mut},
};
type H = *mut c_void;
const ANCHORS: &str = r"SOFTWARE\ZH3 Shared Control\ReceiptAnchors";
const STREAM_ANCHORS: &str = r"SOFTWARE\ZH3 Shared Control\StreamReceiptAnchors";
const LIMIT: u64 = 32 * 1024 * 1024;
#[link(name = "Bcrypt")]
unsafe extern "system" {
    fn BCryptOpenAlgorithmProvider(h: *mut H, alg: *const u16, imp: *const u16, flags: u32) -> i32;
    fn BCryptHash(
        h: H,
        secret: *const u8,
        secretlen: u32,
        input: *const u8,
        inputlen: u32,
        out: *mut u8,
        outlen: u32,
    ) -> i32;
    fn BCryptCloseAlgorithmProvider(h: H, flags: u32) -> i32;
    fn BCryptGenRandom(h: H, buf: *mut u8, len: u32, flags: u32) -> i32;
}
#[link(name = "Kernel32")]
unsafe extern "system" {
    fn CreateMutexW(sa: H, owner: i32, name: *const u16) -> H;
    fn WaitForSingleObject(h: H, timeout: u32) -> u32;
    fn ReleaseMutex(h: H) -> i32;
    fn CloseHandle(h: H) -> i32;
    fn GetDriveTypeW(root: *const u16) -> u32;
}
fn hex(b: &[u8]) -> String {
    b.iter().map(|v| format!("{v:02x}")).collect()
}
pub fn id() -> Result<String> {
    let mut b = [0u8; 16];
    if unsafe { BCryptGenRandom(null_mut(), b.as_mut_ptr(), 16, 2) } < 0 {
        return Err("系统随机数发生器不可用".into());
    }
    Ok(hex(&b))
}
fn digest(bytes: &[u8]) -> Result<String> {
    let mut handle = null_mut();
    let mut out = [0u8; 32];
    if unsafe { BCryptOpenAlgorithmProvider(&mut handle, wide("SHA256").as_ptr(), null(), 0) } < 0 {
        return Err("无法打开 SHA-256 提供程序".into());
    }
    let rc = unsafe {
        BCryptHash(
            handle,
            null(),
            0,
            bytes.as_ptr(),
            bytes.len() as u32,
            out.as_mut_ptr(),
            32,
        )
    };
    unsafe {
        BCryptCloseAlgorithmProvider(handle, 0);
    }
    if rc < 0 {
        return Err("无法核验回执摘要".into());
    }
    Ok(hex(&out))
}
pub struct Lock(H);
impl Lock {
    pub fn acquire() -> Result<Self> {
        #[cfg(test)]
        if registry::scratch::fixture_exe().is_some() {
            let h = unsafe { CreateMutexW(null_mut(), 1, null()) };
            check(!h.is_null(), "CreateMutexW (isolated test)")?;
            return Ok(Self(h));
        }
        let h =
            unsafe { CreateMutexW(null_mut(), 0, wide(r"Global\ZH3SharedControl.v1").as_ptr()) };
        check(!h.is_null(), "CreateMutexW")?;
        if !matches!(unsafe { WaitForSingleObject(h, 0) }, 0 | 0x80) {
            unsafe {
                CloseHandle(h);
            }
            return Err("另一项 ZH3 配置操作正在进行".into());
        }
        Ok(Self(h))
    }
}
impl Drop for Lock {
    fn drop(&mut self) {
        unsafe {
            ReleaseMutex(self.0);
            CloseHandle(self.0);
        }
    }
}
struct Dir {
    path: PathBuf,
    _handles: Vec<File>,
}
impl Dir {
    fn open(create: bool) -> Result<Option<Self>> {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        #[cfg(test)]
        let exe = registry::scratch::fixture_exe().unwrap_or(exe);
        let parent = exe.parent().ok_or("EXE 路径无效")?;
        let drive = match parent.components().next() {
            Some(Component::Prefix(p)) => match p.kind() {
                Prefix::Disk(c) | Prefix::VerbatimDisk(c) => c as char,
                _ => return Err("请将 EXE 与 ZH3-Data 放在本地磁盘上".into()),
            },
            _ => return Err("便携回执需要本地磁盘的绝对路径".into()),
        };
        if !matches!(
            unsafe { GetDriveTypeW(wide(&format!("{drive}:\\")).as_ptr()) },
            2 | 3 | 6
        ) {
            return Err("便携回执所在位置不是可验证的本地卷".into());
        }
        let mut path = PathBuf::new();
        let mut handles = vec![];
        for c in parent
            .components()
            .chain([Component::Normal("ZH3-Data".as_ref())])
        {
            path.push(c.as_os_str());
            if matches!(c, Component::Prefix(_) | Component::RootDir) {
                continue;
            }
            if !matches!(c, Component::Normal(_)) {
                return Err("EXE 路径含有不安全的相对分段".into());
            }
            if create && path == parent.join("ZH3-Data") {
                match fs::create_dir(&path) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(e) => return Err(format!("无法建立回执目录 {}：{e}", path.display())),
                }
            }
            // Hold every component without FILE_SHARE_DELETE so it cannot be replaced
            // with a junction during the privileged save operation.
            let opened = OpenOptions::new()
                .access_mode(0x80)
                .share_mode(1)
                .custom_flags(0x02000000 | 0x00200000)
                .open(&path);
            let f = match opened {
                Ok(f) => f,
                Err(e)
                    if !create
                        && path == parent.join("ZH3-Data")
                        && e.kind() == std::io::ErrorKind::NotFound =>
                {
                    return Ok(None);
                }
                Err(e) => return Err(format!("无法锁定回执路径 {}：{e}", path.display())),
            };
            let m = f.metadata().map_err(|e| e.to_string())?;
            if !m.is_dir() || m.file_attributes() & 0x400 != 0 {
                return Err(format!(
                    "回执路径是重解析点或非常规目录：{}",
                    path.display()
                ));
            }
            handles.push(f);
        }
        Ok(Some(Self {
            path,
            _handles: handles,
        }))
    }
}
fn valid_hex(s: &str, n: usize) -> bool {
    s.len() == n && s.bytes().all(|c| c.is_ascii_hexdigit())
}
fn parse_anchor(anchor: &str) -> Result<(&str, &str)> {
    let (fid, hash) = anchor.split_once('.').ok_or("本机回执校验锚损坏")?;
    if valid_hex(fid, 32) && valid_hex(hash, 64) {
        Ok((fid, hash))
    } else {
        Err("本机回执校验锚格式无效".into())
    }
}
const BOOKS: &str = r"SOFTWARE\ZH3 State File Reservations";

fn read_book(scope: &str) -> Result<Option<slots::Book>> {
    let Some(v) = anchor(BOOKS, scope, None)? else {
        return Ok(None);
    };
    let b: slots::Book = serde_json::from_str(&v).map_err(|e| format!("文件预留信息损坏：{e}"))?;
    b.validate()?;
    Ok(Some(b))
}
fn reserve(scope: &str, b: &slots::Book) -> Result<()> {
    // Deliberately independent of the transaction owning the driver source and
    // StreamReceiptAnchors, and stored under a separate registry branch.
    let reservation = Tx::start()?;
    let (k, _) = registry::create(BOOKS, registry::SET, &reservation)?;
    k.put(
        scope,
        &Raw::sz(&serde_json::to_string(b).map_err(|e| e.to_string())?),
    )?;
    reservation.commit()
}
fn current_slot(anchor: Option<&str>) -> Result<Option<slots::Slot>> {
    match anchor {
        Some(a) => slots::Slot::parse(a),
        None => Ok(None),
    }
}
pub fn commit(r: &Receipt, tx: Tx) -> Result<String> {
    let scope = scope_key(&r.device)?;
    let old = anchor(STREAM_ANCHORS, &scope, None)?;
    let current = current_slot(old.as_deref())?;
    let dir = Dir::open(true)?.ok_or("状态目录未建立")?;
    let mut book = match read_book(&scope)? {
        Some(b) => b,
        None if current.is_none() => slots::Book::new()?,
        None => return Err("缺少当前状态文件的本机预留记录".into()),
    };
    slots::save(
        &dir.path,
        &mut book,
        current.as_ref(),
        r,
        |b| reserve(&scope, b),
        |slot| {
            verify_legacy_anchor(r, &tx)?;
            super::need(
                anchor(STREAM_ANCHORS, &scope, Some(&tx))? == old,
                "提交期间本机状态锚发生变化",
            )?;
            let (key, _) = registry::create(STREAM_ANCHORS, registry::SET, &tx)?;
            key.put(&scope, &Raw::sz(&slot.anchor()))?;
            tx.commit()
        },
    )
}
fn file(usb: &str, anchor: &str) -> Result<Option<Receipt>> {
    let slot = slots::Slot::parse(anchor)?;
    if slot.is_none() {
        parse_anchor(anchor)?;
    }
    let Some(dir) = Dir::open(false)? else {
        return Ok(None);
    };
    file_at(&dir.path, usb, anchor)
}
fn file_at(dir: &std::path::Path, usb: &str, anchor: &str) -> Result<Option<Receipt>> {
    if let Some(slot) = slots::Slot::parse(anchor)? {
        let r = slots::read(dir, &slot)?;
        if let Some(r) = &r {
            super::need(
                r.version == 1 && r.device.usb.eq_ignore_ascii_case(usb),
                "状态版本或 USB 实例不符",
            )?
        }
        return Ok(r);
    }
    let (fid, hash) = parse_anchor(anchor)?;
    let p = dir.join(format!("receipt-{fid}.json"));
    let f = match OpenOptions::new()
        .read(true)
        .share_mode(1)
        .custom_flags(0x00200000)
        .open(&p)
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("读取便携回执 {}：{e}", p.display())),
    };
    let meta = f.metadata().map_err(|e| e.to_string())?;
    if !meta.is_file() || meta.file_attributes() & 0x400 != 0 || meta.len() > LIMIT {
        return Err("便携回执类型或长度无效".into());
    }
    let mut b = vec![];
    f.take(LIMIT + 1)
        .read_to_end(&mut b)
        .map_err(|e| e.to_string())?;
    if b.len() as u64 > LIMIT || digest(&b)? != hash {
        return Err("便携回执校验失败，请恢复对应的 ZH3-Data 文件".into());
    }
    let r: Receipt = serde_json::from_slice(&b).map_err(|e| format!("回执内容损坏：{e}"))?;
    if r.version != 1 || !r.device.usb.eq_ignore_ascii_case(usb) {
        return Err("回执版本或 USB 实例不符".into());
    }
    Ok(Some(r))
}
fn records(path: &str) -> Result<Vec<String>> {
    match registry::open(path, registry::READ, None)? {
        Some(k) => k.names(),
        None => Ok(vec![]),
    }
}

#[derive(Default, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq, Debug)]
pub(super) enum Legacy {
    #[default]
    None,
    Same(String),
    Separate(String),
}
pub(super) struct Session {
    pub receipt: Option<Receipt>,
    pub legacy: Legacy,
    pub context: Option<String>,
}
impl Session {
    fn new(receipt: Option<Receipt>, legacy: Legacy) -> Self {
        Self {
            receipt,
            legacy,
            context: None,
        }
    }
    fn verified(receipt: Receipt, context: String) -> Self {
        let legacy = receipt.legacy.clone();
        Self {
            receipt: Some(receipt),
            legacy,
            context: Some(context),
        }
    }
}
fn scope_key(d: &Device) -> Result<String> {
    let id = [
        &d.usb,
        &d.ks,
        &d.container,
        &d.driver,
        &d.stream,
        &d.base_ref,
    ]
    .map(|s| s.to_ascii_lowercase())
    .join("\0");
    Ok(format!("{}|{}|{}", d.usb, d.stream, digest(id.as_bytes())?))
}
fn anchor(path: &str, name: &str, tx: Option<&Tx>) -> Result<Option<String>> {
    registry::read(path, name, tx)?
        .map(|v| v.string())
        .transpose()
}
fn compatible_legacy(r: &Receipt, live: Option<&str>) -> bool {
    match &r.legacy {
        Legacy::None => live.is_none(),
        Legacy::Same(a) => live == Some(a),
        Legacy::Separate(a) => live.is_none() || live == Some(a),
    }
}
fn verify_legacy_anchor(r: &Receipt, tx: &Tx) -> Result<()> {
    super::need(
        compatible_legacy(r, anchor(ANCHORS, &r.device.usb, Some(tx))?.as_deref()),
        "旧版程序的设备回执已变化，请重新检测",
    )
}
fn legacy_scope(d: &Device, legacy: &Receipt) -> Result<bool> {
    if super::same_device(&legacy.device, d) {
        return Ok(true);
    }
    // Even when the KS or container has changed, the same USB registry branch
    // and stream selector must never be claimed by two receipts.
    super::need(
        !legacy.device.stream.eq_ignore_ascii_case(&d.stream),
        "旧回执对应相同播放流，但 KS、容器、驱动或基础引用不同。已保留原配置",
    )?;
    Ok(false)
}
pub(super) fn load(d: &Device, snapshot: Option<&super::Snapshot>) -> Result<Session> {
    let key = scope_key(d)?;
    let prefix = format!("{}|{}|", d.usb, d.stream).to_ascii_lowercase();
    for n in records(STREAM_ANCHORS)? {
        if n.to_ascii_lowercase().starts_with(&prefix) && !n.eq_ignore_ascii_case(&key) {
            if anchor(STREAM_ANCHORS, &n, None)?.is_some_and(|a| {
                a.strip_prefix("closed:")
                    .is_some_and(|p| parse_anchor(p).is_ok())
            }) {
                continue;
            }
            return Err("此 USB 播放流有另一设备身份的保护记录，请核对连接".into());
        }
    }
    let old = anchor(ANCHORS, &d.usb, None)?;
    let current = anchor(STREAM_ANCHORS, &key, None)?;
    resolve(
        d,
        current,
        old,
        |a| file(&d.usb, a),
        || {
            let s = snapshot.ok_or("旧版回执文件缺失，须从活动端点核对播放流")?;
            super::confirm_unused_stream(s)
        },
    )
}
fn resolve(
    d: &Device,
    current: Option<String>,
    old: Option<String>,
    mut read: impl FnMut(&str) -> Result<Option<Receipt>>,
    confirm_unused: impl FnOnce() -> Result<()>,
) -> Result<Session> {
    if let Some(current) = current {
        if let Some(proven) = current.strip_prefix("closed:") {
            parse_anchor(proven)?;
            if old.as_deref() == Some(proven) {
                return Ok(Session::new(None, Legacy::Separate(proven.into())));
            }
        } else {
            let r = read(&current)?
                .ok_or("本播放流的已认证回执不在当前 EXE 的 ZH3-Data 中，请放回对应回执")?;
            super::need(super::same_device(&r.device, d), "当前流的回执身份不符")?;
            super::need(
                compatible_legacy(&r, old.as_deref()),
                "旧版程序已改动同一设备的回执，请核对两份记录",
            )?;
            return Ok(Session::verified(r, current));
        }
    }
    let Some(old) = old else {
        return Ok(Session::new(None, Legacy::None));
    };
    if let Some(mut receipt) = read(&old)? {
        if legacy_scope(d, &receipt)? {
            receipt.legacy = Legacy::Same(old.clone());
            return Ok(Session::verified(receipt, old));
        }
    } else {
        confirm_unused()?;
    }
    Ok(Session::new(None, Legacy::Separate(old)))
}

// When no active reference is available, use receipts only if they unambiguously
// identify one physical device and one recorded stream. A detached legacy file
// cannot be silently assigned to a currently active stream.
pub(super) fn recoverable() -> Result<Device> {
    let mut found: Option<Device> = None;
    let mut migrated = Vec::new();
    let mut consider = |r: Receipt| -> Result<()> {
        if super::supported(&r.device).is_ok() && super::probe::verify_physical(&r.device).is_ok() {
            super::need(
                found.is_none(),
                "有多个待恢复的播放流，请先在驱动面板选择目标格式",
            )?;
            found = Some(r.device)
        }
        Ok(())
    };
    for key in records(STREAM_ANCHORS)? {
        let Some((usb, _)) = key.split_once('|') else {
            continue;
        };
        if !super::interface::physical_present(usb)? {
            continue;
        }
        let Some(a) = anchor(STREAM_ANCHORS, &key, None)? else {
            continue;
        };
        if a.starts_with("closed:") {
            continue;
        }
        let r = file(usb, &a)?.ok_or("找不到该设备的便携回执，请从原目录打开程序")?;
        super::need(
            scope_key(&r.device)?.eq_ignore_ascii_case(&key),
            "播放流恢复回执不符",
        )?;
        if let Legacy::Same(a) = &r.legacy {
            migrated.push((usb.to_ascii_lowercase(), a.clone()))
        }
        consider(r)?;
    }
    for usb in records(ANCHORS)? {
        if !super::interface::physical_present(&usb)? {
            continue;
        }
        let Some(a) = anchor(ANCHORS, &usb, None)? else {
            continue;
        };
        if migrated.contains(&(usb.to_ascii_lowercase(), a.clone())) {
            continue;
        }
        let r = file(&usb, &a)?.ok_or("原版回执文件缺失，无法在没有活动播放流时定位恢复目标")?;
        consider(r)?;
    }
    found.ok_or("未找到与在场物理设备相符的恢复回执".into())
}
pub(super) fn commit_close(r: &Receipt, tx: Tx) -> Result<()> {
    let scope = scope_key(&r.device)?;
    let old = anchor(STREAM_ANCHORS, &scope, Some(&tx))?;
    let current = current_slot(old.as_deref())?;
    let book = read_book(&scope)?;
    let dir = if book.is_some() {
        Dir::open(false)?
    } else {
        None
    };
    if let Some(slot) = current {
        let b = book.as_ref().ok_or("缺少待关闭状态的本机文件预留记录")?;
        super::need(b.matches(&slot), "待关闭文件与本机预留信息不符")?;
        let d = dir.as_ref().ok_or("此目录没有当前流的状态文件")?;
        let saved = slots::read(&d.path, &slot)?.ok_or("当前流的状态文件缺失")?;
        super::need(
            super::same_device(&r.device, &saved.device),
            "待关闭文件的设备或播放流不符",
        )?;
    }
    verify_legacy_anchor(r, &tx)?;
    match completed_anchors(
        &r.legacy,
        anchor(ANCHORS, &r.device.usb, Some(&tx))?.as_deref(),
    ) {
        ClosedAnchors::KeepSeparate(a) => {
            registry::create(STREAM_ANCHORS, registry::SET, &tx)?
                .0
                .put(&scope, &Raw::sz(&format!("closed:{a}")))?;
        }
        action => {
            if old.is_some() {
                registry::open(STREAM_ANCHORS, registry::SET, Some(&tx))?
                    .ok_or("播放流状态锚缺失")?
                    .erase(&scope)?;
            }
            if action == ClosedAnchors::RemoveMatchedLegacy {
                registry::open(ANCHORS, registry::SET, Some(&tx))?
                    .ok_or("旧回执锚缺失")?
                    .erase(&r.device.usb)?;
            }
        }
    }
    tx.commit()?;
    if let (Some(b), Some(d)) = (book, dir) {
        clean_book(&scope, &b, &d)?;
    }
    Ok(())
}
fn clean_book(scope: &str, b: &slots::Book, dir: &Dir) -> Result<()> {
    let cleanup = [b.remove(&dir.path, 0), b.remove(&dir.path, 1)];
    for result in cleanup {
        result.map_err(|e| format!("原厂配置已确认，部分本地状态文件已保留。{e}"))?;
    }
    let done = Tx::start()?;
    let recorded = anchor(BOOKS, scope, Some(&done))?;
    if recorded.as_deref() == Some(&serde_json::to_string(b).map_err(|e| e.to_string())?) {
        if let Some(k) = registry::open(BOOKS, registry::SET, Some(&done))? {
            k.erase(scope)?;
        }
    }
    done.commit()
}
fn scope_belongs_to(name: &str, usb: &str) -> bool {
    name.split_once('|')
        .is_some_and(|(first, _)| first.eq_ignore_ascii_case(usb))
}
pub(super) struct RetiredDevice {
    usb: String,
}
fn retire_values(k: &registry::Key, usb: &str, streams: bool) -> Result<()> {
    for name in k.names()? {
        let ours = if streams {
            scope_belongs_to(&name, usb)
        } else {
            name.eq_ignore_ascii_case(usb)
        };
        if ours {
            k.erase(&name)?;
        }
    }
    Ok(())
}
pub(super) fn retire_device(usb: &str, tx: &Tx) -> Result<RetiredDevice> {
    super::need(
        !usb.is_empty() && !usb.contains(['|', '\0']),
        "物理 USB 身份无效",
    )?;
    for (path, streams) in [(ANCHORS, false), (STREAM_ANCHORS, true)] {
        if let Some(k) = registry::open(path, registry::READ | registry::SET, Some(tx))? {
            retire_values(&k, usb, streams)?;
        }
    }
    Ok(RetiredDevice { usb: usb.into() })
}
impl RetiredDevice {
    /// Call after transaction commit under the mutation lock. An explicit
    /// later reset also finds reservations left by an interrupted cleanup.
    pub(super) fn cleanup(self) -> Result<()> {
        let names: Vec<_> = records(BOOKS)?
            .into_iter()
            .filter(|s| scope_belongs_to(s, &self.usb))
            .collect();
        if names.is_empty() {
            return Ok(());
        }
        let Some(dir) = Dir::open(false)? else {
            return Ok(());
        };
        let mut issues = vec![];
        for scope in names {
            let cleaned = (|| {
                if anchor(STREAM_ANCHORS, &scope, None)?.is_some() {
                    return Ok(());
                }
                if let Some(b) = read_book(&scope)? {
                    clean_book(&scope, &b, &dir)?
                }
                Ok::<(), String>(())
            })();
            if let Err(e) = cleaned {
                issues.push(e)
            }
        }
        if issues.is_empty() {
            Ok(())
        } else {
            Err(issues.join("。"))
        }
    }
}
#[derive(PartialEq, Eq, Debug)]
enum ClosedAnchors<'a> {
    KeepSeparate(&'a str),
    RemoveScope,
    RemoveMatchedLegacy,
}
fn completed_anchors<'a>(relation: &'a Legacy, old: Option<&str>) -> ClosedAnchors<'a> {
    match relation {
        Legacy::Same(a) if old == Some(a) => ClosedAnchors::RemoveMatchedLegacy,
        Legacy::Separate(a) if old == Some(a) => ClosedAnchors::KeepSeparate(a),
        _ => ClosedAnchors::RemoveScope,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "uses a newly created, isolated HKCU test key; run explicitly"]
    fn retire_device_anchors_atomically_by_exact_name_even_if_values_are_corrupt() {
        let hive = registry::scratch::Hive::new().unwrap();
        let usb = r"USB\VID_152A&PID_889B\DEVICE";
        let other = format!("{usb}2");
        let source1 = format!("{usb}|StreamFormatOut_01|a");
        let source2 = format!("{usb}|StreamFormatOut_02|b");
        let foreign = format!("{other}|StreamFormatOut_01|c");
        let corrupt = Raw::binary(vec![255, 254]);
        let tx = Tx::start().unwrap();
        let old = hive.key("Source", Some(&tx)).unwrap();
        for n in [usb, &other] {
            old.put(n, &corrupt).unwrap();
        }
        let scoped = hive.key("Anchor", Some(&tx)).unwrap();
        for n in [&source1, &source2, &foreign] {
            scoped.put(n, &corrupt).unwrap();
        }
        tx.commit().unwrap();
        drop(old);
        drop(scoped);
        for should_commit in [false, true] {
            let tx = Tx::start().unwrap();
            retire_values(&hive.key("Source", Some(&tx)).unwrap(), usb, false).unwrap();
            retire_values(&hive.key("Anchor", Some(&tx)).unwrap(), usb, true).unwrap();
            if should_commit {
                tx.commit().unwrap()
            } else {
                drop(tx)
            }
            let k = hive.key("Source", None).unwrap();
            assert_eq!(k.get(usb).unwrap().is_none(), should_commit);
            assert_eq!(k.get(&other).unwrap().as_ref(), Some(&corrupt));
            let k = hive.key("Anchor", None).unwrap();
            for n in [&source1, &source2] {
                assert_eq!(k.get(n).unwrap().is_none(), should_commit);
            }
            assert_eq!(k.get(&foreign).unwrap().as_ref(), Some(&corrupt));
        }
    }
    fn device(stream: &str) -> Device {
        Device {
            usb: r"USB\VID_152A&PID_889B\A".into(),
            ks: r"FOSI\KS\A".into(),
            container: "A".into(),
            driver: super::super::SUPPORTED_DRIVER.into(),
            stream: format!("StreamFormatOut_{stream}"),
            base_ref: format!("pcm_out_{stream}_c_00"),
            active_ref: format!("pcm_out_{stream}_c_00"),
            endpoint_id: "live-id".into(),
        }
    }
    fn old_anchor() -> String {
        format!("{}.{}", "a".repeat(32), "b".repeat(64))
    }
    #[test]
    fn portable_files_without_a_local_anchor_never_authorize_writes() {
        let d = device("01");
        let s = resolve(
            &d,
            None,
            None,
            |_| panic!("a second PC must not scan portable files"),
            || panic!(),
        )
        .unwrap();
        assert!(s.receipt.is_none());
        assert_eq!(s.legacy, Legacy::None);
        let mut elsewhere = d.clone();
        elsewhere.usb.push('B');
        assert_ne!(scope_key(&elsewhere).unwrap(), scope_key(&d).unwrap());
        let mut other_ks = d.clone();
        other_ks.container.push('B');
        assert_ne!(scope_key(&other_ks).unwrap(), scope_key(&d).unwrap());
        assert!(
            resolve(
                &other_ks,
                Some(old_anchor()),
                None,
                |_| Ok(Some(Receipt::init(&d, None, None))),
                || panic!()
            )
            .is_err()
        );
    }
    #[test]
    fn legacy_journal_only_resumes_on_its_own_stream() {
        let old = device("02");
        let active = device("01");
        let a = old_anchor();
        let make = |_: &str| {
            let mut json = serde_json::to_value(Receipt::init(&old, None, None)).unwrap();
            json.as_object_mut().unwrap().remove("legacy");
            Ok(Some(serde_json::from_value(json).unwrap()))
        };
        let resume = resolve(&old, None, Some(a.clone()), make, || panic!()).unwrap();
        assert_eq!(resume.legacy, Legacy::Same(a.clone()));
        assert!(resume.receipt.is_some());
        let switched = resolve(&active, None, Some(a.clone()), make, || panic!()).unwrap();
        assert_eq!(switched.legacy, Legacy::Separate(a.clone()));
        assert!(switched.receipt.is_none());
        assert_eq!(
            completed_anchors(&switched.legacy, Some(&a)),
            ClosedAnchors::KeepSeparate(&a)
        );
        assert_eq!(
            completed_anchors(&resume.legacy, Some(&a)),
            ClosedAnchors::RemoveMatchedLegacy
        );
        assert_eq!(
            completed_anchors(&resume.legacy, Some("changed")),
            ClosedAnchors::RemoveScope
        );
        assert_ne!(scope_key(&active).unwrap(), scope_key(&old).unwrap());
    }
    #[test]
    fn orphaned_old_files_require_distinct_stream_evidence_but_corruption_is_never_waived() {
        let d = device("01");
        let a = old_anchor();
        assert!(
            resolve(
                &d,
                None,
                Some(a.clone()),
                |_| Ok(None),
                || Err("本流有写入迹象".into())
            )
            .is_err()
        );
        let s = resolve(&d, None, Some(a.clone()), |_| Ok(None), || Ok(())).unwrap();
        assert_eq!(s.legacy, Legacy::Separate(a.clone()));
        assert!(
            resolve(
                &d,
                None,
                Some(a.clone()),
                |_| Err("SHA-256 mismatch".into()),
                || panic!()
            )
            .is_err()
        );
        assert!(resolve(&d, Some(a.clone()), None, |_| Ok(None), || panic!()).is_err());
        let closed = resolve(
            &d,
            Some(format!("closed:{a}")),
            Some(a.clone()),
            |_| panic!(),
            || panic!(),
        )
        .unwrap();
        assert_eq!(closed.legacy, Legacy::Separate(a));
    }
    #[test]
    fn anchors_cannot_supply_paths_or_unauthenticated_bytes() {
        let id = "a".repeat(32);
        let hash = digest(b"abc").unwrap();
        assert_eq!(
            hash,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert!(parse_anchor(&format!("{id}.{hash}")).is_ok());
        assert!(parse_anchor(&format!("../{id}.{hash}")).is_err());
        assert!(parse_anchor(&format!("{id}.")).is_err());
        assert_ne!(digest(b"changed").unwrap(), hash);
    }
}
