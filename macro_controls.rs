use super::*;
use macros::{Action, MacroFile, Step, Viewport};

impl App {
    pub(super) fn set_macro(&mut self, id: TabId, file: MacroFile) -> anyhow::Result<()> {
        file.validate()?;
        let t = self
            .tabs
            .get_mut(&id)
            .ok_or_else(|| anyhow::anyhow!("Profile was closed"))?;
        anyhow::ensure!(
            t.state != State::Running && t.macro_recording.is_none(),
            "Stop playback or recording before replacing the macro"
        );
        t.macro_file = file;
        t.macro_revision += 1;
        t.macro_step = 0;
        t.macro_cycles = 0;
        let profile = self.profiles.iter().find(|p| p.tab_id == id).unwrap().id;
        self.db.save_macro(profile, &t.macro_file)?;
        self.form_revision += 1;
        Ok(())
    }
    pub(super) fn stop_macro(&mut self, id: TabId, pause: bool) {
        if pause {
            self.macro_player.pause(id);
        } else {
            self.macro_player.stop(id);
        }
        if let Some(t) = self.tabs.get_mut(&id) {
            t.last_input = Instant::now();
            platform::release_all(&t.view, &t.held);
        }
    }
    pub(super) fn stop_all_macros(&mut self) {
        self.macro_player.stop_all();
        let ids: Vec<_> = self.tabs.keys().copied().collect();
        for id in ids {
            if let Some(t) = self.tabs.get_mut(&id) {
                t.last_input = Instant::now();
                platform::release_all(&t.view, &t.held);
            }
            if let Err(e) = self.finish_recording(id) {
                self.message = e.to_string();
            }
        }
    }
    pub(super) fn finish_recording(&mut self, id: TabId) -> anyhow::Result<()> {
        let Some(t) = self.tabs.get_mut(&id) else {
            return Ok(());
        };
        if t.macro_recording.take().is_none() {
            return Ok(());
        }
        let _ = t.view.evaluate_script("window.__tabpilotMacro?.stop()");
        t.macro_file.finish_recording();
        t.macro_revision += 1;
        t.macro_file.validate()?;
        t.state = State::Online;
        let profile = self.profiles.iter().find(|p| p.tab_id == id).unwrap().id;
        self.db.save_macro(profile, &t.macro_file)?;
        self.form_revision += 1;
        self.log(id, "RECORD_STOP", "success", None);
        self.message = "Recording saved to this profile. Use Save file to export it.".into();
        Ok(())
    }
    pub(super) fn macro_command(&mut self, id: TabId, c: UiCommand) -> anyhow::Result<()> {
        match c.action.as_str() {
            "macro_new" => {
                self.stop_macro(id, false);
                self.engine.stop(id, false);
                self.finish_recording(id)?;
                self.tabs.get_mut(&id).unwrap().state = State::Online;
                self.set_macro(id, MacroFile::default())?;
            }
            "macro_update" => {
                self.set_macro(
                    id,
                    c.macro_file
                        .ok_or_else(|| anyhow::anyhow!("Missing macro data"))?,
                )?;
            }
            "macro_record" => {
                self.stop_macro(id, false);
                self.engine.stop(id, false);
                self.finish_recording(id)?;
                let session = TabId::new_v4();
                let t = self.tabs.get_mut(&id).unwrap();
                t.macro_file = MacroFile::default();
                t.macro_revision += 1;
                t.macro_recording = Some(session);
                t.macro_step = 0;
                t.macro_cycles = 0;
                t.error = None;
                t.automation_kind = "record";
                t.state = State::Online;
                t.view.evaluate_script(&format!(
                    "window.__tabpilotMacro?.start({}, {})",
                    serde_json::to_string(&session.to_string())?,
                    c.record_moves.unwrap_or(true)
                ))?;
                t.view.focus()?;
                self.form_revision += 1;
                self.message =
                    "Recording this profile. Interact with its page, then press Finish recording."
                        .into();
                self.log(id, "RECORD_START", "success", None);
            }
            "macro_record_stop" => {
                self.finish_recording(id)?;
            }
            "macro_play" => {
                self.finish_recording(id)?;
                anyhow::ensure!(
                    self.tabs[&id].state != State::Running,
                    "Pause or stop the current automation first"
                );
                let resume = self.tabs[&id].state == State::Paused
                    && self.tabs[&id].automation_kind == "macro";
                let file = c
                    .macro_file
                    .unwrap_or_else(|| self.tabs[&id].macro_file.clone());
                file.validate()?;
                let options = c
                    .playback
                    .or_else(|| self.macro_player.resume_options(id))
                    .unwrap_or_else(|| self.tabs[&id].playback.clone());
                self.engine.stop(id, false);
                self.tabs.get_mut(&id).unwrap().automation_kind = "macro";
                self.macro_player
                    .play(id, file.clone(), options.clone(), resume)?;
                let t = self.tabs.get_mut(&id).unwrap();
                t.macro_file = file;
                t.playback = options;
                t.macro_revision += 1;
                t.state = State::Running;
                t.error = None;
                self.stopped = false;
                self.log(
                    id,
                    if resume { "MACRO_RESUME" } else { "MACRO_PLAY" },
                    "success",
                    None,
                );
            }
            "macro_pause" => {
                anyhow::ensure!(
                    self.tabs[&id].automation_kind == "macro"
                        && self.tabs[&id].state == State::Running,
                    "No macro is currently playing"
                );
                self.stop_macro(id, true);
                self.tabs.get_mut(&id).unwrap().state = State::Paused;
                self.log(id, "MACRO_PAUSE", "success", None);
            }
            "macro_stop" => {
                self.engine.stop(id, false);
                self.stop_macro(id, false);
                self.finish_recording(id)?;
                let t = self.tabs.get_mut(&id).unwrap();
                t.state = State::Online;
                t.macro_step = 0;
                self.log(id, "MACRO_STOP", "success", None);
            }
            "macro_open" => {
                anyhow::ensure!(!self.file_dialog_open, "A file dialog is already open");
                self.file_dialog_open = true;
                let proxy = self.proxy.clone();
                std::thread::spawn(move || {
                    let result = (|| -> anyhow::Result<Option<MacroFile>> {
                        let Some(path) = rfd::FileDialog::new()
                            .set_title("Open macro file")
                            .add_filter("HexBot PB MOBILE macro", &["json"])
                            .pick_file()
                        else {
                            return Ok(None);
                        };
                        anyhow::ensure!(
                            std::fs::metadata(&path)?.len() <= macros::MAX_FILE_BYTES,
                            "Macro file exceeds 4 MB"
                        );
                        Ok(Some(MacroFile::from_json(&std::fs::read_to_string(path)?)?))
                    })()
                    .map_err(|e| e.to_string());
                    let _ = proxy.send_event(AppEvent::MacroLoaded(id, result));
                });
            }
            "macro_save" => {
                self.finish_recording(id)?;
                if let Some(file) = c.macro_file {
                    self.set_macro(id, file)?;
                }
                anyhow::ensure!(!self.file_dialog_open, "A file dialog is already open");
                let file = self.tabs[&id].macro_file.clone();
                file.validate()?;
                self.file_dialog_open = true;
                let proxy = self.proxy.clone();
                std::thread::spawn(move || {
                    let result = (|| -> anyhow::Result<bool> {
                        let Some(path) = rfd::FileDialog::new()
                            .set_title("Save macro file")
                            .set_file_name("macro.tabmacro.json")
                            .add_filter("HexBot PB MOBILE macro", &["json"])
                            .save_file()
                        else {
                            return Ok(false);
                        };
                        file.save_file(&path)?;
                        Ok(true)
                    })()
                    .map_err(|e| e.to_string());
                    let _ = proxy.send_event(AppEvent::MacroSaved(id, result));
                });
            }
            _ => anyhow::bail!("Unknown macro action"),
        }
        Ok(())
    }
    pub(super) fn capture_macro(&mut self, id: TabId, raw: &str) {
        let Some(t) = self.tabs.get_mut(&id) else {
            return;
        };
        let Some(session) = t.macro_recording else {
            return;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) else {
            return;
        };
        if v["session"] != session.to_string() {
            return;
        }
        if v["kind"] == "macro_begin" {
            if let Ok(viewport) = serde_json::from_value::<Viewport>(v["viewport"].clone()) {
                if viewport.width.is_finite()
                    && viewport.height.is_finite()
                    && viewport.width > 0.0
                    && viewport.height > 0.0
                {
                    t.macro_file.viewport = viewport;
                    t.macro_revision += 1;
                }
            }
            return;
        }
        if v["kind"] != "macro_event" {
            return;
        }
        let Ok(step) = serde_json::from_value::<Step>(v["step"].clone()) else {
            return;
        };
        // Validate each captured action in a balanced miniature macro before storing.
        let mut probe = MacroFile {
            viewport: t.macro_file.viewport.clone(),
            ..MacroFile::default()
        };
        match &step.action {
            Action::KeyUp {
                key,
                code,
                virtual_key,
                modifiers,
            } => probe.steps.push(Step {
                delay_ms: 0,
                action: Action::KeyDown {
                    key: key.clone(),
                    code: code.clone(),
                    virtual_key: *virtual_key,
                    modifiers: *modifiers,
                },
            }),
            Action::MouseUp {
                x,
                y,
                button,
                modifiers,
            } => probe.steps.push(Step {
                delay_ms: 0,
                action: Action::MouseDown {
                    x: *x,
                    y: *y,
                    button: button.clone(),
                    modifiers: *modifiers,
                },
            }),
            _ => {}
        }
        probe.steps.push(step.clone());
        probe.finish_recording();
        if probe.validate().is_err() {
            return;
        }
        if t.macro_file.steps.len() >= macros::MAX_STEPS - 64 {
            let _ = self.finish_recording(id);
            self.message = "Recording stopped at the step limit".into();
            return;
        }
        t.macro_file.steps.push(step);
        t.macro_revision += 1;
    }
    pub(super) fn macro_event(&mut self, event: macros::Event) {
        match event {
            macros::Event::Input(request) => {
                if !self.macro_player.current(request.tab_id, request.run_id) {
                    let _ = request.reply.send(Err("CANCELLED".into()));
                    return;
                }
                self.prepare_input(request.tab_id);
                if let Some(t) = self.tabs.get(&request.tab_id) {
                    platform::macro_input(&t.view, t.held.clone(), request);
                } else {
                    let _ = request.reply.send(Err("TAB_CLOSED".into()));
                }
            }
            macros::Event::Status {
                tab_id,
                run_id,
                state,
                step,
                cycles,
                error,
            } => {
                if !self.macro_player.current(tab_id, run_id) {
                    return;
                }
                if let Some(t) = self.tabs.get_mut(&tab_id) {
                    t.state = state;
                    t.macro_step = step;
                    t.macro_cycles = cycles;
                    t.error = error.clone();
                }
                if state != State::Running {
                    self.stop_macro(tab_id, false);
                    self.log(
                        tab_id,
                        "MACRO_FINISH",
                        if error.is_some() { "error" } else { "success" },
                        error.as_deref(),
                    );
                }
            }
        }
    }
}
