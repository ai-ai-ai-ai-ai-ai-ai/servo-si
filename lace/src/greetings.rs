use std::collections::HashMap;

use codex_core_api::{EventMsg, ThreadId};
use codex_protocol::items::{CollabAgentTool, CollabAgentToolCallStatus, TurnItem};
use codex_protocol::protocol::{AgentStatus, SubAgentActivityKind};

use crate::LaceError;

pub(crate) struct Greetings {
    parent: ThreadId,
    children: HashMap<ThreadId, Option<String>>,
}

impl Greetings {
    pub(crate) fn new(parent: ThreadId) -> Self {
        Self {
            parent,
            children: HashMap::new(),
        }
    }

    pub(crate) fn observe(&mut self, event: EventMsg) -> Result<bool, LaceError> {
        match event {
            EventMsg::ItemCompleted(event) if event.thread_id == self.parent => match event.item {
                TurnItem::SubAgentActivity(activity)
                    if activity.kind == SubAgentActivityKind::Started =>
                {
                    self.record_spawn(activity.agent_thread_id)?;
                }
                TurnItem::CollabAgentToolCall(call) if call.sender_thread_id == self.parent => {
                    if call.status == CollabAgentToolCallStatus::Failed {
                        return Err(LaceError::Greeting(format!(
                            "subagent tool {:?} failed",
                            call.tool
                        )));
                    }
                    if call.tool == CollabAgentTool::SpawnAgent {
                        for id in call.receiver_thread_ids {
                            self.record_spawn(id)?;
                        }
                    }
                    for (id, status) in call.agents_states {
                        self.record_status(id, status)?;
                    }
                }
                _ => {}
            },
            EventMsg::CollabAgentSpawnEnd(event) if event.sender_thread_id == self.parent => {
                let id = event.new_thread_id.ok_or_else(|| {
                    LaceError::Greeting(format!("subagent failed to start: {:?}", event.status))
                })?;
                self.record_spawn(id)?;
                self.record_status(id, event.status)?;
            }
            EventMsg::CollabWaitingEnd(event) if event.sender_thread_id == self.parent => {
                for (id, status) in event.statuses {
                    self.record_status(id, status)?;
                }
            }
            EventMsg::TurnComplete(event) => {
                if let Some(error) = event.error {
                    return Err(LaceError::Greeting(error.message));
                }
                return Ok(true);
            }
            EventMsg::Error(event) => return Err(LaceError::Greeting(event.message)),
            EventMsg::TurnAborted(_) | EventMsg::ShutdownComplete => {
                return Err(LaceError::Greeting(
                    "parent stopped before completing the demo".into(),
                ));
            }
            EventMsg::ExecApprovalRequest(_)
            | EventMsg::ApplyPatchApprovalRequest(_)
            | EventMsg::RequestPermissions(_)
            | EventMsg::RequestUserInput(_)
            | EventMsg::DynamicToolCallRequest(_)
            | EventMsg::ElicitationRequest(_) => {
                return Err(LaceError::Greeting(
                    "greeting demo unexpectedly requested interaction".into(),
                ));
            }
            _ => {}
        }
        Ok(false)
    }

    fn record_spawn(&mut self, id: ThreadId) -> Result<(), LaceError> {
        self.children.entry(id).or_default();
        if self.children.len() > 2 {
            return Err(LaceError::Greeting(
                "parent created more than two subagents".into(),
            ));
        }
        Ok(())
    }

    pub(crate) fn child_ids(&self) -> Vec<ThreadId> {
        self.children.keys().copied().collect()
    }

    pub(crate) fn record_status(
        &mut self,
        id: ThreadId,
        status: AgentStatus,
    ) -> Result<(), LaceError> {
        let Some(reply) = self.children.get_mut(&id) else {
            return Err(LaceError::Greeting(format!(
                "received status for unknown subagent {id}"
            )));
        };
        match status {
            AgentStatus::Completed(Some(message)) if message.trim() == "Hi!" => {
                *reply = Some(message);
            }
            AgentStatus::PendingInit | AgentStatus::Running => {}
            status => {
                return Err(LaceError::Greeting(format!(
                    "subagent {id} did not say hi: {status:?}"
                )));
            }
        }
        Ok(())
    }

    pub(crate) fn finish(self) -> Result<Vec<(ThreadId, String)>, LaceError> {
        if self.children.len() != 2 {
            return Err(LaceError::Greeting(format!(
                "expected two subagents, but the parent created {}",
                self.children.len()
            )));
        }
        let mut replies = self
            .children
            .into_iter()
            .map(|(id, reply)| {
                reply.map(|reply| (id, reply)).ok_or_else(|| {
                    LaceError::Greeting(format!("parent finished before subagent {id} replied"))
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        replies.sort_by_key(|(id, _)| id.to_string());
        Ok(replies)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::items::SubAgentActivityItem;
    use codex_protocol::protocol::{
        CollabAgentSpawnEndEvent, CollabWaitingEndEvent, ErrorEvent, ItemCompletedEvent,
        TurnCompleteEvent,
    };

    fn completed(parent: ThreadId, item: TurnItem) -> EventMsg {
        EventMsg::ItemCompleted(ItemCompletedEvent {
            thread_id: parent,
            turn_id: "turn".into(),
            item,
            started_at_ms: None,
            completed_at_ms: 0,
        })
    }

    fn spawned(parent: ThreadId, child: Option<ThreadId>, status: AgentStatus) -> EventMsg {
        EventMsg::CollabAgentSpawnEnd(CollabAgentSpawnEndEvent {
            call_id: "spawn".into(),
            completed_at_ms: 0,
            sender_thread_id: parent,
            new_thread_id: child,
            new_agent_nickname: None,
            new_agent_role: None,
            prompt: "Say hi".into(),
            model: "test-model".into(),
            reasoning_effort: Default::default(),
            status,
        })
    }

    fn waited(parent: ThreadId, statuses: Vec<(ThreadId, AgentStatus)>) -> EventMsg {
        EventMsg::CollabWaitingEnd(CollabWaitingEndEvent {
            sender_thread_id: parent,
            call_id: "wait".into(),
            completed_at_ms: 0,
            agent_statuses: Vec::new(),
            statuses: statuses.into_iter().collect(),
        })
    }

    #[test]
    fn reads_v2_activity_and_actual_child_outcomes() -> anyhow::Result<()> {
        let parent = ThreadId::new();
        let mut greetings = Greetings::new(parent);
        for index in 1..=2 {
            let id = ThreadId::from_u128(index);
            let item = TurnItem::SubAgentActivity(SubAgentActivityItem {
                model: None,
                reasoning_effort: None,
                id: format!("spawn-{index}"),
                kind: SubAgentActivityKind::Started,
                agent_thread_id: id,
                agent_path: format!("/root/greeting_{index}")
                    .try_into()
                    .map_err(anyhow::Error::msg)?,
            });
            greetings.observe(completed(parent, item.clone()))?;
            greetings.observe(completed(parent, item))?;
        }
        assert_eq!(greetings.child_ids().len(), 2);
        for id in greetings.child_ids() {
            greetings.record_status(id, AgentStatus::Completed(Some("Hi!".into())))?;
        }
        assert_eq!(greetings.finish()?.len(), 2);
        Ok(())
    }

    #[test]
    fn parent_completion_cannot_substitute_for_children_or_hide_errors() -> anyhow::Result<()> {
        let event = TurnCompleteEvent {
            root_turn_id: None,
            turn_id: "turn".into(),
            last_agent_message: Some("Both children said Hi!".into()),
            error: None,
            started_at: None,
            completed_at: None,
            duration_ms: None,
            time_to_first_token_ms: None,
        };
        let mut greetings = Greetings::new(ThreadId::new());
        assert!(greetings.observe(EventMsg::TurnComplete(event.clone()))?);
        assert!(greetings.finish().is_err());

        let event = TurnCompleteEvent {
            error: Some(ErrorEvent {
                message: "parent failed".into(),
                codex_error_info: None,
                misalignment: None,
            }),
            ..event
        };
        assert!(
            Greetings::new(ThreadId::new())
                .observe(EventMsg::TurnComplete(event))
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn waits_for_two_distinct_children_and_deduplicates_replies() -> anyhow::Result<()> {
        let parent = ThreadId::new();
        let first = ThreadId::from_u128(1);
        let second = ThreadId::from_u128(2);
        let mut greetings = Greetings::new(parent);
        for child in [first, second] {
            greetings.observe(spawned(parent, Some(child), AgentStatus::Running))?;
        }
        let done = AgentStatus::Completed(Some("Hi!".into()));
        greetings.observe(waited(
            parent,
            vec![(first, done.clone()), (second, AgentStatus::Running)],
        ))?;
        greetings.observe(waited(parent, vec![(first, done.clone()), (second, done)]))?;
        assert_eq!(
            greetings.finish()?,
            vec![(first, "Hi!".into()), (second, "Hi!".into())]
        );
        Ok(())
    }

    #[test]
    fn rejects_missing_or_unfinished_children() -> anyhow::Result<()> {
        let parent = ThreadId::new();
        assert!(Greetings::new(parent).finish().is_err());
        let mut greetings = Greetings::new(parent);
        greetings.observe(spawned(
            parent,
            Some(ThreadId::new()),
            AgentStatus::Completed(Some("Hi!".into())),
        ))?;
        greetings.observe(spawned(parent, Some(ThreadId::new()), AgentStatus::Running))?;
        assert!(greetings.finish().is_err());
        Ok(())
    }

    #[test]
    fn rejects_failed_spawns_and_extra_children() -> anyhow::Result<()> {
        let parent = ThreadId::new();
        let mut greetings = Greetings::new(parent);
        assert!(
            greetings
                .observe(spawned(parent, None, AgentStatus::Errored("failed".into())))
                .is_err()
        );
        for _ in 0..2 {
            greetings.observe(spawned(parent, Some(ThreadId::new()), AgentStatus::Running))?;
        }
        assert!(
            greetings
                .observe(spawned(parent, Some(ThreadId::new()), AgentStatus::Running))
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn rejects_failed_empty_or_incorrect_child_replies() -> anyhow::Result<()> {
        let parent = ThreadId::new();
        for status in [
            AgentStatus::Errored("failed".into()),
            AgentStatus::Interrupted,
            AgentStatus::Shutdown,
            AgentStatus::NotFound,
            AgentStatus::Completed(None),
            AgentStatus::Completed(Some("something else".into())),
        ] {
            let child = ThreadId::new();
            let mut greetings = Greetings::new(parent);
            greetings.observe(spawned(parent, Some(child), AgentStatus::Running))?;
            assert!(
                greetings
                    .observe(waited(parent, vec![(child, status)]))
                    .is_err()
            );
        }
        Ok(())
    }

    #[test]
    fn unrelated_threads_cannot_supply_greetings() -> anyhow::Result<()> {
        let parent = ThreadId::new();
        let child = ThreadId::new();
        let mut greetings = Greetings::new(parent);
        greetings.observe(spawned(ThreadId::new(), Some(child), AgentStatus::Running))?;
        assert!(
            greetings
                .observe(waited(
                    parent,
                    vec![(child, AgentStatus::Completed(Some("Hi!".into())))]
                ))
                .is_err()
        );
        Ok(())
    }
}
