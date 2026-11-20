//! End-to-end checks against actual isolated WebView2 instances on a local fixture.
use super::*;
use serde_json::Value;

pub struct SelfTest {
    phase: u32,
    due: Instant,
    started: Instant,
    ids: Vec<TabId>,
    pub values: HashMap<String, Value>,
    checks: Vec<Value>,
    metrics: Vec<Value>,
    cursor: Option<(i32, i32)>,
    cursor_unchanged: bool,
}
impl SelfTest {
    pub fn new() -> Self {
        Self {
            phase: 0,
            due: Instant::now() + Duration::from_secs(2),
            started: Instant::now(),
            ids: Vec::new(),
            values: HashMap::new(),
            checks: Vec::new(),
            metrics: Vec::new(),
            cursor: None,
            cursor_unchanged: true,
        }
    }
    fn advance(&mut self, seconds: u64) {
        self.phase += 1;
        self.due = Instant::now() + Duration::from_secs(seconds);
    }
    fn check(&mut self, name: &str, passed: bool, details: Value) {
        println!("SELF_TEST {name}: {}", if passed { "PASS" } else { "FAIL" });
        self.checks
            .push(json!({"name":name,"passed":passed,"details":details}));
    }
    fn command(
        app: &mut App,
        id: Option<TabId>,
        action: &str,
        preset: Option<Preset>,
    ) -> anyhow::Result<()> {
        app.command(UiCommand {
            action: action.into(),
            tab_id: id,
            preset,
            url: None,
            macro_file: None,
            macro_owner: None,
            playback: None,
            record_moves: None,
            performance_mode: None,
        })
    }
    fn create_to(&mut self, app: &mut App, n: usize) -> anyhow::Result<()> {
        while app.tabs.len() < n {
            Self::command(app, None, "new", None)?;
            self.ids.push(app.active.unwrap());
        }
        Ok(())
    }
    fn probe(&self, app: &App, id: TabId, key: &str, script: &str) -> anyhow::Result<()> {
        let proxy = app.proxy.clone();
        let key = key.to_string();
        app.tabs[&id]
            .view
            .evaluate_script_with_callback(script, move |v| {
                let _ = proxy.send_event(AppEvent::Probe(key.clone(), v));
            })?;
        Ok(())
    }
    fn counters(&self, app: &App, prefix: &str) -> anyhow::Result<()> {
        for (i, &id) in self.ids.iter().take(3).enumerate() {
            self.probe(app,id,&format!("{prefix}{i}"),"({clicks,canvasClicks,identity:localStorage.getItem('identity'),cookie:document.cookie,last:document.querySelector('#last').textContent})")?;
        }
        Ok(())
    }
    fn count(&self, key: &str) -> u64 {
        self.values
            .get(key)
            .and_then(|v| v["clicks"].as_u64())
            .unwrap_or(u64::MAX)
    }
    fn metric(&mut self, app: &mut App) {
        app.resources();
        self.metrics.push(json!({"tabs":app.tabs.len(),"cpu_percent_sum":app.cpu,"resident_memory_mb_sum":app.memory_mb}));
    }
    pub fn tick(&mut self, app: &mut App) -> anyhow::Result<bool> {
        if self.started.elapsed() > Duration::from_secs(150) {
            anyhow::bail!("Self-test exceeded 150 seconds");
        }
        if (3..=11).contains(&self.phase) {
            if let Some(start) = self.cursor {
                self.cursor_unchanged &= platform::cursor_position()? == start;
            }
        }
        if Instant::now() < self.due {
            return Ok(false);
        }
        match self.phase {
            0 => {
                self.create_to(app, 1)?;
                self.advance(3);
            }
            1 => {
                self.metric(app);
                self.create_to(app, 3)?;
                self.advance(3);
            }
            2 => {
                self.metric(app);
                for (i, &id) in self.ids.iter().enumerate() {
                    app.tabs[&id].view.evaluate_script(&format!("localStorage.setItem('identity','profile-{i}');document.cookie='identity=profile-{i};path=/;SameSite=Strict;Max-Age=86400';show();"))?;
                }
                Self::command(
                    app,
                    Some(self.ids[0]),
                    "start",
                    Some(Preset {
                        count: None,
                        interval_ms: 1000,
                        ..Preset::default()
                    }),
                )?;
                Self::command(
                    app,
                    Some(self.ids[1]),
                    "start",
                    Some(Preset {
                        count: None,
                        interval_ms: 3000,
                        ..Preset::default()
                    }),
                )?;
                app.select(self.ids[2])?;
                self.cursor = Some(platform::cursor_position()?);
                self.advance(5);
            }
            3 => {
                self.counters(app, "initial")?;
                self.advance(1);
            }
            4 => {
                let a = self.count("initial0");
                let b = self.count("initial1");
                let c = self.count("initial2");
                self.check(
                    "background_tabs_different_intervals",
                    (4..=8).contains(&a) && (2..=3).contains(&b) && c == 0,
                    json!({"a":a,"b":b,"c":c}),
                );
                let isolated = (0..3).all(|i| {
                    self.values[&format!("initial{i}")]["identity"] == format!("profile-{i}")
                        && self.values[&format!("initial{i}")]["cookie"]
                            == format!("identity=profile-{i}")
                });
                self.check("cookie_and_storage_isolation", isolated, json!(self.values));
                self.check(
                    "trusted_browser_click",
                    self.values["initial0"]["last"]
                        .as_str()
                        .is_some_and(|s| s.contains("Trusted: true")),
                    json!(self.values["initial0"]["last"]),
                );
                Self::command(app, Some(self.ids[0]), "stop", None)?;
                self.advance(1);
            }
            5 => {
                self.counters(app, "stopped")?;
                self.advance(4);
            }
            6 => {
                self.counters(app, "continued")?;
                self.advance(1);
            }
            7 => {
                self.check("stop_one_keeps_other_running",self.count("stopped0")==self.count("continued0")&&self.count("continued1")>self.count("stopped1"),json!({"before_a":self.count("stopped0"),"after_a":self.count("continued0"),"before_b":self.count("stopped1"),"after_b":self.count("continued1")}));
                Self::command(app, Some(self.ids[1]), "pause", None)?;
                self.counters(app, "paused")?;
                self.advance(3);
            }
            8 => {
                self.counters(app, "afterpause")?;
                self.probe(app,self.ids[2],"canvas_point","(()=>{const e=document.querySelector('#canvas');e.scrollIntoView({block:'center'});const r=e.getBoundingClientRect();return {x:r.x+r.width/2,y:r.y+r.height/2,w:innerWidth,h:innerHeight}})()")?;
                self.advance(1);
            }
            9 => {
                self.check(
                    "pause_holds_count",
                    self.count("paused1") == self.count("afterpause1"),
                    json!({"before":self.count("paused1"),"after":self.count("afterpause1")}),
                );
                Self::command(app, Some(self.ids[1]), "start_saved", None)?;
                let p = &self.values["canvas_point"];
                let target = Target::Coordinate {
                    x: p["x"]
                        .as_f64()
                        .ok_or_else(|| anyhow::anyhow!("Missing canvas X"))?,
                    y: p["y"].as_f64().unwrap(),
                    viewport_width: p["w"].as_f64().unwrap(),
                    viewport_height: p["h"].as_f64().unwrap(),
                };
                Self::command(
                    app,
                    Some(self.ids[2]),
                    "start",
                    Some(Preset {
                        target,
                        count: Some(2),
                        interval_ms: 250,
                        ..Preset::default()
                    }),
                )?;
                self.advance(2);
            }
            10 => {
                self.counters(app, "canvas")?;
                let start = Instant::now();
                Self::command(app, None, "stop_all", None)?;
                let ms = start.elapsed().as_secs_f64() * 1000.0;
                self.check(
                    "global_stop_host_latency_under_100ms",
                    ms < 100.0,
                    json!({"ms":ms}),
                );
                self.advance(1);
            }
            11 => {
                self.check(
                    "coordinate_canvas_exact_count",
                    self.values["canvas2"]["canvasClicks"] == 2,
                    json!(self.values["canvas2"]),
                );
                self.check(
                    "resume_continues_other_tab",
                    self.count("canvas1") > self.count("afterpause1"),
                    json!({"before":self.count("afterpause1"),"after":self.count("canvas1")}),
                );
                self.check(
                    "real_cursor_unchanged",
                    self.cursor_unchanged,
                    json!({"initial":self.cursor,"final":platform::cursor_position()?}),
                );
                self.counters(app, "global")?;
                self.advance(2);
            }
            12 => {
                self.counters(app, "afterglobal")?;
                self.advance(1);
            }
            13 => {
                let stable = (0..3).all(|i| {
                    self.count(&format!("global{i}")) == self.count(&format!("afterglobal{i}"))
                });
                self.check(
                    "global_stop_prevents_later_clicks",
                    stable,
                    json!({"tabs":3}),
                );
                Self::command(
                    app,
                    Some(self.ids[0]),
                    "start",
                    Some(Preset {
                        count: None,
                        interval_ms: 50,
                        delay_ms: 1000,
                        ..Preset::default()
                    }),
                )?;
                Self::command(app, Some(self.ids[0]), "close", None)?;
                self.check(
                    "close_removes_tab",
                    !app.tabs.contains_key(&self.ids[0]),
                    json!({"remaining":app.tabs.len()}),
                );
                self.advance(2);
            }
            14 => {
                app.open(self.ids[0])?;
                self.advance(2);
            }
            15 => {
                self.probe(
                    app,
                    self.ids[0],
                    "reopened",
                    "({clicks,identity:localStorage.getItem('identity'),cookie:document.cookie})",
                )?;
                self.advance(1);
            }
            16 => {
                self.check(
                    "reopen_restores_profile_session",
                    self.values["reopened"]["identity"] == "profile-0"
                        && self.values["reopened"]["cookie"] == "identity=profile-0",
                    json!(self.values["reopened"]),
                );
                self.check(
                    "closed_worker_does_not_resume",
                    self.values["reopened"]["clicks"] == 0,
                    json!(self.values["reopened"]),
                );
                let db = Database::open(&app.root.join("workspace.db"))?;
                let restored = db.profiles()?;
                self.check(
                    "saved_profiles_and_presets_reload",
                    restored.len() == 3 && restored[1].preset.interval_ms == 3000,
                    json!({"profiles":restored.len()}),
                );
                self.create_to(app, 6)?;
                self.advance(3);
            }
            17 => {
                self.metric(app);
                self.create_to(app, 9)?;
                self.advance(3);
            }
            18 => {
                self.metric(app);
                platform::test_crash(&app.tabs[&self.ids[0]].view)?;
                self.advance(3);
            }
            19 => {
                self.check("renderer_crash_is_contained",app.tabs[&self.ids[0]].state==State::Error&&app.tabs[&self.ids[1]].state!=State::Error,json!({"crashed_tab_state":app.tabs[&self.ids[0]].state,"other_tab_state":app.tabs[&self.ids[1]].state}));
                self.finish(app)?;
                return Ok(true);
            }
            _ => return Ok(true),
        }
        Ok(false)
    }
    pub fn fail(&mut self, app: &mut App, error: &str) {
        self.check(
            "test_harness",
            false,
            json!({"error":error,"phase":self.phase}),
        );
        let _ = self.finish(app);
    }
    fn finish(&mut self, app: &mut App) -> anyhow::Result<()> {
        app.engine.stop_all();
        let passed = self.checks.iter().all(|c| c["passed"] == true);
        let report = json!({"passed":passed,"elapsed_seconds":self.started.elapsed().as_secs_f64(),"checks":self.checks,"resources":self.metrics,"resource_notes":"Sum of application and descendant process resident memory; CPU is sum per-core percentages. Local fixture, idle sample after loading. Not a website workload benchmark.","data_directory":app.root});
        let path = app.root.join("self-test-report.json");
        std::fs::write(&path, serde_json::to_string_pretty(&report)?)?;
        println!("SELF_TEST_REPORT {}", path.display());
        println!("SELF_TEST_RESULT {}", if passed { "PASS" } else { "FAIL" });
        Ok(())
    }
}
