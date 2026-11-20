//! Portable, versioned macros. Files contain data only, never executable scripts.
use crate::config::{MouseButton, TabId};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::{sync::oneshot, task::JoinHandle};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub const MAX_STEPS: usize = 10_000;
pub const MAX_FILE_BYTES: u64 = 4_194_304;
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Viewport {
    pub width: f64,
    pub height: f64,
}
impl Default for Viewport {
    fn default() -> Self {
        Self {
            width: 1000.0,
            height: 600.0,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    KeyDown {
        key: String,
        code: String,
        virtual_key: u32,
        #[serde(default)]
        modifiers: u8,
    },
    KeyUp {
        key: String,
        code: String,
        virtual_key: u32,
        #[serde(default)]
        modifiers: u8,
    },
    MouseDown {
        x: f64,
        y: f64,
        button: MouseButton,
        #[serde(default)]
        modifiers: u8,
    },
    MouseUp {
        x: f64,
        y: f64,
        button: MouseButton,
        #[serde(default)]
        modifiers: u8,
    },
    MouseMove {
        x: f64,
        y: f64,
        #[serde(default)]
        modifiers: u8,
    },
    Wheel {
        x: f64,
        y: f64,
        delta_x: f64,
        delta_y: f64,
        #[serde(default)]
        modifiers: u8,
    },
    Wait,
}
impl Action {
    pub fn release(&self) -> Option<Self> {
        match self {
            Self::KeyDown {
                key,
                code,
                virtual_key,
                ..
            } => Some(Self::KeyUp {
                key: key.clone(),
                code: code.clone(),
                virtual_key: *virtual_key,
                modifiers: 0,
            }),
            Self::MouseDown { x, y, button, .. } => Some(Self::MouseUp {
                x: *x,
                y: *y,
                button: button.clone(),
                modifiers: 0,
            }),
            _ => None,
        }
    }
    pub fn hold_id(&self) -> Option<String> {
        match self {
            Self::KeyDown { code, .. } | Self::KeyUp { code, .. } => Some(format!("key:{code}")),
            Self::MouseDown { button, .. } | Self::MouseUp { button, .. } => {
                Some(format!("mouse:{}", button.cdp()))
            }
            _ => None,
        }
    }
    pub fn is_down(&self) -> bool {
        matches!(self, Self::KeyDown { .. } | Self::MouseDown { .. })
    }
    pub fn position(&self) -> Option<(f64, f64)> {
        match self {
            Self::MouseDown { x, y, .. }
            | Self::MouseUp { x, y, .. }
            | Self::MouseMove { x, y, .. }
            | Self::Wheel { x, y, .. } => Some((*x, *y)),
            _ => None,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Step {
    pub delay_ms: u64,
    pub action: Action,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct MacroFile {
    pub format: String,
    pub version: u32,
    pub name: String,
    pub viewport: Viewport,
    pub steps: Vec<Step>,
}
impl Default for MacroFile {
    fn default() -> Self {
        Self {
            format: "tabpilot-macro".into(),
            version: 1,
            name: "Untitled macro".into(),
            viewport: Viewport::default(),
            steps: Vec::new(),
        }
    }
}
impl MacroFile {
    pub fn save_file(&self, path: &std::path::Path) -> anyhow::Result<()> {
        self.validate()?;
        let parent = path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("Choose a file inside a folder"))?;
        let temp = parent.join(format!(".tabpilot-{}.tmp", Uuid::new_v4()));
        let result = (|| -> anyhow::Result<()> {
            use std::io::Write;
            let mut out = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp)?;
            out.write_all(serde_json::to_string_pretty(self)?.as_bytes())?;
            out.sync_all()?;
            drop(out);
            std::fs::rename(&temp, path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
        result
    }
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.format == "tabpilot-macro" && self.version == 1,
            "Unsupported macro file format or version"
        );
        anyhow::ensure!(
            !self.name.trim().is_empty() && self.name.len() <= 200,
            "Macro name must be 1 to 200 characters"
        );
        anyhow::ensure!(
            self.viewport.width.is_finite()
                && self.viewport.height.is_finite()
                && self.viewport.width > 0.0
                && self.viewport.height > 0.0,
            "Invalid recorded viewport"
        );
        anyhow::ensure!(self.steps.len() <= MAX_STEPS, "Macro exceeds 10000 steps");
        let mut duration = 0u64;
        let mut held = BTreeMap::new();
        for (i, step) in self.steps.iter().enumerate() {
            anyhow::ensure!(
                step.delay_ms <= 3_600_000,
                "Step {} delay exceeds one hour",
                i + 1
            );
            duration = duration.saturating_add(step.delay_ms);
            anyhow::ensure!(duration <= 86_400_000, "Macro exceeds 24 hours");
            if let Some((x, y)) = step.action.position() {
                anyhow::ensure!(
                    x.is_finite()
                        && y.is_finite()
                        && x >= 0.0
                        && y >= 0.0
                        && x < self.viewport.width
                        && y < self.viewport.height,
                    "Step {} is outside recorded viewport",
                    i + 1
                );
            }
            match &step.action {
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
                } => anyhow::ensure!(
                    !key.is_empty()
                        && key.len() <= 64
                        && !code.is_empty()
                        && code.len() <= 64
                        && code.chars().all(|c| c.is_ascii_alphanumeric())
                        && *virtual_key <= 255
                        && *modifiers <= 15,
                    "Invalid key in step {}",
                    i + 1
                ),
                Action::Wheel {
                    delta_x,
                    delta_y,
                    modifiers,
                    ..
                } => anyhow::ensure!(
                    delta_x.is_finite()
                        && delta_y.is_finite()
                        && delta_x.abs() <= 10000.0
                        && delta_y.abs() <= 10000.0
                        && *modifiers <= 15,
                    "Invalid wheel step {}",
                    i + 1
                ),
                Action::MouseDown { modifiers, .. }
                | Action::MouseUp { modifiers, .. }
                | Action::MouseMove { modifiers, .. } => {
                    anyhow::ensure!(*modifiers <= 15, "Invalid mouse modifiers")
                }
                _ => {}
            }
            if let Some(id) = step.action.hold_id() {
                if step.action.is_down() {
                    anyhow::ensure!(
                        held.insert(id, true).is_none(),
                        "Step {} presses an already-held input",
                        i + 1
                    );
                } else {
                    anyhow::ensure!(
                        held.remove(&id).is_some(),
                        "Step {} releases an input that is not held",
                        i + 1
                    );
                }
            }
        }
        anyhow::ensure!(
            held.is_empty(),
            "Macro ends with held keys or mouse buttons; add release steps"
        );
        Ok(())
    }
    pub fn held_before(&self, index: usize) -> Vec<Action> {
        let mut held = BTreeMap::new();
        for s in self.steps.iter().take(index) {
            if let Some(id) = s.action.hold_id() {
                if s.action.is_down() {
                    held.insert(id, s.action.clone());
                } else {
                    held.remove(&id);
                }
            }
        }
        held.into_values().collect()
    }
    pub fn finish_recording(&mut self) {
        for action in self.held_before(self.steps.len()) {
            if let Some(action) = action.release() {
                self.steps.push(Step {
                    delay_ms: 0,
                    action,
                });
            }
        }
    }
    pub fn from_json(text: &str) -> anyhow::Result<Self> {
        anyhow::ensure!(
            text.len() as u64 <= MAX_FILE_BYTES,
            "Macro file exceeds 4 MB"
        );
        let file: Self = serde_json::from_str(text)?;
        file.validate()?;
        Ok(file)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlaybackOptions {
    pub repeats: Option<u64>,
    pub speed: f64,
}
impl Default for PlaybackOptions {
    fn default() -> Self {
        Self {
            repeats: Some(1),
            speed: 1.0,
        }
    }
}
impl PlaybackOptions {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.repeats != Some(0) && self.speed.is_finite() && (0.1..=10.0).contains(&self.speed),
            "Repeat must be positive and speed must be 0.1 to 10"
        );
        Ok(())
    }
}
pub struct Request {
    pub tab_id: TabId,
    pub run_id: Uuid,
    pub action: Action,
    pub viewport: Viewport,
    pub cancel: CancellationToken,
    pub reply: oneshot::Sender<Result<(), String>>,
}
pub enum Event {
    Input(Request),
    Status {
        tab_id: TabId,
        run_id: Uuid,
        state: crate::automation::State,
        step: usize,
        cycles: u64,
        error: Option<String>,
    },
}
type Sink = Arc<dyn Fn(Event) + Send + Sync>;
#[derive(Default)]
struct Cursor {
    index: usize,
    cycles: u64,
    deadline: Option<Instant>,
    remaining: Option<Duration>,
}
struct Job {
    file: MacroFile,
    options: PlaybackOptions,
    cursor: Arc<Mutex<Cursor>>,
    cancel: CancellationToken,
    run_id: Uuid,
    task: Option<JoinHandle<()>>,
    paused: bool,
}
pub struct Player {
    jobs: HashMap<TabId, Job>,
    runtime: tokio::runtime::Handle,
    sink: Sink,
}
impl Player {
    pub fn new(runtime: tokio::runtime::Handle, sink: Sink) -> Self {
        Self {
            jobs: HashMap::new(),
            runtime,
            sink,
        }
    }
    pub fn play(
        &mut self,
        id: TabId,
        file: MacroFile,
        options: PlaybackOptions,
        resume: bool,
    ) -> anyhow::Result<()> {
        file.validate()?;
        options.validate()?;
        anyhow::ensure!(!file.steps.is_empty(), "Record or add macro steps first");
        let cursor = if resume {
            self.jobs
                .get(&id)
                .filter(|j| j.paused && j.file == file)
                .map(|j| j.cursor.clone())
                .unwrap_or_default()
        } else {
            Arc::new(Mutex::new(Cursor::default()))
        };
        self.stop(id);
        let cancel = CancellationToken::new();
        let run_id = Uuid::new_v4();
        let task = self.runtime.spawn(play(
            id,
            run_id,
            file.clone(),
            options.clone(),
            cursor.clone(),
            cancel.clone(),
            self.sink.clone(),
        ));
        self.jobs.insert(
            id,
            Job {
                file,
                options,
                cursor,
                cancel,
                run_id,
                task: Some(task),
                paused: false,
            },
        );
        Ok(())
    }
    pub fn pause(&mut self, id: TabId) {
        if let Some(j) = self.jobs.get_mut(&id) {
            j.cancel.cancel();
            {
                let mut c = j.cursor.lock().unwrap();
                if let Some(deadline) = c.deadline.take() {
                    c.remaining = Some(deadline.saturating_duration_since(Instant::now()));
                }
            }
            if let Some(task) = j.task.take() {
                task.abort();
            }
            j.paused = true;
        }
    }
    pub fn stop(&mut self, id: TabId) {
        if let Some(j) = self.jobs.remove(&id) {
            j.cancel.cancel();
            if let Some(task) = j.task {
                task.abort();
            }
        }
    }
    pub fn stop_all(&mut self) {
        let ids: Vec<_> = self.jobs.keys().copied().collect();
        for id in ids {
            self.stop(id);
        }
    }
    pub fn current(&self, id: TabId, run: Uuid) -> bool {
        self.jobs
            .get(&id)
            .is_some_and(|j| j.run_id == run && !j.cancel.is_cancelled())
    }
    pub fn resume_options(&self, id: TabId) -> Option<PlaybackOptions> {
        self.jobs
            .get(&id)
            .filter(|j| j.paused)
            .map(|j| j.options.clone())
    }
}
impl Drop for Player {
    fn drop(&mut self) {
        self.stop_all();
    }
}
async fn input(
    id: TabId,
    run: Uuid,
    action: Action,
    viewport: Viewport,
    cancel: CancellationToken,
    sink: &Sink,
) -> Result<(), String> {
    if cancel.is_cancelled() {
        return Err("CANCELLED".into());
    }
    if action == Action::Wait {
        return Ok(());
    }
    let (tx, rx) = oneshot::channel();
    sink(Event::Input(Request {
        tab_id: id,
        run_id: run,
        action,
        viewport,
        cancel: cancel.clone(),
        reply: tx,
    }));
    tokio::select! {biased;_=cancel.cancelled()=>Err("CANCELLED".into()),r=tokio::time::timeout(Duration::from_secs(5),rx)=>match r{Ok(Ok(result))=>result,_=>Err("MACRO_INPUT_TIMEOUT".into())}}
}
async fn play(
    id: TabId,
    run: Uuid,
    file: MacroFile,
    options: PlaybackOptions,
    cursor: Arc<Mutex<Cursor>>,
    cancel: CancellationToken,
    sink: Sink,
) {
    let initial = cursor.lock().unwrap().index;
    // Re-establish held inputs after pause; Pause itself always releases them.
    for action in file.held_before(initial) {
        if let Err(error) = input(
            id,
            run,
            action,
            file.viewport.clone(),
            cancel.clone(),
            &sink,
        )
        .await
        {
            if !cancel.is_cancelled() {
                sink(Event::Status {
                    tab_id: id,
                    run_id: run,
                    state: crate::automation::State::Error,
                    step: initial,
                    cycles: 0,
                    error: Some(error),
                });
            }
            return;
        }
    }
    loop {
        if cancel.is_cancelled() {
            return;
        }
        let (index, cycles, delay) = {
            let mut c = cursor.lock().unwrap();
            let i = c.index;
            let delay = c.remaining.take().unwrap_or_else(|| {
                Duration::from_secs_f64(file.steps[i].delay_ms as f64 / 1000.0 / options.speed)
            });
            c.deadline = Some(Instant::now() + delay);
            (i, c.cycles, delay)
        };
        sink(Event::Status {
            tab_id: id,
            run_id: run,
            state: crate::automation::State::Running,
            step: index,
            cycles,
            error: None,
        });
        tokio::select! {biased;_=cancel.cancelled()=>return,_=tokio::time::sleep(delay)=>{}}
        {
            cursor.lock().unwrap().deadline = None;
        }
        if let Err(error) = input(
            id,
            run,
            file.steps[index].action.clone(),
            file.viewport.clone(),
            cancel.clone(),
            &sink,
        )
        .await
        {
            if !cancel.is_cancelled() {
                sink(Event::Status {
                    tab_id: id,
                    run_id: run,
                    state: crate::automation::State::Error,
                    step: index,
                    cycles,
                    error: Some(error),
                });
            }
            return;
        }
        let done = {
            let mut c = cursor.lock().unwrap();
            c.index += 1;
            if c.index == file.steps.len() {
                c.index = 0;
                c.cycles += 1;
            }
            options.repeats.is_some_and(|n| c.cycles >= n)
        };
        if done {
            let cycles = cursor.lock().unwrap().cycles;
            sink(Event::Status {
                tab_id: id,
                run_id: run,
                state: crate::automation::State::Online,
                step: file.steps.len(),
                cycles,
                error: None,
            });
            return;
        }
        // Yield even for all-zero-delay repeats. Never spin an unbounded empty loop.
        tokio::select! {biased;_=cancel.cancelled()=>return,_=tokio::time::sleep(Duration::from_millis(1))=>{}}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn hold() -> MacroFile {
        let down = Action::KeyDown {
            key: "w".into(),
            code: "KeyW".into(),
            virtual_key: 87,
            modifiers: 0,
        };
        MacroFile {
            steps: vec![
                Step {
                    delay_ms: 0,
                    action: down.clone(),
                },
                Step {
                    delay_ms: 100,
                    action: down.release().unwrap(),
                },
            ],
            ..MacroFile::default()
        }
    }
    #[test]
    fn portable_roundtrip() {
        let f = hold();
        let s = serde_json::to_string(&f).unwrap();
        assert_eq!(MacroFile::from_json(&s).unwrap(), f);
        assert!(!s.contains("tab_id"));
    }
    #[test]
    fn reject_unreleased_keys_and_unknown_versions() {
        let mut f = hold();
        f.steps.pop();
        assert!(f.validate().is_err());
        f.finish_recording();
        assert!(f.validate().is_ok());
        f.version = 2;
        assert!(f.validate().is_err());
    }
    #[test]
    fn held_state_for_diagonal_resume() {
        let mut f = hold();
        let d = Action::KeyDown {
            key: "d".into(),
            code: "KeyD".into(),
            virtual_key: 68,
            modifiers: 0,
        };
        f.steps.insert(
            1,
            Step {
                delay_ms: 0,
                action: d.clone(),
            },
        );
        f.steps.push(Step {
            delay_ms: 0,
            action: d.release().unwrap(),
        });
        assert!(f.validate().is_ok());
        assert_eq!(f.held_before(2).len(), 2);
        assert_eq!(f.held_before(4).len(), 0);
    }
    #[test]
    fn schema_does_not_allow_script_actions() {
        assert!(MacroFile::from_json(r#"{"format":"tabpilot-macro","version":1,"name":"bad","viewport":{"width":10,"height":10},"steps":[{"delay_ms":0,"action":{"type":"javascript","code":"alert(1)"}}]}"#).is_err());
    }
    #[test]
    fn repeats_and_order_are_per_tab() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let mut p = Player::new(
            rt.handle().clone(),
            Arc::new(move |e| {
                let _ = tx.send(e);
            }),
        );
        let id = Uuid::new_v4();
        p.play(
            id,
            hold(),
            PlaybackOptions {
                repeats: Some(2),
                speed: 10.0,
            },
            false,
        )
        .unwrap();
        let mut actions = Vec::new();
        loop {
            match rx.recv_timeout(Duration::from_secs(2)).unwrap() {
                Event::Input(r) => {
                    assert_eq!(r.tab_id, id);
                    actions.push(r.action.clone());
                    r.reply.send(Ok(())).unwrap();
                }
                Event::Status {
                    state: crate::automation::State::Online,
                    cycles,
                    ..
                } => {
                    assert_eq!(cycles, 2);
                    break;
                }
                _ => {}
            }
        }
        assert_eq!(actions.len(), 4);
        assert!(actions[0].is_down());
        assert!(!actions[1].is_down());
    }
    #[test]
    fn stop_cancels_queued_input() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let mut p = Player::new(
            rt.handle().clone(),
            Arc::new(move |e| {
                let _ = tx.send(e);
            }),
        );
        let id = Uuid::new_v4();
        p.play(id, hold(), PlaybackOptions::default(), false)
            .unwrap();
        let request = loop {
            if let Event::Input(r) = rx.recv_timeout(Duration::from_secs(2)).unwrap() {
                break r;
            }
        };
        p.stop(id);
        assert!(request.cancel.is_cancelled());
        assert!(!p.current(id, request.run_id));
    }
    #[test]
    fn pause_resume_restores_hold_and_remaining_delay() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let mut p = Player::new(
            rt.handle().clone(),
            Arc::new(move |e| {
                let _ = tx.send(e);
            }),
        );
        let id = Uuid::new_v4();
        let file = hold();
        p.play(id, file.clone(), PlaybackOptions::default(), false)
            .unwrap();
        loop {
            match rx.recv_timeout(Duration::from_secs(2)).unwrap() {
                Event::Input(r) => {
                    r.reply.send(Ok(())).unwrap();
                }
                Event::Status { step: 1, .. } => break,
                _ => {}
            }
        }
        p.pause(id);
        assert!(p.jobs[&id].cursor.lock().unwrap().remaining.is_some());
        p.play(id, file, PlaybackOptions::default(), true).unwrap();
        let mut actions = Vec::new();
        loop {
            match rx.recv_timeout(Duration::from_secs(2)).unwrap() {
                Event::Input(r) => {
                    actions.push(r.action.clone());
                    r.reply.send(Ok(())).unwrap();
                }
                Event::Status {
                    state: crate::automation::State::Online,
                    ..
                } => break,
                _ => {}
            }
        }
        assert_eq!(actions.len(), 2);
        assert!(actions[0].is_down());
        assert!(!actions[1].is_down());
    }
    #[test]
    fn atomic_file_save_can_replace_a_macro() {
        let path = std::env::temp_dir().join(format!("tabpilot-macro-{}.json", Uuid::new_v4()));
        let mut file = hold();
        file.save_file(&path).unwrap();
        file.name = "Revised macro".into();
        file.save_file(&path).unwrap();
        assert_eq!(
            MacroFile::from_json(&std::fs::read_to_string(&path).unwrap()).unwrap(),
            file
        );
        std::fs::remove_file(path).unwrap();
    }
}
