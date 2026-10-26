//! Codex owns goals, including the turns it starts without an ACP prompt.
use super::*;

impl State {
    pub(super) fn read_goal(&mut self, sid: &str, out: &mut Vec<Out>) {
        let Some(t) = self.threads.get_mut(sid) else { return };
        t.goal_version += 1;
        let version = t.goal_version;
        out.push(self.send(
            Pending::GoalRead { thread: sid.into(), version },
            "thread/goal/get",
            json!({"threadId": sid}),
        ));
    }

    pub(super) fn goal_update(&mut self, sid: &str, goal: Option<&Value>, force: bool, out: &mut Vec<Out>) {
        let Some(t) = self.threads.get_mut(sid) else { return };
        let next = goal.filter(|v| v.is_object()).cloned();
        // Even a repeated clear invalidates a read issued before that notification.
        t.goal_version += 1;
        if !force && t.goal == next {
            return;
        }
        t.goal = next;
        out.push(update(sid, json!({"sessionUpdate": "session_info_update", "_meta": {"goal": t.goal}})));
    }

    pub(super) fn has_active_goal(&self, sid: &str) -> bool {
        self.threads.get(sid).and_then(|t| t.goal.as_ref()).is_some_and(|g| g["status"] == "active")
    }

    /// An explicit busy refusal means the input was not accepted. Read the actual turn once,
    /// then steer it. Other errors are never replayed and a closed session is never revived.
    pub(super) fn reconcile_goal_send(&mut self, client: Value, thread: String, input: Value, out: &mut Vec<Out>) {
        out.push(self.send(
            Pending::GoalSend { client, thread: thread.clone(), input },
            "thread/read",
            json!({"threadId": thread, "includeTurns": true}),
        ));
    }

    pub(super) fn steer_goal_send(
        &mut self,
        client: Value,
        sid: &str,
        input: Value,
        thread: &Value,
        out: &mut Vec<Out>,
    ) {
        let turn = thread
            .get("turns")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .rev()
            .find(|t| t["status"] == "inProgress")
            .and_then(|t| t["id"].as_str());
        let Some(t) = self.threads.get_mut(sid) else {
            return out.push(Out::Client(err(&client, INTERNAL, crate::STEER_NO_TURN)));
        };
        let Some(turn) = turn else {
            return out.push(Out::Client(err(&client, INTERNAL, crate::STEER_NO_TURN)));
        };
        t.turn = Some(turn.into());
        out.push(self.send(
            Pending::TurnSteer { client },
            "turn/steer",
            json!({"threadId": sid, "expectedTurnId": turn, "input": input}),
        ));
    }
}

pub(super) fn busy_refusal(code: i64, message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    code == -32600
        && message.contains("already")
        && (message.contains("active turn")
            || message.contains("turn is already running")
            || message.contains("already in a turn"))
}
