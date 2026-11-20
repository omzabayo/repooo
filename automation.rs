use crate::config::{Preset, ProfileId, TabId};
use serde::Serialize;
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy, Debug, Serialize, PartialEq)]
pub enum State {
    Online,
    Running,
    Paused,
    Error,
    Offline,
}
#[derive(Clone)]
pub struct Permit {
    generation: Arc<AtomicU64>,
    expected: u64,
    pub cancel: CancellationToken,
}
impl Permit {
    pub fn valid(&self) -> bool {
        !self.cancel.is_cancelled() && self.generation.load(Ordering::SeqCst) == self.expected
    }
}
pub struct ClickRequest {
    pub tab_id: TabId,
    pub preset: Preset,
    pub permit: Permit,
    pub reply: oneshot::Sender<Result<(), String>>,
}
pub enum WorkerEvent {
    Click(ClickRequest),
    Status {
        tab_id: TabId,
        profile_id: ProfileId,
        generation: u64,
        state: State,
        completed: u64,
        error: Option<String>,
    },
}
pub type Sink = Arc<dyn Fn(WorkerEvent) + Send + Sync>;
enum Command {
    Start(Preset, Permit, bool),
    Pause,
    Stop,
}
struct Worker {
    tx: mpsc::Sender<Command>,
    generation: Arc<AtomicU64>,
    cancel: CancellationToken,
    run_cancel: CancellationToken,
    task: JoinHandle<()>,
}
pub struct Engine {
    workers: HashMap<TabId, Worker>,
    sink: Sink,
    runtime: tokio::runtime::Runtime,
}
impl Engine {
    pub fn runtime_handle(&self) -> tokio::runtime::Handle {
        self.runtime.handle().clone()
    }
    pub fn new(sink: Sink) -> anyhow::Result<Self> {
        Ok(Self {
            workers: HashMap::new(),
            sink,
            runtime: tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()?,
        })
    }
    pub fn add(&mut self, id: TabId, profile: ProfileId) {
        if self.workers.contains_key(&id) {
            return;
        }
        let (tx, rx) = mpsc::channel(16);
        let cancel = CancellationToken::new();
        let generation = Arc::new(AtomicU64::new(0));
        let task = self.runtime.spawn(worker(
            id,
            profile,
            rx,
            cancel.clone(),
            generation.clone(),
            self.sink.clone(),
        ));
        self.workers.insert(
            id,
            Worker {
                tx,
                generation,
                cancel,
                run_cancel: CancellationToken::new(),
                task,
            },
        );
    }
    pub fn start(&mut self, id: TabId, preset: Preset, resume: bool) -> anyhow::Result<()> {
        preset.validate()?;
        let w = self
            .workers
            .get_mut(&id)
            .ok_or_else(|| anyhow::anyhow!("Tab is closed"))?;
        w.run_cancel.cancel();
        let expected = w.generation.fetch_add(1, Ordering::SeqCst) + 1;
        w.run_cancel = w.cancel.child_token();
        let permit = Permit {
            generation: w.generation.clone(),
            expected,
            cancel: w.run_cancel.clone(),
        };
        w.tx.try_send(Command::Start(preset, permit, resume))
            .map_err(|_| anyhow::anyhow!("Tab command queue is busy"))?;
        Ok(())
    }
    pub fn stop(&mut self, id: TabId, pause: bool) {
        if let Some(w) = self.workers.get_mut(&id) {
            w.generation.fetch_add(1, Ordering::SeqCst);
            w.run_cancel.cancel();
            let _ =
                w.tx.try_send(if pause { Command::Pause } else { Command::Stop });
        }
    }
    pub fn stop_all(&mut self) {
        let ids: Vec<_> = self.workers.keys().copied().collect();
        for id in ids {
            self.stop(id, false);
        }
    }
    pub fn remove(&mut self, id: TabId) {
        if let Some(w) = self.workers.remove(&id) {
            w.cancel.cancel();
            w.run_cancel.cancel();
            w.task.abort();
        }
    }
    pub fn is_current(&self, id: TabId, generation: u64) -> bool {
        self.workers
            .get(&id)
            .is_some_and(|w| w.generation.load(Ordering::SeqCst) == generation)
    }
}
impl Drop for Engine {
    fn drop(&mut self) {
        self.stop_all();
        for (_, w) in self.workers.drain() {
            w.cancel.cancel();
            w.task.abort();
        }
    }
}
async fn worker(
    id: TabId,
    profile: ProfileId,
    mut rx: mpsc::Receiver<Command>,
    cancel: CancellationToken,
    generation: Arc<AtomicU64>,
    sink: Sink,
) {
    let mut completed = 0;
    let mut running: Option<(Preset, Permit)> = None;
    let mut deadline = tokio::time::Instant::now();
    loop {
        tokio::select! { biased;
            _=cancel.cancelled()=>break,
            cmd=rx.recv()=>match cmd {
                Some(Command::Start(p,permit,resume))=> { if !permit.valid(){continue;}
                if !resume {completed=0;} deadline=tokio::time::Instant::now()+Duration::from_millis(if resume {0}else{p.delay_ms}); sink(WorkerEvent::Status {tab_id:id,profile_id:profile,generation:permit.expected,state:State::Running,completed,error:None}); running=Some((p,permit)); },
                Some(Command::Pause)=> {running=None; sink(WorkerEvent::Status {tab_id:id,profile_id:profile,generation:generation.load(Ordering::SeqCst),state:State::Paused,completed,error:None});},
                Some(Command::Stop)=> {running=None; completed=0; sink(WorkerEvent::Status {tab_id:id,profile_id:profile,generation:generation.load(Ordering::SeqCst),state:State::Online,completed,error:None});},
                None=>break,
            },
            _=tokio::time::sleep_until(deadline),if running.is_some()=> {
                let (p,permit)=running.as_ref().unwrap().clone();
                if !permit.valid(){running=None;continue;}
                if p.count.is_some_and(|limit|completed>=limit) {running=None;sink(WorkerEvent::Status {tab_id:id,profile_id:profile,generation:permit.expected,state:State::Online,completed,error:None});continue;}
                let result=tokio::select! { biased;
                    _=cancel.cancelled()=>break,
                    _=permit.cancel.cancelled()=>{running=None;continue;},
                    result=attempt_click(id,&p,&permit,&sink)=>result,
                };
                if !permit.valid(){running=None;continue;}
                let (state,error)=match result {Ok(())=>{completed+=1; (if p.count.is_some_and(|n|completed>=n){State::Online}else{State::Running},None)},Err(e)=>{permit.cancel.cancel();(State::Error,Some(e))}};
                sink(WorkerEvent::Status {tab_id:id,profile_id:profile,generation:permit.expected,state,completed,error});
                if state!=State::Running {running=None;}
                // Host clock; missed ticks are skipped instead of bursting.
                deadline+=Duration::from_millis(p.interval_ms);
                if deadline<tokio::time::Instant::now(){deadline=tokio::time::Instant::now()+Duration::from_millis(p.interval_ms);}
            }
        }
    }
}

async fn attempt_click(id: TabId, p: &Preset, permit: &Permit, sink: &Sink) -> Result<(), String> {
    let expires = tokio::time::Instant::now() + Duration::from_millis(p.timeout_ms);
    loop {
        if !permit.valid() {
            return Err("CANCELLED".into());
        }
        let (tx, reply) = oneshot::channel();
        sink(WorkerEvent::Click(ClickRequest {
            tab_id: id,
            preset: p.clone(),
            permit: permit.clone(),
            reply: tx,
        }));
        match tokio::time::timeout_at(expires, reply).await {
            Ok(Ok(Err(error))) if error == "ELEMENT_NOT_FOUND" => {
                if tokio::time::Instant::now() + Duration::from_millis(100) >= expires {
                    return Err("ELEMENT_TIMEOUT".into());
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Ok(Ok(result)) => return result,
            Ok(Err(_)) => return Err("WEBVIEW_UNAVAILABLE".into()),
            Err(_) => return Err("INPUT_TIMEOUT".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn independent_tabs_stop_close_and_global_stop() {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut engine = Engine::new(Arc::new(move |event| {
            let _ = tx.send(event);
        }))
        .unwrap();
        let a = TabId::new_v4();
        let b = TabId::new_v4();
        let profile = ProfileId::new_v4();
        engine.add(a, profile);
        engine.add(b, ProfileId::new_v4());
        let p = Preset {
            count: None,
            interval_ms: 50,
            ..Preset::default()
        };
        engine.start(a, p.clone(), false).unwrap();
        engine.start(b, p, false).unwrap();
        let mut requests = Vec::new();
        while requests.len() < 2 {
            if let WorkerEvent::Click(r) = rx.recv_timeout(Duration::from_secs(2)).unwrap() {
                requests.push(r);
            }
        }
        engine.stop(a, false);
        assert!(requests
            .iter()
            .find(|r| r.tab_id == b)
            .unwrap()
            .permit
            .valid());
        assert!(!requests
            .iter()
            .find(|r| r.tab_id == a)
            .unwrap()
            .permit
            .valid());
        engine.stop_all();
        assert!(requests.iter().all(|r| !r.permit.valid()));
        engine.remove(a);
        assert!(!engine.workers.contains_key(&a));
    }
    #[test]
    fn exact_count() {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut e = Engine::new(Arc::new(move |event| {
            let _ = tx.send(event);
        }))
        .unwrap();
        let id = TabId::new_v4();
        e.add(id, ProfileId::new_v4());
        let p = Preset {
            count: Some(2),
            interval_ms: 50,
            ..Preset::default()
        };
        e.start(id, p, false).unwrap();
        let mut clicks = 0;
        loop {
            match rx.recv_timeout(Duration::from_secs(2)).unwrap() {
                WorkerEvent::Click(r) => {
                    clicks += 1;
                    r.reply.send(Ok(())).unwrap();
                }
                WorkerEvent::Status {
                    state: State::Online,
                    completed: 2,
                    ..
                } => break,
                _ => {}
            }
        }
        assert_eq!(clicks, 2);
    }
    #[test]
    fn pause_resume_preserves_count_and_cancels_inflight() {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut e = Engine::new(Arc::new(move |event| {
            let _ = tx.send(event);
        }))
        .unwrap();
        let id = TabId::new_v4();
        e.add(id, ProfileId::new_v4());
        let p = Preset {
            count: Some(3),
            interval_ms: 50,
            ..Preset::default()
        };
        e.start(id, p.clone(), false).unwrap();
        loop {
            if let WorkerEvent::Click(r) = rx.recv_timeout(Duration::from_secs(2)).unwrap() {
                r.reply.send(Ok(())).unwrap();
                break;
            }
        }
        loop {
            if let WorkerEvent::Status { completed: 1, .. } =
                rx.recv_timeout(Duration::from_secs(2)).unwrap()
            {
                break;
            }
        }
        e.stop(id, true);
        loop {
            if let WorkerEvent::Status {
                state: State::Paused,
                completed,
                ..
            } = rx.recv_timeout(Duration::from_secs(2)).unwrap()
            {
                assert_eq!(completed, 1);
                break;
            }
        }
        e.start(id, p, true).unwrap();
        let mut remaining = 0;
        loop {
            match rx.recv_timeout(Duration::from_secs(2)).unwrap() {
                WorkerEvent::Click(r) => {
                    if r.permit.valid() {
                        remaining += 1;
                        let _ = r.reply.send(Ok(()));
                    }
                }
                WorkerEvent::Status {
                    state: State::Online,
                    completed: 3,
                    ..
                } => break,
                _ => {}
            }
        }
        assert_eq!(remaining, 2);
    }
    #[test]
    fn timeout_invalidates_late_input() {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut e = Engine::new(Arc::new(move |event| {
            let _ = tx.send(event);
        }))
        .unwrap();
        let id = TabId::new_v4();
        e.add(id, ProfileId::new_v4());
        e.start(
            id,
            Preset {
                timeout_ms: 100,
                ..Preset::default()
            },
            false,
        )
        .unwrap();
        let pending = loop {
            if let WorkerEvent::Click(r) = rx.recv_timeout(Duration::from_secs(2)).unwrap() {
                break r;
            }
        };
        loop {
            if let WorkerEvent::Status {
                state: State::Error,
                error,
                ..
            } = rx.recv_timeout(Duration::from_secs(2)).unwrap()
            {
                assert_eq!(error.as_deref(), Some("INPUT_TIMEOUT"));
                break;
            }
        }
        assert!(!pending.permit.valid());
    }
    #[test]
    fn missing_element_retries_on_host() {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut e = Engine::new(Arc::new(move |event| {
            let _ = tx.send(event);
        }))
        .unwrap();
        let id = TabId::new_v4();
        e.add(id, ProfileId::new_v4());
        e.start(
            id,
            Preset {
                count: Some(1),
                timeout_ms: 1000,
                ..Preset::default()
            },
            false,
        )
        .unwrap();
        let mut tries = 0;
        loop {
            match rx.recv_timeout(Duration::from_secs(2)).unwrap() {
                WorkerEvent::Click(r) => {
                    tries += 1;
                    let _ = r.reply.send(if tries < 3 {
                        Err("ELEMENT_NOT_FOUND".into())
                    } else {
                        Ok(())
                    });
                }
                WorkerEvent::Status {
                    state: State::Online,
                    completed: 1,
                    ..
                } => break,
                _ => {}
            }
        }
        assert_eq!(tries, 3);
    }
}
