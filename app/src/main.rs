#![windows_subsystem = "windows"]

use std::{
    borrow::Cow,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
};
use tao::{
    dpi::{LogicalSize, PhysicalSize},
    event::{Event, WindowEvent},
    event_loop::{ControlFlow, EventLoopBuilder},
    platform::windows::{IconExtWindows, WindowBuilderExtWindows},
    window::{Icon, Theme, WindowBuilder},
};
use windows::{
    Win32::{
        Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE},
        System::Threading::CreateMutexW,
        UI::WindowsAndMessaging::{MB_ICONINFORMATION, MB_OK, MessageBoxW},
    },
    core::{HSTRING, w},
};
use wry::{NewWindowResponse, WebContext, WebViewBuilder, WebViewBuilderExtWindows};
use zh3_control::controller::{self, Command, Event as Update};

const WINDOW_URL: &str = "zh3://localhost/index.html";
const DOCUMENT_URL: &str = "http://zh3.localhost/index.html";

fn message(text: &str) {
    unsafe {
        MessageBoxW(
            None,
            &HSTRING::from(text),
            w!("ZH3 Control"),
            MB_ICONINFORMATION | MB_OK,
        );
    }
}

struct Instance(HANDLE);
impl Drop for Instance {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

fn desktop() -> Result<(), String> {
    let handle = unsafe {
        CreateMutexW(
            None,
            false,
            w!("Local\\ZH3-Control-3E366FB1-AE77-4295-A45E-CE079993ECB1"),
        )
    }
    .map_err(|e| e.to_string())?;
    let exists = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    let instance = Instance(handle);
    if exists {
        return Err("ZH3 Control 已在此 Windows 会话中运行。请切回现有窗口。".into());
    }

    let event_loop = EventLoopBuilder::<Update>::with_user_event().build();
    let title_icon =
        Icon::from_resource(101, Some(PhysicalSize::new(32, 32))).map_err(|e| e.to_string())?;
    let taskbar_icon =
        Icon::from_resource(101, Some(PhysicalSize::new(64, 64))).map_err(|e| e.to_string())?;
    let window = WindowBuilder::new()
        .with_title("ZH3 Control · 第三方工具")
        .with_window_icon(Some(title_icon))
        .with_taskbar_icon(Some(taskbar_icon))
        .with_theme(Some(Theme::Dark))
        .with_inner_size(LogicalSize::new(1160.0, 765.0))
        .with_min_inner_size(LogicalSize::new(620.0, 520.0))
        .build(&event_loop)
        .map_err(|e| e.to_string())?;
    let (outbox, receiver) = mpsc::sync_channel::<Command>(4);
    let busy = Arc::new(AtomicBool::new(false));
    let posting = busy.clone();
    let user_data = std::env::var_os("LOCALAPPDATA").ok_or("无法定位 WebView2 的本地缓存目录。")?;
    let mut context = WebContext::new(Some(
        PathBuf::from(user_data)
            .join("ZH3-Control")
            .join("WebView2"),
    ));
    let webview = WebViewBuilder::new_with_web_context(&mut context)
        .with_background_color((28, 28, 29, 255))
        .with_custom_protocol("zh3".into(), move |_, request| {
            let (status, mime, body): (_, _, Cow<'static, [u8]>) = match request.uri().path() {
                "/index.html" => (
                    200,
                    "text/html; charset=utf-8",
                    Cow::Borrowed(include_bytes!("../ui/index.html").as_slice()),
                ),
                "/zh3-front.svg" => (
                    200,
                    "image/svg+xml",
                    Cow::Borrowed(include_bytes!("../ui/zh3-front.svg").as_slice()),
                ),
                "/zh3-mark.svg" => (
                    200,
                    "image/svg+xml",
                    Cow::Borrowed(include_bytes!("../assets/zh3-icon.svg").as_slice()),
                ),
                _ => (404, "text/plain", Cow::Borrowed(b"Not found")),
            };
            wry::http::Response::builder()
                .status(status)
                .header("Content-Type", mime)
                .header("Cache-Control", "no-store")
                .body(body)
                .expect("valid embedded resource headers")
        })
        .with_url(WINDOW_URL)
        .with_devtools(false)
        .with_default_context_menus(false)
        .with_browser_accelerator_keys(false)
        .with_navigation_handler(|url| {
            url == DOCUMENT_URL || url == WINDOW_URL || url == "about:blank"
        })
        .with_new_window_req_handler(|_, _| NewWindowResponse::Deny)
        .with_ipc_handler(move |request| {
            if !(request.uri() == DOCUMENT_URL || request.uri() == WINDOW_URL)
                || request.body().len() > 4096
            {
                return;
            }
            let Ok(cmd) = serde_json::from_str::<Command>(request.body()) else {
                return;
            };
            let change = cmd.changes_device();
            if change && posting.swap(true, Ordering::AcqRel) {
                return;
            }
            if outbox.try_send(cmd).is_err() && change {
                posting.store(false, Ordering::Release);
            }
        })
        .build(&window)
        .map_err(|e| format!("无法启动界面。请检查 Microsoft Edge WebView2 Runtime。\n{e}"))?;
    let proxy = event_loop.create_proxy();
    thread::Builder::new()
        .name("ZH3 device monitor".into())
        .spawn(move || {
            controller::run(receiver, |event| {
                let _ = proxy.send_event(event);
            });
        })
        .map_err(|e| e.to_string())?;
    let mut close_when_safe = false;
    event_loop.run(move |event, _, flow| {
        let _keep_alive = (&instance, &window, &context);
        *flow = ControlFlow::Wait;
        match event {
            Event::UserEvent(Update::Frame(data)) => {
                let _ = webview.evaluate_script(&format!("window.zh3Update?.({data});"));
            }
            Event::UserEvent(Update::OperationFinished) => {
                busy.store(false, Ordering::Release);
                if close_when_safe {
                    *flow = ControlFlow::Exit;
                }
            }
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => {
                if busy.load(Ordering::Acquire) {
                    close_when_safe = true;
                } else {
                    *flow = ControlFlow::Exit;
                }
            }
            _ => {}
        }
    })
}

fn main() {
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("--inspect")) {
        println!("{}", controller::inspect());
        return;
    }
    if let Err(e) = desktop() {
        message(&e);
    }
}
