//! Hidden-window WebView2 smoke test; never attaches to a user's browser session.
use super::*;
use serde_json::Value;
pub struct MacroTest {
    phase: u8,
    due: Instant,
    id: Option<TabId>,
    pub values: HashMap<String, Value>,
    checks: Vec<Value>,
}
impl MacroTest {
    pub fn new() -> Self {
        Self {
            phase: 0,
            due: Instant::now() + Duration::from_secs(1),
            id: None,
            values: HashMap::new(),
            checks: Vec::new(),
        }
    }
    fn next(&mut self, ms: u64) {
        self.phase += 1;
        self.due = Instant::now() + Duration::from_millis(ms);
    }
    fn check(&mut self, name: &str, passed: bool, details: Value) {
        self.checks
            .push(json!({"name":name,"passed":passed,"details":details}));
    }
    fn probe(&self, app: &App, key: &str) -> anyhow::Result<()> {
        let p = app.proxy.clone();
        let key = key.to_owned();
        app.tabs[&self.id.unwrap()]
            .view
            .evaluate_script_with_callback(
                "({events:window.testEvents,down:Array.from(window.testDown)})",
                move |v| {
                    let _ = p.send_event(AppEvent::Probe(key.clone(), v));
                },
            )?;
        Ok(())
    }
    fn file(hold: u64, diagonal: bool) -> macros::MacroFile {
        use macros::{Action, MacroFile, Step};
        let w = Action::KeyDown {
            key: "w".into(),
            code: "KeyW".into(),
            virtual_key: 87,
            modifiers: 0,
        };
        let d = Action::KeyDown {
            key: "d".into(),
            code: "KeyD".into(),
            virtual_key: 68,
            modifiers: 0,
        };
        let mut steps = vec![Step {
            delay_ms: 0,
            action: w.clone(),
        }];
        if diagonal {
            steps.push(Step {
                delay_ms: 0,
                action: d.clone(),
            });
            steps.push(Step {
                delay_ms: hold,
                action: d.release().unwrap(),
            });
        }
        steps.push(Step {
            delay_ms: if diagonal { 0 } else { hold },
            action: w.release().unwrap(),
        });
        MacroFile {
            steps,
            ..MacroFile::default()
        }
    }
    pub fn tick(&mut self, app: &mut App) -> anyhow::Result<bool> {
        if Instant::now() < self.due {
            return Ok(false);
        }
        match self.phase {
            0 => {
                app.command(UiCommand {
                    action: "new".into(),
                    ..Default::default()
                })?;
                self.id = app.active;
                self.next(2000);
            }
            1 => {
                let id = self.id.unwrap();
                app.tabs[&id].view.evaluate_script("window.testEvents=[];window.testDown=new Set;addEventListener('keydown',e=>{testEvents.push({type:'down',code:e.code,trusted:e.isTrusted});testDown.add(e.code)});addEventListener('keyup',e=>{testEvents.push({type:'up',code:e.code,trusted:e.isTrusted});testDown.delete(e.code)});")?;
                app.command(UiCommand {
                    action: "macro_record".into(),
                    tab_id: Some(id),
                    record_moves: Some(false),
                    ..Default::default()
                })?;
                self.next(500);
            }
            2 => {
                let id = self.id.unwrap();
                app.tabs.get_mut(&id).unwrap().automation_kind = "macro";
                app.macro_player.play(
                    id,
                    Self::file(250, true),
                    macros::PlaybackOptions::default(),
                    false,
                )?;
                self.next(2000);
            }
            3 => {
                self.probe(app, "completed")?;
                self.next(500);
            }
            4 => {
                let id = self.id.unwrap();
                let v = self.values.get("completed").cloned().unwrap_or_default();
                let events = v["events"].as_array();
                let good = events
                    .is_some_and(|es| es.len() == 4 && es.iter().all(|e| e["trusted"] == true));
                self.check("native_webview2_key_sequence", good, v.clone());
                self.check(
                    "all_keys_released_after_playback",
                    v["down"].as_array().is_some_and(|a| a.is_empty()),
                    v["down"].clone(),
                );
                app.finish_recording(id)?;
                let file = app.tabs[&id].macro_file.clone();
                self.check(
                    "native_trusted_events_recorded",
                    file.steps.len() == 4,
                    json!({"steps":file.steps.len()}),
                );
                file.save_file(&app.root.join("recorded.tabmacro.json"))?;
                let restored = macros::MacroFile::from_json(&std::fs::read_to_string(
                    app.root.join("recorded.tabmacro.json"),
                )?)?;
                self.check(
                    "recorded_file_roundtrip",
                    file == restored,
                    json!({"steps":restored.steps.len()}),
                );
                app.macro_player.play(
                    id,
                    Self::file(10000, false),
                    macros::PlaybackOptions::default(),
                    false,
                )?;
                self.next(500);
            }
            5 => {
                app.command(UiCommand {
                    action: "stop_all".into(),
                    ..Default::default()
                })?;
                self.next(500);
            }
            6 => {
                self.probe(app, "stopped")?;
                self.next(500);
            }
            7 => {
                let v = self.values.get("stopped").cloned().unwrap_or_default();
                self.check(
                    "global_stop_releases_held_key",
                    v["down"].as_array().is_some_and(|a| a.is_empty()),
                    v,
                );
                app.command(UiCommand {
                    action: "new".into(),
                    ..Default::default()
                })?;
                self.next(3000);
            }
            8 => {
                let id = self.id.unwrap();
                self.check(
                    "idle_background_hidden",
                    app.tabs[&id].render_policy.is_some_and(|p| !p.visible),
                    json!(true),
                );
                app.command(UiCommand {
                    action: "macro_play".into(),
                    tab_id: Some(id),
                    macro_file: Some(Self::file(100, true)),
                    macro_owner: Some(id),
                    ..Default::default()
                })?;
                self.next(2000);
            }
            9 => {
                self.probe(app, "background")?;
                self.next(500);
            }
            10 => {
                let id = self.id.unwrap();
                let v = self.values.get("background").cloned().unwrap_or_default();
                self.check(
                    "background_macro_wakes_and_finishes",
                    app.tabs[&id].error.is_none()
                        && v["events"].as_array().is_some_and(|a| a.len() >= 10)
                        && v["down"].as_array().is_some_and(|a| a.is_empty()),
                    v,
                );
                app.command(UiCommand {
                    action: "start".into(),
                    tab_id: Some(id),
                    preset: Some(Preset {
                        target: Target::Element {
                            selector: "#counter".into(),
                        },
                        count: Some(3),
                        interval_ms: 100,
                        ..Default::default()
                    }),
                    ..Default::default()
                })?;
                self.next(4000);
            }
            11 => {
                let id = self.id.unwrap();
                self.check(
                    "background_mouse_acknowledgements",
                    app.tabs[&id].completed == 3 && app.tabs[&id].error.is_none(),
                    json!({"completed":app.tabs[&id].completed,"error":app.tabs[&id].error}),
                );
                app.select(id)?;
                self.check(
                    "select_restores_full_speed",
                    app.tabs[&id].render_policy
                        == Some(RenderPolicy {
                            visible: true,
                            fps: 0,
                        }),
                    json!(true),
                );
                for tab in app.tabs.values() {
                    tab.view.evaluate_script("window.iso=[];for(const type of ['keydown','keyup','mousedown','mouseup'])addEventListener(type,e=>iso.push({type,code:e.code||'',trusted:e.isTrusted}));")?;
                }
                self.next(500);
            }
            12 => {
                let a = self.id.unwrap();
                let b = *app.tabs.keys().find(|&&id| id != a).unwrap();
                let wrong = app.command(UiCommand {
                    action: "macro_play".into(),
                    tab_id: Some(b),
                    macro_owner: Some(a),
                    macro_file: Some(Self::file(300, false)),
                    ..Default::default()
                });
                self.check("reject_cross_tab_macro_file", wrong.is_err(), json!(true));
                for (id, key, code, vk) in [(a, "w", "KeyW", 87), (b, "d", "KeyD", 68)] {
                    let mut file = Self::file(300, false);
                    for step in &mut file.steps {
                        match &mut step.action {
                            macros::Action::KeyDown {
                                key: k,
                                code: c,
                                virtual_key: v,
                                ..
                            }
                            | macros::Action::KeyUp {
                                key: k,
                                code: c,
                                virtual_key: v,
                                ..
                            } => {
                                *k = key.into();
                                *c = code.into();
                                *v = vk;
                            }
                            _ => {}
                        }
                    }
                    file.steps.push(macros::Step {
                        delay_ms: 0,
                        action: macros::Action::MouseDown {
                            x: 20.0,
                            y: 20.0,
                            button: config::MouseButton::Left,
                            modifiers: 0,
                        },
                    });
                    file.steps.push(macros::Step {
                        delay_ms: 20,
                        action: macros::Action::MouseUp {
                            x: 20.0,
                            y: 20.0,
                            button: config::MouseButton::Left,
                            modifiers: 0,
                        },
                    });
                    app.command(UiCommand {
                        action: "macro_play".into(),
                        tab_id: Some(id),
                        macro_owner: Some(id),
                        macro_file: Some(file),
                        playback: Some(macros::PlaybackOptions {
                            repeats: Some(8),
                            speed: 1.0,
                        }),
                        ..Default::default()
                    })?;
                }
                app.select(b)?;
                self.next(500);
            }
            13 => {
                let a = self.id.unwrap();
                app.command(UiCommand {
                    action: "macro_stop".into(),
                    tab_id: Some(a),
                    ..Default::default()
                })?;
                app.select(a)?;
                self.next(500);
            }
            14 => {
                let a = self.id.unwrap();
                let proxy = app.proxy.clone();
                app.tabs[&a]
                    .view
                    .evaluate_script_with_callback("window.iso", move |v| {
                        let _ = proxy.send_event(AppEvent::Probe("iso_a".into(), v));
                    })?;
                self.next(500);
            }
            15 => {
                let a = self.id.unwrap();
                app.command(UiCommand {
                    action: "close".into(),
                    tab_id: Some(a),
                    ..Default::default()
                })?;
                self.next(3500);
            }
            16 => {
                let b = app.active.unwrap();
                let proxy = app.proxy.clone();
                app.tabs[&b]
                    .view
                    .evaluate_script_with_callback("window.iso", move |v| {
                        let _ = proxy.send_event(AppEvent::Probe("iso_b".into(), v));
                    })?;
                self.next(500);
            }
            17 => {
                let a = self.values.get("iso_a").cloned().unwrap_or_default();
                let b = self.values.get("iso_b").cloned().unwrap_or_default();
                let clean = |v: &Value, code: &str| {
                    v.as_array().is_some_and(|es| {
                        !es.is_empty()
                            && es.iter().all(|e| {
                                e["trusted"] == true && (e["code"] == "" || e["code"] == code)
                            })
                    })
                };
                self.check("tab_a_receives_only_own_keys", clean(&a, "KeyW"), a);
                self.check("tab_b_receives_only_own_keys", clean(&b, "KeyD"), b.clone());
                let tab = &app.tabs[&app.active.unwrap()];
                self.check(
                    "stop_close_a_does_not_stop_b",
                    tab.macro_cycles == 8 && tab.error.is_none(),
                    json!({"cycles":tab.macro_cycles,"error":tab.error}),
                );
                self.check(
                    "tab_b_receives_exact_own_mouse_count",
                    b.as_array().is_some_and(|es| {
                        es.iter().filter(|e| e["type"] == "mousedown").count() == 8
                            && es.iter().filter(|e| e["type"] == "mouseup").count() == 8
                    }),
                    json!(b),
                );
                app.command(UiCommand {
                    action: "new".into(),
                    ..Default::default()
                })?;
                self.next(2000);
            }
            18 => {
                let ids: Vec<_> = app.tabs.keys().copied().collect();
                for id in ids {
                    app.tabs[&id].view.evaluate_script("window.game={frames:0,moving:0,blur:0,held:new Set};addEventListener('keydown',e=>game.held.add(e.code));addEventListener('keyup',e=>game.held.delete(e.code));addEventListener('blur',()=>{game.blur++;game.held.clear()});function gameFrame(){game.frames++;if(document.hasFocus()&&!document.hidden&&game.held.size)game.moving++;requestAnimationFrame(gameFrame)}requestAnimationFrame(gameFrame);")?;
                    app.command(UiCommand {
                        action: "macro_play".into(),
                        tab_id: Some(id),
                        macro_owner: Some(id),
                        macro_file: Some(Self::file(15000, false)),
                        ..Default::default()
                    })?;
                }
                app.apply_render_policies();
                self.next(500);
            }
            19..=25 => {
                let next = *app.tabs.keys().find(|&&id| Some(id) != app.active).unwrap();
                app.select(next)?;
                let key = format!("concurrent_{}_{}", self.phase, next);
                let proxy = app.proxy.clone();
                for (&id, tab) in &app.tabs {
                    let name = format!("{key}_{id}");
                    let p = proxy.clone();
                    tab.view.evaluate_script_with_callback("({frames:game.frames,moving:game.moving,blur:game.blur,held:[...game.held],focus:document.hasFocus(),hidden:document.hidden})",move|v|{let _=p.send_event(AppEvent::Probe(name.clone(),v));})?;
                }
                self.next(500);
            }
            26 => {
                let samples: Vec<_> = self
                    .values
                    .iter()
                    .filter(|(key, _)| key.starts_with("concurrent_"))
                    .map(|(k, v)| json!({"sample":k,"value":v}))
                    .collect();
                self.check(
                    "concurrent_games_keep_focus_and_held_keys",
                    samples.len() == 14
                        && samples.iter().all(|s| {
                            s["value"]["focus"] == true
                                && s["value"]["hidden"] == false
                                && s["value"]["held"].as_array().is_some_and(|a| a.len() == 1)
                        }),
                    json!(samples),
                );
                let progressing = app.tabs.keys().all(|id| {
                    let mut values: Vec<_> = self
                        .values
                        .iter()
                        .filter(|(k, _)| {
                            k.starts_with("concurrent_") && k.ends_with(&id.to_string())
                        })
                        .collect();
                    values.sort_by_key(|(k, _)| *k);
                    values.len() == 7
                        && values.windows(2).all(|pair| {
                            pair[1].1["moving"].as_u64().unwrap_or(0)
                                > pair[0].1["moving"].as_u64().unwrap_or(0)
                        })
                });
                self.check(
                    "both_game_loops_progress_through_every_switch",
                    progressing,
                    json!(true),
                );
                self.check(
                    "running_profiles_keep_full_rate",
                    app.tabs.values().all(|t| {
                        t.render_policy
                            == Some(RenderPolicy {
                                visible: true,
                                fps: 0,
                            })
                            && t.background_active == Some(true)
                            && t.background_failed.is_none()
                    }),
                    json!(true),
                );
                app.command(UiCommand {
                    action: "stop_all".into(),
                    ..Default::default()
                })?;
                self.next(1000);
            }
            27 => {
                self.check(
                    "background_focus_released_after_stop",
                    app.tabs
                        .values()
                        .all(|t| t.background_active == Some(false)),
                    json!(true),
                );
                self.finish(app)?;
                return Ok(true);
            }
            _ => return Ok(true),
        }
        Ok(false)
    }
    pub fn fail(&mut self, app: &App, error: &str) {
        self.check("harness", false, json!(error));
        let _ = self.finish(app);
    }
    fn finish(&self, app: &App) -> anyhow::Result<()> {
        let report =
            json!({"passed":self.checks.iter().all(|v|v["passed"]==true),"checks":self.checks});
        std::fs::write(
            app.root.join("native-macro-report.json"),
            serde_json::to_string_pretty(&report)?,
        )?;
        Ok(())
    }
}
