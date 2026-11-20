use crate::{
    automation::{Engine, State, WorkerEvent},
    config::{self, Config, Preset, Profile, TabId, Target},
    macros,
    performance::{PerformanceMode, RenderPolicy},
    platform,
    storage::Database,
};
use serde::Deserialize;
use serde_json::json;
use std::{
    collections::HashMap,
    io::{Read, Write},
    net::TcpListener,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};
use tao::{
    event::{Event, WindowEvent},
    event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy},
    platform::windows::{IconExtWindows, WindowBuilderExtWindows},
    window::{Icon, Window, WindowBuilder},
};
use wry::{
    dpi::{LogicalPosition, LogicalSize},
    Rect, WebContext, WebView, WebViewBuilder, WebViewBuilderExtWindows, WebViewExtWindows,
};
mod macro_controls;
mod macro_test;
mod self_test;

enum AppEvent {
    Ui(String),
    Worker(WorkerEvent),
    Record(TabId, String),
    Navigated(TabId, String),
    Title(TabId, String),
    Crash(TabId),
    Probe(String, String),
    Macro(macros::Event),
    MacroLoaded(TabId, Result<Option<macros::MacroFile>, String>),
    MacroSaved(TabId, Result<bool, String>),
    PageReady(TabId),
    ShowCursor(Option<TabId>),
    BackgroundActive(TabId, bool, Result<(), String>),
}
struct LiveTab {
    view: WebView,
    _context: WebContext,
    state: State,
    completed: u64,
    error: Option<String>,
    recording: bool,
    title: String,
    held: platform::HeldInputs,
    macro_file: macros::MacroFile,
    macro_recording: Option<TabId>,
    macro_step: usize,
    macro_cycles: u64,
    automation_kind: &'static str,
    macro_revision: u64,
    playback: macros::PlaybackOptions,
    render_policy: Option<RenderPolicy>,
    background_active: Option<bool>,
    background_pending: bool,
    background_failed: Option<bool>,
    last_input: Instant,
}
#[derive(Default, Deserialize)]
struct UiCommand {
    action: String,
    tab_id: Option<TabId>,
    preset: Option<Preset>,
    url: Option<String>,
    #[serde(default)]
    macro_file: Option<macros::MacroFile>,
    #[serde(default)]
    macro_owner: Option<TabId>,
    #[serde(default)]
    playback: Option<macros::PlaybackOptions>,
    #[serde(default)]
    record_moves: Option<bool>,
    #[serde(default)]
    performance_mode: Option<PerformanceMode>,
}
struct App {
    window: Window,
    shell: WebView,
    tabs: HashMap<TabId, LiveTab>,
    profiles: Vec<Profile>,
    active: Option<TabId>,
    engine: Engine,
    macro_player: macros::Player,
    file_dialog_open: bool,
    db: Database,
    root: PathBuf,
    config: Config,
    proxy: EventLoopProxy<AppEvent>,
    panel: bool,
    profiles_visible: bool,
    logs_visible: bool,
    stopped: bool,
    message: String,
    form_revision: u64,
    system: sysinfo::System,
    cpu: f32,
    memory_mb: f64,
    demo_url: String,
    last_resource_update: Instant,
    last_render: Instant,
    last_editor: Option<(TabId, u64)>,
    last_recording_session: Option<TabId>,
    last_recorded_steps: usize,
    last_ui_payload: String,
    process_ids: Vec<sysinfo::Pid>,
    last_process_discovery: Instant,
}

pub fn run() -> anyhow::Result<()> {
    let test_mode = std::env::args().any(|a| a == "--self-test");
    let diagnose = std::env::args().any(|a| a == "--diagnose-shell");
    let macro_test_mode = std::env::args().any(|a| a == "--test-macro");
    if (test_mode || macro_test_mode) && std::env::var_os("TABPILOT_DATA_DIR").is_none() {
        std::env::set_var(
            "TABPILOT_DATA_DIR",
            std::env::current_dir()?
                .join("data")
                .join(format!("self-test-{}", TabId::new_v4())),
        );
    }
    let root = config::data_root()?;
    let config = Config::load(&root)?;
    let db = Database::open(&root.join("workspace.db"))?;
    let profiles = db.profiles()?;
    let event_loop = EventLoopBuilder::<AppEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    let window = WindowBuilder::new()
        .with_window_icon(Some(Icon::from_resource(
            1,
            Some(tao::dpi::PhysicalSize::new(32, 32)),
        )?))
        .with_taskbar_icon(Some(Icon::from_resource(
            1,
            Some(tao::dpi::PhysicalSize::new(64, 64)),
        )?))
        .with_visible(!diagnose && !macro_test_mode)
        .with_title("HexBot PB MOBILE")
        .with_inner_size(tao::dpi::LogicalSize::new(config.width, config.height))
        .with_min_inner_size(tao::dpi::LogicalSize::new(900, 600))
        .build(&event_loop)?;
    let p = proxy.clone();
    let shell_initial_load = std::cell::Cell::new(true);
    let shell = WebViewBuilder::new()
        .with_html(include_str!("ui/shell.html"))
        .with_navigation_handler(move |url| {
            let local = url == "about:blank" || url.starts_with("data:text/html");
            if local && shell_initial_load.get() {
                shell_initial_load.set(false);
                true
            } else {
                false
            }
        })
        .with_ipc_handler(move |request| {
            if request.body().len() < macros::MAX_FILE_BYTES as usize {
                let _ = p.send_event(AppEvent::Ui(request.body().clone()));
            }
        })
        .build_as_child(&window)?;
    let shortcut_proxy = proxy.clone();
    platform::watch_cursor_shortcut(&shell, move || {
        let _ = shortcut_proxy.send_event(AppEvent::ShowCursor(None));
    })?;
    let p = proxy.clone();
    let engine = Engine::new(Arc::new(move |e| {
        let _ = p.send_event(AppEvent::Worker(e));
    }))?;
    let p = proxy.clone();
    let macro_player = macros::Player::new(
        engine.runtime_handle(),
        Arc::new(move |event| {
            let _ = p.send_event(AppEvent::Macro(event));
        }),
    );
    let demo_url = start_demo_server(test_mode || macro_test_mode)?;
    let mut app = App {
        window,
        shell,
        tabs: HashMap::new(),
        profiles,
        active: None,
        engine,
        macro_player,
        file_dialog_open: false,
        db,
        root,
        config,
        proxy,
        panel: true,
        profiles_visible: true,
        logs_visible: false,
        stopped: false,
        message: String::new(),
        form_revision: 0,
        system: sysinfo::System::new(),
        cpu: 0.0,
        memory_mb: 0.0,
        demo_url,
        last_resource_update: Instant::now() - Duration::from_secs(1),
        last_render: Instant::now() - Duration::from_secs(1),
        last_editor: None,
        last_recording_session: None,
        last_recorded_steps: 0,
        last_ui_payload: String::new(),
        process_ids: Vec::new(),
        last_process_discovery: Instant::now() - Duration::from_secs(30),
    };
    let ids: Vec<_> = app
        .profiles
        .iter()
        .filter(|p| p.open)
        .map(|p| p.tab_id)
        .collect();
    let restore_limit = if app.config.lazy_restore {
        1
    } else {
        usize::MAX
    };
    for id in ids.into_iter().take(restore_limit).filter(|_| !diagnose) {
        if let Err(e) = app.open(id) {
            app.message = format!("Could not restore tab: {e}");
        }
    }
    if app.profiles.is_empty() && !test_mode && !diagnose && !macro_test_mode {
        app.command(UiCommand {
            action: "new".into(),
            tab_id: None,
            preset: None,
            url: None,
            macro_file: None,
            macro_owner: None,
            playback: None,
            record_moves: None,
            performance_mode: None,
        })?;
    }
    app.layout();
    let mut self_test = test_mode.then(self_test::SelfTest::new);
    let mut macro_test = macro_test_mode.then(macro_test::MacroTest::new);
    let mut next_tick = Instant::now();
    let diagnostic_start = Instant::now();
    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::WaitUntil(next_tick);
        match event {
            Event::UserEvent(AppEvent::ShowCursor(target)) => {
                if target.is_none() || target == app.active {
                    if let Some(t) = app.active.and_then(|id| app.tabs.get(&id)) {
                        match t.view.evaluate_script("window.__hexCursor?.release()") {
                            Ok(()) => app.message = "Mouse released (Ctrl + 1). Click the game to capture it again.".into(),
                            Err(e) => app.message = format!("Could not release mouse: {e}"),
                        }
                    }
                    app.render();
                }
            }
            Event::UserEvent(AppEvent::Ui(raw)) => {
                match serde_json::from_str::<UiCommand>(&raw) {
                    Ok(cmd) => {
                        if cmd.action.starts_with("macro_") && cmd.tab_id != app.active {
                            app.message = "Profile changed; use the selected profile's macro controls".into();
                        } else if let Err(e) = app.command(cmd) {
                            app.message = e.to_string();
                        }
                    }
                    Err(_) => app.message = "Invalid UI command".into(),
                };
                app.render();
            }
            Event::UserEvent(AppEvent::Worker(event)) => {
                app.worker_event(event);
                app.render();
            }
            Event::UserEvent(AppEvent::PageReady(id))=>{if let Some(t)=app.tabs.get_mut(&id){t.render_policy=None;t.background_active=None;t.background_failed=None;}app.apply_render_policies();}
            Event::UserEvent(AppEvent::BackgroundActive(id, enabled, result)) => {
                if let Some(t) = app.tabs.get_mut(&id) {
                    t.background_pending=false;
                    match result {
                        Ok(()) => {t.background_active=Some(enabled);t.background_failed=None;},
                        Err(error) => {t.background_failed=Some(enabled);app.message=format!("Background focus unavailable: {error}");}
                    }
                }
                app.apply_render_policies();
            }
            Event::UserEvent(AppEvent::Macro(event))=>{app.macro_event(event);app.render();}
            Event::UserEvent(AppEvent::MacroLoaded(id,result))=>{app.file_dialog_open=false;match result{Ok(Some(file))=>{if let Err(e)=app.set_macro(id,file){app.message=e.to_string();}else{app.message="Macro opened".into();}},Ok(None)=>{},Err(e)=>app.message=e}app.render();}
            Event::UserEvent(AppEvent::MacroSaved(id,result))=>{app.file_dialog_open=false;match result{Ok(true)=>{app.message="Macro file saved".into();app.log(id,"MACRO_SAVE","success",None);},Ok(false)=>{},Err(e)=>app.message=e}app.render();}
            Event::UserEvent(AppEvent::Record(id, raw)) => {
                app.record(id, &raw);
                app.render();
            }
            Event::UserEvent(AppEvent::Navigated(id, url)) => {
                if app.tabs.get(&id).is_some_and(|t|t.macro_recording.is_some()){let _=app.finish_recording(id);}
                if let Some(p) = app.profiles.iter_mut().find(|p| p.tab_id == id) {
                    p.url = if url == app.demo_url {
                        "about:blank".into()
                    } else if url==format!("{}test",app.demo_url) {
                        "tabpilot:test".into()
                    } else {
                        url
                    };
                    if let Err(e) = app.db.save(p) {
                        app.message = format!("Save failed: {e}");
                    }
                }
            }
            Event::UserEvent(AppEvent::Title(id,title))=>{if let Some(t)=app.tabs.get_mut(&id){t.title=title.chars().take(100).collect();}app.render();}
            Event::UserEvent(AppEvent::Crash(id)) => {
                app.stop_macro(id,false);
                app.engine.remove(id);
                if let Some(t) = app.tabs.get_mut(&id) {
                    t.state = State::Error;
                    t.error = Some("WEBVIEW_PROCESS_FAILED: close and reopen this profile".into());
                }
                app.log(id, "CRASH", "error", Some("WEBVIEW_PROCESS_FAILED"));
                app.render();
            }
            Event::UserEvent(AppEvent::Probe(key, value)) => {
                if let Some(test)=&mut macro_test{test.values.insert(key.clone(),serde_json::from_str(&value).unwrap_or_default());}
                if let Some(test) = &mut self_test {
                    test.values.insert(
                        key,
                        serde_json::from_str(&value).unwrap_or(serde_json::Value::Null),
                    );
                }
            }
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => {
                app.engine.stop_all();
                app.stop_all_macros();
                let size = app
                    .window
                    .inner_size()
                    .to_logical::<u32>(app.window.scale_factor());
                app.config.width = size.width;
                app.config.height = size.height;
                if let Err(e) = app.config.save(&app.root) {
                    app.message = format!("Save failed: {e}");
                }
                *control_flow = ControlFlow::Exit;
            }
            Event::WindowEvent {
                event: WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. },
                ..
            } => app.layout(),
            Event::MainEventsCleared if Instant::now() >= next_tick => {
                    app.resources();
                    app.apply_render_policies();
                    app.render();
                if let Some(test)=&mut macro_test{
                    match test.tick(&mut app){Ok(true)=>*control_flow=ControlFlow::Exit,Ok(false)=>{},Err(error)=>{test.fail(&app,&error.to_string());*control_flow=ControlFlow::Exit;}}
                }
                    if diagnose {
                        let _=app.shell.evaluate_script_with_callback("JSON.stringify({url:location.href,ready:document.readyState,title:document.title,body:document.body?.innerText?.slice(0,500),render:typeof window.render})",|value|eprintln!("SHELL_DIAGNOSTIC {value}"));
                        if diagnostic_start.elapsed()>Duration::from_secs(4){*control_flow=ControlFlow::Exit;}
                    }
                    if let Some(test) = &mut self_test {
                        match test.tick(&mut app) {
                            Ok(true) => {
                                *control_flow = ControlFlow::Exit;
                            }
                            Ok(false) => {}
                            Err(error) => {
                                test.fail(&mut app, &error.to_string());
                                *control_flow = ControlFlow::Exit;
                            }
                        }
                    }
                    next_tick = Instant::now() + Duration::from_millis(500);
            }
            _ => {}
        }
    });
}
impl App {
    fn log(&mut self, id: TabId, action: &str, result: &str, code: Option<&str>) {
        if let Some(p) = self.profiles.iter().find(|p| p.tab_id == id) {
            if let Err(e) = self.db.log(id, p.id, action, result, code) {
                self.message = format!("Log storage failed: {e}");
            }
        }
    }
    fn bounds(&self) -> Rect {
        let s = self
            .window
            .inner_size()
            .to_logical::<f64>(self.window.scale_factor());
        Rect {
            position: LogicalPosition::new(if self.profiles_visible { 200.0 } else { 0.0 }, 100.0)
                .into(),
            size: LogicalSize::new(
                (s.width
                    - if self.profiles_visible { 200.0 } else { 0.0 }
                    - if self.panel { 360.0 } else { 0.0 })
                .max(100.0),
                (s.height - 100.0 - if self.logs_visible { 150.0 } else { 32.0 }).max(100.0),
            )
            .into(),
        }
    }
    fn layout(&mut self) {
        let s = self
            .window
            .inner_size()
            .to_logical::<f64>(self.window.scale_factor());
        let _ = self.shell.set_bounds(Rect {
            position: LogicalPosition::new(0.0, 0.0).into(),
            size: LogicalSize::new(s.width, s.height).into(),
        });
        let bounds = self.bounds();
        for t in self.tabs.values() {
            if t.view.set_bounds(bounds).is_err() {
                self.message = "Unable to resize browser viewport".into();
            }
        }
    }
    fn open(&mut self, id: TabId) -> anyhow::Result<()> {
        if self.tabs.contains_key(&id) {
            return self.select(id);
        }
        let p = self
            .profiles
            .iter()
            .find(|p| p.tab_id == id)
            .ok_or_else(|| anyhow::anyhow!("Unknown profile"))?
            .clone();
        let path = self.root.join("profiles").join(p.id.to_string());
        std::fs::create_dir_all(&path)?;
        let mut context = WebContext::new(Some(path));
        let proxy = self.proxy.clone();
        let nav = self.proxy.clone();
        let title_proxy = self.proxy.clone();
        let load_proxy = self.proxy.clone();
        let url = if p.url == "about:blank" {
            self.demo_url.clone()
        } else if p.url == "tabpilot:test" {
            format!("{}test", self.demo_url)
        } else {
            config::validated_url(&p.url)?
        };
        let view = WebViewBuilder::new_with_web_context(&mut context)
            .with_additional_browser_args("--disable-background-timer-throttling --disable-renderer-backgrounding --disable-backgrounding-occluded-windows")
            .with_url(url)
            .with_bounds(self.bounds())
            .with_focused(false)
            .with_devtools(false)
            .with_initialization_script_for_main_only(include_str!("ui/cursor.js"), false)
            .with_initialization_script(include_str!("ui/performance.js"))
            .with_on_page_load_handler(move|event,_|{if matches!(event,wry::PageLoadEvent::Finished){let _=load_proxy.send_event(AppEvent::PageReady(id));}})
            .with_document_title_changed_handler(move|title|{let _=title_proxy.send_event(AppEvent::Title(id,title));})
            .with_initialization_script(include_str!("ui/record.js"))
            .with_ipc_handler(move |request| {
                if request.body().len() < 1024 {
                    let _ = proxy.send_event(AppEvent::Record(id, request.body().clone()));
                }
            })
            .with_navigation_handler(move |url| {
                if config::validated_url(&url).is_ok() {
                    let _ = nav.send_event(AppEvent::Navigated(id, url));
                    true
                } else {
                    false
                }
            })
            .build_as_child(&self.window)?;
        let proxy = self.proxy.clone();
        platform::watch_crash(&view, move || {
            let _ = proxy.send_event(AppEvent::Crash(id));
        })?;
        let shortcut_proxy = self.proxy.clone();
        platform::watch_cursor_shortcut(&view, move || {
            let _ = shortcut_proxy.send_event(AppEvent::ShowCursor(Some(id)));
        })?;
        self.tabs.insert(
            id,
            LiveTab {
                view,
                _context: context,
                state: State::Online,
                completed: 0,
                error: None,
                recording: false,
                title: "New tab".into(),
                held: Default::default(),
                macro_file: self.db.load_macro(p.id)?,
                macro_recording: None,
                macro_step: 0,
                macro_cycles: 0,
                automation_kind: "click",
                macro_revision: 0,
                playback: Default::default(),
                render_policy: None,
                background_active: None,
                background_pending: false,
                background_failed: None,
                last_input: Instant::now() - Duration::from_secs(5),
            },
        );
        self.engine.add(id, p.id);
        let p = self.profiles.iter_mut().find(|p| p.tab_id == id).unwrap();
        p.open = true;
        self.db.save(p)?;
        self.log(id, "OPEN", "success", None);
        self.select(id)
    }
    fn select(&mut self, id: TabId) -> anyhow::Result<()> {
        self.active = Some(id);
        self.apply_render_policies();
        platform::bring_to_front(&self.tabs[&id].view)?;
        self.tabs[&id].view.focus()?;
        self.form_revision += 1;
        Ok(())
    }
    fn command(&mut self, c: UiCommand) -> anyhow::Result<()> {
        self.message.clear();
        match c.action.as_str() {
            "ready" => {
                self.last_editor = None;
                self.last_ui_payload.clear();
                self.last_render = Instant::now() - Duration::from_secs(1);
                return Ok(());
            }
            "performance" => {
                self.config.performance_mode = c
                    .performance_mode
                    .ok_or_else(|| anyhow::anyhow!("Choose a performance mode"))?;
                self.config.save(&self.root)?;
                self.apply_render_policies();
                return Ok(());
            }
            "new" => {
                let p = Profile::new(self.profiles.len() + 1);
                self.db.save(&p)?;
                let id = p.tab_id;
                self.profiles.push(p);
                return self.open(id);
            }
            "stop_all" => {
                self.engine.stop_all();
                self.stop_all_macros();
                self.stopped = true;
                let ids: Vec<_> = self.tabs.keys().copied().collect();
                for id in ids {
                    if let Some(t) = self.tabs.get_mut(&id) {
                        t.state = State::Online;
                        t.completed = 0;
                    }
                    self.log(id, "GLOBAL_STOP", "success", None);
                }
                return Ok(());
            }
            "panel" => {
                self.panel = !self.panel;
                self.layout();
                return Ok(());
            }
            "profiles" => {
                self.profiles_visible = !self.profiles_visible;
                self.layout();
                return Ok(());
            }
            "logs" => {
                self.logs_visible = !self.logs_visible;
                self.layout();
                return Ok(());
            }
            _ => {}
        }
        let id = c
            .tab_id
            .ok_or_else(|| anyhow::anyhow!("Select a tab first"))?;
        anyhow::ensure!(self.profiles.iter().any(|p| p.tab_id == id), "Unknown tab");
        if c.action == "select" {
            return self.open(id);
        }
        if c.action == "start_saved" && !self.tabs.contains_key(&id) {
            self.open(id)?;
        }
        anyhow::ensure!(self.tabs.contains_key(&id), "Open this profile first");
        if c.action.starts_with("macro_") {
            if c.macro_file.is_some() {
                anyhow::ensure!(
                    c.macro_owner == Some(id),
                    "Macro belongs to another profile; select the profile again"
                );
            }
            return self.macro_command(id, c);
        }
        match c.action.as_str() {
            "close" => {
                self.stop_macro(id, false);
                self.finish_recording(id)?;
                self.engine.remove(id);
                self.tabs.remove(&id);
                let p = self.profiles.iter_mut().find(|p| p.tab_id == id).unwrap();
                p.open = false;
                self.db.save(p)?;
                self.log(id, "CLOSE", "success", None);
                if self.active == Some(id) {
                    self.active = None;
                    if let Some(next) = self
                        .profiles
                        .iter()
                        .find(|p| self.tabs.contains_key(&p.tab_id))
                        .map(|p| p.tab_id)
                    {
                        self.select(next)?;
                    }
                }
            }
            "start" | "start_saved" | "save" => {
                if c.action != "save" {
                    self.stop_macro(id, false);
                    self.finish_recording(id)?;
                    self.tabs.get_mut(&id).unwrap().automation_kind = "click";
                }
                let p = self.profiles.iter_mut().find(|p| p.tab_id == id).unwrap();
                if let Some(preset) = c.preset {
                    preset.validate()?;
                    p.preset = preset;
                    self.db.save(p)?;
                }
                if c.action != "save" {
                    let resume = self.tabs[&id].state == State::Paused;
                    self.engine.start(id, p.preset.clone(), resume)?;
                    self.stopped = false;
                    let t = self.tabs.get_mut(&id).unwrap();
                    t.state = State::Running;
                    t.error = None;
                    if !resume {
                        t.completed = 0;
                    }
                    self.log(id, if resume { "RESUME" } else { "START" }, "success", None);
                } else {
                    self.message = "Preset saved".into();
                    self.log(id, "SAVE", "success", None);
                }
            }
            "pause" | "stop" => {
                self.stop_macro(id, c.action == "pause");
                self.finish_recording(id)?;
                let pause = c.action == "pause";
                self.engine.stop(id, pause);
                let t = self.tabs.get_mut(&id).unwrap();
                t.state = if pause { State::Paused } else { State::Online };
                if !pause {
                    t.completed = 0;
                }
                self.log(id, if pause { "PAUSE" } else { "STOP" }, "success", None);
            }
            "navigate" => {
                let url = config::validated_url(c.url.as_deref().unwrap_or(""))?;
                self.tabs[&id].view.load_url(&url)?;
                self.log(id, "NAVIGATE", "success", None);
            }
            "reload" => {
                self.tabs[&id].view.reload()?;
                self.log(id, "RELOAD", "success", None);
            }
            "back" | "forward" => {
                platform::navigate_history(&self.tabs[&id].view, c.action == "back")?;
            }
            "record" => {
                let t = self.tabs.get_mut(&id).unwrap();
                t.recording = true;
                t.view
                    .evaluate_script("window.__tabpilotRecord && window.__tabpilotRecord()")?;
                self.message = "Click a point in the web content to record it".into();
            }
            _ => anyhow::bail!("Unknown action"),
        }
        Ok(())
    }
    fn record(&mut self, id: TabId, raw: &str) {
        if raw.contains("macro_") {
            self.capture_macro(id, raw);
            return;
        }
        let Some(tab) = self.tabs.get_mut(&id) else {
            return;
        };
        if !tab.recording {
            return;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) else {
            return;
        };
        if v["kind"] != "record" {
            return;
        }
        let (Some(x), Some(y), Some(w), Some(h)) = (
            v["x"].as_f64(),
            v["y"].as_f64(),
            v["viewport_width"].as_f64(),
            v["viewport_height"].as_f64(),
        ) else {
            return;
        };
        let p = self.profiles.iter_mut().find(|p| p.tab_id == id).unwrap();
        let mut preset = p.preset.clone();
        preset.target = Target::Coordinate {
            x,
            y,
            viewport_width: w,
            viewport_height: h,
        };
        if preset.validate().is_err() {
            return;
        }
        tab.recording = false;
        p.preset = preset;
        match self.db.save(p) {
            Ok(()) => {
                self.form_revision += 1;
                self.message = "Point recorded in viewport coordinates".into();
            }
            Err(e) => self.message = format!("Save failed: {e}"),
        }
    }
    fn worker_event(&mut self, event: WorkerEvent) {
        match event {
            WorkerEvent::Click(request) => {
                self.prepare_input(request.tab_id);
                if let Some(tab) = self.tabs.get(&request.tab_id) {
                    platform::dispatch(&tab.view, request);
                } else {
                    let _ = request.reply.send(Err("TAB_CLOSED".into()));
                }
            }
            WorkerEvent::Status {
                tab_id,
                profile_id,
                generation,
                state,
                completed,
                error,
            } => {
                if !self.engine.is_current(tab_id, generation)
                    || self
                        .tabs
                        .get(&tab_id)
                        .is_some_and(|t| t.automation_kind != "click")
                {
                    return;
                }
                if let Some(tab) = self.tabs.get_mut(&tab_id) {
                    let progressed = completed > tab.completed;
                    tab.state = state;
                    tab.completed = completed;
                    tab.error = error.clone();
                    if progressed || error.is_some() {
                        if let Err(e) = self.db.log(
                            tab_id,
                            profile_id,
                            "CLICK",
                            if error.is_some() { "error" } else { "success" },
                            error.as_deref(),
                        ) {
                            self.message = format!("Log storage failed: {e}");
                        }
                    }
                }
            }
        }
    }
    fn prepare_input(&mut self, id: TabId) {
        if let Some(t) = self.tabs.get_mut(&id) {
            t.last_input = Instant::now();
        }
        self.apply_render_policies();
    }
    fn apply_render_policies(&mut self) {
        let mut changed = false;
        for (&id, t) in &mut self.tabs {
            let keep_active = t.state == State::Running || t.macro_recording.is_some();
            if !t.background_pending
                && t.background_active != Some(keep_active)
                && t.background_failed != Some(keep_active)
            {
                t.background_pending = true;
                let proxy = self.proxy.clone();
                platform::set_background_active(&t.view, keep_active, move |result| {
                    let _ = proxy.send_event(AppEvent::BackgroundActive(id, keep_active, result));
                });
            }
            let policy = crate::performance::policy(
                self.config.performance_mode,
                self.active == Some(id),
                t.state == State::Running,
                t.macro_recording.is_some() || t.recording,
                t.last_input.elapsed() < Duration::from_secs(2),
            );
            if t.render_policy == Some(policy) {
                continue;
            }
            if t.view.set_visible(policy.visible).is_err() {
                continue;
            }
            changed = true;
            let _ = t.view.set_memory_usage_level(if policy.visible {
                wry::MemoryUsageLevel::Normal
            } else {
                wry::MemoryUsageLevel::Low
            });
            if t.view
                .evaluate_script(&format!("window.__hexPerformance?.setFps({})", policy.fps))
                .is_ok()
            {
                t.render_policy = Some(policy);
            }
        }
        if changed {
            if let Some(t) = self.active.and_then(|id| self.tabs.get(&id)) {
                let _ = platform::bring_to_front(&t.view);
            }
        }
    }
    fn resources(&mut self) {
        if self.last_resource_update.elapsed() < Duration::from_secs(2) {
            return;
        }
        self.last_resource_update = Instant::now();
        if self.last_process_discovery.elapsed() >= Duration::from_secs(10) {
            self.system.refresh_processes_specifics(
                sysinfo::ProcessesToUpdate::All,
                true,
                sysinfo::ProcessRefreshKind::nothing(),
            );
            let root = sysinfo::Pid::from_u32(std::process::id());
            let mut ids = vec![root];
            loop {
                let new: Vec<_> = self
                    .system
                    .processes()
                    .iter()
                    .filter(|(pid, p)| {
                        !ids.contains(pid) && p.parent().is_some_and(|p| ids.contains(&p))
                    })
                    .map(|(&id, _)| id)
                    .collect();
                if new.is_empty() {
                    break;
                }
                ids.extend(new);
            }
            self.process_ids = ids;
            self.last_process_discovery = Instant::now();
        }
        self.system.refresh_processes_specifics(
            sysinfo::ProcessesToUpdate::Some(&self.process_ids),
            true,
            sysinfo::ProcessRefreshKind::nothing()
                .with_cpu()
                .with_memory(),
        );
        self.cpu = 0.0;
        self.memory_mb = 0.0;
        for &id in &self.process_ids {
            if let Some(p) = self.system.process(id) {
                self.cpu += p.cpu_usage();
                self.memory_mb += p.memory() as f64 / 1048576.0;
            }
        }
        self.cpu /= std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1) as f32;
    }
    fn render(&mut self) {
        if self.last_render.elapsed() < Duration::from_millis(250) {
            return;
        }
        self.last_render = Instant::now();
        let selected = self
            .active
            .and_then(|id| self.tabs.get(&id).map(|t| (id, t)));
        let editor = selected.map(|(id, t)| (id, t.macro_revision));
        let session = selected.and_then(|(_, t)| t.macro_recording);
        let editor_changed = editor != self.last_editor;
        let append = editor_changed
            && session.is_some()
            && session == self.last_recording_session
            && editor.map(|e| e.0) == self.last_editor.map(|e| e.0)
            && selected.is_some_and(|(_, t)| t.macro_file.steps.len() >= self.last_recorded_steps);
        let editor_append = if append {
            selected.map(|(_,t)|json!({"steps":&t.macro_file.steps[self.last_recorded_steps..],"viewport":t.macro_file.viewport}))
        } else {
            None
        };
        let full_editor = editor_changed && !append;
        let profiles:Vec<_>=self.profiles.iter().map(|p|{let t=self.tabs.get(&p.tab_id);json!({"tab_id":p.tab_id,"name":p.name,"title":t.map(|t|t.title.as_str()).unwrap_or(""),"playback":t.map(|t|&t.playback),"macro_file":t.filter(|_|self.active==Some(p.tab_id)&&full_editor).map(|t|&t.macro_file),"macro_recording":t.is_some_and(|t|t.macro_recording.is_some()),"macro_step":t.map(|t|t.macro_step).unwrap_or(0),"macro_cycles":t.map(|t|t.macro_cycles).unwrap_or(0),"automation_kind":t.map(|t|t.automation_kind).unwrap_or("macro"),"url":p.url,"preset":p.preset,"open":t.is_some(),"state":t.map(|t|t.state).unwrap_or(State::Offline),"completed":t.map(|t|t.completed).unwrap_or(0),"error":t.and_then(|t|t.error.as_ref())})}).collect();
        let logs = if self.logs_visible {
            self.active
                .and_then(|id| self.db.logs(id).ok())
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let data = json!({"editor_owner":self.active,"editor_update":full_editor,"editor_append":editor_append,"performance_mode":self.config.performance_mode,"profiles":profiles,"active":self.active,"running":self.tabs.values().filter(|t|t.state==State::Running).count(),"tabs":self.tabs.len(),"stopped":self.stopped,"panel":self.panel,"profiles_visible":self.profiles_visible,"logs_visible":self.logs_visible,"message":self.message,"form_revision":self.form_revision,"theme":self.config.theme,"cpu":self.cpu,"memory_mb":self.memory_mb,"logs":logs});
        let payload = data.to_string();
        if payload == self.last_ui_payload {
            return;
        }
        if self
            .shell
            .evaluate_script(&format!("window.render && window.render({payload})"))
            .is_ok()
        {
            self.last_ui_payload = payload;
            self.last_editor = editor;
            self.last_recording_session = session;
            self.last_recorded_steps = selected.map(|(_, t)| t.macro_file.steps.len()).unwrap_or(0);
        }
    }
}
fn start_demo_server(test_mode: bool) -> anyhow::Result<String> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let address = listener.local_addr()?;
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
            let _ = stream.set_write_timeout(Some(Duration::from_secs(1)));
            let mut buffer = [0; 2048];
            if stream.read(&mut buffer).is_err() {
                continue;
            }
            let request = String::from_utf8_lossy(&buffer);
            let body = if test_mode || request.starts_with("GET /test ") {
                include_str!("ui/demo.html")
            } else {
                include_str!("ui/new_tab.html")
            };
            let response=format!("HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}",body.len());
            let _ = stream.write_all(response.as_bytes());
        }
    });
    Ok(format!("http://{address}/"))
}
