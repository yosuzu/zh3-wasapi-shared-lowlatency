//! One machine-local pending reset per physical USB instance. The selector
//! transaction creates it and verified restoration retires it. Pure reads
//! never create the registry key or any portable files.
pub(crate) use super::restart::factory_gate;
use super::{
    Result, factory, need,
    registry::{self, Raw, Tx},
    restart, run_device_restart, store,
};
use crate::{
    probe::{self, ConnectedHardware},
    usb_port,
};
use serde::{Deserialize, Serialize};

const ROOT: &str = r"SOFTWARE\ZH3 Shared Control\PendingFactoryReset";

#[derive(Serialize, Deserialize)]
pub(crate) struct Pending {
    pub id: String,
    container: String,
    pub(super) arrival: u64,
    pub checked_restart: bool,
    pub used_usb: bool,
    pub message: String,
}

impl Pending {
    pub fn awaits_reconnect(&self, h: &ConnectedHardware) -> bool {
        self.arrival != 0 && h.arrival == Some(self.arrival)
    }
}

fn read(h: &ConnectedHardware, tx: Option<&Tx>) -> Result<Option<Pending>> {
    let Some(raw) = registry::read(ROOT, &h.usb, tx)? else {
        return Ok(None);
    };
    let marker: Pending = serde_json::from_str(&raw.string()?)
        .map_err(|_| "原厂恢复的待办状态无法读取，请重新确认恢复原厂。")?;
    need(
        h.container.is_empty()
            || marker.container.is_empty()
            || marker.container.eq_ignore_ascii_case(&h.container),
        "旧恢复状态与本次连接的容器不同，请重新确认恢复原厂。",
    )?;
    Ok(Some(marker))
}

fn put(h: &ConnectedHardware, p: &Pending, tx: &Tx) -> Result<()> {
    let (key, _) = registry::create(ROOT, registry::SET, tx)?;
    key.put(
        &h.usb,
        &Raw::sz(&serde_json::to_string(p).map_err(|e| e.to_string())?),
    )
}

pub(crate) fn pending(h: &ConnectedHardware) -> Result<Option<Pending>> {
    read(h, None)
}

pub(super) fn require_no_pending(usb: &str) -> Result<()> {
    need(
        registry::read(ROOT, usb, None)?.is_none(),
        "该 ZH3 正在等待全档原厂恢复验收，请重新检测。",
    )
}

pub(super) fn factory_pending(h: &ConnectedHardware) -> Result<Option<u64>> {
    Ok(read(h, None)?.map(|p| p.arrival))
}

pub(super) fn begin_factory(h: &ConnectedHardware, arrival: u64, tx: &Tx) -> Result<()> {
    let p = Pending {
        id: store::id()?,
        container: h.container.clone(),
        arrival,
        checked_restart: false,
        used_usb: false,
        message: String::new(),
    };
    put(h, &p, tx)
}

pub(super) fn complete_factory(h: &ConnectedHardware, tx: &Tx) -> Result<store::RetiredDevice> {
    if let Some(key) = registry::open(ROOT, registry::SET | 1, Some(tx))?
        && key
            .names()?
            .iter()
            .any(|name| name.eq_ignore_ascii_case(&h.usb))
    {
        key.erase(&h.usb)?;
    }
    store::retire_device(&h.usb, tx)
}

fn persist(h: &ConnectedHardware, expected: &str, p: &Pending) -> Result<()> {
    let tx = Tx::start()?;
    need(
        read(h, Some(&tx))?.is_some_and(|live| live.id == expected),
        "恢复状态已变化，请重新检测。",
    )?;
    put(h, p, &tx)?;
    tx.commit()
}

pub(crate) fn restart_pending(h: &ConnectedHardware, expected: &str) -> Result<()> {
    let _lock = store::Lock::acquire()?;
    let mut p = pending(h)?.ok_or("未找到待生效的原厂恢复操作。")?;
    super::confirm_restart_revision(expected, &p.id)?;
    let d = probe::physical_target(h)?;
    factory::ready_to_restart(h)?;
    let gate = factory_gate(&d);
    // Consume this confirmation before dispatch. A crash or a second window
    // cannot replay a retry or a port offer issued for the previous revision.
    p.id = store::id()?;
    if gate.kind != "ready" {
        p.checked_restart |= matches!(gate.kind.as_str(), "needs_reboot" | "panel_open");
        p.message = format!("原厂配置已保存。{}", gate.message);
        persist(h, expected, &p)?;
        return Err(p.message);
    }
    probe::verify_physical(&d)?;
    p.message = "已请求重启 ZH3，等待原厂端点读回。".into();
    persist(h, expected, &p)?;
    let outcome = run_device_restart(&d.usb).and_then(|out| {
        if out.status.success() {
            Ok(())
        } else {
            Err(restart::NativeResult::read(&out).failure())
        }
    });
    p.checked_restart = true;
    p.message = outcome
        .as_ref()
        .err()
        .cloned()
        .unwrap_or_else(|| "已请求重启 ZH3，等待原厂端点读回。".into());
    persist(h, &p.id, &p)?;
    outcome
}

pub(crate) fn reconnect(
    h: &ConnectedHardware,
    port: &usb_port::PortTarget,
    operation: &str,
) -> Result<()> {
    let _lock = store::Lock::acquire()?;
    let mut p = pending(h)?.ok_or("未找到待生效的原厂恢复操作。")?;
    need(
        p.id == operation && p.checked_restart && !p.used_usb,
        "USB 重连确认已过期或已使用。",
    )?;
    let d = probe::physical_target(h)?;
    factory::ready_to_restart(h)?;
    let gate = factory_gate(&d);
    need(
        matches!(gate.kind.as_str(), "ready" | "needs_reboot") && !gate.panel_running,
        &gate.message,
    )?;
    need(
        usb_port::target(&d)? == *port,
        "ZH3 所在的 USB 端口已变化。",
    )?;
    p.used_usb = true;
    p.message = "已请求重新连接 ZH3，等待原厂端点读回。".into();
    persist(h, &p.id, &p)?;
    let result = usb_port::reconnect(&d, port);
    if let Err(e) = &result {
        p.message = format!("端口重连未完成：{e}。请拔插 ZH3 后重新检测。");
        persist(h, &p.id, &p)?;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "uses a newly created, isolated HKCU test key and temporary receipt directory"]
    fn replacing_a_factory_operation_invalidates_old_restart_and_write_confirmations() -> Result<()>
    {
        registry::scratch::with_install(|| {
            let h = ConnectedHardware {
                usb: "fixture".into(),
                container: "container".into(),
                driver: super::super::SUPPORTED_DRIVER.into(),
                arrival: Some(43),
            };
            let begin = || -> Result<Pending> {
                let tx = Tx::start()?;
                begin_factory(&h, 43, &tx)?;
                tx.commit()?;
                Ok(pending(&h)?.unwrap())
            };
            let old = begin()?;
            let mut new = begin()?;
            assert_ne!(old.id, new.id);
            assert!(
                restart_pending(&h, &old.id)
                    .unwrap_err()
                    .contains("操作已变化")
            );
            assert!(persist(&h, &old.id, &old).is_err());
            let confirmed = new.id.clone();
            new.id = store::id()?;
            new.checked_restart = true;
            persist(&h, &confirmed, &new)?;
            assert!(
                restart_pending(&h, &confirmed)
                    .unwrap_err()
                    .contains("操作已变化")
            );
            let live = pending(&h)?.unwrap();
            assert_eq!(live.id, new.id);
            assert!(live.checked_restart && !live.used_usb);
            Ok(())
        })
    }
}
