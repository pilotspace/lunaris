//! Six bi-temporal primitives — verbatim per blueprint §3.3.
//!
//! Every primitive carries a `BiTemporal { valid, sys }` stamp from a shared `HlcClock`.
//! Every primitive is `Send + Sync + 'static`, `Debug`, `Clone`, `PartialEq`, and serde-roundtrippable.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use ulid::Ulid;

use crate::bitemporal::BiTemporal;
use crate::hlc::HlcClock;

// ---------------- Episode ----------------

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Episode {
    pub id: Ulid,
    pub source: String,
    pub content: String,
    pub t_ref: Option<DateTime<Utc>>,
    pub bt: BiTemporal,
    #[serde(default)]
    pub metadata: serde_json::Map<String, serde_json::Value>,
}

impl Episode {
    pub fn new(source: impl Into<String>, content: impl Into<String>, clock: &HlcClock) -> Self {
        Self {
            id: Ulid::new(),
            source: source.into(),
            content: content.into(),
            t_ref: None,
            bt: BiTemporal::now(clock),
            metadata: serde_json::Map::new(),
        }
    }
}

// ---------------- Chunk ----------------

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Chunk {
    pub id: Ulid,
    pub episode_id: Ulid,
    pub text: String,
    pub tokens: u32,
    pub offset: u32,
    #[serde(default)]
    pub heading_path: Vec<String>,
    #[serde(default)]
    pub overlap_tail: String,
    #[serde(default)]
    pub embedding: Option<Vec<f32>>,
    pub bt: BiTemporal,
}

impl Chunk {
    pub fn new(
        episode_id: Ulid,
        text: impl Into<String>,
        tokens: u32,
        offset: u32,
        heading_path: Vec<String>,
        clock: &HlcClock,
    ) -> Self {
        Self {
            id: Ulid::new(),
            episode_id,
            text: text.into(),
            tokens,
            offset,
            heading_path,
            overlap_tail: String::new(),
            embedding: None,
            bt: BiTemporal::now(clock),
        }
    }
}

// ---------------- Entity ----------------

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Entity {
    pub id: Ulid,
    pub name: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    pub entity_type: String,
    #[serde(default)]
    pub embedding: Option<Vec<f32>>,
    pub bt: BiTemporal,
    pub confidence: f32,
}

impl Entity {
    pub fn new(
        name: impl Into<String>,
        entity_type: impl Into<String>,
        confidence: f32,
        clock: &HlcClock,
    ) -> Self {
        Self {
            id: Ulid::new(),
            name: name.into(),
            aliases: Vec::new(),
            entity_type: entity_type.into(),
            embedding: None,
            bt: BiTemporal::now(clock),
            confidence,
        }
    }
}

// ---------------- Relation ----------------

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Relation {
    pub id: Ulid,
    pub src: Ulid,
    pub dst: Ulid,
    pub rel_type: String,
    pub bt: BiTemporal,
    pub confidence: f32,
    #[serde(default)]
    pub provenance: Vec<Ulid>,
}

impl Relation {
    pub fn new(
        src: Ulid,
        dst: Ulid,
        rel_type: impl Into<String>,
        confidence: f32,
        clock: &HlcClock,
    ) -> Self {
        Self {
            id: Ulid::new(),
            src,
            dst,
            rel_type: rel_type.into(),
            bt: BiTemporal::now(clock),
            confidence,
            provenance: Vec::new(),
        }
    }
}

// ---------------- Fact ----------------

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Fact {
    pub id: Ulid,
    pub subject: Ulid,
    pub predicate: String,
    pub object: Ulid,
    pub fact_text: String,
    #[serde(default)]
    pub embedding: Option<Vec<f32>>,
    pub bt: BiTemporal,
    pub confidence: f32,
    #[serde(default)]
    pub provenance: Vec<Ulid>,
    pub activation: f32,
}

impl Fact {
    pub fn new(
        subject: Ulid,
        predicate: impl Into<String>,
        object: Ulid,
        fact_text: impl Into<String>,
        confidence: f32,
        clock: &HlcClock,
    ) -> Self {
        Self {
            id: Ulid::new(),
            subject,
            predicate: predicate.into(),
            object,
            fact_text: fact_text.into(),
            embedding: None,
            bt: BiTemporal::now(clock),
            confidence,
            provenance: Vec::new(),
            activation: 0.0,
        }
    }
}

// ---------------- Community ----------------

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Community {
    pub id: Ulid,
    pub level: u8,
    pub parent: Option<Ulid>,
    #[serde(default)]
    pub members: Vec<Ulid>,
    pub summary: String,
    #[serde(default)]
    pub summary_embedding: Option<Vec<f32>>,
    pub bt: BiTemporal,
}

impl Community {
    pub fn new(level: u8, summary: impl Into<String>, clock: &HlcClock) -> Self {
        Self {
            id: Ulid::new(),
            level,
            parent: None,
            members: Vec::new(),
            summary: summary.into(),
            summary_embedding: None,
            bt: BiTemporal::now(clock),
        }
    }
}
