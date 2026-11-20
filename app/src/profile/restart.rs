//! Read-only prerequisites and diagnostics for a restart of one physical ZH3.
use super::{Device, probe};
use serde::{Deserialize, Serialize};
use windows::Win32::{
    Devices::{DeviceAndDriverInstallation::*, Properties::*},
    Foundation::ERROR_NO_MORE_FILES,
    Globalization::{CP_OEMCP, MULTI_BYTE_TO_WIDE_CHAR_FLAGS, MultiByteToWideChar},
    System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    },
};
use windows::core::{HSTRING, Owned};

#[derive(Clone, Debug, Serialize)]
pub struct RestartGate {
    pub kind: String,
    pub message: String,
    pub panel_running: bool,
}

const PANEL_HINT: &str = "检测到 Fosi 控制面板正在运行，可能阻止 ZH3 重启。请在系统托盘右键 Fosi 图标，选择「退出」(Exit)。";
const REBOOT_HINT: &str = "Windows 已将这台 ZH3 标记为需要重新启动。可先拔插 ZH3 并重新检测；若标记仍在，请保存工作后重启 Windows。";

const PANEL_IMAGE: &str = "FosiAudioUsbAudioCpl.exe";

fn process_present(matches: impl Fn(&str) -> bool) -> windows::core::Result<bool> {
    unsafe {
        let snapshot = Owned::new(CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)?);
        let mut entry = PROCESSENTRY32W {
            dwSize: size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut row = Process32FirstW(*snapshot, &mut entry);
        loop {
            if let Err(e) = row {
                return if e.code() == ERROR_NO_MORE_FILES.to_hresult() {
                    Ok(false)
                } else {
                    Err(e)
                };
            }
            let end = entry
                .szExeFile
                .iter()
                .position(|c| *c == 0)
                .unwrap_or(entry.szExeFile.len());
            if matches(&String::from_utf16_lossy(&entry.szExeFile[..end])) {
                return Ok(true);
            }
            row = Process32NextW(*snapshot, &mut entry);
        }
    }
}

fn reboot_flags(usb: &str) -> (Option<bool>, Option<bool>) {
    let mut node = 0;
    let rc =
        unsafe { CM_Locate_DevNodeW(&mut node, &HSTRING::from(usb), CM_LOCATE_DEVNODE_NORMAL) };
    if rc != CR_SUCCESS {
        return (None, None);
    }
    let mut kind = DEVPROPTYPE::default();
    let mut value = 0u8;
    let mut size = 1u32;
    let rc = unsafe {
        CM_Get_DevNode_PropertyW(
            node,
            &DEVPKEY_Device_IsRebootRequired,
            &mut kind,
            Some(&mut value),
            &mut size,
            0,
        )
    };
    let property =
        (rc == CR_SUCCESS && size == 1 && kind == DEVPROP_TYPE_BOOLEAN).then_some(value != 0);
    let mut flags = CM_DEVNODE_STATUS_FLAGS::default();
    let mut problem = CM_PROB::default();
    let rc = unsafe { CM_Get_DevNode_Status(&mut flags, &mut problem, node, 0) };
    let status = (rc == CR_SUCCESS)
        .then_some(flags.contains(DN_NEED_RESTART) || problem == CM_PROB_NEED_RESTART);
    (property, status)
}

fn gate(
    identity: Result<(), String>,
    reboot: (Option<bool>, Option<bool>),
    panel: Option<bool>,
) -> RestartGate {
    let running = panel == Some(true);
    let (kind, mut message) = if let Err(e) = identity {
        ("unavailable", format!("无法确认要重启的物理 ZH3。{e}"))
    } else if reboot.0 == Some(true) || reboot.1 == Some(true) {
        ("needs_reboot", REBOOT_HINT.into())
    } else if running {
        ("panel_open", String::new())
    } else {
        let mut detail = String::new();
        if reboot == (None, None) {
            detail.push_str("Windows 未提供该设备的待重启状态。")
        }
        if panel.is_none() {
            detail.push_str("当前无法枚举控制面板进程。")
        }
        ("ready", detail)
    };
    if running {
        message.push_str(PANEL_HINT);
        if kind == "panel_open" {
            message.push_str("退出后可重试。")
        }
    }
    RestartGate {
        kind: kind.into(),
        message,
        panel_running: running,
    }
}

/// This check never writes a receipt, changes PnP state, or acts on a process.
/// Read physical presence independently of the started flag: a devnode with
/// CM_PROB_NEED_RESTART can still uniquely match the USB ID, container and driver.
pub fn restart_gate(d: &Device) -> RestartGate {
    check_gate(d, true)
}

/// Whole-device restoration verifies the vendor-owned source independently
/// before requesting a physical restart, including on older Fosi drivers.
pub(crate) fn factory_gate(d: &Device) -> RestartGate {
    check_gate(d, false)
}

fn check_gate(d: &Device, require_profile_version: bool) -> RestartGate {
    let current = probe::connected_hardware().and_then(|h| {
        super::need(
            h.usb.eq_ignore_ascii_case(&d.usb)
                && !d.container.is_empty()
                && h.container.eq_ignore_ascii_case(&d.container)
                && h.driver == d.driver
                && (!require_profile_version || d.driver == super::SUPPORTED_DRIVER),
            "当前物理实例、容器或 Fosi 驱动与目标不匹配",
        )
    });
    let flags = if current.is_ok() {
        reboot_flags(&d.usb)
    } else {
        (None, None)
    };
    gate(current, flags, process_present(panel_name).ok())
}
fn panel_name(name: &str) -> bool {
    name.eq_ignore_ascii_case(PANEL_IMAGE)
}

fn utf16(bytes: &[u8], big_endian: bool) -> String {
    let chars = bytes
        .chunks_exact(2)
        .map(|c| {
            if big_endian {
                u16::from_be_bytes([c[0], c[1]])
            } else {
                u16::from_le_bytes([c[0], c[1]])
            }
        })
        .collect::<Vec<_>>();
    String::from_utf16_lossy(&chars)
}

/// PnPUtil versions can redirect OEM, UTF-8, or UTF-16 text to a pipe.
fn decode(bytes: &[u8], codepage: u32) -> String {
    if bytes.is_empty() {
        return String::new();
    }
    let text = if let Some(data) = bytes.strip_prefix(&[0xff, 0xfe]) {
        utf16(data, false)
    } else if let Some(data) = bytes.strip_prefix(&[0xfe, 0xff]) {
        utf16(data, true)
    } else if bytes.len() % 2 == 0
        && bytes
            .chunks_exact(2)
            .any(|p| p[0].is_ascii() && p[0] != 0 && p[1] == 0)
    {
        utf16(bytes, false)
    } else if let Ok(s) =
        std::str::from_utf8(bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes))
    {
        s.to_string()
    } else {
        let mut chars = vec![0u16; bytes.len()];
        let count = unsafe {
            MultiByteToWideChar(
                codepage,
                MULTI_BYTE_TO_WIDE_CHAR_FLAGS::default(),
                bytes,
                Some(&mut chars),
            )
        };
        if count > 0 {
            String::from_utf16_lossy(&chars[..count as usize])
        } else {
            String::from_utf8_lossy(bytes).into_owned()
        }
    };
    text.trim_matches(|c: char| c == '\0' || c.is_whitespace())
        .to_string()
}

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct NativeResult {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}
impl NativeResult {
    pub fn read(out: &std::process::Output) -> Self {
        Self {
            code: out.status.code(),
            stdout: decode(&out.stdout, CP_OEMCP),
            stderr: decode(&out.stderr, CP_OEMCP),
        }
    }
    pub fn failure(&self) -> String {
        let mut msg=match self.code{
            Some(3010)=>"Windows 要求重新启动才能完成 ZH3 的设备变更（3010）。可先拔插 ZH3 并重新检测；若仍提示待重启，请保存工作后重启 Windows。".into(),
            Some(50)=>"Windows 未执行本次 ZH3 软件重启（50）。可拔插 ZH3 后重新检测，或按 Windows 的诊断处理。".into(),
            Some(code)=>format!("ZH3 软件重启返回 {code}。可拔插设备并重新检测。"),
            None=>"ZH3 重启工具未返回退出代码。请重新检测设备。".into(),
        };
        let diagnostic = [self.stdout.as_str(), self.stderr.as_str()]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        if !diagnostic.is_empty() {
            msg.push_str(" Windows 诊断：");
            let mut chars = diagnostic.chars();
            msg.extend(chars.by_ref().take(1000));
            if chars.next().is_some() {
                msg.push('…')
            }
        }
        msg
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_the_vendor_panel_image_matches() {
        assert!(panel_name("FosiAudioUsbAudioCpl.exe"));
        assert!(panel_name("FOSIAUDIOUSBAUDIOCPL.EXE"));
        assert!(!panel_name("FosiAudioUsbAudioCpl.exe.copy"));
        assert!(!panel_name("FosiAudioUsbAudioDfu.exe"));
    }
    #[test]
    fn toolhelp_finds_this_test_executable_without_opening_any_process() {
        let path = std::env::current_exe().unwrap();
        let name = path.file_name().unwrap().to_str().unwrap();
        assert!(process_present(|image| image.eq_ignore_ascii_case(name)).unwrap());
    }
    #[test]
    fn physical_reboot_state_and_tray_notification_can_coexist() {
        for flags in [(Some(true), Some(false)), (Some(false), Some(true))] {
            let g = gate(Ok(()), flags, Some(true));
            assert_eq!(g.kind, "needs_reboot");
            assert!(g.panel_running);
            assert!(g.message.contains("托盘") && g.message.contains("重启 Windows"));
        }
        let g = gate(Ok(()), (Some(false), Some(false)), Some(true));
        assert_eq!(g.kind, "panel_open");
        assert!(g.message.contains("退出后"));
        assert_eq!(gate(Ok(()), (None, None), None).kind, "ready");
        assert_eq!(
            gate(Err("身份已变".into()), (Some(true), None), Some(true)).kind,
            "unavailable"
        );
    }
    #[test]
    fn console_output_preserves_oem_utf8_and_unicode_diagnostics() {
        let msg = "Windows 设备正在等待重新启动";
        let utf16le = msg
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>();
        assert_eq!(decode(&utf16le, CP_OEMCP), msg);
        assert_eq!(
            decode(&[&[0xff, 0xfe][..], &utf16le].concat(), CP_OEMCP),
            msg
        );
        assert_eq!(decode(msg.as_bytes(), CP_OEMCP), msg);
        assert_eq!(decode(&[0xc9, 0xe8, 0xb1, 0xb8], 936), "设备");
    }
    #[test]
    fn unsupported_restart_keeps_windows_reason_without_inventing_a_blocker() {
        let res = NativeResult {
            code: Some(50),
            stdout: "The request is not supported.".into(),
            stderr: "native detail".into(),
        };
        let msg = res.failure();
        assert!(
            msg.contains("50") && msg.contains("not supported") && msg.contains("native detail")
        );
        assert!(!msg.contains("控制面板") && !msg.contains("待重启"));
        let reboot = NativeResult {
            code: Some(3010),
            stdout: String::new(),
            stderr: String::new(),
        };
        assert!(reboot.failure().contains("3010") && reboot.failure().contains("重启 Windows"));
    }
}
