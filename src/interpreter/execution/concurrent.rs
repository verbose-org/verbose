//! Ordered publication of pure phases admitted in bounded consecutive waves.
//! Rendezvous sends apply backpressure; cancellation drops every receiver before
//! joining, so an unpublished worker cannot remain blocked on its result.
use super::{error, RuntimeError, Value};
use crate::ast::Rule;
use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc::{sync_channel, SyncSender},
};
use std::thread;

const WORKER_STACK: usize = 64 * 1024 * 1024;

enum Message {
    Value(usize, Value),
    Error(usize, RuntimeError),
    Done(bool),
}

fn produce(
    rule: &Rule,
    records: &[HashMap<String, Value>],
    evaluate: &(impl Fn(&Rule, &HashMap<String, Value>) -> Result<Value, RuntimeError> + Sync),
    cancel: &AtomicBool,
    sender: SyncSender<Message>,
) {
    let mut failed = false;
    for (index, record) in records.iter().enumerate() {
        if cancel.load(Ordering::Acquire) {
            return;
        }
        let value = evaluate(rule, record);
        if cancel.load(Ordering::Acquire) {
            return;
        }
        let value = match value {
            Ok(value) => value,
            Err(e) => {
                let _ = sender.send(Message::Error(index, e));
                return;
            }
        };
        failed |= matches!(value, Value::Bool(false));
        if sender.send(Message::Value(index, value)).is_err() {
            return;
        }
    }
    if !cancel.load(Ordering::Acquire) {
        let _ = sender.send(Message::Done(failed));
    }
}

pub(super) fn run(
    name: &str,
    phases: &[&Rule],
    records: &[HashMap<String, Value>],
    limit: usize,
    evaluate: &(impl Fn(&Rule, &HashMap<String, Value>) -> Result<Value, RuntimeError> + Sync),
    emit: &mut impl FnMut(usize, &Rule, usize, Value) -> Result<(), RuntimeError>,
) -> Result<i32, RuntimeError> {
    run_with_start(name, phases, records, limit, evaluate, emit, &|_| Ok(()))
}

// The start hook makes partial admission failure testable without exhausting
// host threads. Production uses the same checked Builder::spawn_scoped result.
fn run_with_start(
    name: &str,
    phases: &[&Rule],
    records: &[HashMap<String, Value>],
    limit: usize,
    evaluate: &(impl Fn(&Rule, &HashMap<String, Value>) -> Result<Value, RuntimeError> + Sync),
    emit: &mut impl FnMut(usize, &Rule, usize, Value) -> Result<(), RuntimeError>,
    before_start: &impl Fn(usize) -> std::io::Result<()>,
) -> Result<i32, RuntimeError> {
    if !(1..=64).contains(&limit) {
        return Err(error(
            "concurrent execution max_in_flight must be in [1, 64]",
        ));
    }
    for (wave_index, wave) in phases.chunks(limit).enumerate() {
        let cancel = AtomicBool::new(false);
        let result = thread::scope(|scope| {
            let mut workers = Vec::with_capacity(wave.len());
            let mut outcome = Ok(0);
            for (offset, &rule) in wave.iter().enumerate() {
                let phase = wave_index * limit + offset + 1;
                let (sender, receiver) = sync_channel(0);
                let flag = &cancel;
                let worker = before_start(phase).and_then(|()| {
                    thread::Builder::new()
                        .name(format!("verbose-phase-{phase}"))
                        .stack_size(WORKER_STACK)
                        .spawn_scoped(scope, move || {
                            produce(rule, records, evaluate, flag, sender)
                        })
                });
                match worker {
                    Ok(handle) => workers.push((phase, rule, receiver, handle)),
                    Err(e) => {
                        outcome = Err(error(format!(
                            "execution '{name}', phase {phase} ('{}'): cannot start worker: {e}",
                            rule.name
                        )));
                        break;
                    }
                }
            }
            // No output from a partially admitted wave. Receive one phase at a
            // time; even an already-failed later phase cannot overtake it.
            if outcome.is_ok() {
                'publish: for (phase, rule, receiver, _) in &workers {
                    loop {
                        let contextual = |index, e: RuntimeError| {
                            error(format!(
                                "execution '{name}', phase {phase} ('{}'), record {index}: {}",
                                rule.name, e.message
                            ))
                        };
                        match receiver.recv() {
                            Ok(Message::Value(index, value)) => {
                                if let Err(e) = emit(*phase, rule, index, value) {
                                    outcome = Err(contextual(index, e));
                                    break 'publish;
                                }
                            }
                            Ok(Message::Error(index, e)) => {
                                outcome = Err(contextual(index, e));
                                break 'publish;
                            }
                            Ok(Message::Done(false)) => break,
                            Ok(Message::Done(true)) => {
                                outcome = Ok(1);
                                break 'publish;
                            }
                            Err(_) => {
                                outcome = Err(error(format!("execution '{name}', phase {phase} ('{}'): worker stopped without completion", rule.name)));
                                break 'publish;
                            }
                        }
                    }
                }
            }
            cancel.store(true, Ordering::Release);
            // Drop ALL receivers first, releasing blocked sends before waiting
            // for any worker. Slots stay owned until every handle is joined.
            let handles: Vec<_> = workers
                .into_iter()
                .map(|(phase, rule, receiver, handle)| {
                    drop(receiver);
                    (phase, rule, handle)
                })
                .collect();
            for (phase, rule, handle) in handles {
                if handle.join().is_err() && matches!(outcome, Ok(0)) {
                    outcome = Err(error(format!(
                        "execution '{name}', phase {phase} ('{}'): worker panicked",
                        rule.name
                    )));
                }
            }
            outcome
        });
        match result {
            Ok(0) => {}
            result => return result,
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests;
