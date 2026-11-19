use tokio::sync::mpsc::UnboundedSender;

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
}

pub struct AgentNode {
    pub id: String,
    pub name: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TrafficScope {
    pub cycle: usize,
    pub phase: &'static str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delivery {
    Queued,
    Delivered,
    Dropped,
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
    Tui(UnboundedSender<Event>),
}

impl Reporter {
    pub fn emit(&self, kind: Kind, title: impl Into<String>, body: impl Into<String>) {
        if matches!(self, Self::Silent) {
            return;
        }
        let title = clean(title.into());
        let body = clean(body.into());
        match self {
            Self::Silent => {}
            Self::Terminal => {
                let limit = if matches!(kind, Kind::Output) {
                    600
                } else {
                    24 * 1024
                };
                eprintln!("{title}\n{}", clip(&body, limit));
            }
            Self::Tui(sender) => {
                let _ = sender.send(Event {
                    kind,
                    title,
                    body,
                    network: None,
                });
            }
        }
    }

    pub fn observes_network(&self) -> bool {
        matches!(self, Self::Tui(_))
    }

    pub fn network(&self, event: NetworkEvent) {
        if let Self::Tui(sender) = self {
            let _ = sender.send(Event {
                kind: Kind::Ipc,
                title: String::new(),
                body: String::new(),
                network: Some(event),
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

pub(crate) fn clean(text: String) -> String {
    text.chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .collect()
}
