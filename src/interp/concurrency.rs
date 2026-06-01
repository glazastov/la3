//! Interpreter: cooperative concurrency (Section 12) — forcing futures,
//! running spawned tasks, and channel receive. Split out of `interp.rs`.

use std::cell::RefCell;
use std::rc::Rc;

use super::*;

impl Interp {
    // ---- concurrency (Section 12) ----

    /// Force a value: a `Future` runs to completion (memoized); anything else is
    /// returned unchanged. Used by `await`, `join`, `all`, and `race`.
    pub(super) fn force(&mut self, v: Value, pos: Pos) -> R<Value> {
        match v {
            Value::Future(task) => self.run_task(&task, pos),
            other => Ok(other),
        }
    }

    /// Evaluate a task's body once and memoize its result.
    pub(super) fn run_task(&mut self, task: &Rc<TaskState>, _pos: Pos) -> R<Value> {
        if let Some(v) = task.result.borrow().as_ref() {
            return Ok(v.clone());
        }
        let v = match self.eval_block(&task.body, &task.env) {
            Ok(v) => v,
            Err(Signal::Return(v)) => v,
            Err(e) => return Err(e),
        };
        *task.result.borrow_mut() = Some(v.clone());
        Ok(v)
    }

    /// Run one not-yet-finished spawned task to completion. Returns `false` when
    /// no runnable task remains. This is the cooperative scheduler's single step.
    pub(super) fn run_one_ready(&mut self, pos: Pos) -> R<bool> {
        while let Some(task) = self.ready.pop_front() {
            if task.result.borrow().is_some() {
                continue;
            }
            self.run_task(&task, pos)?;
            return Ok(true);
        }
        Ok(false)
    }

    /// Receive from a channel, driving the scheduler when the buffer is empty but
    /// the channel is still open. Returns `None` once the channel is closed and
    /// drained. Errors if every task is blocked and the channel can never fill
    /// (a genuine deadlock).
    pub(super) fn channel_recv(
        &mut self,
        ch: &Rc<RefCell<ChannelData>>,
        pos: Pos,
    ) -> R<Option<Value>> {
        loop {
            {
                let mut c = ch.borrow_mut();
                if let Some(v) = c.buf.pop_front() {
                    return Ok(Some(v));
                }
                if c.closed {
                    return Ok(None);
                }
            }
            // Empty and open: let a producer task run, then retry.
            if !self.run_one_ready(pos)? {
                return rt(
                    pos,
                    "deadlock: receiving from an empty channel that no running task will fill or close",
                );
            }
        }
    }
}
