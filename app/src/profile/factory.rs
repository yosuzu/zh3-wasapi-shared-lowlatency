//! A factory reset belongs to the physical ZH3 and covers all of its Fosi PCM
//! playback formats. Named profiles may be shared, foreign, or unauthenticated.
//! Only an authenticated generation may supply names for resource cleanup.
use super::{
    Classes, Device, Interface, Raw, Result, Snapshot, Tx, interface, need, probe, registry, store,
};
use crate::probe::ConnectedHardware;
use std::collections::BTreeMap;
use windows::Win32::{
    Devices::{DeviceAndDriverInstallation::*, Properties::DEVPROP_TYPE_EMPTY},
    Foundation::{DEVPROPKEY, ERROR_SUCCESS},
    System::Registry::{
        HKEY, KEY_QUERY_VALUE, KEY_READ, KEY_SET_VALUE, KEY_WOW64_64KEY, RegCloseKey,
        RegDeleteValueW, RegOpenKeyExW,
    },
};
use windows::core::{GUID, HSTRING};

pub struct FactoryResult {
    pub device: Option<Device>,
    pub verified: bool,
    pub restart_context: Option<String>,
    pub message: String,
}

fn same_hardware(expected: &ConnectedHardware) -> Result<ConnectedHardware> {
    let live = probe::connected_hardware()?;
    need(
        confirmed_connection(expected, &live),
        "ZH3 的 USB 实例、驱动或本次连接已变化，或容器与到达时间均无法核对。请重新连接、检测并确认。",
    )?;
    Ok(live)
}

fn confirmed_connection(expected: &ConnectedHardware, live: &ConnectedHardware) -> bool {
    let container = !expected.container.is_empty() && !live.container.is_empty();
    let arrival = expected
        .arrival
        .filter(|n| *n != 0)
        .zip(live.arrival.filter(|n| *n != 0));
    live.usb.eq_ignore_ascii_case(&expected.usb)
        && (expected.driver.is_empty() || live.driver.is_empty() || live.driver == expected.driver)
        && (container || arrival.is_some())
        && (!container || live.container.eq_ignore_ascii_case(&expected.container))
        && arrival.is_none_or(|(a, b)| a == b)
}

fn number(s: &str) -> bool {
    // %02u is minimum width two for the USB alternate setting.
    s.parse::<u8>().is_ok_and(|v| format!("{v:02}") == s)
}

fn playback_key(name: &str) -> bool {
    name.to_ascii_lowercase()
        .strip_prefix("streamformatout_")
        .is_some_and(number)
}

fn bare_reference(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    let mut fields = lower.split('_');
    matches!((fields.next(),fields.next(),fields.next(),fields.next(),fields.next(),fields.next()),
        (Some("pcm"),Some("out"),Some(n),Some("c"),Some(g),None) if number(n) && number(g))
}

fn enum_path(h: &ConnectedHardware) -> String {
    format!(r"SYSTEM\CurrentControlSet\Enum\{}", h.usb)
}

fn config(h: &ConnectedHardware) -> String {
    format!(r"{}\Device Parameters\Config", enum_path(h))
}

fn fosi_source(h: &ConnectedHardware) -> Result<()> {
    let instance = enum_path(h);
    let text = |path: &str, name: &str| -> Result<Option<String>> {
        registry::read(path, name, None)?
            .map(|v| v.string())
            .transpose()
    };
    let service = text(&instance, "Service")?;
    if service
        .as_ref()
        .is_some_and(|s| s.eq_ignore_ascii_case("FosiAudioUsbAudio"))
    {
        return Ok(());
    }
    // The Fosi INF writes this vendor interface GUID on this USB instance.
    let marker = text(&format!(r"{instance}\Device Parameters"), "InterfaceGUID")?;
    need(
        marker
            .as_ref()
            .is_some_and(|s| s.eq_ignore_ascii_case("{FABB4DE8-2025-4414-8F08-FA8ADCF4398D}")),
        "已识别 ZH3，但当前 USB 实例上未能确认 Fosi 播放配置。请连接并启用 Fosi USB 驱动后再次恢复。",
    )
}

fn selectors(h: &ConnectedHardware, tx: Option<&Tx>) -> Result<BTreeMap<String, Raw>> {
    let path = config(h);
    let Some(key) = registry::open(&path, registry::READ, tx)? else {
        return Ok(BTreeMap::new());
    };
    selected_playback_values(key.subkeys()?, |name| {
        registry::read(&format!(r"{path}\{name}"), super::SELECT, tx)
    })
}

pub fn named_playback_count(h: &ConnectedHardware) -> Result<usize> {
    fosi_source(h)?;
    Ok(selectors(h, None)?.len())
}

fn selected_playback_values(
    names: Vec<String>,
    read: impl Fn(&str) -> Result<Option<Raw>>,
) -> Result<BTreeMap<String, Raw>> {
    let mut values = BTreeMap::new();
    for name in names.into_iter().filter(|name| playback_key(name)) {
        if let Some(raw) = read(&name)? {
            values.insert(name, raw);
        }
    }
    Ok(values)
}

fn erase_selected_values(
    before: &BTreeMap<String, Raw>,
    open: impl Fn(&str) -> Result<registry::Key>,
) -> Result<()> {
    for (name, raw) in before {
        need(playback_key(name), "待恢复的键不是已核实的播放流。")?;
        let key = open(name)?;
        need(
            key.get(super::SELECT)?.as_ref() == Some(raw),
            "播放选档在恢复前已变化。",
        )?;
        key.erase(super::SELECT)?;
    }
    Ok(())
}

struct Base {
    link: String,
    reference: String,
    render: bool,
    packet: Option<Vec<u8>>,
    legacy: bool,
    oem: (Option<Raw>, Option<Raw>),
}

impl Base {
    fn open(&self, ks: &str) -> Result<Interface> {
        if self.render {
            Interface::render(ks, &self.reference, &self.link)
        } else {
            Interface::open(ks, &self.reference, &self.link)
        }
    }
}

fn base_overrides(ks: &str) -> Result<Vec<Base>> {
    let classes = Classes::load(ks)?;
    let mut out = Vec::new();
    for (render, links) in [(false, &classes.audio), (true, &classes.render)] {
        for link in links {
            let Some((_, reference)) = link.rsplit_once('\\') else {
                continue;
            };
            if !bare_reference(reference) {
                continue;
            }
            let mut base = Base {
                link: link.clone(),
                reference: reference.into(),
                render,
                packet: None,
                legacy: false,
                oem: (None, None),
            };
            // SetupAPI independently verifies the complete KS + class + reference
            // and reads the reference-string property of the opened interface.
            let iface = base.open(ks)?;
            base.packet = iface.packet()?;
            base.legacy = !iface.legacy_packet_absent()?;
            base.oem = interface::oem(link)?;
            if base.packet.is_some() || base.legacy || base.oem != (None, None) {
                out.push(base);
            }
        }
    }
    Ok(out)
}

fn known_base_defaults(driver: &str) -> bool {
    driver == super::SUPPORTED_DRIVER || driver == "5.68.0.17543"
}

struct RegHandle(HKEY);
impl Drop for RegHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = RegCloseKey(self.0);
        }
    }
}

fn clear_oem(base: &Base) -> Result<()> {
    if base.oem == (None, None) {
        return Ok(());
    }
    let mut key = HKEY::default();
    let result = unsafe {
        CM_Open_Device_Interface_KeyW(
            &HSTRING::from(&base.link),
            KEY_READ.0,
            RegDisposition_OpenExisting,
            &mut key,
            0,
        )
    };
    need(
        result == CR_SUCCESS,
        &format!("打开已核对的原厂接口参数失败（CM {}）。", result.0),
    )?;
    let owner = RegHandle(key);
    let mut sub = HKEY::default();
    let rc = unsafe {
        RegOpenKeyExW(
            owner.0,
            &HSTRING::from(r"EP\0"),
            None,
            KEY_QUERY_VALUE | KEY_SET_VALUE | KEY_WOW64_64KEY,
            &mut sub,
        )
    };
    need(
        rc == ERROR_SUCCESS,
        &format!("打开原厂接口的 EP 周期参数失败（Windows {}）。", rc.0),
    )?;
    let sub = RegHandle(sub);
    need(
        interface::oem(&base.link)? == base.oem,
        "原厂接口 OEM 周期刚被其他程序修改，请重新恢复。",
    )?;
    for (name, expected) in [(super::OEM, &base.oem.0), (super::USE_DEFAULT, &base.oem.1)] {
        if expected.is_some() {
            let rc = unsafe { RegDeleteValueW(sub.0, &HSTRING::from(name)) };
            need(
                rc == ERROR_SUCCESS,
                &format!("清除原厂接口周期参数失败（Windows {}）。", rc.0),
            )?;
        }
    }
    need(
        interface::oem(&base.link)? == (None, None),
        "原厂接口 OEM 周期清除后读回不符。",
    )
}

fn clear_base(h: &ConnectedHardware, ks: &str, base: &Base) -> Result<()> {
    need(
        probe::verified_ks(h)?.eq_ignore_ascii_case(ks),
        "ZH3 的直接 KS 设备已变化，请重新恢复。",
    )?;
    let iface = base.open(ks)?;
    if let Some(expected) = &base.packet {
        need(
            iface.packet()?.as_ref() == Some(expected),
            "原厂接口包长已被其他程序修改，请重试。",
        )?;
        iface.set_packet(None)?;
    }
    if base.legacy && !iface.legacy_packet_absent()? {
        let key = DEVPROPKEY {
            fmtid: GUID::from_u128(0x13e004d6_b066_43bd_913b_a415cd13da87),
            pid: 2,
        };
        let rc = unsafe {
            CM_Set_Device_Interface_PropertyW(
                &HSTRING::from(&base.link),
                &key,
                DEVPROP_TYPE_EMPTY,
                None,
                0,
            )
        };
        need(
            rc == CR_SUCCESS && iface.legacy_packet_absent()?,
            "原厂接口旧版包长清除后核验失败。",
        )?;
    }
    clear_oem(base)
}

fn active(h: &ConnectedHardware) -> Option<Snapshot> {
    probe::read().ok().filter(|s| {
        s.device.usb.eq_ignore_ascii_case(&h.usb)
            && s.device.container.eq_ignore_ascii_case(&h.container)
            && s.device.driver == h.driver
    })
}

fn verified_output(s: &Snapshot, ks: &str) -> Result<bool> {
    let verdict = super::endpoint_verdict(s, None, &Classes::load(ks)?)?;
    Ok(verdict == super::EndpointVerdict::Matched
        || (verdict == super::EndpointVerdict::DifferentPeriod
            && !known_base_defaults(&s.device.driver)))
}

// A verified per-stream journal may authorize removing exactly its own dormant
// resources. Unreadable, missing or conflicting ownership never supplies a name.
fn clean_owned_active(s: &Snapshot) -> Result<()> {
    if s.device.driver != super::SUPPORTED_DRIVER {
        return Ok(());
    }
    let session = match store::load(&s.device, Some(s)) {
        Ok(x) => x,
        Err(_) => return Ok(()),
    };
    let Some(r) = session.receipt else {
        return Ok(());
    };
    let classes = Classes::load(&s.device.ks)?;
    let usage = Tx::start()?;
    for generation in &r.generations {
        let name = generation.reference(&s.device);
        if let Some(link) = super::registered(&s.device, generation, &classes)?
            && Classes::find(&classes.present_audio, &name)?.is_none()
            && Classes::find(&classes.present_render, &name)?.is_none()
            && !super::referenced_elsewhere(&s.device, generation, &usage)?
        {
            let iface = Interface::open(&s.device.ks, &generation.reference(&s.device), link)?;
            if iface.packet()?.as_ref() == Some(&super::packet(generation.half)) {
                iface.set_packet(None)?;
            }
        }
    }
    drop(usage);
    super::finish(s, r, &classes)?;
    Ok(())
}

fn finish_if_verified(
    h: &ConnectedHardware,
    ks: &str,
    arrival: Option<u64>,
) -> Result<Option<FactoryResult>> {
    if !selectors(h, None)?.is_empty() || !base_overrides(ks)?.is_empty() {
        return Ok(None);
    }
    if let Some(before) = arrival.filter(|n| *n != 0) {
        if interface::arrival(&h.usb)? == before {
            return Ok(None);
        }
    }
    let Some(s) = active(h) else { return Ok(None) };
    if !verified_output(&s, ks)? {
        return Ok(None);
    }
    let cleanup = clean_owned_active(&s);
    // Recheck source and active output after best-effort cleanup. Even a bad
    // journal cannot block returning to the independently proven factory state.
    let s = active(h).ok_or("原厂验收时 ZH3 的活动输出发生变化，请重新检测。")?;
    need(
        verified_output(&s, ks)? && base_overrides(ks)?.is_empty(),
        "原厂验收时目标周期属性发生变化，请重新检测。",
    )?;
    let tx = Tx::start()?;
    same_hardware(h)?;
    need(
        selectors(h, Some(&tx))?.is_empty(),
        "原厂验收时播放选档发生变化，请重试。",
    )?;
    let retired = super::factory_state::complete_factory(h, &tx)?;
    tx.commit()?;
    let files = retired.cleanup();
    let mut message = format!(
        "所有 Fosi 播放格式的自定义选档均已取消，原厂基础接口的周期覆盖均为空。当前原厂端点实读默认 {} 帧、最小 {} 帧（{} Hz）。",
        s.default_frames, s.min_frames, s.mix.rate
    );
    if cleanup.is_err() {
        message.push_str("未能核实所有权的历史档案和接口已保留。");
    }
    if let Err(e) = files {
        message.push_str(&format!("部分旧回执文件未能清理：{e}"));
    }
    Ok(Some(FactoryResult {
        device: Some(s.device),
        verified: true,
        restart_context: None,
        message,
    }))
}

fn pending(
    h: &ConnectedHardware,
    ks: Option<&str>,
    restart_context: Option<String>,
    message: String,
) -> FactoryResult {
    FactoryResult {
        device: ks.and_then(|_| probe::physical_target(h).ok()),
        verified: false,
        restart_context,
        message,
    }
}

pub fn restore_detected(
    expected: &ConnectedHardware,
    snapshot: Option<&Snapshot>,
) -> Result<FactoryResult> {
    let _lock = store::Lock::acquire()?;
    let h = same_hardware(expected)?;
    if let Some(s) = snapshot {
        need(
            s.device.usb.eq_ignore_ascii_case(&h.usb)
                && s.device.container.eq_ignore_ascii_case(&h.container),
            "本次快照属于其他物理设备，请刷新后确认恢复。",
        )?;
    }
    fosi_source(&h)?;
    // Explicitly resetting a freshly re-identified physical unit is independent
    // of damaged bookkeeping. No ownership path comes from an unreadable marker.
    let saved = super::factory_state::factory_pending(&h).ok().flatten();
    let ks = probe::verified_ks(&h);
    let bases = ks
        .as_ref()
        .map_err(|e| e.clone())
        .and_then(|k| base_overrides(k));
    let before = selectors(&h, None)?;
    if before.is_empty()
        && let Ok(ref k) = ks
        && bases.as_ref().is_ok_and(Vec::is_empty)
    {
        if let Some(done) = finish_if_verified(&h, k, saved)? {
            return Ok(done);
        }
    }
    let changes = !before.is_empty()
        || (known_base_defaults(&h.driver) && bases.as_ref().is_ok_and(|v| !v.is_empty()));
    let live_active = active(&h);
    let factory_active = match (&live_active, &ks) {
        (Some(s), Ok(k)) => verified_output(s, k).ok(),
        _ => None,
    };
    let stale = factory_active == Some(false);
    let require_reconnect = must_reload(changes, factory_active);
    if changes || saved.is_none() || (saved == Some(0) && stale) {
        let stamp = if require_reconnect {
            // A verified container is enough to reset existing selectors. If
            // Windows lacks an arrival stamp, finish on independent readback
            // and leave any necessary manual reconnection to the user.
            h.arrival.unwrap_or(0)
        } else {
            saved.unwrap_or(0)
        };
        let tx = Tx::start()?;
        same_hardware(&h)?;
        fosi_source(&h)?;
        need(
            selectors(&h, Some(&tx))? == before,
            "其他程序刚修改了播放选档，请重试。",
        )?;
        erase_selected_values(&before, |name| {
            let path = format!(r"{}\{}", config(&h), name);
            registry::open(&path, 1 | registry::SET, Some(&tx))?
                .ok_or_else(|| "待恢复的播放流键已消失。".into())
        })?;
        need(
            selectors(&h, Some(&tx))?.is_empty(),
            "全设备播放选档事务读回失败。",
        )?;
        super::factory_state::begin_factory(&h, stamp, &tx)?;
        tx.commit()?;
        need(
            selectors(&h, None)?.is_empty(),
            "提交后目标播放档发生变化，请重新恢复。",
        )?;
    }
    let k = match ks {
        Ok(k) => k,
        Err(e) => {
            return Ok(pending(
                &h,
                None,
                None,
                format!(
                    "已核对并取消这台 ZH3 的 {} 个播放选档。暂时无法核对原厂基础接口或播放端点：{e} 请接好设备并启用 Fosi 驱动后再次恢复。",
                    before.len()
                ),
            ));
        }
    };
    let overrides = match bases {
        Ok(b) => b,
        Err(e) => {
            return Ok(pending(
                &h,
                Some(&k),
                None,
                format!("全设备播放选档已取消，原厂接口暂时无法核对：{e} 请重新检测后再次恢复。"),
            ));
        }
    };
    if !overrides.is_empty() && !known_base_defaults(&h.driver) {
        return Ok(pending(
            &h,
            Some(&k),
            None,
            format!(
                "全设备播放选档已取消。驱动 {} 的原厂接口周期默认值未验证；请切回已验证的 Fosi 驱动后再次恢复。",
                h.driver
            ),
        ));
    }
    for base in &overrides {
        clear_base(&h, &k, base)?;
    }
    if !selectors(&h, None)?.is_empty() || !base_overrides(&k)?.is_empty() {
        return Err(
            "全设备播放选档或原厂接口周期清除后读回不符，请关闭驱动面板后再次恢复。".into(),
        );
    }
    let settled = super::factory_state::pending(&h)?;
    if let Some(done) = finish_if_verified(&h, &k, settled.as_ref().map(|p| p.arrival))? {
        return Ok(done);
    }
    let restart = match settled {
        Some(p) if p.arrival != 0 && interface::arrival(&h.usb)? == p.arrival => Some(p.id),
        _ => None,
    };
    let msg = if restart.is_some() {
        "全设备播放选档已取消，原厂基础接口的周期覆盖已清除。请重新连接 ZH3，等待活动端点验收。"
    } else {
        "所有可核对的播放选档和基础接口均无自定义周期，暂未确认活动端点的原厂周期。请启用播放端点、重新连接并检测。"
    };
    Ok(pending(&h, Some(&k), restart, msg.into()))
}

fn must_reload(changed_inputs: bool, active_was_factory: Option<bool>) -> bool {
    match active_was_factory {
        Some(v) => !v,
        None => changed_inputs,
    }
}

pub(super) fn ready_to_restart(expected: &ConnectedHardware) -> Result<()> {
    let h = same_hardware(expected)?;
    fosi_source(&h)?;
    let stamp =
        super::factory_state::factory_pending(&h)?.ok_or("未找到该 ZH3 的待验收全设备恢复。")?;
    need(
        stamp != 0 && interface::arrival(&h.usb)? == stamp,
        "当前恢复正在等待端点核验，无需再发起 USB 重启。",
    )?;
    need(
        selectors(&h, None)?.is_empty(),
        "其他程序已重新选择播放档，请重新确认恢复。",
    )?;
    need(
        base_overrides(&probe::verified_ks(&h)?)?.is_empty(),
        "基础接口还有待核对的周期覆盖，请重新确认恢复。",
    )
}

pub fn reconcile_detected(expected: &ConnectedHardware) -> Result<Option<FactoryResult>> {
    let h = same_hardware(expected)?;
    let Some(first) = super::factory_state::pending(&h)? else {
        return Ok(None);
    };
    let _lock = store::Lock::acquire()?;
    let Some(current) = super::factory_state::pending(&h)? else {
        return Ok(None);
    };
    need(current.id == first.id, "设备恢复进度已变化，请重新检测。")?;
    let arrival = current.arrival;
    fosi_source(&h)?;
    let ks = match probe::verified_ks(&h) {
        Ok(k) => k,
        Err(e) => {
            return Ok(Some(pending(
                &h,
                None,
                None,
                format!("已保存全设备原厂恢复，等待 Fosi 设备重新就绪。{e}"),
            )));
        }
    };
    if let Some(result) = finish_if_verified(&h, &ks, Some(arrival))? {
        return Ok(Some(result));
    }
    let clean = selectors(&h, None)?.is_empty() && base_overrides(&ks)?.is_empty();
    let needs_arrival = arrival != 0 && interface::arrival(&h.usb)? == arrival;
    let msg = if clean && needs_arrival {
        "全设备原厂配置已保存，等待设备重新连接并读回活动端点。"
    } else if clean {
        "全设备播放选档及基础接口已核对，正在等候唯一的活动原厂端点读数。请检查播放端点是否启用。"
    } else {
        "全设备原厂恢复尚未完成，或播放配置发生变化。请再次确认恢复原厂。"
    };
    Ok(Some(pending(
        &h,
        Some(&ks),
        (clean && needs_arrival).then_some(current.id),
        msg.into(),
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn physical_confirmation_needs_a_shared_witness_and_rejects_changed_usb_arrival() {
        let h = ConnectedHardware {
            usb: "USB-A".into(),
            container: String::new(),
            driver: String::new(),
            arrival: Some(17),
        };
        let mut live = h.clone();
        assert!(confirmed_connection(&h, &live));
        live.container = "CONTAINER".into();
        assert!(confirmed_connection(&h, &live));
        let original = live.clone();
        live.arrival = Some(18);
        assert!(!confirmed_connection(&original, &live));
        live.arrival = None;
        assert!(confirmed_connection(&original, &live));
        assert!(!confirmed_connection(&h, &live));
        live.container = "DIFFERENT".into();
        assert!(!confirmed_connection(&original, &live));
    }

    #[test]
    #[ignore = "uses a newly created, isolated HKCU test key; run explicitly"]
    fn registry_transaction_removes_only_existing_playback_selections_after_commit() -> Result<()> {
        let out24 = "StreamFormatOut_01";
        let out32 = "StreamFormatOut_02";
        for (names, selected) in [
            (
                &[
                    "StreamFormatOut_01",
                    "StreamFormatOut_02",
                    "StreamFormatIn_01",
                    "Settings",
                    "SoundDeviceProfiles",
                ][..],
                &[out24, out32][..],
            ),
            (
                &[
                    "StreamFormatOut_02",
                    "StreamFormatIn_01",
                    "Settings",
                    "SoundDeviceProfiles",
                ][..],
                &[out32][..],
            ),
            (&[out32, out24][..], &[out32][..]),
            (&[out24][..], &[out24][..]),
            (&[out24, out32][..], &[out24][..]),
            (&[out24][..], &[][..]),
        ] {
            let hive = registry::scratch::Hive::with_children(names)?;
            let seed = Tx::start()?;
            for &name in names {
                let k = hive.key(name, Some(&seed))?;
                if selected.contains(&name) || (name != out24 && name != out32) {
                    k.put(super::super::SELECT, &Raw::sz(&format!("selected-{name}")))?;
                }
                k.put("DefaultSoundDeviceProfile", &Raw::sz("keep vendor default"))?;
                k.put("Unrelated", &Raw::binary(vec![0xde, 0xad]))?;
            }
            seed.commit()?;
            let get = |tx: Option<&Tx>| {
                selected_playback_values(hive.names()?, |n| {
                    hive.key(n, tx)?.get(super::super::SELECT)
                })
            };
            let before = get(None)?;
            let expected: BTreeMap<_, _> = selected
                .iter()
                .map(|&name| (name.to_owned(), Raw::sz(&format!("selected-{name}"))))
                .collect();
            assert_eq!(before, expected);
            let original_names = hive.names()?;
            {
                let rollback = Tx::start()?;
                erase_selected_values(&before, |n| hive.key(n, Some(&rollback)))?;
                assert!(get(Some(&rollback))?.is_empty());
                // Drop emulates interruption before commit.
            }
            assert_eq!(get(None)?, before);
            let commit = Tx::start()?;
            erase_selected_values(&before, |n| hive.key(n, Some(&commit)))?;
            commit.commit()?;
            for &name in names {
                let k = hive.key(name, None)?;
                let expected =
                    (name != out24 && name != out32).then(|| Raw::sz(&format!("selected-{name}")));
                assert_eq!(k.get(super::super::SELECT)?, expected);
                assert_eq!(
                    k.get("DefaultSoundDeviceProfile")?,
                    Some(Raw::sz("keep vendor default"))
                );
                assert_eq!(k.get("Unrelated")?, Some(Raw::binary(vec![0xde, 0xad])));
            }
            assert_eq!(hive.names()?, original_names);
            assert!(get(None)?.is_empty());
        }
        Ok(())
    }

    #[test]
    fn reset_includes_both_playback_formats_and_reads_no_capture_or_unrelated_selector() {
        let names = [
            "StreamFormatOut_01",
            "StreamFormatOut_02",
            "StreamFormatOut_03",
            "sTrEaMfOrMaToUt_11",
            "StreamFormatIn_01",
            "StreamOut",
            "Settings",
            "SoundDeviceProfiles",
            "StreamFormatOut_2",
            "StreamFormatOut_001",
            "StreamFormatOut_256",
            r"StreamFormatOut_01\foreign",
        ];
        let values =
            selected_playback_values(names.map(String::from).to_vec(), |name| match name {
                "StreamFormatOut_01" => Ok(Some(Raw::sz("my external name"))),
                "StreamFormatOut_02" => Ok(Some(Raw::sz("ZH3S0123456789abcdef"))),
                "StreamFormatOut_03" | "sTrEaMfOrMaToUt_11" => Ok(None),
                _ => panic!("unexpected selector read {name}"),
            })
            .unwrap();
        assert_eq!(values.len(), 2);
        assert_eq!(values["StreamFormatOut_01"], Raw::sz("my external name"));
        assert_eq!(
            values["StreamFormatOut_02"],
            Raw::sz("ZH3S0123456789abcdef")
        );
        assert!(playback_key("StreamFormatOut_100"));
        assert!(playback_key("StreamFormatOut_255"));
    }

    #[test]
    fn only_bare_physical_playback_references_are_factory_overlay_targets() {
        for name in ["pcm_out_01_c_00", "PCM_OUT_02_C_00", "pcm_out_17_c_03"] {
            assert!(bare_reference(name));
        }
        for name in [
            "pcm_out_01_c_00_named",
            "pcm_out_01_v_00",
            "pcm_in_01_c_00",
            "Topology",
            "pcm_out_02_c_00_",
            "pcm_out_2_c_00",
            "pcm_out_02_c_0",
            "pcm_out_256_c_00",
        ] {
            assert!(!bare_reference(name));
        }
        assert!(known_base_defaults("5.68.0.17543"));
        assert!(!known_base_defaults("5.68.0.99999"));
    }

    #[test]
    fn clearing_only_inactive_profiles_does_not_require_restarting_the_current_factory_output() {
        assert!(!must_reload(true, Some(true)));
        assert!(!must_reload(false, Some(true)));
        assert!(must_reload(true, Some(false)));
        assert!(must_reload(false, Some(false)));
        assert!(must_reload(true, None));
        assert!(!must_reload(false, None));
    }
}
