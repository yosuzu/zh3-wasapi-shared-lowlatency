//! Fosi playback profiles. All resource names are derived from a verified device
//! and a system-generated session ID. A selector never supplies a registry path.
pub mod factory;
pub(crate) mod factory_state;
mod interface;
mod registry;
mod restart;
mod store;
use super::probe::{self, Device, Snapshot};
use interface::{Classes, Interface};
use registry::{Raw, Tx};
pub use restart::{RestartGate, restart_gate};
use serde::{Deserialize, Serialize};
use std::{io, os::windows::process::CommandExt, path::PathBuf, process::Command};
type Result<T> = std::result::Result<T, String>;
pub const SUPPORTED_DRIVER: &str = "6.16.0.15392";
const SELECT: &str = "CurrentSoundDeviceProfile";
const OEM: &str = "{E4870E26-3CC5-4CD2-BA46-CA0A9A70ED04},6";
const USE_DEFAULT: &str = "{E4870E26-3CC5-4CD2-BA46-CA0A9A70ED04},7";
fn check(ok: bool, op: &str) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(format!("{op}：{}", io::Error::last_os_error()))
    }
}
fn need(ok: bool, msg: &str) -> Result<()> {
    if ok { Ok(()) } else { Err(msg.into()) }
}
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}
fn same_device(a: &Device, b: &Device) -> bool {
    [
        &a.usb,
        &a.ks,
        &a.container,
        &a.driver,
        &a.stream,
        &a.base_ref,
    ]
    .into_iter()
    .zip([
        &b.usb,
        &b.ks,
        &b.container,
        &b.driver,
        &b.stream,
        &b.base_ref,
    ])
    .all(|(x, y)| x.eq_ignore_ascii_case(y))
}
fn supported(d: &Device) -> Result<()> {
    need(
        d.driver == SUPPORTED_DRIVER,
        "当前驱动版本没有对应的已验证写入路径",
    )?;
    let n = d
        .stream
        .strip_prefix("StreamFormatOut_")
        .ok_or("播放流名称格式错误")?;
    need(
        n.len() == 2 && n.bytes().all(|b| b.is_ascii_digit()),
        "播放流编号无效",
    )?;
    let group = d
        .base_ref
        .strip_prefix(&format!("pcm_out_{n}_c_"))
        .ok_or("播放引用与流不符")?;
    need(
        group.len() == 2 && group.bytes().all(|b| b.is_ascii_digit()),
        "播放引用通道编号无效",
    )
}
fn root(d: &Device) -> String {
    format!(r"SYSTEM\CurrentControlSet\Enum\{}\Device Parameters", d.usb)
}
fn path(d: &Device, p: &str) -> String {
    format!(r"{}\{}", root(d), p)
}
fn selector_path(d: &Device) -> String {
    path(d, &format!(r"Config\{}", d.stream))
}
fn selector(d: &Device, tx: Option<&Tx>) -> Result<Option<Raw>> {
    registry::read(&selector_path(d), SELECT, tx)
}
fn packet(half: u8) -> Vec<u8> {
    let mut b = vec![0; 40];
    b[..4].copy_from_slice(&(u32::from(half) * 5000).to_le_bytes());
    b
}
fn oem(half: u8) -> Raw {
    let mut b = vec![65, 0, 0, 0, 8, 0, 0, 0];
    b.extend((u64::from(half) * 5000).to_le_bytes());
    Raw::binary(b)
}
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Mode {
    Fixed,
    Range,
}
impl Mode {
    fn parse(s: &str) -> Result<Self> {
        match s {
            "range" => Ok(Self::Range),
            "fixed" => Ok(Self::Fixed),
            _ => Err("无效的配置模式".into()),
        }
    }
    fn label(self) -> &'static str {
        match self {
            Self::Fixed => "fixed",
            Self::Range => "range",
        }
    }
}
#[derive(Serialize, Deserialize, PartialEq, Eq, Debug, Clone, Copy)]
enum Phase {
    Preparing,
    Creating,
    Registered,
    Setting,
    Source,
    Waiting,
    Restoring,
    Factory,
    Closed,
}
#[derive(Serialize, Deserialize)]
struct Generation {
    id: String,
    mode: Mode,
    half: u8,
    before_absent_keys: Vec<String>,
    registered: Option<String>,
    initial_interface: Option<interface::Metadata>,
    committed: bool,
}
impl Generation {
    fn profile(&self) -> String {
        format!("ZH3S{}", &self.id[..16])
    }
    fn entry(&self) -> String {
        format!("Z{}", &self.id[16..])
    }
    fn reference(&self, d: &Device) -> String {
        format!("{}_{}", d.base_ref, self.entry())
    }
    fn folder(&self) -> String {
        format!(r"Config\SoundDeviceProfiles\{}", self.profile())
    }
    fn values(&self) -> Vec<(String, &'static str, Raw)> {
        let e = format!(r"{}\{}", self.folder(), self.entry());
        let mut v = vec![
            (e.clone(), "Type", Raw::dword(0)),
            (format!(r"{e}\Loc_00"), "ChannelIndex", Raw::dword(0)),
            (format!(r"{e}\Loc_01"), "ChannelIndex", Raw::dword(1)),
        ];
        if self.mode == Mode::Fixed {
            v.push((format!(r"{e}\ks\EP\0"), OEM, oem(self.half)))
        }
        v
    }
    fn keys(&self, d: &Device) -> Vec<String> {
        let mut keys = vec![];
        for p in self
            .values()
            .into_iter()
            .map(|v| v.0)
            .chain([format!(r"Config\{}", d.stream)])
        {
            let mut acc = String::new();
            for component in p.split('\\') {
                if !acc.is_empty() {
                    acc.push('\\')
                }
                acc.push_str(component);
                if !keys.contains(&acc) {
                    keys.push(acc.clone())
                }
            }
        }
        keys
    }
}
#[derive(Serialize, Deserialize)]
struct Receipt {
    version: u32,
    device: Device,
    initial_selector: Option<Raw>,
    initial_endpoint: Option<InitialEndpoint>,
    phase: Phase,
    generations: Vec<Generation>,
    restart_note: String,
    base_packet: Option<BasePacket>,
    restart_completed: bool,
    #[serde(default)]
    normal_restart_checked: bool,
    #[serde(default)]
    port_attempt: Option<PortAttempt>,
    #[serde(default)]
    legacy: store::Legacy,
}
#[derive(Debug, Serialize, Deserialize)]
struct PortAttempt {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    port: Option<u32>,
}
// Old receipts stored the whole Snapshot here. Keep the two fields used to
// explain a changed mix format after restoration, with their original keys.
#[derive(Serialize, Deserialize)]
struct InitialEndpoint {
    device: InitialReference,
    mix: InitialMix,
}
#[derive(Serialize, Deserialize)]
struct InitialReference {
    active_ref: String,
}
#[derive(Serialize, Deserialize)]
struct InitialMix {
    raw_hex: String,
}
#[derive(Serialize, Deserialize)]
struct BasePacket {
    audio: String,
    render: String,
    kind: u32,
    before: Vec<u8>,
    usb_arrival: u64,
}
impl Receipt {
    fn init(d: &Device, s: Option<&Snapshot>, before: Option<Raw>) -> Self {
        Self {
            version: 1,
            device: d.clone(),
            initial_selector: before,
            initial_endpoint: s.map(|s| InitialEndpoint {
                device: InitialReference {
                    active_ref: s.device.active_ref.clone(),
                },
                mix: InitialMix {
                    raw_hex: s.mix.raw_hex.clone(),
                },
            }),
            phase: Phase::Preparing,
            generations: vec![],
            restart_note: String::new(),
            base_packet: None,
            restart_completed: false,
            normal_restart_checked: false,
            port_attempt: None,
            legacy: store::Legacy::None,
        }
    }
    fn save(&mut self, phase: Phase) -> Result<()> {
        self.phase = phase;
        store::commit(self, Tx::start()?).map(|_| ())
    }
    fn latest(&self) -> Result<&Generation> {
        self.generations.last().ok_or("配置回执缺少写入记录".into())
    }
    fn reset_restart(&mut self) {
        self.restart_note.clear();
        self.restart_completed = false;
        self.normal_restart_checked = false;
        self.port_attempt = None;
    }
    fn can_reconnect(&self) -> bool {
        // 0.1.2 kept the returned PnP status only in its authenticated note.
        let legacy_return = self.restart_note.starts_with("ZH3 软件重启返回 ")
            || self.restart_note.starts_with("ZH3 软件重启失败，")
            || (self.restart_completed && self.restart_note.starts_with("已请求定向重启，"));
        matches!(self.phase, Phase::Waiting | Phase::Factory)
            && self.port_attempt.is_none()
            && (self.normal_restart_checked || legacy_return)
    }
    fn confirm_reconnect(&self, submitted: &str, current: &str) -> Result<()> {
        need(
            submitted == current,
            "本次 USB 重连确认已过期，请重新核对当前设备和操作",
        )?;
        need(
            self.can_reconnect(),
            "当前操作没有可用的 USB 后备重连，或已发送过本次请求",
        )
    }
    fn published(&self) -> Result<&Generation> {
        let g = self.latest()?;
        need(
            self.phase == Phase::Waiting && g.committed,
            "选档事务尚未提交",
        )?;
        Ok(g)
    }
}
#[derive(Debug, Serialize)]
pub struct ProfileStatus {
    pub mode: String,
    pub half_ms: Option<u8>,
    pub phase: String,
    pub message: String,
    pub can_write: bool,
    pub can_restore: bool,
    pub retry_context: Option<String>,
    pub reconnect_context: Option<String>,
}
fn state(
    mode: &str,
    half: Option<u8>,
    phase: &str,
    msg: impl Into<String>,
    write: bool,
    restore: bool,
) -> ProfileStatus {
    ProfileStatus {
        mode: mode.into(),
        half_ms: half,
        phase: phase.into(),
        message: msg.into(),
        can_write: write,
        can_restore: restore,
        retry_context: None,
        reconnect_context: None,
    }
}
// The variable minimum rounds up. The fixed OEM conversion in the archived
// arithmetic model rounds to the nearest frame, with positive ties rounding up.
fn valid_frames(mode: Option<Mode>, half: u8, rate: u32, frames: [u32; 4]) -> bool {
    if rate == 0 {
        return false;
    }
    let numerator = u64::from(rate) * u64::from(half);
    let nearest = (numerator + 1000) / 2000;
    let high = numerator.div_ceil(2000);
    let default = (u64::from(rate) + 50) / 100;
    let [def, step, min, max] = frames.map(u64::from);
    match mode {
        None => [def, step, min, max] == [default; 4],
        Some(Mode::Fixed) => nearest > 0 && [def, step, min, max] == [nearest; 4],
        Some(Mode::Range) => [def, step, min, max] == [default, 1, high, default],
    }
}
fn periods(s: &Snapshot) -> [u32; 4] {
    [s.default_frames, s.step_frames, s.min_frames, s.max_frames]
}
fn endpoint_matches(s: &Snapshot, g: Option<&Generation>, classes: &Classes) -> Result<bool> {
    Ok(endpoint_verdict(s, g, classes)? == EndpointVerdict::Matched)
}
#[derive(PartialEq, Eq)]
enum EndpointVerdict {
    Matched,
    DifferentReference,
    DifferentProperties,
    DifferentPeriod,
}
fn endpoint_verdict(
    s: &Snapshot,
    g: Option<&Generation>,
    classes: &Classes,
) -> Result<EndpointVerdict> {
    let d = &s.device;
    let r = g
        .map(|x| x.reference(d))
        .unwrap_or_else(|| d.base_ref.clone());
    if !d.active_ref.eq_ignore_ascii_case(&r) || !classes.active(&r)? {
        return Ok(EndpointVerdict::DifferentReference);
    }
    let matches = valid_frames(
        g.map(|x| x.mode),
        g.map_or(20, |x| x.half),
        s.mix.rate,
        periods(s),
    );
    let a = Classes::find(&classes.present_audio, &r)?.ok_or("活动 AUDIO 接口缺失")?;
    let b = Classes::find(&classes.present_render, &r)?.ok_or("活动 RENDER 接口缺失")?;
    let expected_packet = g.filter(|x| x.mode == Mode::Range).map(|x| packet(x.half));
    let audio = Interface::open(&d.ks, &r, a)?;
    if audio.packet()? != expected_packet {
        return Ok(EndpointVerdict::DifferentProperties);
    }
    if g.is_none() {
        let render = Interface::render(&d.ks, &r, b)?;
        if !audio.legacy_packet_absent()?
            || !render.legacy_packet_absent()?
            || render.packet()?.is_some()
        {
            return Ok(EndpointVerdict::DifferentProperties);
        }
    }
    let expected_oem = g.filter(|x| x.mode == Mode::Fixed).map(|x| oem(x.half));
    for p in [a, b] {
        let (observed, flag) = interface::oem(p)?;
        if observed != expected_oem || flag.is_some() {
            return Ok(EndpointVerdict::DifferentProperties);
        }
    }
    let mm = s
        .endpoint_period_properties
        .as_ref()
        .ok_or("活动端点的 OEM 周期属性读取失败")?;
    let expected_blob = expected_oem.as_ref().map(|v| &v.bytes[8..]);
    if mm.0.as_deref() != expected_blob || mm.1 {
        return Ok(EndpointVerdict::DifferentProperties);
    }
    Ok(if matches {
        EndpointVerdict::Matched
    } else {
        EndpointVerdict::DifferentPeriod
    })
}
// Reads every other period-bearing input before allowing a one-property reset
// on an existing factory reference. All three restored range trials record the
// factory value of Constraints2 as absent on both interface classes.
fn base_links<'a>(
    d: &Device,
    c: &'a Classes,
    mm: &(Option<Vec<u8>>, bool),
) -> Result<(&'a str, &'a str, Interface)> {
    need(
        d.active_ref.eq_ignore_ascii_case(&d.base_ref) && c.active(&d.base_ref)?,
        "活动引用尚未回到原厂 AUDIO 和 RENDER",
    )?;
    need(
        mm.0.is_none() && !mm.1,
        "活动 MMDevice 的 OEM 或默认周期标记被修改，请核对端点配置",
    )?;
    let a = Classes::find(&c.present_audio, &d.base_ref)?.ok_or("基础 AUDIO 缺失")?;
    let b = Classes::find(&c.present_render, &d.base_ref)?.ok_or("基础 RENDER 缺失")?;
    for (label, path) in [("AUDIO", a), ("RENDER", b)] {
        need(
            interface::oem(path)? == (None, None),
            &format!("基础 {label} 的 OEM 或默认周期标记被修改，超出单项包长恢复范围"),
        )?;
    }
    let audio = Interface::open(&d.ks, &d.base_ref, a)?;
    let render = Interface::render(&d.ks, &d.base_ref, b)?;
    need(
        audio.legacy_packet_absent()? && render.legacy_packet_absent()?,
        "基础引用还有旧版包长属性，超出单项恢复范围",
    )?;
    need(
        render.packet()?.is_none(),
        "基础 RENDER 也有包长声明，超出单项 AUDIO 恢复范围",
    )?;
    Ok((a, b, audio))
}
fn observe_base(s: &Snapshot, c: &Classes) -> Result<BasePacket> {
    let (a, b, audio) = base_links(
        &s.device,
        c,
        s.endpoint_period_properties
            .as_ref()
            .ok_or("活动端点的 OEM 周期读取失败")?,
    )?;
    let before = audio
        .packet()?
        .ok_or("基础包长和 OEM 均为原厂，当前引擎周期仍与原厂记录不同")?;
    Ok(BasePacket {
        audio: a.into(),
        render: b.into(),
        kind: 0x1003,
        before,
        usb_arrival: interface::arrival(&s.device.usb)?,
    })
}
fn reappeared(r: &Receipt) -> Result<bool> {
    match &r.base_packet {
        Some(b) if !r.restart_completed => Ok(interface::arrival(&r.device.usb)? != b.usb_arrival),
        _ => Ok(true),
    }
}
fn describe(s: &Snapshot, rec: Option<&Receipt>) -> Result<ProfileStatus> {
    let d = &s.device;
    if let Err(e) = supported(d) {
        return Ok(state("unknown", None, "error", e, false, false));
    }
    let classes = Classes::load(&d.ks)?;
    let chosen = selector(d, None)?;
    let stereo = s.mix.channels == 2;
    let Some(r) = rec else {
        if chosen.is_none() && endpoint_matches(s, None, &classes)? {
            return Ok(state(
                "factory",
                None,
                "active",
                "原厂配置已由活动播放端点确认",
                stereo,
                false,
            ));
        }
        if chosen.is_none() && d.active_ref.eq_ignore_ascii_case(&d.base_ref) {
            return Ok(match observe_base(s, &classes) {
                Ok(_) => state(
                    "custom",
                    None,
                    "active",
                    "原厂 AUDIO 上有单独的外部包长声明，可恢复原厂",
                    false,
                    true,
                ),
                Err(msg) => state("unknown", None, "error", msg, false, false),
            });
        }
        let valid = chosen.as_ref().is_some_and(|x| x.string().is_ok());
        return Ok(state(
            "custom",
            None,
            "active",
            "当前配置与原厂基线不同。恢复原厂并读回后可选择模式",
            false,
            valid,
        ));
    };
    let latest = r.generations.last();
    let (mode, half) = if r.phase == Phase::Factory {
        ("factory", None)
    } else {
        (
            latest.map_or("custom", |x| x.mode.label()),
            latest.map(|x| x.half),
        )
    };
    match r.phase {
        Phase::Waiting => {
            let g = r.published()?;
            if chosen != Some(Raw::sz(&g.profile())) {
                return Ok(state(
                    mode,
                    half,
                    "error",
                    "配置选择已被其他程序更改",
                    false,
                    true,
                ));
            }
            match endpoint_verdict(s, Some(g), &classes)? {
                EndpointVerdict::Matched => {
                    return Ok(state(
                        mode,
                        half,
                        "active",
                        "已从活动播放端点校验配置",
                        stereo,
                        true,
                    ));
                }
                EndpointVerdict::DifferentPeriod => {
                    return Ok(state(
                        mode,
                        half,
                        "mismatch",
                        format!(
                            "当前 {} Hz 的选档和周期属性已生效，引擎公布的默认 / 步长 / 最小 / 最大为 {:?} 帧，与请求不符。可调整本模式或恢复原厂",
                            s.mix.rate,
                            periods(s)
                        ),
                        stereo,
                        true,
                    ));
                }
                EndpointVerdict::DifferentProperties => {
                    return Ok(state(
                        mode,
                        half,
                        "error",
                        "当前流的具名端点已活动，但实读包长或 OEM 属性与写入不同。可恢复该播放流的原厂配置",
                        false,
                        true,
                    ));
                }
                EndpointVerdict::DifferentReference => {}
            }
        }
        Phase::Factory => {
            if chosen.is_some() {
                return Ok(state(
                    mode,
                    half,
                    "error",
                    "原厂选择在恢复后发生变化",
                    false,
                    true,
                ));
            }
            if !reappeared(r)? {
                return Ok(state(
                    mode,
                    half,
                    "pending",
                    if r.restart_note.is_empty() {
                        "已清除基础 AUDIO 的包长，等待 ZH3 重新枚举。请拔插设备后刷新"
                    } else {
                        &r.restart_note
                    },
                    false,
                    true,
                ));
            }
            if endpoint_matches(s, None, &classes)? {
                return Ok(state(
                    mode,
                    half,
                    "pending",
                    "已读回原厂，正在核对本次创建的资源",
                    false,
                    true,
                ));
            }
            if d.active_ref.eq_ignore_ascii_case(&d.base_ref) {
                return Ok(state(
                    mode,
                    half,
                    "error",
                    "原厂引用已活动，但周期或 OEM 属性与原厂记录不符。请核对外部配置并刷新",
                    false,
                    false,
                ));
            }
        }
        _ => {
            return Ok(state(
                mode,
                half,
                "error",
                "上一次写入尚未完成，可继续恢复原厂",
                false,
                true,
            ));
        }
    }
    let msg = if r.restart_note.is_empty() {
        "等待 ZH3 重新枚举并读回活动端点；必要时拔插设备"
    } else {
        &r.restart_note
    };
    Ok(state(mode, half, "pending", msg, false, true))
}
fn session_status(s: &Snapshot, session: &store::Session) -> Result<ProfileStatus> {
    let mut st = describe(s, session.receipt.as_ref())?;
    if st.phase == "pending" {
        st.retry_context = session.context.clone();
        if session.receipt.as_ref().is_some_and(Receipt::can_reconnect) {
            st.reconnect_context = session.context.clone();
        }
    }
    if st.mode == "factory"
        && st.phase == "active"
        && matches!(session.legacy, store::Legacy::Separate(_))
    {
        st.message = format!(
            "当前播放流 {} 的原厂配置已确认，其他播放流的旧配置单独保留",
            s.device.stream
        );
    }
    Ok(st)
}
pub fn status(s: &Snapshot) -> Result<ProfileStatus> {
    if supported(&s.device).is_err() {
        return describe(s, None);
    }
    match store::load(&s.device, Some(s)) {
        Ok(session) => session_status(s, &session),
        Err(e) => Ok(state(
            "unknown",
            None,
            "error",
            format!("已读到当前播放端点。{e}"),
            false,
            false,
        )),
    }
}
fn fresh_target(s: &Snapshot) -> Result<()> {
    supported(&s.device)?;
    let live = probe::read()?;
    need(
        same_target_format(s, &live),
        "活动播放流、端点或音频格式已变化，请刷新后重新确认",
    )
}
fn same_target_format(a: &Snapshot, b: &Snapshot) -> bool {
    a.device == b.device && a.mix == b.mix && a.output == b.output
}

fn generated_id(name: &str, prefix: &str) -> bool {
    name.get(..prefix.len())
        .is_some_and(|s| s.eq_ignore_ascii_case(prefix))
        && name.len() == prefix.len() + 16
        && name[prefix.len()..].bytes().all(|c| c.is_ascii_hexdigit())
}
fn generated_ref(link: &str) -> Option<(&str, &str)> {
    let name = link.rsplit('\\').next()?;
    let (base, entry) = name.rsplit_once('_')?;
    let fields: Vec<_> = base.split('_').collect();
    if let [pcm, out, n, c, group] = &fields[..] {
        let pair = |s: &str| s.len() == 2 && s.bytes().all(|c| c.is_ascii_digit());
        if pcm.eq_ignore_ascii_case("pcm")
            && out.eq_ignore_ascii_case("out")
            && c.eq_ignore_ascii_case("c")
            && pair(n)
            && pair(group)
            && generated_id(entry, "Z")
        {
            return Some((n, entry));
        }
    }
    None
}
fn distinct_stream_evidence(number: &str, classes: &Classes, entries: &[String]) -> bool {
    let registered = [
        &classes.audio,
        &classes.render,
        &classes.present_audio,
        &classes.present_render,
    ];
    if registered
        .iter()
        .flat_map(|list| list.iter())
        .filter_map(|link| generated_ref(link))
        .any(|(n, _)| n == number)
    {
        return false;
    }
    classes
        .audio
        .iter()
        .filter_map(|link| generated_ref(link))
        .any(|(other, suffix)| {
            entries
                .iter()
                .any(|entry| entry.eq_ignore_ascii_case(suffix))
                && classes
                    .render
                    .iter()
                    .filter_map(|p| generated_ref(p))
                    .any(|(n, e)| n == other && e.eq_ignore_ascii_case(suffix))
        })
}
/// Missing old journal: prove the current selector is untouched and positively
/// tie a surviving named profile to a different registered stream on this KS.
/// A corrupt digest or an unreadable ACL never reaches this fallback.
fn confirm_unused_stream(s: &Snapshot) -> Result<()> {
    let d = &s.device;
    let classes = Classes::load(&d.ks)?;
    let blocked = "旧版 ZH3-Data 缺失，无法排除本播放流属于未完成的操作。请将原回执文件放回 EXE 同级的 ZH3-Data";
    need(
        registry::open(&selector_path(d), 1, None)?.is_none()
            && endpoint_matches(s, None, &classes)?,
        blocked,
    )?;
    let number = d
        .stream
        .strip_prefix("StreamFormatOut_")
        .ok_or("播放流无有效编号")?;
    let profiles = path(d, r"Config\SoundDeviceProfiles");
    let root = registry::open(&profiles, registry::READ, None)?.ok_or(blocked)?;
    let mut entries = vec![];
    for name in root
        .subkeys()?
        .into_iter()
        .filter(|n| generated_id(n, "ZH3S"))
    {
        let Some(folder) = registry::open(&format!(r"{profiles}\{name}"), registry::READ, None)?
        else {
            continue;
        };
        entries.extend(
            folder
                .subkeys()?
                .into_iter()
                .filter(|n| generated_id(n, "Z")),
        );
    }
    need(
        distinct_stream_evidence(number, &classes, &entries),
        blocked,
    )
}
fn apply_source(d: &Device, r: &mut Receipt, before_selector: Option<Raw>) -> Result<String> {
    let tx = Tx::start()?;
    let g = r.latest()?;
    need(
        registry::open(&root(d), registry::READ, Some(&tx))?.is_some(),
        "物理设备配置根已消失",
    )?;
    need(
        selector(d, Some(&tx))? == before_selector,
        "选档已被其他程序更改",
    )?;
    for key in g.keys(d) {
        let full = path(d, &key);
        if g.before_absent_keys.contains(&key) {
            let (_, created) = registry::create(&full, 1, &tx)?;
            need(created, "目标注册表键已被占用")?;
        } else {
            need(
                registry::open(&full, 1, Some(&tx))?.is_some(),
                "预先存在的配置键已消失",
            )?
        }
    }
    for (key, name, val) in g.values() {
        let handle =
            registry::open(&path(d, &key), 1 | registry::SET, Some(&tx))?.ok_or("源配置键缺失")?;
        need(handle.get(name)?.is_none(), "新配置的源值不再为空")?;
        handle.put(name, &val)?;
        need(handle.get(name)? == Some(val), "源值写入校验失败")?;
    }
    let key = registry::open(&selector_path(d), 1 | registry::SET, Some(&tx))?
        .ok_or("播放流配置键缺失")?;
    let desired = Raw::sz(&g.profile());
    key.put(SELECT, &desired)?;
    need(
        key.get(SELECT)? == Some(desired.clone()),
        "选档写入校验失败",
    )?;
    r.generations.last_mut().unwrap().committed = true;
    r.phase = Phase::Waiting;
    let revision = store::commit(r, tx)?;
    need(selector(d, None)? == Some(desired), "提交后选档发生变化")?;
    for (key, name, v) in r.latest()?.values() {
        need(
            registry::read(&path(d, &key), name, None)? == Some(v),
            "提交后源配置读回失败",
        )?
    }
    Ok(revision)
}
pub fn apply(s: &Snapshot, mode: &str, half_ms: u8) -> Result<String> {
    let _lock = store::Lock::acquire()?;
    let mode = Mode::parse(mode)?;
    need((1..=20).contains(&half_ms), "可提交 0.5 至 10 ms")?;
    need(s.mix.channels == 2, "此配置仅适用于双声道播放流")?;
    fresh_target(s)?;
    let d = &s.device;
    factory_state::require_no_pending(&d.usb)?;
    let session = store::load(d, Some(s))?;
    let previous = session.receipt;
    let st = describe(s, previous.as_ref())?;
    need(
        st.can_write && (st.mode == "factory" || st.mode == mode.label()),
        "须先恢复原厂并从活动端点确认，才能启用此模式",
    )?;
    let chosen = selector(d, None)?;
    need(
        chosen == previous.as_ref().and_then(expected_selector),
        "配置检查后选档发生变化",
    )?;
    let mut r = previous.unwrap_or_else(|| Receipt::init(d, Some(s), chosen.clone()));
    r.legacy = session.legacy;
    // Repeated changes in the same route get fresh names; rollback owns every
    // generation, including the keys shared by those generations.
    let classes = Classes::load(&d.ks)?;
    let mut g = Generation {
        id: store::id()?,
        mode,
        half: half_ms,
        before_absent_keys: vec![],
        registered: None,
        initial_interface: None,
        committed: false,
    };
    let reference = g.reference(d);
    need(
        classes.vacant(&reference)?,
        "随机生成的设备接口名已占用，请重试",
    )?;
    need(
        registry::open(&path(d, &g.folder()), 1, None)?.is_none(),
        "随机生成的源档名已占用，请重试",
    )?;
    for k in g.keys(d) {
        if registry::open(&path(d, &k), 1, None)?.is_none() {
            g.before_absent_keys.push(k)
        }
    }
    r.generations.push(g);
    r.reset_restart();
    if mode == Mode::Range {
        r.save(Phase::Creating)?;
        need(
            Classes::load(&d.ks)?.vacant(&reference)?,
            "新接口名已被其他程序使用",
        )?;
        interface::create(&d.ks, &reference)?;
        let c = Classes::load(&d.ks)?;
        let link = Classes::find(&c.audio, &reference)?.ok_or("新 AUDIO 注册后缺失")?;
        need(
            Classes::find(&c.present_audio, &reference)?.is_none()
                && Classes::find(&c.render, &reference)?.is_none(),
            "新引用已经启用或被其他接口占用",
        )?;
        let iface = Interface::open(&d.ks, &reference, link)?;
        need(iface.packet()?.is_none(), "新接口已有其他包长声明")?;
        r.generations.last_mut().unwrap().registered = Some(link.to_string());
        r.save(Phase::Registered)?;
        r.generations.last_mut().unwrap().initial_interface = Some(iface.metadata()?);
        r.save(Phase::Setting)?;
        iface.set_packet(Some(&packet(half_ms)))?;
    }
    r.save(Phase::Source)?;
    fresh_target(s)?;
    let c = Classes::load(&d.ks)?;
    if mode == Mode::Range {
        let link = registered(d, r.latest()?, &c)?.ok_or("预注册 AUDIO 缺失")?;
        need(
            Classes::find(&c.present_audio, &reference)?.is_none()
                && Classes::find(&c.render, &reference)?.is_none(),
            "选档前新引用状态发生变化",
        )?;
        need(
            Interface::open(&d.ks, &reference, link)?.packet()? == Some(packet(half_ms)),
            "选档前包长双读与写入不符",
        )?;
    } else {
        need(c.vacant(&reference)?, "选档前待用引用已被其他程序占用")?
    }
    apply_source(d, &mut r, chosen)
}
fn expected_selector(r: &Receipt) -> Option<Raw> {
    r.generations
        .iter()
        .rev()
        .find(|g| g.committed)
        .map(|g| Raw::sz(&g.profile()))
        .or_else(|| r.initial_selector.clone())
}
/// Factory selection is represented by a missing value. The pre-write selector
/// limits which existing value may be removed, including on a crash retry.
fn remove_selector_for_factory(r: &Receipt, actual: &Option<Raw>) -> Result<bool> {
    let already_restoring = matches!(r.phase, Phase::Factory | Phase::Restoring);
    need(
        *actual == expected_selector(r) || (already_restoring && actual.is_none()),
        "选档已被外部修改，请先核对设备状态",
    )?;
    Ok(actual.is_some())
}
fn registered<'a>(d: &Device, g: &Generation, classes: &'a Classes) -> Result<Option<&'a str>> {
    let Some(owned) = &g.registered else {
        return Ok(None);
    };
    let reference = g.reference(d);
    let Some(found) = Classes::find(&classes.audio, &reference)? else {
        return Ok(None);
    };
    need(
        found.eq_ignore_ascii_case(owned),
        "已登记的 AUDIO 路径与创建回执不符",
    )?;
    Ok(Some(found))
}
fn restore_inner(d: &Device, s: Option<&Snapshot>) -> Result<()> {
    let _lock = store::Lock::acquire()?;
    supported(d)?;
    check_recovery_scope(d)?;
    let original = selector(d, None)?;
    // A formerly selected custom profile is retained. Only the stream selector
    // itself is unset, as observed in all four verified factory baselines.
    if let Some(raw) = &original {
        raw.string()?;
    }
    if let Some(s) = s {
        fresh_target(s)?
    }
    let session = store::load(d, s)?;
    let existing = session.receipt;
    let mut base_before = None;
    if existing.is_none() {
        let fresh = probe::recovery_device()?;
        need(same_device(d, &fresh), "无法重新定位当前播放流")?;
        if original.is_none() {
            let live = s.ok_or("恢复外部基础包长需要活动端点的实读属性")?;
            need(probe::identify()? == live.device, "恢复前活动端点发生变化")?;
            base_before = Some(observe_base(live, &Classes::load(&d.ks)?)?);
        }
    }
    let mut r = existing.unwrap_or_else(|| Receipt::init(d, s, original.clone()));
    r.legacy = session.legacy;
    if base_before.is_some() {
        r.base_packet = base_before;
    }
    let remove = remove_selector_for_factory(&r, &original)?;
    if r.phase != Phase::Factory {
        r.reset_restart();
        r.phase = Phase::Restoring;
        let tx = Tx::start()?;
        if let Some(s) = s {
            fresh_target(s)?
        }
        need(selector(d, Some(&tx))? == original, "恢复前选档发生变化")?;
        if remove {
            let key = registry::open(&selector_path(d), 1 | registry::SET, Some(&tx))?
                .ok_or("待恢复的播放流键缺失")?;
            key.erase(SELECT)?;
            need(key.get(SELECT)?.is_none(), "原厂选档回读失败")?;
        }
        store::commit(&r, tx)?;
    }
    need(selector(d, None)?.is_none(), "原厂选档提交后读回失败")?;
    let classes = Classes::load(&d.ks)?;
    if let Some(b) = &r.base_packet {
        // The receipt is on disk and authenticated before this point. A crash
        // after the setter is handled by independently reading absence.
        let fresh = probe::identify()?;
        need(same_device(d, &fresh), "基础包长恢复前设备身份已变化")?;
        let mm = probe::endpoint_period_properties(&fresh)?;
        let (a, render, iface) = base_links(&fresh, &classes, &mm)?;
        need(
            b.kind == 0x1003
                && b.audio.eq_ignore_ascii_case(a)
                && b.render.eq_ignore_ascii_case(render),
            "活动链接与基础包长回执不符",
        )?;
        match base_reset_action(&b.before, iface.packet()?.as_deref())? {
            true => {
                need(
                    selector(&fresh, None)?.is_none(),
                    "清除基础包长前出现了其他选档",
                )?;
                iface.set_packet(None)?;
            }
            false => {}
        }
    }
    let mut conflicts = 0;
    let usage = Tx::start()?;
    for g in &r.generations {
        if let Some(link) = registered(d, g, &classes)? {
            if referenced_elsewhere(d, g, &usage)? {
                continue;
            }
            let iface = Interface::open(&d.ks, &g.reference(d), link)?;
            if let Some(current) = iface.packet()? {
                if current == packet(g.half) {
                    iface.set_packet(None)?
                } else {
                    conflicts += 1;
                }
            }
        }
    }
    drop(usage);
    r.restart_note = if conflicts > 0 {
        format!(
            "已恢复原厂选档；已保留 {conflicts} 个包长被外部修改的接口。等待重新枚举，必要时拔插 ZH3"
        )
    } else {
        String::new()
    };
    r.save(Phase::Factory)
}
fn base_reset_action(before: &[u8], current: Option<&[u8]>) -> Result<bool> {
    match current {
        None => Ok(false),
        Some(v) if v == before => Ok(true),
        Some(_) => Err("基础 AUDIO 包长在保存回执后发生变化，已保留当前值".into()),
    }
}
pub fn restore(s: &Snapshot) -> Result<()> {
    restore_inner(&s.device, Some(s))
}
pub fn restore_device(d: &Device) -> Result<()> {
    restore_inner(d, None)
}
pub fn recovery_device() -> Result<Device> {
    store::recoverable()
}
fn check_recovery_scope(d: &Device) -> Result<()> {
    let live = match probe::recovery_device() {
        Err(e) if e.starts_with("NO_RECOVERY_REF:") => store::recoverable()?,
        other => other?,
    };
    need(
        same_device(d, &live),
        "活动播放流或物理设备已变化，请重新确认目标",
    )
}
fn finish(s: &Snapshot, mut r: Receipt, classes: &Classes) -> Result<ProfileStatus> {
    let d = &s.device;
    fresh_target(s)?;
    let mut notes = vec![];
    let usage = Tx::start()?;
    for g in &r.generations {
        if referenced_elsewhere(d, g, &usage)? {
            continue;
        }
        let reference = g.reference(d);
        if let Some(link) = registered(d, g, classes)? {
            let iface = Interface::open(&d.ks, &reference, link)?;
            let base =
                Classes::find(&classes.present_audio, &d.base_ref)?.ok_or("原厂 AUDIO 已离线")?;
            let factory = Interface::open(&d.ks, &d.base_ref, base)?.metadata()?;
            let observed = iface.metadata()?;
            if Classes::find(&classes.present_audio, &reference)?.is_some()
                || iface.packet()?.is_some()
                || interface::oem(link)? != (None, None)
                || !g
                    .initial_interface
                    .as_ref()
                    .is_some_and(|initial| observed.clean_for_removal(initial, &factory))
            {
                notes.push(format!("保留有活动状态或属性变化的 AUDIO {reference}"));
                continue;
            }
            iface.remove()?;
            need(
                Classes::find(&Classes::load(&d.ks)?.audio, &reference)?.is_none(),
                "旧 AUDIO 移除后仍在登记列表中",
            )?;
        } else if g.mode == Mode::Range
            && g.registered.is_none()
            && Classes::find(&classes.audio, &reference)?.is_some()
        {
            notes.push(format!("保留未取得创建回执的 AUDIO {reference}"))
        }
    }
    drop(usage);
    let tx = Tx::start()?;
    need(selector(d, Some(&tx))?.is_none(), "清理前选档发生变化")?;
    for g in r.generations.iter().rev().filter(|x| x.committed) {
        if referenced_elsewhere(d, g, &tx)? {
            notes.push(format!("保留仍被驱动选用的档案 {}", g.profile()));
            continue;
        }
        let conflict = source_changed(d, g, &tx)?;
        if conflict {
            notes.push(format!("保留已被外部修改的档案 {}", g.profile()));
            continue;
        }
        for (k, n, v) in g.values() {
            let full = path(d, &k);
            if registry::read(&full, n, Some(&tx))? == Some(v) {
                registry::open(&full, registry::SET, Some(&tx))?
                    .ok_or("清理时源档消失")?
                    .erase(n)?;
            }
        }
        for k in g.before_absent_keys.iter().rev() {
            registry::remove_empty(&path(d, k), &tx)?
        }
    }
    if let Some(before) = &r.initial_endpoint {
        if before.device.active_ref.eq_ignore_ascii_case(&d.base_ref)
            && before.mix.raw_hex != s.mix.raw_hex
        {
            notes.push(format!(
                "当前混音格式 {} Hz / {} 声道与首次记录不同，已按当前格式校验周期",
                s.mix.rate, s.mix.channels
            ));
        }
    }
    r.phase = Phase::Closed;
    r.restart_note = notes.join("；");
    store::commit_close(&r, tx)?;
    let mut msg = String::from("已从活动端点确认原厂播放配置");
    if !r.restart_note.is_empty() {
        msg.push_str("。");
        msg.push_str(&r.restart_note)
    }
    Ok(state(
        "factory",
        None,
        "active",
        msg,
        s.mix.channels == 2,
        false,
    ))
}
fn source_changed(d: &Device, g: &Generation, tx: &Tx) -> Result<bool> {
    let values = g.values();
    let folder = g.folder();
    let known = g
        .keys(d)
        .into_iter()
        .filter(|k| k == &folder || k.starts_with(&format!(r"{folder}\")))
        .collect::<Vec<_>>();
    for k in &known {
        let Some(key) = registry::open(&path(d, k), registry::READ, Some(tx))? else {
            continue;
        };
        for name in key.names()? {
            let Some((_, _, expected)) = values
                .iter()
                .find(|(p, n, _)| p == k && n.eq_ignore_ascii_case(&name))
            else {
                return Ok(true);
            };
            if key.get(&name)?.as_ref() != Some(expected) {
                return Ok(true);
            }
        }
        for child in key.subkeys()? {
            if !known
                .iter()
                .any(|p| p.eq_ignore_ascii_case(&format!(r"{k}\{child}")))
            {
                return Ok(true);
            }
        }
    }
    Ok(false)
}
fn referenced_elsewhere(d: &Device, g: &Generation, tx: &Tx) -> Result<bool> {
    let folder = path(d, "Config");
    let Some(key) = registry::open(&folder, registry::READ, Some(tx))? else {
        return Ok(false);
    };
    referenced_in_config(key.subkeys()?, &g.profile(), |child, name| {
        registry::read(&format!(r"{folder}\{child}"), name, Some(tx))
    })
}
fn referenced_in_config(
    children: Vec<String>,
    profile: &str,
    read: impl Fn(&str, &str) -> Result<Option<Raw>>,
) -> Result<bool> {
    for k in children {
        for name in [SELECT, "DefaultSoundDeviceProfile"] {
            if let Some(value) = read(&k, name)?
                && value.string()?.eq_ignore_ascii_case(profile)
            {
                return Ok(true);
            }
        }
    }
    Ok(false)
}
pub fn reconcile(s: &Snapshot) -> Result<ProfileStatus> {
    let session = store::load(&s.device, Some(s))?;
    if !session
        .receipt
        .as_ref()
        .is_some_and(|r| r.phase == Phase::Factory)
    {
        return session_status(s, &session);
    }
    let _lock = store::Lock::acquire()?;
    let mut session = store::load(&s.device, Some(s))?;
    let classes = Classes::load(&s.device.ks)?;
    if let Some(r) = session.receipt.as_ref()
        && r.phase == Phase::Factory
        && selector(&s.device, None)?.is_none()
        && reappeared(r)?
        && endpoint_matches(s, None, &classes)?
    {
        return finish(s, session.receipt.take().unwrap(), &classes);
    }
    session_status(s, &session)
}
#[link(name = "Kernel32")]
unsafe extern "system" {
    fn GetSystemDirectoryW(buf: *mut u16, size: u32) -> u32;
}
fn pending_restart(d: &Device, expected: &str) -> Result<(Receipt, String)> {
    supported(d)?;
    factory_state::require_no_pending(&d.usb)?;
    let session = store::load(d, None)?;
    let r = session.receipt.ok_or("未找到本次设备操作的回执")?;
    let context = session.context.ok_or("缺少本次已核验回执的标识")?;
    confirm_restart_revision(expected, &context)?;
    let wanted = match r.phase {
        Phase::Waiting => Some(Raw::sz(&r.published()?.profile())),
        Phase::Factory => None,
        _ => return Err("配置步骤尚未完成，不能重启设备".into()),
    };
    need(selector(d, None)? == wanted, "重启前选档发生变化")?;
    Ok((r, context))
}
fn confirm_restart_revision(expected: &str, current: &str) -> Result<()> {
    need(
        expected == current,
        "待生效操作已变化，请重新检测后确认重启。",
    )
}
pub fn restart(d: &Device, expected: &str) -> Result<()> {
    let _lock = store::Lock::acquire()?;
    let (mut r, _) = pending_restart(d, expected)?;
    if r.phase == Phase::Factory
        && let Ok(s) = probe::read()
        && same_device(d, &s.device)
        && reappeared(&r)?
        && let Ok(classes) = Classes::load(&d.ks)
        && endpoint_matches(&s, None, &classes).unwrap_or(false)
    {
        // A canceled activation may never have left the factory endpoint.
        // Complete the verified rollback even if a new PnP restart is blocked.
        finish(&s, r, &classes)?;
        return Ok(());
    }
    let phase = r.phase;
    let preflight = restart_gate(d);
    if preflight.kind != "ready" {
        let msg = format!("配置已保存，未发起新的设备重启。{}", preflight.message);
        r.normal_restart_checked |=
            matches!(preflight.kind.as_str(), "needs_reboot" | "panel_open");
        r.restart_note = msg.clone();
        r.save(phase)?;
        return Err(msg);
    }
    check_recovery_scope(d)?;
    r.restart_note = "已请求定向重启，等待活动端点。若设备未重新出现，请拔插 ZH3".into();
    r.save(phase)?;
    let result = run_device_restart(&d.usb);
    r.normal_restart_checked = true;
    let msg = match result {
        Ok(output) if output.status.success() => {
            r.restart_completed = true;
            return r.save(phase);
        }
        Ok(output) => restart::NativeResult::read(&output).failure(),
        Err(e) => format!("ZH3 软件重启失败，请拔插设备，等待活动端点读回。{e}"),
    };
    r.restart_note = msg.clone();
    r.save(phase)?;
    Err(msg)
}

fn run_device_restart(usb: &str) -> Result<std::process::Output> {
    let mut sys = [0u16; 32768];
    let size = unsafe { GetSystemDirectoryW(sys.as_mut_ptr(), sys.len() as u32) };
    need(
        size > 0 && (size as usize) < sys.len(),
        "无法定位 Windows 系统目录",
    )?;
    let exe = PathBuf::from(String::from_utf16(&sys[..size as usize]).map_err(|e| e.to_string())?)
        .join("pnputil.exe");
    Command::new(exe)
        .args(["/restart-device", usb])
        .creation_flags(0x08000000)
        .output()
        .map_err(|e| e.to_string())
}

/// Manual fallback only. The one-shot intent is committed before entering the
/// OS API. A crash during the request cannot trigger a duplicate port cycle.
pub fn reconnect_usb(
    before: &Snapshot,
    expected: &crate::usb_port::PortTarget,
    context: &str,
) -> Result<()> {
    let d = &before.device;
    let _lock = store::Lock::acquire()?;
    let (mut r, current) = pending_restart(d, context)?;
    r.confirm_reconnect(context, &current)?;
    check_recovery_scope(d)?;
    let s = probe::read()?;
    need(
        same_target_format(before, &s),
        "确认后活动播放流、端点或音频格式已变化，请重新确认",
    )?;
    let classes = Classes::load(&d.ks)?;
    if r.phase == Phase::Factory && reappeared(&r)? && endpoint_matches(&s, None, &classes)? {
        finish(&s, r, &classes)?;
        return Ok(());
    }
    need(
        describe(&s, Some(&r))?.phase == "pending",
        "当前活动端点已不在等待此操作，请刷新读数",
    )?;
    let gate = restart_gate(d);
    need(
        matches!(gate.kind.as_str(), "ready" | "needs_reboot") && !gate.panel_running,
        &gate.message,
    )?;
    need(
        crate::usb_port::target(d)? == *expected,
        "已确认的 USB 端口或连接证据发生变化，请重新确认",
    )?;
    fresh_target(before)?;
    let phase = r.phase;
    r.port_attempt = Some(PortAttempt {
        port: Some(expected.port),
    });
    r.restart_note = "已记录本次 USB 端口重连请求，正在等待 Windows 和活动端点。".into();
    r.save(phase)?;
    let result = crate::usb_port::reconnect(d, expected);
    let message = match &result {
        Ok(()) => "Windows 已接受 USB 端口重连请求，等待活动端点重新出现并核对周期。".into(),
        Err(e) => format!("USB 端口重连未完成：{e}。可手动拔插 ZH3 后重新检测。"),
    };
    r.restart_note = message.clone();
    // Only update the portable journal and its HKLM digest after the OS call:
    // the physical devnode can already be gone at this point.
    r.save(phase)?;
    result.map_err(|_| message)
}
#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> Snapshot {
        let fmt = super::probe::Format {
            rate: 44100,
            channels: 2,
            valid_bits: 32,
            bits: 32,
            encoding: "Float".into(),
            raw_hex: String::new(),
        };
        let device = Device {
            usb: "USB fixture".into(),
            ks: "KS fixture".into(),
            container: "example".into(),
            driver: SUPPORTED_DRIVER.into(),
            endpoint_id: "queried endpoint".into(),
            stream: "StreamFormatOut_02".into(),
            base_ref: "pcm_out_02_c_00".into(),
            active_ref: "pcm_out_02_c_00".into(),
        };
        Snapshot {
            device,
            mix: fmt,
            output: None,
            current: Some(441),
            current_format: None,
            current_rate: Some(44100),
            default_frames: 441,
            step_frames: 441,
            min_frames: 441,
            max_frames: 441,
            volume: None,
            muted: None,
            endpoint_period_properties: Some((None, false)),
        }
    }
    #[test]
    #[ignore = "uses a newly created, isolated HKCU test key and temporary receipt directory"]
    fn source_failure_and_stale_retries_preserve_the_current_transaction() -> Result<()> {
        registry::scratch::with_install(|| {
            let s = fixture();
            let d = &s.device;
            let seed = Tx::start()?;
            registry::create(&path(d, "Config"), registry::READ, &seed)?;
            seed.commit()?;
            let mut r = Receipt::init(d, Some(&s), None);
            let mut g = generation("11111111111111112222222222222222", Mode::Fixed);
            for key in g.keys(d) {
                if registry::open(&path(d, &key), 1, None)?.is_none() {
                    g.before_absent_keys.push(key);
                }
            }
            let folder = path(d, &g.folder());
            let selected = Raw::sz(&g.profile());
            r.generations.push(g);
            r.save(Phase::Source)?;
            let before = store::load(d, None)?.context.unwrap();

            // The preflight covered an absent stream key. Create it at the
            // end of the writer's key sequence, after the plan was saved.
            let concurrent = Tx::start()?;
            registry::create(&selector_path(d), 1, &concurrent)?;
            concurrent.commit()?;
            assert!(apply_source(d, &mut r, None).unwrap_err().contains("占用"));
            assert!(registry::open(&folder, 1, None)?.is_none());
            assert!(selector(d, None)?.is_none());
            let failed = store::load(d, None)?;
            assert_eq!(failed.context.as_deref(), Some(before.as_str()));
            assert!(!failed.receipt.unwrap().latest()?.committed);
            assert!(pending_restart(d, &before).is_err());

            let release = Tx::start()?;
            registry::remove_empty(&selector_path(d), &release)?;
            release.commit()?;
            let first = apply_source(d, &mut r, None)?;
            assert_eq!(selector(d, None)?, Some(selected));
            assert!(pending_restart(d, &first).is_ok());
            // Returning to the identical payload still creates a new revision.
            r.restart_note = "second window".into();
            r.save(Phase::Waiting)?;
            r.restart_note.clear();
            r.save(Phase::Waiting)?;
            let current = store::load(d, None)?.context.unwrap();
            assert_ne!(current, first);
            assert!(
                pending_restart(d, &first)
                    .err()
                    .unwrap()
                    .contains("待生效操作已变化")
            );
            assert!(pending_restart(d, &current).is_ok());

            let h = probe::ConnectedHardware {
                usb: d.usb.clone(),
                container: d.container.clone(),
                driver: d.driver.clone(),
                arrival: Some(37),
            };
            let reset = Tx::start()?;
            factory_state::begin_factory(&h, 37, &reset)?;
            reset.commit()?;
            assert!(pending_restart(d, &current).err().unwrap().contains("全档"));
            Ok(())
        })
    }
    #[test]
    fn format_changes_invalidate_old_confirmations_but_keep_the_same_stream_identity() {
        let a = fixture();
        let mut b = a.clone();
        b.mix.rate = 96000;
        assert!(!same_target_format(&a, &b));
        assert!(same_device(&a.device, &b.device));
        assert!(valid_frames(None, 20, b.mix.rate, [960; 4]));
        assert!(valid_frames(
            Some(Mode::Range),
            7,
            b.mix.rate,
            [960, 1, 336, 960]
        ));
        assert!(valid_frames(Some(Mode::Fixed), 16, b.mix.rate, [768; 4]));
        assert!(!valid_frames(None, 20, b.mix.rate, [441; 4]));
        b = a.clone();
        b.output = Some(b.mix.clone());
        b.output.as_mut().unwrap().valid_bits = 24;
        assert!(!same_target_format(&a, &b));
        b = a.clone();
        b.volume = Some(12);
        b.current = Some(155);
        assert!(same_target_format(&a, &b));
        b.device.stream = "StreamFormatOut_01".into();
        b.device.base_ref = "pcm_out_01_c_00".into();
        assert!(!same_target_format(&a, &b));
        assert!(!same_device(&a.device, &b.device));
        assert_ne!(selector_path(&a.device), selector_path(&b.device));
    }
    #[test]
    fn other_stream_source_must_match_both_interface_classes_and_leave_this_stream_untouched() {
        let entry = "Z4c3dc8a739f19033".to_string();
        let old = format!(r"path\pcm_out_02_c_00_{entry}");
        let factory = r"path\pcm_out_01_c_00".to_string();
        let mut c = Classes {
            audio: vec![old.clone(), factory.clone()],
            render: vec![old.clone(), factory.clone()],
            present_audio: vec![factory.clone()],
            present_render: vec![factory],
        };
        assert!(distinct_stream_evidence(
            "01",
            &c,
            std::slice::from_ref(&entry)
        ));
        assert!(!distinct_stream_evidence(
            "02",
            &c,
            std::slice::from_ref(&entry)
        ));
        assert!(!distinct_stream_evidence(
            "01",
            &c,
            &["Z0123456789abcdef".into()]
        ));
        c.audio.retain(|s| s != &old);
        assert!(!distinct_stream_evidence(
            "01",
            &c,
            std::slice::from_ref(&entry)
        ));
        c.audio.push(old);
        c.render.push(format!(r"path\pcm_out_01_c_01_{entry}"));
        assert!(!distinct_stream_evidence(
            "01",
            &c,
            std::slice::from_ref(&entry)
        ));
    }
    fn generation(id: &str, mode: Mode) -> Generation {
        Generation {
            id: id.into(),
            mode,
            half: 7,
            before_absent_keys: vec![],
            registered: None,
            initial_interface: None,
            committed: false,
        }
    }
    #[test]
    fn legacy_receipts_keep_restore_ownership_without_rewriting_the_history() {
        let s = fixture();
        let mut r = Receipt::init(&s.device, Some(&s), None);
        let mut g = generation("00000000000000011111111111111111", Mode::Range);
        let selected = Raw::sz(&g.profile());
        g.committed = true;
        g.registered = Some(g.reference(&s.device));
        g.before_absent_keys.push(g.folder());
        r.generations.push(g);
        r.phase = Phase::Waiting;
        r.normal_restart_checked = true;

        let mut old = serde_json::to_value(&r).unwrap();
        old["initial_endpoint"] = serde_json::to_value(&s).unwrap();
        old["verified_endpoint"] = old["initial_endpoint"].clone();
        old["generations"][0]["before_endpoint"] = old["initial_endpoint"].clone();
        old["generations"][0]["before_interfaces"] = serde_json::json!({"audio": [], "render": [], "present_audio": [], "present_render": []});
        old["restart_result"] =
            serde_json::json!({"code":50,"stdout":"previous tool output","stderr":""});
        old["port_attempt"] = serde_json::json!({"target":{"port":3},"result":"previous request"});
        let restored: Receipt = serde_json::from_value(old).unwrap();
        assert_eq!(expected_selector(&restored), Some(selected));
        let saved = restored.published().unwrap();
        assert_eq!(saved.before_absent_keys, [saved.folder()]);
        assert_eq!(saved.registered, Some(saved.reference(&s.device)));
        assert!(!restored.can_reconnect());

        let compact = serde_json::to_value(&restored).unwrap();
        assert!(compact.get("verified_endpoint").is_none());
        assert!(compact.get("restart_result").is_none());
        assert!(compact["generations"][0].get("before_endpoint").is_none());
        assert!(compact["generations"][0].get("before_interfaces").is_none());
        assert!(compact["port_attempt"].get("target").is_none());
        assert_eq!(
            compact["initial_endpoint"],
            serde_json::json!({"device":{"active_ref":s.device.active_ref},"mix":{"raw_hex":s.mix.raw_hex}})
        );
    }

    #[test]
    #[ignore = "uses a newly created, isolated HKCU test key; run explicitly"]
    fn cleanup_preserves_driver_defaults_and_capture_profile_references() -> Result<()> {
        let current = "StreamFormatOut_01";
        let other = "StreamFormatOut_02";
        let capture = "StreamFormatIn_01";
        let profile = "ZH3S0011223344556677";
        let hive = registry::scratch::Hive::with_children(&[current, other, capture])?;
        for (child, name) in [
            (current, "DefaultSoundDeviceProfile"),
            (other, "DefaultSoundDeviceProfile"),
            (capture, "DefaultSoundDeviceProfile"),
            (capture, SELECT),
            (other, SELECT),
        ] {
            let seed = Tx::start()?;
            hive.key(child, Some(&seed))?
                .put(name, &Raw::sz(&profile.to_ascii_lowercase()))?;
            seed.commit()?;
            let tx = Tx::start()?;
            let is_referenced = || {
                referenced_in_config(hive.names()?, profile, |k, n| {
                    hive.key(k, Some(&tx))?.get(n)
                })
            };
            assert!(is_referenced()?, "lost reference at {child}/{name}");
            hive.key(child, Some(&tx))?.erase(name)?;
            assert!(!is_referenced()?);
            tx.commit()?;
        }
        Ok(())
    }
    #[test]
    fn reset_unsets_an_existing_named_selector_and_preserves_a_foreign_change() {
        let s = fixture();
        let factory = Receipt::init(&s.device, Some(&s), None);
        assert!(!remove_selector_for_factory(&factory, &None).unwrap());
        let original = Some(Raw::sz("MyExistingProfile"));
        let mut custom = Receipt::init(&s.device, Some(&s), original.clone());
        assert!(remove_selector_for_factory(&custom, &original).unwrap());
        assert!(remove_selector_for_factory(&custom, &Some(Raw::sz("OtherProfile"))).is_err());
        custom.phase = Phase::Restoring;
        assert!(!remove_selector_for_factory(&custom, &None).unwrap());
        assert!(custom.generations.is_empty());
    }
    #[test]
    fn manual_port_request_needs_a_previous_restart_and_is_spent_before_calling_windows() {
        let s = fixture();
        let mut r = Receipt::init(&s.device, Some(&s), None);
        r.phase = Phase::Waiting;
        assert!(!r.can_reconnect());
        r.restart_note = "已请求定向重启，等待活动端点。若设备未重新出现，请拔插 ZH3".into();
        assert!(!r.can_reconnect()); // a crashed 0.1.2 request has no outcome
        r.restart_note = "ZH3 软件重启返回 50，请拔插设备，等待活动端点读回".into();
        assert!(r.can_reconnect());
        assert!(r.confirm_reconnect("previous", "current").is_err());
        assert!(r.confirm_reconnect("current", "current").is_ok());
        r.port_attempt = Some(PortAttempt { port: Some(3) });
        r.normal_restart_checked = true;
        assert!(!r.can_reconnect());
        assert!(r.confirm_reconnect("current", "current").is_err());
        let stored = serde_json::to_vec(&r).unwrap();
        let mut reopened: Receipt = serde_json::from_slice(&stored).unwrap();
        assert!(!reopened.can_reconnect());
        reopened.reset_restart();
        assert!(!reopened.can_reconnect());
        reopened.normal_restart_checked = true;
        reopened.phase = Phase::Preparing;
        assert!(!reopened.can_reconnect());
        reopened.phase = Phase::Factory;
        assert!(reopened.can_reconnect());
    }
    #[test]
    fn base_packet_reset_only_clears_the_journaled_bytes() {
        let record: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/factory-interfaces.json"))
                .unwrap();
        for class in ["AUDIO", "RENDER"] {
            let iface = record["Classes"][class]["TargetInterfaces"]
                .as_array()
                .unwrap()
                .iter()
                .find(|i| i["Present"] == true)
                .unwrap();
            assert_eq!(
                iface["DeviceInterfaceProperties"]["PacketSizeConstraints2"]["Status"],
                "Absent"
            );
            assert_eq!(
                iface["DeviceInterfaceProperties"]["PacketSizeConstraints"]["Status"],
                "Absent"
            );
        }
        let before = packet(7);
        assert!(base_reset_action(&before, Some(&before)).unwrap());
        assert!(!base_reset_action(&before, None).unwrap());
        assert!(base_reset_action(&before, Some(&packet(8))).is_err());
    }
    #[test]
    fn interrupted_prepare_never_claims_a_source_or_an_unjournaled_audio_link() {
        let s = fixture();
        let mut r = Receipt::init(&s.device, Some(&s), None);
        let mut old = generation("00000000000000011111111111111111", Mode::Range);
        old.committed = true;
        let selected = Some(Raw::sz(&old.profile()));
        r.generations.push(old);
        let g = generation("00000000000000022222222222222222", Mode::Range);
        let candidate = format!(r"example\{}", g.reference(&s.device));
        let classes = Classes {
            audio: vec![candidate.clone()],
            render: vec![],
            present_audio: vec![],
            present_render: vec![],
        };
        assert!(registered(&s.device, &g, &classes).unwrap().is_none());
        r.generations.push(g);
        r.phase = Phase::Creating;
        assert_eq!(expected_selector(&r), selected);
        assert!(r.published().is_err());
        r.phase = Phase::Waiting;
        assert!(r.published().is_err());
        let g = r.generations.last_mut().unwrap();
        g.registered = Some("different link".into());
        assert!(registered(&s.device, g, &classes).is_err());
        g.registered = Some(candidate.clone());
        assert_eq!(
            registered(&s.device, g, &classes).unwrap(),
            Some(candidate.as_str())
        );
    }
    #[test]
    fn each_source_uses_its_own_name_and_only_fixed_supplies_an_oem_value() {
        let s = fixture();
        let a = generation("1111111111111111aaaaaaaaaaaaaaaa", Mode::Fixed);
        let b = generation("2222222222222222bbbbbbbbbbbbbbbb", Mode::Range);
        assert_ne!(a.profile(), b.profile());
        assert_ne!(a.reference(&s.device), b.reference(&s.device));
        assert!(
            a.values()
                .iter()
                .any(|(p, n, v)| p.ends_with(r"\ks\EP\0") && *n == OEM && *v == oem(7))
        );
        assert!(
            b.values()
                .iter()
                .all(|(_, n, _)| *n != OEM && *n != USE_DEFAULT)
        );
        assert!(
            b.values()
                .iter()
                .all(|(p, _, _)| p.starts_with(&b.folder()) && !p.contains(&a.profile()))
        );
        let stale = Classes {
            audio: vec![],
            render: vec![format!(r"example\{}", b.reference(&s.device))],
            present_audio: vec![],
            present_render: vec![],
        };
        assert!(!stale.vacant(&b.reference(&s.device)).unwrap());
    }
    #[test]
    fn verified_packet_and_fixed_blob() {
        assert_eq!(packet(7).len(), 40);
        assert_eq!(&packet(7)[..4], &[0xb8, 0x88, 0, 0]);
        assert!(packet(7)[4..].iter().all(|b| *b == 0));
        assert_eq!(
            oem(16).bytes,
            [65, 0, 0, 0, 8, 0, 0, 0, 0x80, 0x38, 1, 0, 0, 0, 0, 0]
        );
    }
    #[test]
    fn periods_follow_the_current_mix_rate_across_the_fosi_panel_rates() {
        // Expected frames for 10 ms factory, 3.5 ms minimum and 8 ms fixed.
        for (rate, default, minimum, fixed) in [
            (44100, 441, 155, 353),
            (48000, 480, 168, 384),
            (88200, 882, 309, 706),
            (96000, 960, 336, 768),
            (176400, 1764, 618, 1411),
            (192000, 1920, 672, 1536),
            (352800, 3528, 1235, 2822),
            (384000, 3840, 1344, 3072),
            (705600, 7056, 2470, 5645),
            (768000, 7680, 2688, 6144),
        ] {
            assert!(valid_frames(None, 20, rate, [default; 4]));
            assert!(valid_frames(
                Some(Mode::Range),
                7,
                rate,
                [default, 1, minimum, default]
            ));
            assert!(valid_frames(Some(Mode::Fixed), 16, rate, [fixed; 4]));
        }
    }

    #[test]
    fn all_slider_positions_respect_fixed_rounding_and_range_bounds() {
        for rate in [
            44_100, 48_000, 88_200, 96_000, 176_400, 192_000, 352_800, 384_000, 705_600, 768_000,
        ] {
            for half in 1u8..=20 {
                // These products are smaller than 2^24. Converting exact
                // integer half-milliseconds this way also preserves ties.
                let frames = f64::from(rate) * f64::from(half) / 2000.0;
                let nearest = frames.round() as u32;
                let minimum = frames.ceil() as u32;
                let default = rate / 100;
                for candidate in nearest - 1..=nearest + 1 {
                    assert_eq!(
                        valid_frames(Some(Mode::Fixed), half, rate, [candidate; 4]),
                        candidate == nearest,
                        "fixed at {rate} Hz, {half} half-ms, {candidate} frames"
                    );
                }
                for candidate in minimum - 1..=minimum + 1 {
                    assert_eq!(
                        valid_frames(
                            Some(Mode::Range),
                            half,
                            rate,
                            [default, 1, candidate, default]
                        ),
                        candidate == minimum,
                        "range at {rate} Hz, {half} half-ms, {candidate} frames"
                    );
                }
            }
        }
    }
    #[test]
    fn live_evidence_matches_only_the_intended_route() {
        assert!(valid_frames(None, 20, 44100, [441; 4]));
        assert!(valid_frames(
            Some(Mode::Range),
            7,
            44100,
            [441, 1, 155, 441]
        ));
        assert!(valid_frames(Some(Mode::Fixed), 16, 44100, [353; 4]));
        assert!(!valid_frames(Some(Mode::Fixed), 16, 44100, [352; 4]));
        assert!(!valid_frames(Some(Mode::Range), 6, 44100, [221; 4]));
        assert!(!valid_frames(
            Some(Mode::Range),
            7,
            44100,
            [441, 1, 154, 441]
        ));
    }
}
