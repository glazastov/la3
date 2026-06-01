//! Interpreter: `loop`/`while`/`while let`/`for..in` and iteration.
//! Split out of `interp.rs`.

use std::rc::Rc;

use super::*;

impl Interp {
    pub(super) fn eval_loop(&mut self, body: &Block, env: &Env) -> R<Value> {
        loop {
            match self.eval_block(body, env) {
                Ok(_) => {}
                Err(Signal::Break(v)) => return Ok(v),
                Err(Signal::Continue) => continue,
                Err(other) => return Err(other),
            }
        }
    }

    pub(super) fn eval_while(
        &mut self,
        cond: &Expr,
        body: &Block,
        env: &Env,
        pos: Pos,
    ) -> R<Value> {
        loop {
            let c = self.eval(cond, env)?;
            if !self.as_bool(&c, pos)? {
                break;
            }
            match self.eval_block(body, env) {
                Ok(_) => {}
                Err(Signal::Break(_)) => break,
                Err(Signal::Continue) => continue,
                Err(other) => return Err(other),
            }
        }
        Ok(Value::Unit)
    }

    pub(super) fn eval_while_let(
        &mut self,
        pat: &Pattern,
        expr: &Expr,
        body: &Block,
        env: &Env,
    ) -> R<Value> {
        loop {
            let v = self.eval(expr, env)?;
            let menv = new_scope(Some(env.clone()));
            if !self.try_match(pat, &v, &menv)? {
                break;
            }
            match self.eval_block_in(body, &new_scope(Some(menv))) {
                Ok(_) => {}
                Err(Signal::Break(_)) => break,
                Err(Signal::Continue) => continue,
                Err(other) => return Err(other),
            }
        }
        Ok(Value::Unit)
    }

    pub(super) fn eval_for(
        &mut self,
        pat: &Pattern,
        iter: &Expr,
        body: &Block,
        env: &Env,
        pos: Pos,
    ) -> R<Value> {
        let it = self.eval(iter, env)?;
        // A channel is consumed lazily, blocking until each value arrives or the
        // channel closes, rather than being collected up front.
        if let Value::Channel(ch) = &it {
            while let Some(item) = self.channel_recv(ch, pos)? {
                let loop_env = new_scope(Some(env.clone()));
                self.bind_pattern(pat, item, &loop_env, false)?;
                match self.eval_block_in(body, &new_scope(Some(loop_env))) {
                    Ok(_) => {}
                    Err(Signal::Break(_)) => break,
                    Err(Signal::Continue) => continue,
                    Err(other) => return Err(other),
                }
            }
            return Ok(Value::Unit);
        }
        let items = self.iterate(&it, pos)?;
        for item in items {
            let loop_env = new_scope(Some(env.clone()));
            self.bind_pattern(pat, item, &loop_env, false)?;
            match self.eval_block_in(body, &new_scope(Some(loop_env))) {
                Ok(_) => {}
                Err(Signal::Break(_)) => break,
                Err(Signal::Continue) => continue,
                Err(other) => return Err(other),
            }
        }
        Ok(Value::Unit)
    }

    pub(super) fn iterate(&self, v: &Value, pos: Pos) -> R<Vec<Value>> {
        match v {
            Value::Range {
                start,
                end,
                inclusive,
            } => {
                let mut out = Vec::new();
                let hi = if *inclusive { *end + 1 } else { *end };
                let mut i = *start;
                while i < hi {
                    out.push(Value::Int(i));
                    i += 1;
                }
                Ok(out)
            }
            Value::List(l) => Ok(l.borrow().clone()),
            Value::Set(s) => Ok(s.borrow().clone()),
            Value::Map(m) => Ok(m
                .borrow()
                .iter()
                .map(|(k, val)| Value::Tuple(Rc::new(vec![k.clone(), val.clone()])))
                .collect()),
            Value::Str(s) => Ok(s.chars().map(Value::Char).collect()),
            _ => rt(pos, format!("{} is not iterable", v.type_name())),
        }
    }
}
