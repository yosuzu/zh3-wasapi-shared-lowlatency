use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{Value, json};

use crate::probe::{ConnectedHardware, Device, Snapshot};
use crate::{probe, profile, usb_port};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Ready,
    Refresh,
    Apply,
    Restore,
    Retry,
    Reconnect,
}

#[derive(Debug, Deserialize)]
pub struct Command {
    pub action: Action,
    #[serde(default)]
    usb: String,
    #[serde(default)]
    container: String,
    #[serde(default)]
    endpoint_id: String,
    #[serde(default)]
    target_token: String,
    #[serde(default)]
    factory_token: String,
    #[serde(default)]
    port_token: String,
    #[serde(default)]
    retry_token: String,
    #[serde(default)]
    mode: String,
    #[serde(default)]
    half_ms: u8,
}

impl Command {
    pub fn changes_device(&self) -> bool {
        matches!(
            self.action,
            Action::Apply | Action::Restore | Action::Retry | Action::Reconnect
        )
    }
    fn matches(&self, d: &Device) -> bool {
        !self.usb.is_empty()
            && !self.container.is_empty()
            && self.usb.eq_ignore_ascii_case(&d.usb)
            && self.container.eq_ignore_ascii_case(&d.container)
            && self.endpoint_id.eq_ignore_ascii_case(&d.endpoint_id)
    }

    fn confirmed_retry<'a>(
        &self,
        scope: &str,
        operation: Option<&'a str>,
    ) -> Result<&'a str, String> {
        operation
            .filter(|id| {
                !id.is_empty() && retry_token(scope, id, &self.target_token) == self.retry_token
            })
            .ok_or_else(|| "待重启的操作已变化，请按当前状态重新确认。".into())
    }
}

fn retry_token(scope: &str, operation: &str, target: &str) -> String {
    let mut hash = DefaultHasher::new();
    (scope, operation, target).hash(&mut hash);
    format!("retry-{:016x}", hash.finish())
}

fn same_hardware(a: &Device, b: &Device) -> bool {
    a.usb.eq_ignore_ascii_case(&b.usb) && a.container.eq_ignore_ascii_case(&b.container)
}

// UI confirmation guard only. The backend still independently locates the real
// USB/KS target and verifies profile ownership before every mutation.
fn target_token(d: &Device, snapshot: Option<&Snapshot>) -> String {
    let mut hash = DefaultHasher::new();
    for field in [
        &d.usb,
        &d.ks,
        &d.container,
        &d.driver,
        &d.endpoint_id,
        &d.stream,
        &d.base_ref,
        &d.active_ref,
    ] {
        field.to_ascii_lowercase().hash(&mut hash);
    }
    if let Some(s) = snapshot {
        s.mix.raw_hex.hash(&mut hash);
        s.output.as_ref().map(|f| &f.raw_hex).hash(&mut hash);
    }
    format!("{:016x}", hash.finish())
}

fn readonly_view(s: &Snapshot, message: &str) -> Value {
    json!({"state":"connected", "snapshot":s, "profile":{"mode":"unknown","phase":"error","message":message,"can_write":false,"can_restore":false}, "message":message})
}

fn factory_token(h: &ConnectedHardware) -> String {
    let mut hash = DefaultHasher::new();
    for value in [&h.usb, &h.container, &h.driver] {
        value.to_ascii_lowercase().hash(&mut hash);
    }
    h.arrival.hash(&mut hash);
    format!("factory-{:016x}", hash.finish())
}

pub enum Event {
    Frame(Value),
    OperationFinished,
}

struct Waiting {
    device: Device,
    since: Instant,
    restart_error: Option<String>,
}

struct ReconnectOffer {
    target: usb_port::PortTarget,
    operation: String,
    token: String,
}

impl ReconnectOffer {
    fn new(target: usb_port::PortTarget, operation: &str, format_token: &str) -> Self {
        let mut hash = DefaultHasher::new();
        serde_json::to_vec(&target)
            .expect("serializable port identity")
            .hash(&mut hash);
        operation.hash(&mut hash);
        format_token.hash(&mut hash);
        Self {
            target,
            operation: operation.into(),
            token: format!("{:016x}", hash.finish()),
        }
    }
}

#[derive(Default)]
struct Monitor {
    waiting: Option<Waiting>,
    identity_error: Option<String>,
    notice: Option<String>,
    audio_missing_since: Option<Instant>,
    restart_status: Option<(Device, Instant, profile::RestartGate)>,
    usb_offer: Option<ReconnectOffer>,
    factory_notice: Option<(ConnectedHardware, String)>,
}

fn frame(state: &str, message: &str) -> Value {
    json!({"state":state,"message":message})
}

fn prepend_message(view: &mut Value, message: &str) {
    let current = view["message"].as_str().unwrap_or("");
    if !current.contains(message) {
        view["message"] = format!("{message} {current}").trim().into();
    }
}

fn physical_failure(error: &str) -> Value {
    frame(
        if error.starts_with("NO_DEVICE:") {
            "offline"
        } else {
            "unverified"
        },
        reason(error),
    )
}

fn reason(error: &str) -> &str {
    error
        .strip_prefix("NO_DEVICE:")
        .or_else(|| error.strip_prefix("AUDIO_UNAVAILABLE:"))
        .unwrap_or(error)
        .trim()
}

fn recovery_target() -> Result<Device, String> {
    match probe::recovery_device() {
        Ok(d) => Ok(d),
        Err(primary) if primary.starts_with("NO_RECOVERY_REF:") => {
            let mut device = profile::recovery_device().map_err(|_| primary)?;
            device.endpoint_id.clear();
            device.active_ref.clear();
            Ok(device)
        }
        Err(primary) => Err(primary),
    }
}

impl Monitor {
    fn restart_gate(&mut self, device: &Device) -> profile::RestartGate {
        if let Some((previous, at, status)) = &self.restart_status
            && previous == device
            && at.elapsed() < Duration::from_millis(1500)
        {
            return status.clone();
        }
        let status = profile::restart_gate(device);
        self.restart_status = Some((device.clone(), Instant::now(), status.clone()));
        status
    }

    fn changed_target(&mut self) -> Value {
        let text = "设备身份已变化。请重新检测 ZH3，核对后再确认操作。";
        self.identity_error = Some(text.to_string());
        frame("unverified", text)
    }

    fn unavailable(&mut self, error: &str) -> Value {
        let missing = error.starts_with("NO_DEVICE:");
        let audio_missing = error.starts_with("AUDIO_UNAVAILABLE:");
        if missing {
            self.audio_missing_since = None;
            if let Some(waiting) = &self.waiting {
                if waiting.since.elapsed() < Duration::from_secs(8) {
                    return frame("restarting", "设置已保存，等待 ZH3 重新连接并读回周期。");
                }
                return frame(
                    "offline",
                    "请重新插入刚才配置的 ZH3，程序会继续核对实读值。",
                );
            }
            return physical_failure(error);
        }
        let hardware = match probe::connected_hardware() {
            Ok(hardware) => hardware,
            Err(e) => return physical_failure(&e),
        };
        if self.waiting.as_ref().is_some_and(|w| {
            !hardware.usb.eq_ignore_ascii_case(&w.device.usb)
                || (!hardware.container.is_empty()
                    && !hardware.container.eq_ignore_ascii_case(&w.device.container))
        }) {
            return self.changed_target();
        }
        if let Ok(s) = probe::readonly_snapshot() {
            self.audio_missing_since = None;
            return readonly_view(
                &s,
                &format!("已读取 ZH3 的播放端点，写入目标尚未确认。{}", reason(error)),
            );
        }
        if audio_missing {
            let elapsed = self
                .audio_missing_since
                .get_or_insert_with(Instant::now)
                .elapsed();
            if elapsed >= Duration::from_secs(3)
                && let Ok(d) = recovery_target()
                && d.driver == profile::SUPPORTED_DRIVER
                && d.usb.eq_ignore_ascii_case(&hardware.usb)
                && d.container.eq_ignore_ascii_case(&hardware.container)
            {
                let gate = self.restart_gate(&d);
                return json!({"state":"recovery","device":d,"target_token":target_token(&d,None),"message":format!("{} 可恢复原厂设置后重新检测。", reason(error)),"profile":{"can_write":false,"can_restore":true},"restart_gate":gate});
            }
        } else {
            self.audio_missing_since = None;
        }
        json!({"state":"identified","device":hardware,"message":reason(error)})
    }

    fn view(&mut self, allow_reconcile: bool) -> Value {
        let Ok(h) = probe::connected_hardware() else {
            self.factory_notice = None;
            return self.device_view(allow_reconcile);
        };
        if self
            .factory_notice
            .as_ref()
            .is_some_and(|(previous, _)| previous != &h)
        {
            self.factory_notice = None;
        }
        let reset_token = factory_token(&h);
        let mut pending = profile::factory_state::pending(&h);
        let mut eligible_restart = !allow_reconcile;
        let mut verification = None;
        if allow_reconcile && matches!(pending, Ok(Some(_))) {
            match profile::factory::reconcile_detected(&h) {
                Ok(Some(r)) if r.verified => {
                    self.factory_notice = Some((h.clone(), r.message));
                    self.waiting = None;
                    pending = Ok(None);
                }
                Ok(Some(r)) => {
                    eligible_restart = pending
                        .as_ref()
                        .ok()
                        .and_then(Option::as_ref)
                        .is_some_and(|p| r.restart_context.as_deref() == Some(p.id.as_str()));
                    verification = Some(r.message);
                }
                Ok(None) => pending = Ok(None),
                Err(e) => {
                    verification = Some(e);
                }
            }
        }
        let mut view = match pending {
            Ok(None) => self.device_view(allow_reconcile),
            Ok(Some(p)) => {
                self.all_pending_view(&h, &p, eligible_restart && p.awaits_reconnect(&h))
            }
            Err(e) => {
                self.usb_offer = None;
                match probe::readonly_snapshot() {
                    Ok(s) => readonly_view(&s, &e),
                    Err(_) => json!({"state":"identified","device":h,"message":e}),
                }
            }
        };
        // Bind even a display-only or error frame to a new physical observation.
        // A second check also prevents an offer for a device swapped during the read.
        let named_playback_count = profile::factory::named_playback_count(&h).ok();
        match probe::connected_hardware() {
            Ok(current) if current == h => {}
            Ok(_) => return frame("detecting", "读取期间 ZH3 的连接已变化，正在重新核对。"),
            Err(e) => return physical_failure(&e),
        }
        view["factory_target"] = json!({"usb":h.usb,"container":h.container,"token":reset_token,"named_playback_count":named_playback_count});
        if let Some(msg) = verification {
            prepend_message(&mut view, &msg);
        }
        if let Some((_, msg)) = &self.factory_notice {
            prepend_message(&mut view, msg);
        }
        view
    }

    fn all_pending_view(
        &mut self,
        h: &ConnectedHardware,
        p: &profile::factory_state::Pending,
        can_request: bool,
    ) -> Value {
        self.usb_offer = None;
        let snapshot = probe::read().ok().filter(|s| {
            s.device.usb.eq_ignore_ascii_case(&h.usb)
                && s.device.container.eq_ignore_ascii_case(&h.container)
        });
        let physical = probe::physical_target(h).ok();
        let Some(d) = snapshot.as_ref().map(|s| &s.device).or(physical.as_ref()) else {
            return json!({"state":"identified","device":h,"message":"此 ZH3 的全部播放档正在等待原厂状态核验。请重新连接后检测。"});
        };
        let gate = profile::factory_state::factory_gate(d);
        let tok = target_token(d, snapshot.as_ref());
        let msg = if p.message.is_empty() {
            "已请求恢复全部播放档，正在核对各档配置和活动端点。"
        } else {
            &p.message
        };
        let mut result = json!({"state":if snapshot.is_some(){"connected"}else{"recovery"},"device":d,"snapshot":snapshot,
            "profile":{"mode":"factory","phase":"pending","can_write":false,"can_restore":true}, "target_token":tok,"message":msg,
            "can_retry":can_request && gate.kind=="ready", "restart_gate":gate});
        if can_request && gate.kind == "ready" {
            result["retry_token"] = retry_token("factory", &p.id, &tok).into();
        }
        if can_request
            && p.checked_restart
            && !p.used_usb
            && !gate.panel_running
            && matches!(gate.kind.as_str(), "ready" | "needs_reboot")
            && let Ok(target) = usb_port::target(d)
        {
            let offer = ReconnectOffer::new(target, &p.id, &tok);
            result["usb_reconnect"] = json!({"port":offer.target.port,"token":offer.token});
            self.usb_offer = Some(offer);
        }
        result
    }

    fn device_view(&mut self, allow_reconcile: bool) -> Value {
        self.usb_offer = None;
        if let Some(error) = &self.identity_error {
            return frame("unverified", error);
        }
        let s = match probe::read() {
            Ok(s) => s,
            Err(e) => return self.unavailable(&e),
        };
        self.audio_missing_since = None;
        if self
            .waiting
            .as_ref()
            .is_some_and(|w| !same_hardware(&w.device, &s.device))
        {
            return self.changed_target();
        }
        if self.waiting.as_ref().is_some_and(|w| {
            !w.device.stream.eq_ignore_ascii_case(&s.device.stream)
                || !w.device.base_ref.eq_ignore_ascii_case(&s.device.base_ref)
                || !w.device.ks.eq_ignore_ascii_case(&s.device.ks)
                || w.device.driver != s.device.driver
        }) {
            self.waiting = None;
            self.notice = Some("驱动已切换输出格式，现显示当前输出的配置。".into());
        }
        if s.device.driver != profile::SUPPORTED_DRIVER {
            return readonly_view(
                &s,
                &format!(
                    "当前驱动 {} 可读取状态。配置写入已验证的驱动版本为 {}。",
                    s.device.driver,
                    profile::SUPPORTED_DRIVER
                ),
            );
        }
        let p = match profile::status(&s).and_then(|p| {
            if allow_reconcile && p.phase == "pending" {
                profile::reconcile(&s)
            } else {
                Ok(p)
            }
        }) {
            Ok(p) => p,
            Err(e) => {
                return readonly_view(&s, &format!("配置或恢复记录未通过核对。{e}"));
            }
        };
        let gate = self.restart_gate(&s.device);
        if p.phase == "pending" {
            let wait = self.waiting.get_or_insert_with(|| Waiting {
                device: s.device.clone(),
                since: Instant::now(),
                restart_error: None,
            });
            let finished_waiting = gate.kind != "ready"
                || wait.since.elapsed() >= Duration::from_secs(8)
                || wait.restart_error.is_some();
            let can_retry = finished_waiting && gate.kind == "ready" && p.retry_context.is_some();
            let detail = wait.restart_error.as_deref().unwrap_or(&p.message);
            let token = target_token(&s.device, Some(&s));
            if finished_waiting || !allow_reconcile {
                let mut view = json!({"state":"connected","snapshot":s,"target_token":token,"profile":p,"message":detail,"can_retry":can_retry,"restart_gate":gate});
                if can_retry && let Some(key) = p.retry_context.as_deref() {
                    view["retry_token"] = retry_token("profile", key, &token).into();
                }
                if finished_waiting
                    && !gate.panel_running
                    && matches!(gate.kind.as_str(), "ready" | "needs_reboot")
                    && let Some(key) = p.reconnect_context.as_deref()
                {
                    match usb_port::target(&s.device) {
                        Ok(target) => {
                            let offer = ReconnectOffer::new(target, key, &token);
                            view["usb_reconnect"] =
                                json!({"port":offer.target.port,"token":offer.token});
                            self.usb_offer = Some(offer);
                        }
                        Err(e) => {
                            view["message"] = format!("{detail} USB 端口无法核对：{e}").into()
                        }
                    }
                }
                return view;
            }
            return json!({"state":"restarting","device":s.device,"target_token":token,"profile":p,"message":detail,"can_retry":false,"restart_gate":gate});
        }
        if self.waiting.take().is_some() && p.phase == "active" {
            self.notice = None;
        }
        json!({"state":"connected","snapshot":s,"target_token":target_token(&s.device,Some(&s)),"profile":p,"message":self.notice.as_deref().unwrap_or(&p.message),"restart_gate":gate})
    }

    fn restart(&mut self, d: Device, operation: &str, publish: &impl Fn(Value)) {
        self.restart_status = None;
        if let Err(e) = probe::verify_physical(&d) {
            let message = format!(
                "配置已写入，重启前未能重新核对设备，已停止重启。{}",
                reason(&e)
            );
            publish(frame("detecting", &message));
            self.waiting = Some(Waiting {
                device: d,
                since: Instant::now(),
                restart_error: Some(message),
            });
            return;
        }
        if profile::restart_gate(&d).kind == "ready" {
            publish(frame("restarting", "配置已写入，正在重新启动这台 ZH3。"));
        }
        let restart_error = profile::restart(&d, operation).err();
        self.waiting = Some(Waiting {
            device: d,
            since: Instant::now(),
            restart_error,
        });
    }

    fn restore_all(&mut self, cmd: &Command, h: ConnectedHardware, publish: &impl Fn(Value)) {
        let token = factory_token(&h);
        if cmd.factory_token != token
            || !cmd.usb.eq_ignore_ascii_case(&h.usb)
            || !cmd.container.eq_ignore_ascii_case(&h.container)
        {
            self.factory_notice = Some((h, "恢复确认已过期，请重新核对此 ZH3。".into()));
            return;
        }
        self.usb_offer = None;
        self.identity_error = None;
        self.waiting = None;
        self.notice = None;
        publish(frame(
            "writing",
            "正在核对这台 ZH3 的所有播放档，并恢复已改动的周期。",
        ));
        let s = probe::read().ok();
        let msg = match profile::factory::restore_detected(&h, s.as_ref()) {
            Ok(r) => {
                let mut message = r.message;
                if let Some(operation) = r.restart_context {
                    self.restart_status = None;
                    if r.device
                        .as_ref()
                        .is_some_and(|d| profile::factory_state::factory_gate(d).kind == "ready")
                    {
                        publish(frame(
                            "restarting",
                            "原厂配置已写入，正在请求 ZH3 重新连接。",
                        ));
                    }
                    if let Err(e) = profile::factory_state::restart_pending(&h, &operation) {
                        message = format!("{message} {e}");
                    }
                }
                message
            }
            Err(e) => format!("恢复原厂尚未完成。{e}"),
        };
        self.factory_notice = Some((h, msg));
    }

    fn pending_factory_action(
        &mut self,
        cmd: &Command,
        h: &ConnectedHardware,
        p: &profile::factory_state::Pending,
        publish: &impl Fn(Value),
    ) -> Result<(), String> {
        let s = probe::read().ok();
        let physical = probe::physical_target(h)?;
        let d = s.as_ref().map(|s| &s.device).unwrap_or(&physical);
        if !same_hardware(d, &physical)
            || d.ks != physical.ks
            || d.driver != physical.driver
            || !cmd.matches(d)
            || cmd.target_token != target_token(d, s.as_ref())
        {
            return Err("ZH3 确认已过期，请重新检测。".into());
        }
        match cmd.action {
            Action::Retry => {
                let operation = cmd.confirmed_retry("factory", Some(&p.id))?;
                profile::factory_state::restart_pending(h, operation)
            }
            Action::Reconnect => {
                let offer = self
                    .usb_offer
                    .take()
                    .ok_or("USB 确认已过期，请重新检测。")?;
                if cmd.port_token != offer.token || offer.operation != p.id {
                    return Err("端口或恢复操作已变化。".into());
                }
                publish(frame("restarting", "正在重新连接这台 ZH3 的 USB 端口。"));
                profile::factory_state::reconnect(h, &offer.target, &offer.operation)
            }
            _ => Err("正在验收所有播放档的原厂设置，请等待核验或再次恢复。".into()),
        }
    }

    fn change(&mut self, cmd: &Command, publish: &impl Fn(Value)) {
        self.factory_notice = None;
        let h = match probe::connected_hardware() {
            Ok(h) => h,
            Err(e) => {
                publish(physical_failure(&e));
                return;
            }
        };
        if matches!(cmd.action, Action::Restore) {
            self.restore_all(cmd, h, publish);
            return;
        }
        match profile::factory_state::pending(&h) {
            Err(e) => {
                self.factory_notice = Some((h, e));
                return;
            }
            Ok(Some(p)) => {
                let outcome = self.pending_factory_action(cmd, &h, &p, publish);
                self.factory_notice = outcome.err().map(|e| (h, e));
                self.restart_status = None;
                return;
            }
            Ok(None) => {}
        }
        if let Some(e) = &self.identity_error {
            publish(frame("unverified", e));
            return;
        }
        let snapshot = match probe::read() {
            Ok(s) => s,
            Err(e) => {
                let state = self.unavailable(&e);
                publish(state);
                return;
            }
        };
        let d = snapshot.device.clone();
        if !cmd.usb.eq_ignore_ascii_case(&d.usb)
            || !cmd.container.eq_ignore_ascii_case(&d.container)
        {
            publish(self.changed_target());
            return;
        }
        if !cmd.matches(&d) || cmd.target_token != target_token(&d, Some(&snapshot)) {
            let msg = "ZH3 的播放端点或输出格式已变化，本次操作已取消。请按当前读数重新确认。";
            self.notice = Some(msg.into());
            publish(frame("detecting", msg));
            return;
        }
        if d.driver != profile::SUPPORTED_DRIVER {
            self.notice = Some("当前驱动只支持读取状态。".into());
            return;
        }
        if matches!(cmd.action, Action::Reconnect) {
            let outcome = (|| -> Result<(), String> {
                let s = &snapshot;
                let offer = self
                    .usb_offer
                    .take()
                    .ok_or("USB 重新连接的确认已过期，请重新检测。")?;
                let p = profile::status(s)?;
                if offer.token != cmd.port_token
                    || p.phase != "pending"
                    || p.reconnect_context.as_deref() != Some(&offer.operation)
                {
                    return Err("USB 端口或待生效配置已变化，本次操作已取消。".into());
                }
                publish(frame(
                    "restarting",
                    "正在重新连接 ZH3 所在的 USB 端口。设备返回后将核对实际周期。",
                ));
                profile::reconnect_usb(s, &offer.target, &offer.operation)
            })();
            self.restart_status = None;
            self.notice = outcome.as_ref().err().cloned();
            self.waiting = Some(Waiting {
                device: d,
                since: Instant::now(),
                restart_error: outcome.err(),
            });
            return;
        }
        let operation = || -> Result<String, String> {
            let s = &snapshot;
            match cmd.action {
                Action::Apply => {
                    if !(1..=20).contains(&cmd.half_ms)
                        || !matches!(cmd.mode.as_str(), "range" | "fixed")
                    {
                        return Err("周期或模式无效。".into());
                    }
                    let p = profile::status(s)?;
                    let allowed = match p.phase.as_str() {
                        "active" => p.mode == "factory" || p.mode == cmd.mode,
                        "mismatch" => p.mode == cmd.mode,
                        _ => false,
                    };
                    if !p.can_write || !allowed {
                        return Err("当前模式不能应用此更改。请先恢复原厂并核对读数。".into());
                    }
                    publish(frame("writing", "正在保存配置并校验写入结果。"));
                    profile::apply(s, &cmd.mode, cmd.half_ms)
                }
                Action::Retry => {
                    let p = profile::status(s)?;
                    cmd.confirmed_retry("profile", p.retry_context.as_deref())
                        .map(str::to_owned)
                }
                _ => Err("当前状态无需重启。请重新检测。".into()),
            }
        }();
        match operation {
            Ok(operation) => {
                self.notice = None;
                self.restart(d, &operation, publish);
            }
            Err(e) => {
                self.notice = Some(e.clone());
                publish(frame("error", &e));
            }
        }
    }
}

pub fn run(input: Receiver<Command>, send: impl Fn(Event)) {
    let mut monitor = Monitor::default();
    let mut last = Value::Null;
    let mut ready = false;
    loop {
        let received = if ready {
            input.recv_timeout(Duration::from_millis(600))
        } else {
            input.recv().map_err(|_| RecvTimeoutError::Disconnected)
        };
        let mut finished = false;
        let force = match received {
            Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => false,
            Ok(command) => {
                ready = true;
                if command.changes_device() {
                    monitor.change(&command, &|v| send(Event::Frame(v)));
                    finished = true;
                } else {
                    if monitor.identity_error.is_some() {
                        monitor.waiting = None;
                    }
                    monitor.identity_error = None;
                    monitor.notice = None;
                    if matches!(command.action, Action::Refresh) {
                        monitor.restart_status = None;
                        monitor.factory_notice = None;
                        send(Event::Frame(frame("detecting", "正在重新核对 ZH3。")));
                    }
                }
                true
            }
        };
        let next = monitor.view(true);
        if force || next != last {
            send(Event::Frame(next.clone()));
            last = next;
        }
        if finished {
            send(Event::OperationFinished);
        }
    }
}

/// Reads a fresh snapshot for diagnostics. This path never reconciles or writes a session.
pub fn inspect() -> Value {
    Monitor::default().view(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_confirmation_cannot_follow_another_pending_operation_or_format() {
        let mut cmd: Command = serde_json::from_value(json!({
            "action": "retry", "target_token": "format-A",
            "retry_token": retry_token("profile", "receipt-A", "format-A"),
        }))
        .unwrap();
        assert_eq!(
            cmd.confirmed_retry("profile", Some("receipt-A")).unwrap(),
            "receipt-A"
        );
        assert!(cmd.confirmed_retry("profile", Some("receipt-B")).is_err());
        assert!(cmd.confirmed_retry("profile", None).is_err());
        assert!(cmd.confirmed_retry("factory", Some("receipt-A")).is_err());
        cmd.target_token = "format-B".into();
        assert!(cmd.confirmed_retry("profile", Some("receipt-A")).is_err());
        cmd.retry_token = retry_token("factory", "reset-A", "format-B");
        assert!(cmd.confirmed_retry("factory", Some("reset-A")).is_ok());
        assert!(cmd.confirmed_retry("factory", Some("reset-B")).is_err());
        cmd.retry_token.clear();
        assert!(cmd.confirmed_retry("factory", Some("reset-A")).is_err());
    }

    #[test]
    fn a_factory_confirmation_cannot_follow_a_new_attachment_or_another_usb_instance() {
        let mut h = ConnectedHardware {
            usb: "USB instance".into(),
            container: "the same physical DAC".into(),
            driver: "6.16".into(),
            arrival: Some(71),
        };
        let original = factory_token(&h);
        h.arrival = Some(72);
        assert_ne!(original, factory_token(&h));
        h.arrival = Some(71);
        h.usb = "another USB instance".into();
        assert_ne!(original, factory_token(&h));
    }

    #[test]
    fn reconnect_confirmation_binds_the_host_port_and_specific_saved_operation() {
        let target = usb_port::PortTarget {
            port: 3,
            usb: "USB instance A".into(),
            ks: "KS instance A".into(),
            container: "device A".into(),
            driver: "6.16.0.15392".into(),
            driver_key: "driver key A".into(),
            hub_instance: "external hub A".into(),
            hub_link: "hub interface A".into(),
            usb_arrival: 7,
        };
        let token = |p: &usb_port::PortTarget, operation: &str, format: &str| {
            ReconnectOffer::new(p.clone(), operation, format).token
        };
        let original = token(&target, "save A", "format A");
        assert_eq!(original, token(&target, "save A", "format A"));
        assert_ne!(original, token(&target, "restore A", "format A"));
        assert_ne!(original, token(&target, "save A", "format B"));
        let mut other = target.clone();
        other.hub_instance = "root hub B".into();
        other.driver_key = "driver key B".into();
        assert_ne!(original, token(&other, "save A", "format A"));
    }

    #[test]
    fn a_confirmation_belongs_to_one_physical_device_and_endpoint() {
        let mut device = Device {
            usb: "USB-A".into(),
            ks: "KS-A".into(),
            container: "container-a".into(),
            driver: "6.16.0.15392".into(),
            endpoint_id: "endpoint-a".into(),
            stream: "StreamFormatOut_02".into(),
            base_ref: "pcm_out_02_c_00".into(),
            active_ref: "pcm_out_02_c_00".into(),
        };
        let command: Command = serde_json::from_value(json!({"action":"apply","usb":"usb-a","container":"container-A","endpoint_id":"endpoint-a","mode":"range","half_ms":7})).unwrap();
        assert!(command.matches(&device));
        device.endpoint_id = "endpoint-b".into();
        assert!(!command.matches(&device));
        device.endpoint_id = "endpoint-a".into();
        device.container = "container-b".into();
        assert!(!command.matches(&device));
    }

    #[test]
    fn identity_failures_show_the_reason_and_expose_no_write_controls() {
        for e in [
            "找到多个 ZH3，请只连接一个目标设备。",
            "无法核对 USB 设备标识。",
        ] {
            let view = physical_failure(e);
            assert_eq!(view, json!({"state":"unverified","message":e}));
        }
        assert_eq!(
            physical_failure("NO_DEVICE: 未连接 ZH3。")["state"],
            "offline"
        );
    }

    #[test]
    fn format_switch_invalidates_confirmation_but_keeps_the_verified_endpoint_visible() {
        let report: Value =
            serde_json::from_str(include_str!("../tests/fixtures/connected.json")).unwrap();
        let mut s: Snapshot =
            serde_json::from_value(report["readonly"]["snapshot"].clone()).unwrap();
        let before = target_token(&s.device, Some(&s));
        s.current = Some(256);
        s.volume = Some(37);
        assert_eq!(before, target_token(&s.device, Some(&s)));
        s.mix.rate = 48000;
        s.mix.raw_hex.replace_range(8..16, "80BB0000");
        assert_ne!(before, target_token(&s.device, Some(&s)));
        s.device.stream.clear();
        let display = readonly_view(&s, "无法确认此输出流的配置");
        assert_eq!(display["state"], "connected");
        assert_eq!(display["snapshot"]["mix"]["rate"], 48000);
        assert_eq!(display["profile"]["can_write"], false);
        assert_eq!(display["profile"]["can_restore"], false);
        assert!(display.get("target_token").is_none());
    }
}
