use crate::{AttachmentGeneration, EndpointAddress, MAX_IN_FLIGHT_URBS, TransferKind, UrbId};
use std::collections::BTreeMap;
use std::fmt::{Display, Formatter};

/// Metadata retained for one in-flight URB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UrbRecord {
    pub generation: AttachmentGeneration,
    pub endpoint: EndpointAddress,
    pub transfer_kind: TransferKind,
    pub declared_length: usize,
    pub deadline_millis: u32,
}

/// Bounded in-flight request ledger.
#[derive(Debug, Default)]
pub struct InFlightLedger {
    requests: BTreeMap<UrbId, UrbRecord>,
}

impl InFlightLedger {
    #[must_use]
    pub fn len(&self) -> usize {
        self.requests.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.requests.is_empty()
    }

    /// Registers one unique bounded in-flight request.
    ///
    /// # Errors
    ///
    /// Returns [`LedgerError`] when the ledger is full or the ID exists.
    pub fn submit(&mut self, id: UrbId, record: UrbRecord) -> Result<(), LedgerError> {
        if self.requests.len() >= MAX_IN_FLIGHT_URBS {
            return Err(LedgerError::Capacity);
        }
        if self.requests.contains_key(&id) {
            return Err(LedgerError::Duplicate(id));
        }
        self.requests.insert(id, record);
        Ok(())
    }

    /// Removes one request in response to cancellation.
    ///
    /// # Errors
    ///
    /// Returns [`LedgerError`] for an unknown ID or stale generation.
    pub fn cancel(
        &mut self,
        generation: AttachmentGeneration,
        id: UrbId,
    ) -> Result<UrbRecord, LedgerError> {
        self.take_matching(generation, id)
    }

    /// Removes one completed request.
    ///
    /// # Errors
    ///
    /// Returns [`LedgerError`] for an unknown ID or stale generation.
    pub fn complete(
        &mut self,
        generation: AttachmentGeneration,
        id: UrbId,
    ) -> Result<UrbRecord, LedgerError> {
        self.take_matching(generation, id)
    }

    pub fn drain_generation(&mut self, generation: AttachmentGeneration) -> Vec<UrbRecord> {
        let ids: Vec<_> = self
            .requests
            .iter()
            .filter_map(|(id, record)| (record.generation == generation).then_some(*id))
            .collect();
        ids.into_iter()
            .filter_map(|id| self.requests.remove(&id))
            .collect()
    }

    fn take_matching(
        &mut self,
        generation: AttachmentGeneration,
        id: UrbId,
    ) -> Result<UrbRecord, LedgerError> {
        let Some(record) = self.requests.get(&id).copied() else {
            return Err(LedgerError::Unknown(id));
        };
        if record.generation != generation {
            return Err(LedgerError::StaleGeneration {
                expected: record.generation,
                actual: generation,
            });
        }
        self.requests.remove(&id).ok_or(LedgerError::Unknown(id))
    }
}

/// In-flight ledger rejection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LedgerError {
    Capacity,
    Duplicate(UrbId),
    Unknown(UrbId),
    StaleGeneration {
        expected: AttachmentGeneration,
        actual: AttachmentGeneration,
    },
}

impl Display for LedgerError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Capacity => formatter.write_str("in-flight URB limit reached"),
            Self::Duplicate(id) => write!(formatter, "duplicate URB id {}", id.get()),
            Self::Unknown(id) => write!(formatter, "unknown URB id {}", id.get()),
            Self::StaleGeneration { expected, actual } => write!(
                formatter,
                "stale attachment generation {actual}; expected {expected}"
            ),
        }
    }
}

impl std::error::Error for LedgerError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::{NonZeroU32, NonZeroU64};

    fn generation(value: u64) -> AttachmentGeneration {
        AttachmentGeneration::new(NonZeroU64::new(value).unwrap())
    }

    fn id(value: u32) -> UrbId {
        UrbId::new(NonZeroU32::new(value).unwrap())
    }

    fn record() -> UrbRecord {
        UrbRecord {
            generation: generation(1),
            endpoint: EndpointAddress(0x81),
            transfer_kind: TransferKind::Interrupt,
            declared_length: 10,
            deadline_millis: 1_000,
        }
    }

    #[test]
    fn duplicate_and_stale_completion_fail_closed() {
        let mut ledger = InFlightLedger::default();
        ledger.submit(id(1), record()).unwrap();
        assert_eq!(
            ledger.submit(id(1), record()),
            Err(LedgerError::Duplicate(id(1)))
        );
        assert_eq!(
            ledger.complete(generation(2), id(1)),
            Err(LedgerError::StaleGeneration {
                expected: generation(1),
                actual: generation(2),
            })
        );
        assert_eq!(ledger.complete(generation(1), id(1)), Ok(record()));
        assert!(ledger.is_empty());
    }
}
