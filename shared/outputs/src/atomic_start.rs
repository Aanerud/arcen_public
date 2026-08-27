//! Generic all-or-reverse-rollback start policy.
//!
//! Both hosts hand-wrote the same algorithm as `spawn_all_or_rollback`: start
//! every planned item in order, and the instant any one fails, tear down every
//! item already started, in reverse start order, before returning the failure.
//! Startup is therefore atomic — either every region starts, or none remain
//! running — which is the ADR 0009 policy that a multi-region session never
//! serves a subset of its planned regions.
//!
//! This module owns that policy and nothing else. It names no queue, no child
//! process, no handle, and no host error: the specification, the started item,
//! the start failure, and the teardown failure are all caller-chosen types,
//! and both transitions are caller-supplied closures. A host whose teardown
//! cannot fail instantiates the teardown error as
//! [`core::convert::Infallible`].
//!
//! Like the rest of this crate, a teardown failure is never flattened into
//! the start failure: [`AtomicStartFailure`] keeps the start failure and every
//! teardown failure as separate typed values, each attributed to the index of
//! the item it belongs to.

use core::fmt;
use core::future::Future;

/// One teardown that failed during rollback, attributed to the item it was
/// tearing down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RollbackFailure<R> {
    /// Index of the item in the original start order.
    pub index: usize,
    /// Why the teardown failed.
    pub source: R,
}

impl<R: fmt::Display> fmt::Display for RollbackFailure<R> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "rolling back item {} failed: {}",
            self.index, self.source
        )
    }
}

impl<R: fmt::Debug + fmt::Display> std::error::Error for RollbackFailure<R> {}

/// An atomic start that failed, with the rollback outcome preserved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AtomicStartFailure<E, R> {
    index: usize,
    started: usize,
    source: E,
    rollback_failures: Vec<RollbackFailure<R>>,
}

impl<E, R> AtomicStartFailure<E, R> {
    /// Index of the item whose start failed, in the original start order.
    #[must_use]
    pub const fn index(&self) -> usize {
        self.index
    }

    /// How many items were already running when the failure happened. Every
    /// one of them was torn down before this value was returned.
    #[must_use]
    pub const fn started(&self) -> usize {
        self.started
    }

    /// Why the start failed.
    #[must_use]
    pub const fn start_error(&self) -> &E {
        &self.source
    }

    /// Takes the start failure, dropping the rollback detail.
    #[must_use]
    pub fn into_start_error(self) -> E {
        self.source
    }

    /// Every teardown that failed, in the order teardown was attempted, which
    /// is the reverse of the start order.
    #[must_use]
    pub fn rollback_failures(&self) -> &[RollbackFailure<R>] {
        &self.rollback_failures
    }

    /// Whether every started item was torn down successfully, so the host
    /// holds no outstanding obligation.
    #[must_use]
    pub fn is_fully_rolled_back(&self) -> bool {
        self.rollback_failures.is_empty()
    }
}

impl<E: fmt::Display, R: fmt::Display> fmt::Display for AtomicStartFailure<E, R> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "starting item {} failed after {} already started: {}",
            self.index, self.started, self.source
        )?;
        for failure in &self.rollback_failures {
            write!(formatter, "; {failure}")?;
        }
        Ok(())
    }
}

impl<E: fmt::Debug + fmt::Display, R: fmt::Debug + fmt::Display> std::error::Error
    for AtomicStartFailure<E, R>
{
}

/// Starts every specification in order, or leaves nothing running.
///
/// `start_one` is applied to each specification in iteration order. The
/// instant one fails, every item already started is handed to `stop_one` in
/// reverse start order — every one of them, even if an earlier teardown
/// failed — and the collected outcome is returned.
///
/// The specification is passed to `start_one` by value, so a host that needs
/// the region identity inside its own error type builds that error in the
/// closure, where it still owns the specification.
///
/// # Errors
///
/// Returns [`AtomicStartFailure`] carrying the failing index, the number of
/// items that had started, the start failure, and every teardown failure.
pub async fn start_all_or_rollback<Spec, Started, StartError, StopError, StartFut, StopFut>(
    specs: impl IntoIterator<Item = Spec>,
    mut start_one: impl FnMut(Spec) -> StartFut,
    mut stop_one: impl FnMut(Started) -> StopFut,
) -> Result<Vec<Started>, AtomicStartFailure<StartError, StopError>>
where
    StartFut: Future<Output = Result<Started, StartError>>,
    StopFut: Future<Output = Result<(), StopError>>,
{
    let specs = specs.into_iter();
    let mut started: Vec<Started> = Vec::with_capacity(specs.size_hint().0);
    for (index, spec) in specs.enumerate() {
        match start_one(spec).await {
            Ok(item) => started.push(item),
            Err(source) => {
                let count = started.len();
                let mut rollback_failures = Vec::new();
                // Reverse start order, and every started item is torn down
                // even when an earlier teardown already failed.
                while let Some(item) = started.pop() {
                    if let Err(source) = stop_one(item).await {
                        rollback_failures.push(RollbackFailure {
                            index: started.len(),
                            source,
                        });
                    }
                }
                return Err(AtomicStartFailure {
                    index,
                    started: count,
                    source,
                    rollback_failures,
                });
            }
        }
    }
    Ok(started)
}

#[cfg(test)]
mod tests {
    use core::convert::Infallible;
    use core::future::ready;
    use std::cell::RefCell;

    use super::{AtomicStartFailure, start_all_or_rollback};
    use crate::block_on::block_on;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct Spec(usize);

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct Started(usize);

    #[derive(Debug, Default)]
    struct Log {
        events: RefCell<Vec<String>>,
    }

    impl Log {
        fn push(&self, event: String) {
            self.events.borrow_mut().push(event);
        }

        fn events(&self) -> Vec<String> {
            self.events.borrow().clone()
        }
    }

    fn start_all(
        count: usize,
        failing: Option<usize>,
        failing_stops: &[usize],
    ) -> (
        Log,
        Result<Vec<Started>, AtomicStartFailure<String, String>>,
    ) {
        let log = Log::default();
        let result = block_on(start_all_or_rollback(
            (0..count).map(Spec),
            |Spec(index)| {
                log.push(format!("start:{index}"));
                ready(if failing == Some(index) {
                    Err(format!("start {index} failed"))
                } else {
                    Ok(Started(index))
                })
            },
            |Started(index)| {
                log.push(format!("stop:{index}"));
                ready(if failing_stops.contains(&index) {
                    Err(format!("stop {index} failed"))
                } else {
                    Ok(())
                })
            },
        ));
        (log, result)
    }

    #[test]
    fn starts_every_item_when_none_fail() {
        let (log, result) = start_all(4, None, &[]);
        assert_eq!(
            result.expect("all start"),
            vec![Started(0), Started(1), Started(2), Started(3)]
        );
        assert_eq!(log.events(), ["start:0", "start:1", "start:2", "start:3"]);
    }

    #[test]
    fn starts_nothing_for_an_empty_specification_set() {
        let (log, result) = start_all(0, None, &[]);
        assert_eq!(result.expect("no items"), Vec::new());
        assert!(log.events().is_empty());
    }

    #[test]
    fn a_later_failure_rolls_back_every_started_item_in_reverse_order() {
        let (log, result) = start_all(4, Some(3), &[]);
        let failure = result.expect_err("item 3 must fail");
        assert_eq!(failure.index(), 3);
        assert_eq!(failure.started(), 3);
        assert_eq!(failure.start_error(), "start 3 failed");
        assert!(failure.is_fully_rolled_back());
        assert_eq!(
            log.events(),
            [
                "start:0", "start:1", "start:2", "start:3", "stop:2", "stop:1", "stop:0"
            ]
        );
    }

    #[test]
    fn every_failure_index_rolls_back_exactly_its_predecessors() {
        for failing in 0..4 {
            let (log, result) = start_all(4, Some(failing), &[]);
            let failure = result.expect_err("the selected item must fail");
            assert_eq!(failure.index(), failing);
            assert_eq!(failure.started(), failing);

            let mut expected: Vec<String> = (0..=failing)
                .map(|index| format!("start:{index}"))
                .collect();
            expected.extend((0..failing).rev().map(|index| format!("stop:{index}")));
            assert_eq!(log.events(), expected);
        }
    }

    #[test]
    fn a_partial_teardown_failure_still_tears_down_every_other_item() {
        let (log, result) = start_all(4, Some(3), &[1]);
        let failure = result.expect_err("item 3 must fail");
        assert!(!failure.is_fully_rolled_back());
        assert_eq!(failure.rollback_failures().len(), 1);
        assert_eq!(failure.rollback_failures()[0].index, 1);
        assert_eq!(failure.rollback_failures()[0].source, "stop 1 failed");
        assert_eq!(
            log.events(),
            [
                "start:0", "start:1", "start:2", "start:3", "stop:2", "stop:1", "stop:0"
            ],
            "a failed teardown never stops the remaining teardowns"
        );
    }

    #[test]
    fn every_teardown_failure_survives_separately_from_the_start_failure() {
        let (_log, result) = start_all(4, Some(3), &[0, 1, 2]);
        let failure = result.expect_err("item 3 must fail");
        assert_eq!(failure.start_error(), "start 3 failed");
        let attributed: Vec<(usize, &str)> = failure
            .rollback_failures()
            .iter()
            .map(|failure| (failure.index, failure.source.as_str()))
            .collect();
        assert_eq!(
            attributed,
            [
                (2, "stop 2 failed"),
                (1, "stop 1 failed"),
                (0, "stop 0 failed")
            ],
            "teardown failures are recorded in teardown order, newest first"
        );
        let rendered = failure.to_string();
        assert!(rendered.contains("start 3 failed"), "{rendered}");
        assert!(rendered.contains("stop 0 failed"), "{rendered}");
        assert_eq!(failure.into_start_error(), "start 3 failed");
    }

    #[test]
    fn an_infallible_teardown_never_reports_a_rollback_failure() {
        let log = Log::default();
        let result: Result<Vec<Started>, AtomicStartFailure<&str, Infallible>> =
            block_on(start_all_or_rollback(
                (0..3).map(Spec),
                |Spec(index)| {
                    log.push(format!("start:{index}"));
                    ready(if index == 2 {
                        Err("boom")
                    } else {
                        Ok(Started(index))
                    })
                },
                |Started(index)| {
                    log.push(format!("stop:{index}"));
                    ready(Ok::<(), Infallible>(()))
                },
            ));
        let failure = result.expect_err("item 2 must fail");
        assert!(failure.is_fully_rolled_back());
        assert_eq!(*failure.start_error(), "boom");
        assert_eq!(
            log.events(),
            ["start:0", "start:1", "start:2", "stop:1", "stop:0"]
        );
    }

    #[test]
    fn the_start_future_is_send_when_its_parts_are() {
        fn assert_send<T: Send>(_value: T) {}

        assert_send(start_all_or_rollback(
            (0..2).map(Spec),
            |Spec(index)| ready(Ok::<Started, &str>(Started(index))),
            |_started: Started| ready(Ok::<(), Infallible>(())),
        ));
    }
}
