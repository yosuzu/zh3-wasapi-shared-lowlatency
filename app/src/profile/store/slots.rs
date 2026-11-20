// At most two paths exist for one locally reserved file set. A slot becomes
// readable as current only when its digest is committed with the driver source.
// The independent reservation authenticates file cleanup; it is never a source
// of configuration or authority to restore a device.
use super::{LIMIT, Receipt, Result, check, digest, id, valid_hex};
use serde::{Deserialize, Serialize};
use std::{
    ffi::c_void,
    fs::{File, OpenOptions},
    io::{Read, Write},
    os::windows::{
        fs::{MetadataExt, OpenOptionsExt},
        io::AsRawHandle,
    },
    path::{Path, PathBuf},
};

const OPEN_REPARSE_POINT: u32 = 0x00200000;
const DELETE_ON_CLOSE: u32 = 0x04000000;
const DELETE: u32 = 0x10000;
#[link(name = "Kernel32")]
unsafe extern "system" {
    fn SetFileInformationByHandle(
        file: *mut c_void,
        class: i32,
        data: *const c_void,
        len: u32,
    ) -> i32;
    fn GetFileInformationByHandle(file: *mut c_void, info: *mut ByHandleInfo) -> i32;
}
#[repr(C)]
#[derive(Default)]
struct ByHandleInfo {
    attributes: u32,
    created: [u32; 2],
    accessed: [u32; 2],
    modified: [u32; 2],
    volume: u32,
    size_high: u32,
    size_low: u32,
    links: u32,
    index_high: u32,
    index_low: u32,
}

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct Book {
    owner: String,
    permitted: [Vec<String>; 2],
}
impl Book {
    pub fn new() -> Result<Self> {
        Ok(Self {
            owner: id()?,
            permitted: [vec![], vec![]],
        })
    }
    pub fn validate(&self) -> Result<()> {
        super::super::need(
            valid_hex(&self.owner, 32)
                && self
                    .permitted
                    .iter()
                    .all(|set| set.len() <= 2 && set.iter().all(|h| valid_hex(h, 64))),
            "状态文件预留信息损坏",
        )
    }
    pub fn matches(&self, slot: &Slot) -> bool {
        self.owner == slot.owner && self.permitted[slot.index].contains(&slot.hash)
    }
    fn path(&self, dir: &Path, index: usize) -> PathBuf {
        dir.join(format!("state-{}-{index}.json", self.owner))
    }
    /// Inspect the inactive file before registering the new candidate. The old
    /// digest remains permitted if a crash occurs before it is unlinked.
    pub fn prepare(
        &mut self,
        dir: &Path,
        current: Option<&Slot>,
        state: &Receipt,
    ) -> Result<Prepared> {
        let index = match current {
            Some(slot) => {
                super::super::need(self.matches(slot), "本机文件预留与当前锚不符，已保留现状")?;
                let current =
                    read(dir, slot)?.ok_or("当前状态文件缺失，无法从本目录继续设备操作")?;
                super::super::need(
                    current.version == state.version
                        && super::super::same_device(&current.device, &state.device),
                    "当前状态文件身份与操作目标不符",
                )?;
                1 - slot.index
            }
            None => 0,
        };
        let to = self.path(dir, index);
        let existing = self.owned(dir, index)?;
        let envelope = Envelope {
            format: 1,
            revision: id()?,
            receipt: state,
        };
        let bytes = serde_json::to_vec_pretty(&envelope).map_err(|e| e.to_string())?;
        super::super::need(bytes.len() as u64 <= LIMIT, "状态文件容量超出限制")?;
        let hash = digest(&bytes)?;
        let slot = Slot {
            owner: self.owner.clone(),
            index,
            hash: hash.clone(),
        };
        self.permitted[index] = existing
            .as_ref()
            .map(|(_, h)| h.clone())
            .into_iter()
            .chain([hash])
            .collect();
        Ok(Prepared {
            slot,
            path: to,
            old: existing.map(|(f, _)| f),
            bytes,
        })
    }
    fn owned(&self, dir: &Path, index: usize) -> Result<Option<(File, String)>> {
        let path = self.path(dir, index);
        let Some(f) = open(&path, true)? else {
            return Ok(None);
        };
        let mut info = ByHandleInfo::default();
        check(
            unsafe { GetFileInformationByHandle(f.as_raw_handle(), &mut info) } != 0,
            "读取状态文件标识",
        )?;
        super::super::need(info.links == 1, "状态文件存在其他硬链接，已保留")?;
        let hash = digest(&bytes(&f)?)?;
        super::super::need(
            self.permitted[index].contains(&hash),
            &format!("{} 与本机登记的内容不符，已保留", path.display()),
        )?;
        Ok(Some((f, hash)))
    }
    pub fn remove(&self, dir: &Path, index: usize) -> Result<()> {
        if let Some((f, _)) = self.owned(dir, index)? {
            unlink(f)?
        }
        Ok(())
    }
}
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Slot {
    owner: String,
    pub index: usize,
    hash: String,
}
impl Slot {
    pub fn parse(s: &str) -> Result<Option<Self>> {
        let Some(s) = s.strip_prefix("slot1:") else {
            return Ok(None);
        };
        let mut fields = s.split(':');
        let owner = fields.next().unwrap_or_default();
        let index = match fields.next() {
            Some("0") => 0,
            Some("1") => 1,
            _ => return Err("状态槽编号无效".into()),
        };
        let hash = fields.next().unwrap_or_default();
        super::super::need(
            valid_hex(owner, 32) && valid_hex(hash, 64) && fields.next().is_none(),
            "本机状态锚格式无效",
        )?;
        Ok(Some(Self {
            owner: owner.into(),
            index,
            hash: hash.into(),
        }))
    }
    pub fn anchor(&self) -> String {
        format!("slot1:{}:{}:{}", self.owner, self.index, self.hash)
    }
}
#[derive(Serialize, Deserialize)]
struct Envelope<T> {
    format: u8,
    revision: String,
    receipt: T,
}
pub(super) fn read(dir: &Path, slot: &Slot) -> Result<Option<Receipt>> {
    let file = dir.join(format!("state-{}-{}.json", slot.owner, slot.index));
    let Some(f) = open(&file, false)? else {
        return Ok(None);
    };
    let data = bytes(&f)?;
    super::super::need(
        digest(&data)? == slot.hash,
        "状态文件的 SHA-256 与本机锚不符，已保留文件",
    )?;
    let state: Envelope<Receipt> =
        serde_json::from_slice(&data).map_err(|e| format!("状态内容损坏：{e}"))?;
    super::super::need(
        state.format == 1 && valid_hex(&state.revision, 32),
        "状态文件版本或修订号无效",
    )?;
    Ok(Some(state.receipt))
}
fn open(path: &Path, removing: bool) -> Result<Option<File>> {
    let rights = 0x80000000 | if removing { DELETE } else { 0 };
    match OpenOptions::new()
        .access_mode(rights)
        .share_mode(1)
        .custom_flags(OPEN_REPARSE_POINT)
        .open(path)
    {
        Ok(f) => Ok(Some(f)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("打开状态文件 {}：{e}", path.display())),
    }
}
fn bytes(f: &File) -> Result<Vec<u8>> {
    let meta = f.metadata().map_err(|e| e.to_string())?;
    super::super::need(
        meta.is_file() && meta.file_attributes() & 0x400 == 0 && meta.len() <= LIMIT,
        "状态路径是重解析点、非常规文件或超出大小限制",
    )?;
    let mut b = vec![];
    f.take(LIMIT + 1)
        .read_to_end(&mut b)
        .map_err(|e| e.to_string())?;
    super::super::need(b.len() as u64 <= LIMIT, "状态文件容量超出限制")?;
    Ok(b)
}
fn unlink(file: File) -> Result<()> {
    // Delete by authenticated open handle. Never close the checked file and
    // delete its name through a second, racy path lookup.
    let delete = 1u8;
    check(
        unsafe {
            SetFileInformationByHandle(file.as_raw_handle(), 4, (&delete as *const u8).cast(), 1)
        } != 0,
        "清理已核验状态文件",
    )
}
fn create_candidate(path: &Path) -> Result<File> {
    OpenOptions::new()
        .access_mode(0xc0000000 | DELETE)
        .create_new(true)
        .share_mode(1)
        .custom_flags(OPEN_REPARSE_POINT | DELETE_ON_CLOSE)
        .open(path)
        .map_err(|e| format!("创建受保护的状态文件 {}：{e}", path.display()))
}
pub(super) struct Prepared {
    pub slot: Slot,
    path: PathBuf,
    old: Option<File>,
    bytes: Vec<u8>,
}
impl Prepared {
    /// The candidate is deleted by Windows if the process dies during writing.
    /// Cancel delete-on-close only after the entire payload has reached disk.
    pub fn publish(mut self) -> Result<(Slot, File)> {
        if let Some(f) = self.old.take() {
            unlink(f)?
        }
        let mut f = create_candidate(&self.path)?;
        f.write_all(&self.bytes)
            .and_then(|_| f.sync_all())
            .map_err(|e| format!("保存状态：{e}"))?;
        const FILE_DISPOSITION_ON_CLOSE: u32 = 8;
        let keep = FILE_DISPOSITION_ON_CLOSE;
        check(
            unsafe {
                SetFileInformationByHandle(f.as_raw_handle(), 21, (&keep as *const u32).cast(), 4)
            } != 0,
            "发布完整状态文件",
        )?;
        f.sync_all().map_err(|e| e.to_string())?;
        Ok((self.slot, f))
    }
}
pub(super) fn save(
    dir: &Path,
    book: &mut Book,
    current: Option<&Slot>,
    r: &Receipt,
    reserve: impl FnOnce(&Book) -> Result<()>,
    commit: impl FnOnce(&Slot) -> Result<()>,
) -> Result<String> {
    let next = book.prepare(dir, current, r)?;
    reserve(book)?;
    let (slot, held_candidate) = next.publish()?;
    commit(&slot)?;
    drop(held_candidate);
    // A concurrent reader or a changed obsolete file cannot invalidate a
    // successful driver transaction. Recheck ownership at its next reuse.
    let _ = book.remove(dir, 1 - slot.index);
    Ok(slot.anchor())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::{
        Device, Phase,
        registry::{Tx, scratch::Hive},
    };
    use std::fs;

    struct Folder(PathBuf);
    impl Folder {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("zh3-state-test-{}", id().unwrap()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn count(&self) -> usize {
            fs::read_dir(&self.0).unwrap().count()
        }
    }
    impl Drop for Folder {
        fn drop(&mut self) {
            // All fixtures in this freshly and exclusively created directory are files.
            if let Ok(files) = fs::read_dir(&self.0) {
                for entry in files.flatten() {
                    let _ = fs::remove_file(entry.path());
                }
            }
            let _ = fs::remove_dir(&self.0);
        }
    }
    fn receipt() -> Receipt {
        let d = Device {
            usb: r"USB\fixture\A".into(),
            ks: r"KS\fixture\A".into(),
            container: "test".into(),
            driver: "test".into(),
            stream: "StreamFormatOut_01".into(),
            base_ref: "pcm_out_01_c_00".into(),
            active_ref: String::new(),
            endpoint_id: String::new(),
        };
        Receipt::init(&d, None, None)
    }
    fn saved(dir: &Path, book: &mut Book, previous: Option<&Slot>, r: &Receipt) -> Slot {
        let mut anchor = String::new();
        save(
            dir,
            book,
            previous,
            r,
            |_| Ok(()),
            |s| {
                anchor = s.anchor();
                Ok(())
            },
        )
        .unwrap();
        Slot::parse(&anchor).unwrap().unwrap()
    }
    #[test]
    fn repeated_changes_keep_one_file_and_a_revision_is_never_replayed() {
        let folder = Folder::new();
        let mut b = Book::new().unwrap();
        let mut r = receipt();
        let mut current = None;
        let mut anchors = std::collections::HashSet::new();
        for i in 0..28 {
            r.restart_note = format!("state {}", i % 2);
            let s = saved(&folder.0, &mut b, current.as_ref(), &r);
            assert_eq!(
                read(&folder.0, &s).unwrap().unwrap().restart_note,
                r.restart_note
            );
            assert!(anchors.insert(s.anchor()));
            assert_eq!(folder.count(), 1);
            current = Some(s);
        }
        b.remove(&folder.0, 0).unwrap();
        b.remove(&folder.0, 1).unwrap();
        assert_eq!(folder.count(), 0);
    }
    #[test]
    fn reintroduced_legacy_files_and_foreign_slots_are_never_replaced() {
        let folder = Folder::new();
        let mut b = Book::new().unwrap();
        let mut r = receipt();
        let bytes = serde_json::to_vec_pretty(&r).unwrap();
        let legacy_id = id().unwrap();
        let legacy_hash = digest(&bytes).unwrap();
        let legacy_file = folder.0.join(format!("receipt-{legacy_id}.json"));
        fs::write(&legacy_file, &bytes).unwrap();
        let loaded = crate::profile::store::file_at(
            &folder.0,
            &r.device.usb,
            &format!("{legacy_id}.{legacy_hash}"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(loaded.phase, Phase::Preparing);
        assert_eq!(folder.count(), 1);
        let a = saved(&folder.0, &mut b, None, &r);
        r.restart_note = "next".into();
        let unknown = b.path(&folder.0, 1);
        fs::write(&unknown, b"external").unwrap();
        assert!(b.prepare(&folder.0, Some(&a), &r).is_err());
        assert!(b.remove(&folder.0, 1).is_err());
        assert_eq!(fs::read(&unknown).unwrap(), b"external");
        assert_eq!(fs::read(&legacy_file).unwrap(), bytes);
        b.remove(&folder.0, 0).unwrap();
        assert_eq!(folder.count(), 2);
        let other = Folder::new();
        assert!(read(&other.0, &a).unwrap().is_none());
    }
    #[test]
    fn hardlinks_and_active_slot_corruption_are_preserved() {
        let folder = Folder::new();
        let mut b = Book::new().unwrap();
        let r = receipt();
        let a = saved(&folder.0, &mut b, None, &r);
        let path = b.path(&folder.0, a.index);
        let alias = folder.0.join("other-file");
        fs::hard_link(&path, &alias).unwrap();
        assert!(b.remove(&folder.0, a.index).is_err());
        assert!(path.exists() && alias.exists());
        fs::remove_file(alias).unwrap();
        fs::write(&path, b"changed externally").unwrap();
        assert!(read(&folder.0, &a).is_err());
        assert!(b.prepare(&folder.0, Some(&a), &r).is_err());
        assert!(b.remove(&folder.0, a.index).is_err());
        assert_eq!(fs::read(path).unwrap(), b"changed externally");
    }
    #[test]
    fn separate_machines_and_streams_can_share_a_portable_directory() {
        let folder = Folder::new();
        let r = receipt();
        let mut other_r = receipt();
        other_r.device.stream = "StreamFormatOut_02".into();
        let mut one = Book::new().unwrap();
        let mut two = Book::new().unwrap();
        let a = saved(&folder.0, &mut one, None, &r);
        let b = saved(&folder.0, &mut two, None, &other_r);
        assert_ne!(a.owner, b.owner);
        assert_eq!(folder.count(), 2);
        assert!(one.prepare(&folder.0, Some(&b), &r).is_err());
        let new = saved(&folder.0, &mut one, Some(&a), &r);
        one.remove(&folder.0, new.index).unwrap();
        assert_eq!(folder.count(), 1);
        assert_eq!(
            read(&folder.0, &b).unwrap().unwrap().device.stream,
            "StreamFormatOut_02"
        );
    }
    #[test]
    fn interrupted_candidate_is_removed_when_the_writer_process_exits() {
        const TAG: &str = "ZH3_TEMP_SLOT_CRASH_TEST";
        if let Some(path) = std::env::var_os(TAG) {
            let path = PathBuf::from(path);
            assert!(
                path.parent().unwrap().starts_with(std::env::temp_dir())
                    && path.file_name().unwrap() == "partial.test"
            );
            let mut f = create_candidate(&path).unwrap();
            f.write_all(b"half of a JSON document").unwrap();
            f.sync_all().unwrap();
            // Exit without dropping File, as when the process is interrupted.
            std::process::exit(79);
        }
        let folder = Folder::new();
        let path = folder.0.join("partial.test");
        let out=std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact","profile::store::slots::tests::interrupted_candidate_is_removed_when_the_writer_process_exits"])
            .env(TAG,&path).output().unwrap();
        assert_eq!(out.status.code(), Some(79), "{:?}", out);
        assert!(!path.exists());
    }
    #[test]
    #[ignore = "uses a newly created, isolated HKCU test key; run explicitly"]
    fn real_registry_transaction_keeps_source_and_verified_disk_state_together() {
        let folder = Folder::new();
        let hive = Hive::new().unwrap();
        let mut r = receipt();
        let mut b = Book::new().unwrap();
        let reserve = |b: &Book| -> Result<()> {
            let tx = Tx::start()?;
            hive.set("Reservation", &serde_json::to_string(b).unwrap(), &tx)?;
            tx.commit()
        };
        let current = || {
            hive.get("Anchor")
                .unwrap()
                .and_then(|s| Slot::parse(&s).unwrap())
        };
        let tx = Tx::start().unwrap();
        hive.set("Source", "old", &tx).unwrap();
        save(&folder.0, &mut b, None, &r, reserve, |s| {
            hive.set("Anchor", &s.anchor(), &tx)?;
            tx.commit()
        })
        .unwrap();
        let old = current().unwrap();
        r.phase = Phase::Waiting;
        r.restart_note = "staged".into();
        let tx = Tx::start().unwrap();
        hive.set("Source", "new", &tx).unwrap();
        let failed = save(&folder.0, &mut b, Some(&old), &r, reserve, |s| {
            hive.set("Anchor", &s.anchor(), &tx)?;
            drop(tx); // Close without commit: Windows rolls both values back.
            Err("injected interruption before registry commit".into())
        });
        assert!(failed.is_err());
        assert_eq!(hive.get("Source").unwrap().as_deref(), Some("old"));
        assert_eq!(current().as_ref(), Some(&old));
        assert_eq!(
            read(&folder.0, &old).unwrap().unwrap().phase,
            Phase::Preparing
        );
        assert_eq!(folder.count(), 2);
        let mut reopened: Book =
            serde_json::from_str(&hive.get("Reservation").unwrap().unwrap()).unwrap();
        let tx = Tx::start().unwrap();
        hive.set("Source", "new", &tx).unwrap();
        save(&folder.0, &mut reopened, Some(&old), &r, reserve, |s| {
            hive.set("Anchor", &s.anchor(), &tx)?;
            tx.commit()
        })
        .unwrap();
        let now = current().unwrap();
        assert_ne!(now, old);
        assert_eq!(hive.get("Source").unwrap().as_deref(), Some("new"));
        assert_eq!(
            read(&folder.0, &now).unwrap().unwrap().restart_note,
            "staged"
        );
        assert_eq!(folder.count(), 1);
    }
}
