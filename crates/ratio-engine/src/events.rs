use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

pub use ratio_protocol::{Delivery, LiveGeneration};
use tokio::sync::{mpsc::UnboundedSender, watch};

use crate::context::clip;

#[derive(Clone, Copy)]
pub enum Kind {
    Phase,
    Agent,
    Tool,
    Output,
    Ipc,
    Info,
}

pub struct Event {
    pub kind: Kind,
    pub actor: Option<String>,
    pub title: String,
    pub body: String,
    pub network: Option<NetworkEvent>,
    pub generation: Option<Generation>,
}

pub enum Generation {
    Preview(LiveGeneration),
    End(usize),
}

#[derive(Clone, Default)]
pub struct LiveGenerations(Arc<Mutex<BTreeMap<usize, LiveGeneration>>>);

impl LiveGenerations {
    fn observe(&self, event: &Generation) {
        let mut generations = self.0.lock().expect("live generations lock poisoned");
        match event {
            Generation::Preview(preview) => {
                generations.insert(preview.id, preview.clone());
            }
            Generation::End(id) => {
                generations.remove(id);
            }
        }
    }

    pub fn snapshot(&self) -> Vec<LiveGeneration> {
        self.0
            .lock()
            .expect("live generations lock poisoned")
            .values()
            .cloned()
            .collect()
    }
}

#[derive(Clone, Copy, Default)]
pub struct GenerationControl {
    pub paused: bool,
    pub epoch: u64,
}

pub struct AgentNode {
    pub id: String,
    pub name: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TrafficScope {
    pub cycle: usize,
    pub attempt: usize,
    pub phase: &'static str,
    pub worker_request: Option<i64>,
}

pub struct Traffic {
    pub scope: TrafficScope,
    pub from: String,
    pub to: Vec<String>,
    pub body: String,
    pub kind: TrafficKind,
}

pub enum TrafficKind {
    Ipc {
        sequence: usize,
        reply_to: Option<String>,
        delivery: Delivery,
    },
    Handoff {
        label: &'static str,
    },
}

pub enum NetworkEvent {
    Roster(Vec<AgentNode>),
    Traffic(Traffic),
}

#[derive(Clone)]
pub enum Reporter {
    Silent,
    Terminal,
    Tui(
        UnboundedSender<Event>,
        watch::Receiver<GenerationControl>,
        LiveGenerations,
    ),
    Managed(watch::Receiver<GenerationControl>, LiveGenerations),
}

impl Reporter {
    pub fn emit(&self, kind: Kind, title: impl Into<String>, body: impl Into<String>) {
        self.emit_for(None, kind, title.into(), body.into());
    }

    pub fn agent(
        &self,
        actor: &str,
        kind: Kind,
        title: impl Into<String>,
        body: impl Into<String>,
    ) {
        self.emit_for(Some(actor), kind, title.into(), body.into());
    }

    fn emit_for(&self, actor: Option<&str>, kind: Kind, title: String, body: String) {
        if matches!(self, Self::Silent) {
            return;
        }
        let title = clean(title);
        let body = clean(body);
        match self {
            Self::Silent => {}
            Self::Terminal | Self::Managed(..) => {
                let title = if let Some(actor) = actor {
                    format!("{actor} / {title}")
                } else {
                    title
                };
                if body.is_empty() {
                    eprintln!("{title}");
                } else {
                    eprintln!("{title}\n{}", clip(&body, 24 * 1024));
                }
            }
            Self::Tui(sender, ..) => {
                let _ = sender.send(Event {
                    kind,
                    actor: actor.map(str::to_owned),
                    title,
                    body,
                    network: None,
                    generation: None,
                });
            }
        }
    }

    pub fn observes_network(&self) -> bool {
        matches!(self, Self::Tui(..))
    }

    pub fn observes_generation(&self) -> bool {
        matches!(self, Self::Tui(..) | Self::Managed(..))
    }

    pub fn network(&self, event: NetworkEvent) {
        if let Self::Tui(sender, ..) = self {
            let _ = sender.send(Event {
                kind: Kind::Ipc,
                actor: None,
                title: String::new(),
                body: String::new(),
                network: Some(event),
                generation: None,
            });
        }
    }

    pub fn generation_control(&self) -> Option<watch::Receiver<GenerationControl>> {
        match self {
            Self::Tui(_, control, _) | Self::Managed(control, _) => Some(control.clone()),
            _ => None,
        }
    }

    pub fn generation(&self, event: Generation) {
        match self {
            Self::Tui(sender, _, live) => {
                live.observe(&event);
                let _ = sender.send(Event {
                    kind: Kind::Info,
                    actor: None,
                    title: String::new(),
                    body: String::new(),
                    network: None,
                    generation: Some(event),
                });
            }
            Self::Managed(_, live) => live.observe(&event),
            _ => {}
        }
    }

    pub fn request_started(&self, actor: &str, label: &str) {
        if matches!(self, Self::Terminal | Self::Managed(..)) {
            let actor = (actor != label && !label.starts_with(actor)).then_some(actor);
            self.emit_for(
                actor,
                Kind::Info,
                format!("{label} / Requesting model"),
                String::new(),
            );
        }
    }

    pub fn handoff(
        &self,
        scope: TrafficScope,
        from: &str,
        to: Vec<String>,
        label: &'static str,
        body: &str,
    ) {
        if self.observes_network() && !to.is_empty() {
            self.network(NetworkEvent::Traffic(Traffic {
                scope,
                from: from.into(),
                to,
                body: clean(clip(body, 8 * 1024)),
                kind: TrafficKind::Handoff { label },
            }));
        }
    }
}

pub fn clean(text: String) -> String {
    text.chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .collect()
}
