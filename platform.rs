//! Per-WebView CDP input. No operating-system pointer input is used.
use crate::{
    automation::{ClickRequest, Permit},
    config::Target,
    macros::{Action, Request as MacroRequest},
};
use serde_json::{json, Value};
use std::{cell::RefCell, rc::Rc};
use webview2_com::{
    CallDevToolsProtocolMethodCompletedHandler, Microsoft::Web::WebView2::Win32::ICoreWebView2,
    ProcessFailedEventHandler,
};
use windows::core::HSTRING;
use wry::{WebView, WebViewExtWindows};

type Completion = Box<dyn FnOnce(Result<Value, String>)>;
pub fn set_background_active(
    view: &WebView,
    enabled: bool,
    done: impl FnOnce(Result<(), String>) + 'static,
) {
    match unsafe { view.controller().CoreWebView2() } {
        Ok(core) => cdp(
            &core,
            "Emulation.setFocusEmulationEnabled",
            json!({"enabled":enabled}),
            Box::new(move |result| done(result.map(|_| ()))),
        ),
        Err(_) => done(Err("WEBVIEW_UNAVAILABLE".into())),
    }
}
pub type HeldInputs = Rc<RefCell<std::collections::BTreeMap<String, Action>>>;

pub fn release_all(view: &WebView, held: &HeldInputs) {
    let actions: Vec<_> = held.borrow_mut().values().cloned().collect();
    held.borrow_mut().clear();
    let Ok(core) = (unsafe { view.controller().CoreWebView2() }) else {
        return;
    };
    for action in actions {
        if let Some(release) = action.release() {
            let (method, params) = macro_parameters(&release, 0);
            cdp(&core, method, params, Box::new(|_| {}));
        }
    }
}
fn macro_parameters(action: &Action, buttons: u8) -> (&'static str, Value) {
    match action {
        Action::KeyDown {
            key,
            code,
            virtual_key,
            modifiers,
        }
        | Action::KeyUp {
            key,
            code,
            virtual_key,
            modifiers,
        } => {
            let down = action.is_down();
            let text = if down && key.chars().count() == 1 && modifiers & 7 == 0 {
                key.as_str()
            } else {
                ""
            };
            (
                "Input.dispatchKeyEvent",
                json!({"type":if down{"keyDown"}else{"keyUp"},"key":key,"code":code,"windowsVirtualKeyCode":virtual_key,"nativeVirtualKeyCode":virtual_key,"modifiers":modifiers,"text":text,"unmodifiedText":text}),
            )
        }
        Action::MouseDown {
            x,
            y,
            button,
            modifiers,
        }
        | Action::MouseUp {
            x,
            y,
            button,
            modifiers,
        } => (
            "Input.dispatchMouseEvent",
            json!({"type":if action.is_down(){"mousePressed"}else{"mouseReleased"},"x":x,"y":y,"button":button.cdp(),"buttons":buttons,"clickCount":1,"modifiers":modifiers}),
        ),
        Action::MouseMove { x, y, modifiers } => (
            "Input.dispatchMouseEvent",
            json!({"type":"mouseMoved","x":x,"y":y,"button":"none","buttons":buttons,"modifiers":modifiers}),
        ),
        Action::Wheel {
            x,
            y,
            delta_x,
            delta_y,
            modifiers,
        } => (
            "Input.dispatchMouseEvent",
            json!({"type":"mouseWheel","x":x,"y":y,"deltaX":delta_x,"deltaY":delta_y,"modifiers":modifiers}),
        ),
        Action::Wait => ("Runtime.evaluate", json!({"expression":"void 0"})),
    }
}
pub fn macro_input(view: &WebView, held: HeldInputs, request: MacroRequest) {
    if request.cancel.is_cancelled() {
        let _ = request.reply.send(Err("CANCELLED".into()));
        return;
    }
    let core = match unsafe { view.controller().CoreWebView2() } {
        Ok(c) => c,
        Err(_) => {
            let _ = request.reply.send(Err("WEBVIEW_UNAVAILABLE".into()));
            return;
        }
    };
    if request.action.position().is_none() {
        send_macro_action(core, held, request);
        return;
    }
    let next = core.clone();
    cdp(
        &core,
        "Runtime.evaluate",
        json!({"expression":"({width:innerWidth,height:innerHeight})","returnByValue":true}),
        Box::new(move |result| {
            if request.cancel.is_cancelled() {
                let _ = request.reply.send(Err("CANCELLED".into()));
                return;
            }
            let viewport = result.and_then(|v| {
                let w = v["result"]["value"]["width"]
                    .as_f64()
                    .ok_or("VIEWPORT_UNAVAILABLE")?;
                let h = v["result"]["value"]["height"]
                    .as_f64()
                    .ok_or("VIEWPORT_UNAVAILABLE")?;
                Ok((w, h))
            });
            let (w, h) = match viewport {
                Ok(v) => v,
                Err(e) => {
                    let _ = request.reply.send(Err(e));
                    return;
                }
            };
            let mut request = request;
            match &mut request.action {
                Action::MouseDown { x, y, .. }
                | Action::MouseUp { x, y, .. }
                | Action::MouseMove { x, y, .. }
                | Action::Wheel { x, y, .. } => {
                    *x = (*x * w / request.viewport.width).clamp(0.0, (w - 1.0).max(0.0));
                    *y = (*y * h / request.viewport.height).clamp(0.0, (h - 1.0).max(0.0));
                }
                _ => {}
            }
            send_macro_action(next, held, request);
        }),
    );
}
fn send_macro_action(core: ICoreWebView2, held: HeldInputs, request: MacroRequest) {
    if request.cancel.is_cancelled() {
        let _ = request.reply.send(Err("CANCELLED".into()));
        return;
    }
    // Register before dispatch, so a synchronous Stop also releases pending downs.
    if let Some(id) = request.action.hold_id() {
        if request.action.is_down() {
            held.borrow_mut().insert(id, request.action.clone());
        } else {
            held.borrow_mut().remove(&id);
        }
    }
    let buttons = held.borrow().values().fold(0, |mask, a| {
        if let Action::MouseDown { button, .. } = a {
            mask | button.mask()
        } else {
            mask
        }
    });
    let (method, params) = macro_parameters(&request.action, buttons);
    cdp(
        &core,
        method,
        params,
        Box::new(move |result| {
            let _ = request.reply.send(result.map(|_| ()));
        }),
    );
}
fn cdp(core: &ICoreWebView2, method: &str, params: Value, done: Completion) {
    let completion = Rc::new(RefCell::new(Some(done)));
    let next = completion.clone();
    let callback =
        CallDevToolsProtocolMethodCompletedHandler::create(Box::new(move |status, result| {
            if let Some(done) = next.borrow_mut().take() {
                done(if status.is_err() {
                    Err("WEBVIEW_INPUT_ERROR".into())
                } else {
                    serde_json::from_str(&result).map_err(|_| "INVALID_BROWSER_RESPONSE".into())
                });
            }
            Ok(())
        }));
    let result = unsafe {
        core.CallDevToolsProtocolMethod(
            &HSTRING::from(method),
            &HSTRING::from(params.to_string()),
            &callback,
        )
    };
    if result.is_err() {
        if let Some(done) = completion.borrow_mut().take() {
            done(Err("WEBVIEW_UNAVAILABLE".into()));
        }
    }
}
pub fn watch_crash(view: &WebView, on_crash: impl Fn() + 'static) -> anyhow::Result<()> {
    let core = unsafe { view.controller().CoreWebView2()? };
    let callback = ProcessFailedEventHandler::create(Box::new(move |_, _| {
        on_crash();
        Ok(())
    }));
    let mut token = 0;
    unsafe {
        core.add_ProcessFailed(&callback, &mut token)?;
    }
    Ok(())
}
pub fn watch_cursor_shortcut(
    view: &WebView,
    on_shortcut: impl Fn() + 'static,
) -> anyhow::Result<()> {
    use webview2_com::{
        AcceleratorKeyPressedEventHandler,
        Microsoft::Web::WebView2::Win32::{
            COREWEBVIEW2_KEY_EVENT_KIND_KEY_DOWN, COREWEBVIEW2_KEY_EVENT_KIND_KEY_UP,
            COREWEBVIEW2_PHYSICAL_KEY_STATUS,
        },
    };
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        GetKeyState, VK_CONTROL, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT,
    };
    let captured = std::cell::Cell::new(false);
    let handler = AcceleratorKeyPressedEventHandler::create(Box::new(move |_, args| {
        let Some(args) = args else {
            return Ok(());
        };
        unsafe {
            let mut key = 0;
            args.VirtualKey(&mut key)?;
            if key != 0x31 && key != 0x61 {
                return Ok(());
            }
            let mut kind = COREWEBVIEW2_KEY_EVENT_KIND_KEY_DOWN;
            args.KeyEventKind(&mut kind)?;
            if kind == COREWEBVIEW2_KEY_EVENT_KIND_KEY_UP && captured.replace(false) {
                args.SetHandled(true)?;
            } else if kind == COREWEBVIEW2_KEY_EVENT_KIND_KEY_DOWN
                && GetKeyState(VK_CONTROL.0 as i32) < 0
                && [VK_MENU, VK_SHIFT, VK_LWIN, VK_RWIN]
                    .iter()
                    .all(|k| GetKeyState(k.0 as i32) >= 0)
            {
                args.SetHandled(true)?;
                captured.set(true);
                let mut status = COREWEBVIEW2_PHYSICAL_KEY_STATUS::default();
                args.PhysicalKeyStatus(&mut status)?;
                if !status.WasKeyDown.as_bool() {
                    on_shortcut();
                }
            }
        }
        Ok(())
    }));
    let mut token = 0;
    unsafe {
        view.controller()
            .add_AcceleratorKeyPressed(&handler, &mut token)?;
    }
    Ok(())
}
pub fn navigate_history(view: &WebView, back: bool) -> anyhow::Result<()> {
    let core = unsafe { view.controller().CoreWebView2()? };
    unsafe {
        if back {
            core.GoBack()?;
        } else {
            core.GoForward()?;
        }
    }
    Ok(())
}

pub fn cursor_position() -> anyhow::Result<(i32, i32)> {
    let mut point = windows::Win32::Foundation::POINT::default();
    unsafe {
        windows::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut point)?;
    }
    Ok((point.x, point.y))
}

/// Keep renderers active behind the selected sibling. Hiding a WebView suspends
/// the frame acknowledgements needed by CDP mouse input on current WebView2.
pub fn bring_to_front(view: &WebView) -> anyhow::Result<()> {
    use windows::Win32::{
        Foundation::HWND,
        UI::WindowsAndMessaging::{SetWindowPos, HWND_TOP, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE},
    };
    let mut parent = HWND::default();
    unsafe {
        view.controller().ParentWindow(&mut parent)?;
        SetWindowPos(
            parent,
            Some(HWND_TOP),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        )?;
    }
    Ok(())
}

pub fn test_crash(view: &WebView) -> anyhow::Result<()> {
    let core = unsafe { view.controller().CoreWebView2()? };
    cdp(&core, "Page.crash", json!({}), Box::new(|_| {}));
    Ok(())
}
pub fn dispatch(view: &WebView, request: ClickRequest) {
    if !request.permit.valid() {
        let _ = request.reply.send(Err("CANCELLED".into()));
        return;
    }
    let core = match unsafe { view.controller().CoreWebView2() } {
        Ok(c) => c,
        Err(_) => {
            let _ = request.reply.send(Err("WEBVIEW_UNAVAILABLE".into()));
            return;
        }
    };
    let expression = match &request.preset.target {
        Target::Element { selector } => format!(
            r#"(()=>{{try{{const e=document.querySelector({});if(!e||e.disabled)return {{error:'ELEMENT_NOT_FOUND'}};e.scrollIntoView({{block:'center',inline:'center'}});const r=e.getBoundingClientRect();const x=r.x+r.width/2,y=r.y+r.height/2;const hit=document.elementFromPoint(x,y);if(r.width<=0||r.height<=0||!hit||!(e===hit||e.contains(hit)))return {{error:'ELEMENT_NOT_FOUND'}};return {{x,y}};}}catch(_){{return {{error:'INVALID_SELECTOR'}};}}}})()"#,
            serde_json::to_string(selector).unwrap()
        ),
        Target::Coordinate {
            x,
            y,
            viewport_width,
            viewport_height,
        } => format!("({{x:{x}*innerWidth/{viewport_width},y:{y}*innerHeight/{viewport_height}}})"),
    };
    let next = core.clone();
    cdp(
        &core,
        "Runtime.evaluate",
        json!({"expression":expression,"returnByValue":true}),
        Box::new(move |result| {
            if !request.permit.valid() {
                let _ = request.reply.send(Err("CANCELLED".into()));
                return;
            }
            let result = result.and_then(|v| {
                let value = &v["result"]["value"];
                if let Some(error) = value["error"].as_str() {
                    return Err(error.to_string());
                }
                let x = value["x"].as_f64().ok_or("INVALID_COORDINATES")?;
                let y = value["y"].as_f64().ok_or("INVALID_COORDINATES")?;
                if !x.is_finite() || !y.is_finite() || x < 0.0 || y < 0.0 {
                    return Err("INVALID_COORDINATES".into());
                }
                Ok((x, y))
            });
            match result {
                Err(e) => {
                    let _ = request.reply.send(Err(e));
                }
                Ok((x, y)) => mouse_sequence(
                    next,
                    request.permit,
                    request.reply,
                    request.preset.button.cdp().into(),
                    request.preset.button.mask(),
                    x,
                    y,
                ),
            }
        }),
    );
}
fn mouse_sequence(
    core: ICoreWebView2,
    permit: Permit,
    reply: tokio::sync::oneshot::Sender<Result<(), String>>,
    button: String,
    mask: u8,
    x: f64,
    y: f64,
) {
    let next = core.clone();
    cdp(
        &core,
        "Input.dispatchMouseEvent",
        json!({"type":"mouseMoved","x":x,"y":y,"button":"none"}),
        Box::new(move |moved| {
            if !permit.valid() {
                let _ = reply.send(Err("CANCELLED".into()));
                return;
            }
            if let Err(e) = moved {
                let _ = reply.send(Err(e));
                return;
            }
            let release = next.clone();
            let release_button = button.clone();
            cdp(
                &next,
                "Input.dispatchMouseEvent",
                json!({"type":"mousePressed","x":x,"y":y,"button":button,"buttons":mask,"clickCount":1}),
                Box::new(move |pressed| {
                    // Always release an already dispatched button, even after cancellation.
                    cdp(
                        &release,
                        "Input.dispatchMouseEvent",
                        json!({"type":"mouseReleased","x":x,"y":y,"button":release_button,"buttons":0,"clickCount":1}),
                        Box::new(move |released| {
                            let result = pressed.and(released).map(|_| ());
                            let _ = reply.send(result);
                        }),
                    );
                }),
            );
        }),
    );
}
