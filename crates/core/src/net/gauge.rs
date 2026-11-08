//! Connection census for the process `NetStack`.
//!
//! A protocol is `Known` only when this process read that protocol's own
//! pool. `Unknown` means the protocol is not instrumented, or this sample
//! could not be read. Those are different from zero, and different from
//! `Disabled`. The engine total is `Ready` only when every protocol is
//! `Disabled` or `Known`. There is no denominator on either type.

/// One protocol's contribution to the process connection census.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolGauge {
    /// The setting is off. This protocol adds zero to a `Ready` total.
    Disabled,
    /// Not instrumented, or this sample could not be read. Not zero.
    Unknown,
    /// A sample that was actually read.
    ///
    /// `active + idle` is at most `pooled`. `draining` overlaps `active` and
    /// is not part of that sum. `connecting` is outside the pool.
    Known {
        active: u64,
        idle: u64,
        connecting: u64,
        draining: u64,
        pooled: u64,
    },
}

/// Numbers inside a `Ready` snapshot. Built only by [`EngineConnections::from_protocols`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadyConnections {
    active: u64,
    h1: ProtocolGauge,
    h2: ProtocolGauge,
    h3: ProtocolGauge,
}

/// Process-wide connection total.
///
/// `Unavailable` has no number. Callers read [`EngineConnections::active`],
/// which is `None` here, so a missing sample cannot be shown as zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineConnections {
    Unavailable,
    Ready(ReadyConnections),
}

impl EngineConnections {
    /// Build the process total from the three protocol gauges.
    ///
    /// Returns `Unavailable` when any gauge is `Unknown`. An H3 `Known`
    /// sample beside an H1 or H2 `Unknown` does not produce a number.
    /// `Disabled` contributes 0. There is no cap and no second number.
    pub fn from_protocols(h1: ProtocolGauge, h2: ProtocolGauge, h3: ProtocolGauge) -> Self {
        let Some(h1_active) = contribution(h1) else {
            return EngineConnections::Unavailable;
        };
        let Some(h2_active) = contribution(h2) else {
            return EngineConnections::Unavailable;
        };
        let Some(h3_active) = contribution(h3) else {
            return EngineConnections::Unavailable;
        };
        EngineConnections::Ready(ReadyConnections {
            active: h1_active
                .saturating_add(h2_active)
                .saturating_add(h3_active),
            h1,
            h2,
            h3,
        })
    }

    /// `Some` only for a `Ready` snapshot. `Unavailable` is `None`, not zero.
    pub fn active(self) -> Option<u64> {
        match self {
            EngineConnections::Unavailable => None,
            EngineConnections::Ready(ready) => Some(ready.active),
        }
    }

    /// Protocol breakdown for a `Ready` snapshot, in `(h1, h2, h3)` order.
    ///
    /// `None` while the total is `Unavailable`, so an H3 sample cannot be
    /// read back out of the engine total.
    pub fn protocols(self) -> Option<(ProtocolGauge, ProtocolGauge, ProtocolGauge)> {
        match self {
            EngineConnections::Unavailable => None,
            EngineConnections::Ready(ready) => Some((ready.h1, ready.h2, ready.h3)),
        }
    }
}

fn contribution(gauge: ProtocolGauge) -> Option<u64> {
    match gauge {
        ProtocolGauge::Disabled => Some(0),
        ProtocolGauge::Unknown => None,
        ProtocolGauge::Known { active, .. } => Some(active),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn known(active: u64) -> ProtocolGauge {
        ProtocolGauge::Known {
            active,
            idle: 0,
            connecting: 0,
            draining: 0,
            pooled: active,
        }
    }

    #[test]
    fn h3_alone_cannot_build_the_engine_total() {
        let h3 = known(1);
        let snapshot =
            EngineConnections::from_protocols(ProtocolGauge::Unknown, ProtocolGauge::Unknown, h3);
        assert!(matches!(snapshot, EngineConnections::Unavailable));
        assert_eq!(snapshot.active(), None);
        assert_eq!(snapshot.protocols(), None);
    }

    #[test]
    fn ready_sums_settled_protocols_and_counts_disabled_as_zero() {
        let h1 = known(2);
        let h2 = known(3);
        let snapshot = EngineConnections::from_protocols(h1, h2, ProtocolGauge::Disabled);
        assert_eq!(snapshot.active(), Some(5));
        assert_eq!(
            snapshot.protocols(),
            Some((h1, h2, ProtocolGauge::Disabled))
        );
    }

    #[test]
    fn one_unknown_protocol_refuses_ready() {
        let snapshot = EngineConnections::from_protocols(
            known(1),
            ProtocolGauge::Unknown,
            ProtocolGauge::Disabled,
        );
        assert_eq!(snapshot.active(), None);
    }
}
