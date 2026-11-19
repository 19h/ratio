pub use ratio_protocol::Delivery;
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
    pub title: String,
    pub body: String,
    pub network: Option<NetworkEvent>,
    pub generation: Option<Generation>,
}

pub enum Generation {
    Preview {
        id: usize,
        label: String,
        text: String,
    },
    End(usize),
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
    Tui(UnboundedSender<Event>, watch::Receiver<GenerationControl>),
    Managed(watch::Receiver<GenerationControl>),
}

impl Reporter {
    pub fn emit(&self, kind: Kind, title: impl Into<String>, body: impl Into<String>) {
        if matches!(self, Self::Silent | Self::Managed(_)) {
            return;
        }
        let title = clean(title.into());
        let body = clean(body.into());
        match self {
            Self::Silent | Self::Managed(_) => {}
            Self::Terminal => {
                eprintln!("{title}\n{}", clip(&body, 24 * 1024));
            }
            Self::Tui(sender, _) => {
                let _ = sender.send(Event {
                    kind,
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

    pub fn network(&self, event: NetworkEvent) {
        if let Self::Tui(sender, _) = self {
            let _ = sender.send(Event {
                kind: Kind::Ipc,
                title: String::new(),
                body: String::new(),
                network: Some(event),
                generation: None,
            });
        }
    }

    pub fn generation_control(&self) -> Option<watch::Receiver<GenerationControl>> {
        match self {
            Self::Tui(_, control) | Self::Managed(control) => Some(control.clone()),
            _ => None,
        }
    }

    pub fn generation(&self, event: Generation) {
        if let Self::Tui(sender, _) = self {
            let _ = sender.send(Event {
                kind: Kind::Info,
                title: String::new(),
                body: String::new(),
                network: None,
                generation: Some(event),
            });
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
