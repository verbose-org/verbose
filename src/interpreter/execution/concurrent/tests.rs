use super::*;
use crate::{ast::Item, lexer::Lexer, parser::Parser};
use std::sync::{atomic::AtomicUsize, Condvar, Mutex};
use std::time::Duration;

fn rules(count: usize) -> Vec<Rule> {
    let p = Parser::new(
        Lexer::new(include_str!(
            "../../../../examples/sequential_stack.verbose"
        ))
        .tokenize()
        .unwrap(),
    )
    .parse_program()
    .unwrap();
    let rule = p
        .items
        .iter()
        .find_map(|i| match i {
            Item::Rule(r) => Some(r),
            _ => None,
        })
        .unwrap();
    (1..=count)
        .map(|i| {
            let mut r = rule.clone();
            r.name = i.to_string();
            r
        })
        .collect()
}
fn batch(count: usize) -> Vec<HashMap<String, Value>> {
    (0..count)
        .map(|i| HashMap::from([("index".into(), Value::Number(i as i64))]))
        .collect()
}
fn phase(r: &Rule) -> usize {
    r.name.parse::<usize>().unwrap() - 1
}

// Synchronization-based assertions with a timeout to turn scheduling bugs into
// diagnostics instead of permanently hanging a serialized test suite.
struct Arrival {
    count: Mutex<usize>,
    changed: Condvar,
}
impl Arrival {
    fn new() -> Self {
        Self {
            count: Mutex::new(0),
            changed: Condvar::new(),
        }
    }
    fn arrive(&self) {
        *self.count.lock().unwrap() += 1;
        self.changed.notify_all();
    }
    fn until(&self, n: usize) {
        let (count, wait) = self
            .changed
            .wait_timeout_while(
                self.count.lock().unwrap(),
                Duration::from_secs(10),
                |count| *count < n,
            )
            .unwrap();
        assert!(
            !wait.timed_out() && *count >= n,
            "only {} of {n} arrivals",
            *count
        );
    }
}

#[test]
fn concurrent_waves_overlap_but_publish_in_order_and_join_before_readmission() {
    let rules = rules(6);
    let refs: Vec<_> = rules.iter().collect();
    let arrivals = Arrival::new();
    let second_finished = Arrival::new();
    let published = AtomicUsize::new(0);
    let active = AtomicUsize::new(0);
    let peak = AtomicUsize::new(0);
    let mut output = Vec::new();
    let status = run(
        "test",
        &refs,
        &batch(1),
        2,
        &|r, _| {
            let p = phase(r);
            assert!(
                published.load(Ordering::SeqCst) >= (p / 2) * 2,
                "early admission"
            );
            let now = active.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(now, Ordering::SeqCst);
            arrivals.arrive();
            arrivals.until((p / 2 + 1) * 2);
            // Deliberately complete the second phase first, every wave.
            if p % 2 == 1 {
                second_finished.arrive();
            } else {
                second_finished.until(p / 2 + 1);
            }
            active.fetch_sub(1, Ordering::SeqCst);
            Ok(Value::Number(p as i64))
        },
        &mut |p, _, index, value| {
            assert_eq!(index, 0);
            assert_eq!(value, Value::Number((p - 1) as i64));
            output.push(p);
            published.fetch_add(1, Ordering::SeqCst);
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(status, 0);
    assert_eq!(output, vec![1, 2, 3, 4, 5, 6]);
    assert_eq!(active.load(Ordering::SeqCst), 0);
    assert_eq!(peak.load(Ordering::SeqCst), 2);
}

#[test]
fn rendezvous_bounds_unpublished_results_instead_of_buffering_whole_batches() {
    let rules = rules(3);
    let refs: Vec<_> = rules.iter().collect();
    let arrivals = Arrival::new();
    let evaluated = [
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
    ];
    let mut count = 0;
    assert_eq!(
        run(
            "test",
            &refs,
            &batch(100),
            3,
            &|r, record| {
                let p = phase(r);
                if evaluated[p].fetch_add(1, Ordering::SeqCst) == 0 {
                    arrivals.arrive();
                    arrivals.until(3);
                }
                Ok(record["index"].clone())
            },
            &mut |p, _, index, _| {
                if p == 1 && index == 0 {
                    // Nobody has consumed phase 2/3: they can hold only their first
                    // result, even while phase 1 is allowed to calculate its next one.
                    assert_eq!(evaluated[1].load(Ordering::SeqCst), 1);
                    assert_eq!(evaluated[2].load(Ordering::SeqCst), 1);
                }
                count += 1;
                Ok(())
            }
        )
        .unwrap(),
        0
    );
    assert_eq!(count, 300);
    assert!(evaluated.iter().all(|n| n.load(Ordering::SeqCst) == 100));
}

#[test]
fn concurrent_failure_cancels_blocked_senders_and_prevents_later_waves() {
    let rules = rules(3);
    let refs: Vec<_> = rules.iter().collect();
    for failure in ["false", "evaluation", "output", "panic"] {
        let arrivals = Arrival::new();
        let evaluated = [
            AtomicUsize::new(0),
            AtomicUsize::new(0),
            AtomicUsize::new(0),
        ];
        let mut output = Vec::new();
        let result = run(
            "test",
            &refs,
            &batch(5),
            2,
            &|r, record| {
                let p = phase(r);
                let call = evaluated[p].fetch_add(1, Ordering::SeqCst);
                if call == 0 {
                    arrivals.arrive();
                    arrivals.until(2);
                }
                if p == 0 && call == 1 {
                    if failure == "evaluation" {
                        return Err(error("bad record"));
                    }
                    if failure == "panic" {
                        panic!("injected worker failure");
                    }
                }
                Ok(if failure == "false" {
                    Value::Bool(false)
                } else {
                    record["index"].clone()
                })
            },
            &mut |p, _, index, _| {
                if failure == "output" && index == 1 {
                    return Err(error("writer failed"));
                }
                output.push((p, index));
                Ok(())
            },
        );
        if failure == "false" {
            assert_eq!(result.unwrap(), 1);
            assert_eq!(output, (0..5).map(|n| (1, n)).collect::<Vec<_>>());
        } else {
            assert!(result.unwrap_err().message.contains("phase 1 ('1')"));
            assert_eq!(output, vec![(1, 0)]);
        }
        assert_eq!(
            evaluated[1].load(Ordering::SeqCst),
            1,
            "blocked later worker should stop"
        );
        assert_eq!(
            evaluated[2].load(Ordering::SeqCst),
            0,
            "next wave must not start"
        );
    }
}

#[test]
fn concurrent_partial_start_failure_joins_admitted_workers_without_wave_output() {
    let rules = rules(4);
    let refs: Vec<_> = rules.iter().collect();
    for fail_at in [1, 2, 4] {
        let evaluated = AtomicUsize::new(0);
        let mut output = Vec::new();
        let result = run_with_start(
            "test",
            &refs,
            &batch(1),
            2,
            &|_, _| {
                evaluated.fetch_add(1, Ordering::SeqCst);
                Ok(Value::Number(1))
            },
            &mut |p, _, _, _| {
                output.push(p);
                Ok(())
            },
            &|phase| {
                if phase == fail_at {
                    Err(std::io::Error::other("injected admission failure"))
                } else {
                    Ok(())
                }
            },
        );
        assert!(result.unwrap_err().message.contains(&format!(
            "phase {fail_at} ('{fail_at}'): cannot start worker"
        )));
        assert_eq!(output, if fail_at == 4 { vec![1, 2] } else { vec![] });
        assert!(evaluated.load(Ordering::SeqCst) < fail_at);
    }
}

#[test]
fn concurrent_later_error_waits_for_prior_publication_and_limits_are_closed() {
    let rules = rules(3);
    let refs: Vec<_> = rules.iter().collect();
    let mut output = Vec::new();
    let e = run(
        "test",
        &refs,
        &batch(2),
        3,
        &|r, record| {
            if phase(r) == 1 {
                return Err(error("later error"));
            }
            Ok(record["index"].clone())
        },
        &mut |p, _, i, _| {
            output.push((p, i));
            Ok(())
        },
    )
    .unwrap_err();
    assert!(e.message.contains("phase 2 ('2'), record 0: later error"));
    assert_eq!(output, vec![(1, 0), (1, 1)]);
    for limit in [0, 65] {
        assert!(run(
            "test",
            &refs,
            &batch(1),
            limit,
            &|_, _| panic!(),
            &mut |_, _, _, _| panic!()
        )
        .is_err());
    }
}
