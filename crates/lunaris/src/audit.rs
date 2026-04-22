//! Plan 13-01 — re-export shim for the canonical audit types that now live in
//! `lunaris-core::audit`. This file preserves the old `use crate::audit::*`
//! import surface so `crates/lunaris/src/forget.rs` and every downstream test
//! keeps compiling without modification.
//!
//! The actual type definitions, serde shape, and `publish_audit_event` helper
//! are at `crates/lunaris-core/src/audit.rs`. Wire-shape byte-identity is
//! locked by the fixture-parity test at
//! `crates/lunaris-core/tests/audit_event_fixture_parity.rs`.
//!
//! ## Bridge `From` impls
//!
//! `ForgetReceipt` (at `crates/lunaris/src/forget.rs`) uses the domain types
//! `ForgetTarget` / `ScopeSpec` / `IndexKind`. The canonical enum in
//! `lunaris-core::audit` uses local mirror types (`ForgetTargetData`,
//! `ScopeSpecData`, `IndexKindData`, `ForgetReceiptData`) to stay leaf-pure.
//! The `From<&ForgetReceipt> for ForgetReceiptData` impl below does the
//! field-by-field map so `Lunaris::forget` can keep passing a `ForgetReceipt`
//! to the typed helper via `.into()`.

pub use lunaris_core::audit::{
    AuditEvent, FactIdData, ForgetReceiptData, ForgetTargetData, IndexKindData, PublishError,
    Publisher, ScopeSpecData, AUDIT_TOPIC, publish_audit_event,
};

use crate::forget::{ForgetReceipt, ForgetTarget, IndexKind, ScopeSpec};

impl From<&ForgetTarget> for ForgetTargetData {
    fn from(t: &ForgetTarget) -> Self {
        match t {
            ForgetTarget::Id(u) => ForgetTargetData::Id(*u),
            ForgetTarget::Scope(s) => ForgetTargetData::Scope(s.into()),
            ForgetTarget::Before(h) => ForgetTargetData::Before(*h),
        }
    }
}

impl From<&ScopeSpec> for ScopeSpecData {
    fn from(s: &ScopeSpec) -> Self {
        match s {
            ScopeSpec::BySource(v) => ScopeSpecData::BySource(v.clone()),
            ScopeSpec::ByMetadata(k, v) => ScopeSpecData::ByMetadata(k.clone(), v.clone()),
            ScopeSpec::ByEpisode(u) => ScopeSpecData::ByEpisode(*u),
        }
    }
}

impl From<&IndexKind> for IndexKindData {
    fn from(i: &IndexKind) -> Self {
        match i {
            IndexKind::Kv => IndexKindData::Kv,
            IndexKind::Vector => IndexKindData::Vector,
            IndexKind::Graph => IndexKindData::Graph,
        }
    }
}

impl From<&ForgetReceipt> for ForgetReceiptData {
    fn from(r: &ForgetReceipt) -> Self {
        Self {
            target: (&r.target).into(),
            indices_affected: r.indices_affected.iter().map(Into::into).collect(),
            rows_written: r.rows_written,
            rows_deleted: r.rows_deleted,
            audit_lsn: r.audit_lsn,
            preview: r.preview,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forget::{ForgetTarget, IndexKind, ScopeSpec};
    use lunaris_core::storage::types::Lsn;
    use ulid::Ulid;

    #[test]
    fn re_exported_audit_topic_matches() {
        assert_eq!(AUDIT_TOPIC, "__lunaris_audit__");
    }

    #[test]
    fn forget_receipt_bridge_round_trips_every_target_variant() {
        let lsn = Lsn { wall_ms: 1, counter: 2 };
        // Id variant
        let id = Ulid::new();
        let r1 = ForgetReceipt {
            target: ForgetTarget::Id(id),
            indices_affected: vec![IndexKind::Kv, IndexKind::Vector, IndexKind::Graph],
            rows_written: 3,
            rows_deleted: 4,
            audit_lsn: lsn,
            preview: true,
        };
        let d1: ForgetReceiptData = (&r1).into();
        assert_eq!(d1.target, ForgetTargetData::Id(id));
        assert_eq!(
            d1.indices_affected,
            vec![IndexKindData::Kv, IndexKindData::Vector, IndexKindData::Graph]
        );
        assert_eq!(d1.rows_written, 3);
        assert_eq!(d1.rows_deleted, 4);
        assert_eq!(d1.preview, true);

        // Scope::BySource
        let r2 = ForgetReceipt {
            target: ForgetTarget::Scope(ScopeSpec::BySource("src".into())),
            ..r1.clone()
        };
        let d2: ForgetReceiptData = (&r2).into();
        assert_eq!(
            d2.target,
            ForgetTargetData::Scope(ScopeSpecData::BySource("src".into()))
        );

        // Scope::ByMetadata
        let r3 = ForgetReceipt {
            target: ForgetTarget::Scope(ScopeSpec::ByMetadata("k".into(), "v".into())),
            ..r1.clone()
        };
        let d3: ForgetReceiptData = (&r3).into();
        assert_eq!(
            d3.target,
            ForgetTargetData::Scope(ScopeSpecData::ByMetadata("k".into(), "v".into()))
        );

        // Scope::ByEpisode
        let r4 = ForgetReceipt {
            target: ForgetTarget::Scope(ScopeSpec::ByEpisode(id)),
            ..r1.clone()
        };
        let d4: ForgetReceiptData = (&r4).into();
        assert_eq!(
            d4.target,
            ForgetTargetData::Scope(ScopeSpecData::ByEpisode(id))
        );
    }
}
