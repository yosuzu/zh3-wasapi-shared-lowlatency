use std::collections::BTreeSet;
use windows::Win32::Devices::{DeviceAndDriverInstallation::*, Properties::*};
use windows::Win32::Foundation::DEVPROPKEY;
use windows::core::{GUID, HSTRING, w};

use super::{ConnectedHardware, Device};

const USB_PREFIX: &str = "USB\\VID_152A&PID_889B\\";
const KS_PREFIX: &str = "TUSBAUDIO_ENUM\\VID_152A&PID_889B&KS\\";
const RENDER_PREFIX: &str = "SWD\\MMDEVAPI\\{0.0.0.00000000}.";
const AUDIO: GUID = GUID::from_u128(0x6994ad04_93ef_11d0_a3cc_00a0c9223196);
const RENDER: GUID = GUID::from_u128(0x65e8773e_8f56_11d0_a3b9_00a0c9223196);

fn check(code: CONFIGRET, label: &str) -> Result<(), String> {
    if code == CR_SUCCESS {
        Ok(())
    } else {
        Err(format!(
            "{label}失败（Configuration Manager 0x{:02X}）。",
            code.0
        ))
    }
}

fn strings_from_list(
    label: &str,
    size: impl Fn(&mut u32) -> CONFIGRET,
    read: impl Fn(&mut [u16]) -> CONFIGRET,
) -> Result<Vec<String>, String> {
    // The device set can grow between the size and list calls during hotplug.
    for _ in 0..3 {
        let mut len = 0;
        check(size(&mut len), label)?;
        if !(1..=1_048_576).contains(&len) {
            return Err(format!("{label}返回异常的长度 {len}。"));
        }
        let mut buf = vec![0; len as usize];
        let result = read(&mut buf);
        if result == CR_BUFFER_SMALL {
            continue;
        }
        check(result, label)?;
        return buf
            .split(|&c| c == 0)
            .take_while(|s| !s.is_empty())
            .map(|s| String::from_utf16(s).map_err(|_| format!("{label}返回无效的设备名。")))
            .collect();
    }
    Err(format!("{label}期间设备一直在变化。"))
}

fn node_id(node: u32) -> Result<String, String> {
    let mut buf = [0; MAX_DEVICE_ID_LEN as usize];
    check(
        unsafe { CM_Get_Device_IDW(node, &mut buf, 0) },
        "读取 PnP 实例 ID",
    )?;
    let end = buf
        .iter()
        .position(|&c| c == 0)
        .ok_or("PnP 实例 ID 被截断。")?;
    String::from_utf16(&buf[..end]).map_err(|_| "PnP 实例 ID 无效。".into())
}

fn locate(id: &str) -> Result<u32, String> {
    let mut node = 0;
    check(
        unsafe { CM_Locate_DevNodeW(&mut node, &HSTRING::from(id), CM_LOCATE_DEVNODE_NORMAL) },
        "定位在场设备",
    )?;
    Ok(node)
}

fn children(parent: u32, prefix: &str) -> Result<Vec<(u32, String)>, String> {
    let mut result = Vec::new();
    let mut node = 0;
    let mut status = unsafe { CM_Get_Child(&mut node, parent, 0) };
    while status != CR_NO_SUCH_DEVNODE {
        check(status, "枚举直接子设备")?;
        let id = node_id(node)?;
        if id.to_ascii_uppercase().starts_with(prefix) {
            let mut actual_parent = 0;
            check(
                unsafe { CM_Get_Parent(&mut actual_parent, node, 0) },
                "核对子设备的父节点",
            )?;
            if actual_parent != parent {
                return Err("枚举期间设备的父子关系已改变。".into());
            }
            result.push((node, id));
        }
        let current = node;
        status = unsafe { CM_Get_Sibling(&mut node, current, 0) };
    }
    Ok(result)
}

fn read_property(
    expected: DEVPROPTYPE,
    query: impl Fn(&mut DEVPROPTYPE, Option<*mut u8>, &mut u32) -> CONFIGRET,
) -> Result<Vec<u8>, String> {
    let mut kind = DEVPROPTYPE::default();
    let mut len = 0;
    let size_result = query(&mut kind, None, &mut len);
    if size_result != CR_BUFFER_SMALL {
        check(size_result, "查询 PnP 属性大小")?;
    }
    if !(1..=65536).contains(&len) {
        return Err("PnP 属性长度无效。".into());
    }
    let mut data = vec![0; len as usize];
    check(
        query(&mut kind, Some(data.as_mut_ptr()), &mut len),
        "读取 PnP 属性",
    )?;
    if kind != expected || len as usize > data.len() {
        return Err("PnP 属性的类型或长度不符。".into());
    }
    data.truncate(len as usize);
    Ok(data)
}

fn property(node: u32, key: &DEVPROPKEY, expected: DEVPROPTYPE) -> Result<Vec<u8>, String> {
    read_property(expected, |kind, buf, len| unsafe {
        CM_Get_DevNode_PropertyW(node, key, kind, buf, len, 0)
    })
}

fn decode_text(bytes: &[u8]) -> Result<String, String> {
    if bytes.len() % 2 != 0 {
        return Err("PnP 文本属性被截断。".into());
    }
    let mut units: Vec<_> = bytes
        .chunks_exact(2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .collect();
    if units.pop() != Some(0) || units.contains(&0) {
        return Err("PnP 文本属性格式无效。".into());
    }
    String::from_utf16(&units).map_err(|_| "PnP 文本属性编码无效。".into())
}

fn text_property(node: u32, key: &DEVPROPKEY) -> Result<String, String> {
    decode_text(&property(node, key, DEVPROP_TYPE_STRING)?)
}

fn container_id(node: u32) -> Result<String, String> {
    let bytes = property(node, &DEVPKEY_Device_ContainerId, DEVPROP_TYPE_GUID)?;
    if bytes.len() != size_of::<GUID>() || bytes.iter().all(|b| *b == 0) {
        return Err("设备容器 ID 缺失或无效。".into());
    }
    let guid = unsafe { bytes.as_ptr().cast::<GUID>().read_unaligned() };
    Ok(format!("{guid:?}").to_ascii_lowercase())
}

fn started(node: u32) -> Result<(), String> {
    let mut flags = CM_DEVNODE_STATUS_FLAGS::default();
    let mut problem = CM_PROB::default();
    check(
        unsafe { CM_Get_DevNode_Status(&mut flags, &mut problem, node, 0) },
        "检查设备状态",
    )?;
    if !flags.contains(DN_STARTED) || flags.contains(DN_HAS_PROBLEM) || problem.0 != 0 {
        return Err(format!("设备尚未正常启动（设备问题代码 {}）。", problem.0));
    }
    Ok(())
}

struct Physical {
    usb: String,
    ks: String,
    ks_node: u32,
    container: String,
    driver: String,
}

fn usb_instance() -> Result<(String, u32), String> {
    let flags = CM_GETIDLIST_FILTER_ENUMERATOR | CM_GETIDLIST_FILTER_PRESENT;
    let usb_ids = strings_from_list(
        "枚举在场 USB 设备",
        |n| unsafe { CM_Get_Device_ID_List_SizeW(n, w!("USB"), flags) },
        |s| unsafe { CM_Get_Device_ID_ListW(w!("USB"), s, flags) },
    )?;
    let usb = unique_usb(usb_ids)?;
    let node = locate(&usb)?;
    Ok((usb, node))
}

fn unique_usb(ids: Vec<String>) -> Result<String, String> {
    let usb_ids: Vec<_> = ids
        .into_iter()
        .filter(|s| s.to_ascii_uppercase().starts_with(USB_PREFIX))
        .collect();
    let [usb] = usb_ids.as_slice() else {
        return Err(if usb_ids.is_empty() {
            "NO_DEVICE: 未连接 ZH3，未找到对应的 USB 设备。"
        } else {
            "找到多个 ZH3，请只连接一个目标设备。"
        }
        .into());
    };
    Ok(usb.clone())
}

pub(super) fn connected_hardware() -> Result<ConnectedHardware, String> {
    let (usb, node) = usb_instance()?;
    // Driver metadata can disappear temporarily while PnP reconfigures the device.
    // Presence of the exact physical USB VID/PID still identifies the product for display.
    Ok(ConnectedHardware {
        usb,
        container: container_id(node).unwrap_or_default(),
        driver: text_property(node, &DEVPKEY_Device_DriverVersion).unwrap_or_default(),
        arrival: property(node, &DEVPKEY_Device_LastArrivalDate, DEVPROP_TYPE_FILETIME)
            .ok()
            .and_then(|v| v.try_into().ok())
            .map(u64::from_le_bytes)
            .filter(|n| *n != 0),
    })
}

fn physical() -> Result<Physical, String> {
    let (usb, usb_node) = usb_instance()?;
    started(usb_node)?;
    if !text_property(usb_node, &DEVPKEY_Device_Service)?.eq_ignore_ascii_case("FosiAudioUsbAudio")
    {
        return Err("ZH3 未使用 Fosi Audio USB 驱动。".into());
    }
    let children = children(usb_node, KS_PREFIX)?;
    let [(ks_node, ks)] = children.as_slice() else {
        return Err(format!(
            "ZH3 的直接 KS 音频子设备数量为 {}，需要恰好一个。",
            children.len()
        ));
    };
    started(*ks_node)?;
    if !text_property(*ks_node, &DEVPKEY_Device_Service)?
        .eq_ignore_ascii_case("FosiAudioUsbAudioks")
    {
        return Err("ZH3 的 KS 音频服务与预期不符。".into());
    }
    let container = container_id(usb_node)?;
    let driver = text_property(usb_node, &DEVPKEY_Device_DriverVersion)?;
    if container_id(*ks_node)? != container
        || driver.is_empty()
        || text_property(*ks_node, &DEVPKEY_Device_DriverVersion)? != driver
    {
        return Err("ZH3 的 USB 与 KS 设备容器或驱动版本不一致。".into());
    }
    Ok(Physical {
        usb,
        ks: ks.clone(),
        ks_node: *ks_node,
        container,
        driver,
    })
}

pub(super) fn physical_target(expected: &ConnectedHardware) -> Result<Device, String> {
    let physical = physical()?;
    if !physical.usb.eq_ignore_ascii_case(&expected.usb)
        || (!expected.container.is_empty()
            && !physical.container.eq_ignore_ascii_case(&expected.container))
        || (!expected.driver.is_empty() && physical.driver != expected.driver)
    {
        return Err("恢复前 ZH3 的物理身份已变化，请重新确认。".into());
    }
    Ok(Device {
        usb: physical.usb,
        ks: physical.ks,
        container: physical.container,
        driver: physical.driver,
        endpoint_id: String::new(),
        stream: String::new(),
        base_ref: String::new(),
        active_ref: String::new(),
    })
}

pub(super) fn verify_physical(device: &Device) -> Result<(), String> {
    let p = physical()?;
    if p.usb.eq_ignore_ascii_case(&device.usb)
        && p.ks.eq_ignore_ascii_case(&device.ks)
        && p.container.eq_ignore_ascii_case(&device.container)
        && p.driver == device.driver
    {
        Ok(())
    } else {
        Err("当前 ZH3 的物理实例、容器或驱动与记录不符。".into())
    }
}

struct KsReference {
    link: String,
    name: String,
}

fn references(
    ks: &str,
    class: &GUID,
    flags: CM_GET_DEVICE_INTERFACE_LIST_FLAGS,
) -> Result<Vec<KsReference>, String> {
    let ks = HSTRING::from(ks);
    let links = strings_from_list(
        "读取 KS 音频接口",
        |n| unsafe { CM_Get_Device_Interface_List_SizeW(n, class, &ks, flags) },
        |buf| unsafe { CM_Get_Device_Interface_ListW(class, &ks, buf, flags) },
    )?;
    let mut refs = Vec::new();
    for link in links {
        let path = HSTRING::from(&link);
        let bytes = read_property(DEVPROP_TYPE_STRING, |kind, buf, len| unsafe {
            CM_Get_Device_Interface_PropertyW(
                &path,
                &DEVPKEY_DeviceInterface_ReferenceString,
                kind,
                buf,
                len,
                0,
            )
        })?;
        let name = decode_text(&bytes)?;
        if name.to_ascii_lowercase().starts_with("pcm_out_") {
            refs.push(KsReference { link, name });
        }
    }
    Ok(refs)
}

fn parse_reference(reference: &str) -> Result<(String, String), String> {
    // Fosi 6.16 physical PCM uses the USB alternate setting for the first
    // numeric field and for StreamFormatOut_%02u. The channel group is separate.
    let lower = reference.to_ascii_lowercase();
    let mut parts = lower.splitn(6, '_');
    let (Some("pcm"), Some("out"), Some(stream), Some("c"), Some(group)) = (
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
    ) else {
        return Err(format!("无法识别 ZH3 播放引用 {reference}。"));
    };
    let pair = |s: &str| s.len() == 2 && s.bytes().all(|b| b.is_ascii_digit());
    if !pair(stream) || !pair(group) || parts.next() == Some("") {
        return Err(format!("ZH3 播放引用 {reference} 缺少有效的流或通道编号。"));
    }
    Ok((
        format!("StreamFormatOut_{stream}"),
        format!("pcm_out_{stream}_c_{group}"),
    ))
}

fn select_reference<'a>(
    audio: &'a [KsReference],
    render: &'a [KsReference],
    adapter_ids: &[String],
) -> Result<&'a str, String> {
    if let ([a], [r]) = (audio, render) {
        if a.name.eq_ignore_ascii_case(&r.name) {
            return Ok(&r.name);
        }
        return Err("ZH3 的在场 AUDIO 和 RENDER 引用不同。".into());
    }
    // MMDevice adapter topology IDs observed on Windows have a "{2}." wrapper
    // around the complete KS symbolic link. Match the complete enumerated link
    // in the scoped KS instance; an unfamiliar ID layout cannot authorize writes.
    let mut matched = BTreeSet::new();
    for id in adapter_ids {
        let link = id.strip_prefix("{2}.").unwrap_or(id);
        let mut entries = audio
            .iter()
            .chain(render)
            .filter(|r| link.eq_ignore_ascii_case(&r.link));
        let Some(entry) = entries.next() else {
            return Err("ZH3 端点的连接器未指向当前 KS 的已知播放接口。".into());
        };
        if entries.next().is_some() {
            return Err("ZH3 端点的连接器与多个 KS 接口重复对应。".into());
        }
        matched.insert(entry.name.to_ascii_lowercase());
    }
    if matched.len() != 1 {
        return Err("无法将 ZH3 活动端点的连接器唯一对应到在场播放引用。".into());
    }
    let name = matched.into_iter().next().unwrap();
    let mut a = audio.iter().filter(|a| a.name.eq_ignore_ascii_case(&name));
    let mut r = render.iter().filter(|r| r.name.eq_ignore_ascii_case(&name));
    match (a.next(), r.next()) {
        (Some(_), Some(target)) if a.next().is_none() && r.next().is_none() => Ok(&target.name),
        _ => Err("ZH3 端点所用引用在 AUDIO 和 RENDER 两类接口中未能一一对应。".into()),
    }
}

fn stream_refs(device: &Device, recovery: bool) -> Result<(String, String, String), String> {
    let ks = &device.ks;
    let audio = references(ks, &AUDIO, CM_GET_DEVICE_INTERFACE_LIST_PRESENT)?;
    let render = references(ks, &RENDER, CM_GET_DEVICE_INTERFACE_LIST_PRESENT)?;
    if !audio.is_empty() && !render.is_empty() {
        let adapters = if audio.len() == 1 && render.len() == 1 {
            Vec::new()
        } else {
            if device.endpoint_id.is_empty() {
                return Err("多个在场播放引用需要通过活动 ZH3 端点的连接器确认。".into());
            }
            super::audio::connected_adapter_ids(device)?
        };
        let reference = select_reference(&audio, &render, &adapters)?;
        let (stream, base) = parse_reference(reference)?;
        return Ok((stream, base, reference.to_owned()));
    }
    if !recovery || audio.len() > 1 || render.len() > 1 {
        return Err("无法确认在场 ZH3 播放引用。".into());
    }

    // Disconnected custom profiles can leave multiple suffixed links behind.
    // All of them must independently resolve to the same unambiguous stream and base in both classes.
    let bases = |class: &GUID| -> Result<BTreeSet<(String, String)>, String> {
        references(ks, class, CM_GET_DEVICE_INTERFACE_LIST_ALL_DEVICES)?
            .iter()
            .map(|r| parse_reference(&r.name))
            .collect()
    };
    let audio_bases = bases(&AUDIO)?;
    let render_bases = bases(&RENDER)?;
    if audio_bases.is_empty() && render_bases.is_empty() && audio.is_empty() && render.is_empty() {
        return Err("NO_RECOVERY_REF: ZH3 已通过物理身份核对，但当前没有可识别的播放引用。".into());
    }
    if audio_bases.len() != 1 || audio_bases != render_bases {
        return Err("已登记的 ZH3 接口不足以唯一识别恢复所需的播放流。".into());
    }
    let (stream, base) = audio_bases.into_iter().next().unwrap();
    for item in audio.iter().chain(&render) {
        let (s, b) = parse_reference(&item.name)?;
        if s != stream || b != base {
            return Err("在场引用与已登记的 ZH3 播放流不一致。".into());
        }
    }
    Ok((stream, base, String::new()))
}

pub(super) fn device(recovery: bool) -> Result<Device, String> {
    let mut device = endpoint_device(recovery)?;
    let (stream, base_ref, active_ref) = stream_refs(&device, recovery).map_err(|e| {
        if !device.endpoint_id.is_empty() && e.starts_with("NO_RECOVERY_REF:") {
            return "ZH3 端点仍在场，但其 KS 下没有可用于身份核对的播放引用。".into();
        }
        e
    })?;
    device.stream = stream;
    device.base_ref = base_ref;
    device.active_ref = active_ref;
    Ok(device)
}

pub(super) fn readonly_device() -> Result<Device, String> {
    endpoint_device(false)
}

fn endpoint_device(recovery: bool) -> Result<Device, String> {
    let p = physical()?;
    let renders = children(p.ks_node, RENDER_PREFIX)?;
    let ids: Vec<_> = renders
        .iter()
        .map(|(_, instance)| &instance[r"SWD\MMDEVAPI\".len()..])
        .collect();
    let endpoint_id = match super::audio::active_candidate(&ids)? {
        None if recovery => {
            for (node, _) in &renders {
                if container_id(*node)? != p.container {
                    return Err("未活动播放端点与 ZH3 物理设备容器不一致。".into());
                }
            }
            String::new()
        }
        Some(index) => {
            let node = renders[index].0;
            if container_id(node)? != p.container {
                return Err("播放端点与 ZH3 物理设备容器不一致。".into());
            }
            started(node)?;
            ids[index].to_owned()
        }
        None => return Err("AUDIO_UNAVAILABLE: ZH3 尚未出现活动播放端点。".into()),
    };
    Ok(Device {
        usb: p.usb,
        ks: p.ks,
        container: p.container,
        driver: p.driver,
        endpoint_id,
        stream: String::new(),
        base_ref: String::new(),
        active_ref: String::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn physical_identity_is_discovered_per_host_and_excludes_usb_function_nodes() {
        let host_one = r"USB\VID_152A&PID_889B\fixture-host-one";
        let host_two = r"usb\vid_152a&pid_889b\unrelated-instance-on-another-pc";
        let function = r"USB\VID_152A&PID_889B&MI_00\fixture-host-one";
        for physical in [host_one, host_two] {
            assert_eq!(
                unique_usb(vec![function.into(), physical.into()]).unwrap(),
                physical
            );
        }
        assert!(
            unique_usb(vec![function.into()])
                .unwrap_err()
                .starts_with("NO_DEVICE:")
        );
        assert!(unique_usb(vec![host_one.into(), host_two.into()]).is_err());
    }

    #[test]
    fn an_endpoint_selects_its_own_alternate_setting_among_present_pcm_interfaces() {
        let entries = |class: &str| {
            ["pcm_out_02_c_00", "pcm_out_01_c_00"].map(|name| KsReference {
                name: name.into(),
                link: format!(
                    r"\\?\TUSBAUDIO_ENUM#VID_152A&PID_889B&KS#another-port#{class}\{name}"
                ),
            })
        };
        let audio = entries("{6994ad04-93ef-11d0-a3cc-00a0c9223196}");
        let render = entries("{65e8773e-8f56-11d0-a3b9-00a0c9223196}");
        let connected = format!("{{2}}.{}", audio[1].link.to_ascii_lowercase());
        let selected = select_reference(&audio, &render, std::slice::from_ref(&connected)).unwrap();
        assert_eq!(
            parse_reference(selected).unwrap(),
            ("StreamFormatOut_01".into(), "pcm_out_01_c_00".into())
        );
        assert!(select_reference(&audio, &render, &[]).is_err());
        assert!(
            select_reference(&audio, &render, &[connected.clone(), audio[0].link.clone()]).is_err()
        );
        assert!(
            select_reference(
                &audio,
                &render,
                &[connected.replace("another-port", "other-zh3")]
            )
            .is_err()
        );
        assert!(select_reference(&audio, &render[..1], &[connected]).is_err());
    }

    #[test]
    #[ignore = "requires a connected physical ZH3, no audio stream is opened"]
    fn hardware_and_display_only_snapshot_follow_the_live_endpoint_connector() {
        let hardware = crate::probe::connected_hardware().unwrap();
        let display = crate::probe::readonly_snapshot().unwrap();
        let target = crate::probe::identify().unwrap();
        assert_eq!(hardware.usb, target.usb);
        assert_eq!(hardware.container, display.device.container);
        assert_eq!(display.device.endpoint_id, target.endpoint_id);
        assert!(
            display.device.base_ref.is_empty()
                && display.device.active_ref.is_empty()
                && display.device.stream.is_empty()
        );

        let audio = references(&target.ks, &AUDIO, CM_GET_DEVICE_INTERFACE_LIST_PRESENT).unwrap();
        let paths = super::super::audio::connected_adapter_ids(&target).unwrap();
        let matches = paths.iter().any(|p| {
            audio.iter().any(|r| {
                p.strip_prefix("{2}.")
                    .unwrap_or(p)
                    .eq_ignore_ascii_case(&r.link)
                    && r.name.eq_ignore_ascii_case(&target.active_ref)
            })
        });
        assert!(
            matches,
            "endpoint connector {paths:?} did not identify {}",
            target.active_ref
        );
        println!(
            "USB={} container={} endpoint={} adapters={paths:?} stream={} mix={}Hz output={:?} current={:?} @ {:?}Hz",
            hardware.usb,
            hardware.container,
            target.endpoint_id,
            target.stream,
            display.mix.rate,
            display.output,
            display.current,
            display.current_rate
        );
    }

    #[test]
    fn original_and_named_live_links_resolve_to_the_same_source_stream() {
        for evidence in [
            include_str!("../../tests/fixtures/factory-interfaces.json"),
            include_str!("../../tests/fixtures/named-interfaces.json"),
        ] {
            let report: Value = serde_json::from_str(evidence).unwrap();
            for class in ["AUDIO", "RENDER"] {
                let links = report["Classes"][class]["Present"]["Links"]
                    .as_array()
                    .unwrap();
                assert_eq!(links.len(), 1);
                let (_, reference) = links[0].as_str().unwrap().rsplit_once('\\').unwrap();
                assert_eq!(
                    parse_reference(reference).unwrap(),
                    ("StreamFormatOut_02".into(), "pcm_out_02_c_00".into())
                );
            }
        }
        assert_eq!(
            parse_reference("pcm_out_17_c_00_next").unwrap().0,
            "StreamFormatOut_17"
        );
        assert!(parse_reference("pcm_out_02_c_0_bad").is_err());
        assert!(parse_reference("pcm_out_02_c_00_").is_err());
    }
}
