//! The registry: the catalogue of promises, holding the declarations
//! themselves.
//!
//! Entries are the model's value types wrapped in [`Entry`] — the
//! value registered *is* the definition; nothing is projected,
//! duplicated, or re-described. Per-operation views for dispatch
//! ([`Registry::resolve`]) are computed on lookup from the entry, so
//! there is no per-op storage to drift. `describe()` serializes the
//! entries — the payload of List(Operation), the surface describing
//! itself, in the model's own three halves.
//!
//! Registration is where identity collisions surface (the one
//! guarantee the declared names owe us): a resource claims its derived
//! generic names, a command its `domain.verb`, a report its name — any
//! clash, including a verb shadowing a derived name like
//! `invocation.get`, is refused as [`RegistryError::Duplicate`].

use std::collections::BTreeMap;

use schemars::Schema;

use crate::model::{Authority, Command, Domain, Nature, Report, Resource, Verb};
use crate::opid::{OpCategory, OpId};

/// One registered declaration — the heterogeneous collection is just
/// the model's three value types behind an enum.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Entry {
    Resource(Resource),
    Command(Command),
    Report(Report),
}

impl From<Resource> for Entry {
    fn from(r: Resource) -> Self {
        Entry::Resource(r)
    }
}
impl From<Command> for Entry {
    fn from(c: Command) -> Self {
        Entry::Command(c)
    }
}
impl From<Report> for Entry {
    fn from(r: Report) -> Self {
        Entry::Report(r)
    }
}

/// Why a registration was refused — a defect in the registering code,
/// not a runtime condition to retry.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RegistryError {
    #[error("`{name}` is already registered — one registry, one description per operation (D1)")]
    Duplicate { name: String },
    #[error("domain `{domain:?}` is already in the catalogue — one entry per resource")]
    DuplicateResource { domain: Domain },
}

/// A per-operation view, computed on lookup from the owning entry —
/// what the edge needs to dispatch one call: category (envelope
/// shape), authority (authz middleware), schemas (validation).
#[derive(Debug, Clone, PartialEq)]
pub struct Resolved<'a> {
    pub op: OpId,
    pub category: OpCategory,
    pub authority: Vec<Authority>,
    pub version: u32,
    pub input_schema: &'a Schema,
    pub output_schema: Option<&'a Schema>,
}

#[derive(Debug, Default)]
pub struct Registry {
    /// Rendered name → the entry that claims it. Resources appear once
    /// per derived op; commands and reports once. The map is both the
    /// collision check and the string-addressed index (MCP tool names,
    /// docs routes) — nothing parses.
    names: BTreeMap<String, usize>,
    entries: Vec<Entry>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// The rendered names a resource's derived surface claims.
    fn derived_ops(resource: &Resource) -> Vec<OpId> {
        match resource.nature {
            Nature::Atom => vec![
                OpId::Get(resource.domain),
                OpId::List(resource.domain),
                OpId::Stream(resource.domain),
            ],
            Nature::View => vec![OpId::Get(resource.domain), OpId::List(resource.domain)],
            Nature::Synthetic => vec![OpId::Get(resource.domain)],
        }
    }

    /// Register a declaration. The value is stored as given — it is
    /// the definition — and every rendered name it claims is checked
    /// for collision.
    pub fn register(&mut self, entry: impl Into<Entry>) -> Result<(), RegistryError> {
        let entry = entry.into();
        let claimed: Vec<String> = match &entry {
            Entry::Resource(resource) => {
                if self
                    .entries
                    .iter()
                    .any(|e| matches!(e, Entry::Resource(r) if r.domain == resource.domain))
                {
                    return Err(RegistryError::DuplicateResource {
                        domain: resource.domain,
                    });
                }
                Self::derived_ops(resource)
                    .iter()
                    .map(ToString::to_string)
                    .collect()
            }
            Entry::Command(command) => vec![command.op().to_string()],
            Entry::Report(report) => vec![report.op().to_string()],
        };
        for name in &claimed {
            if self.names.contains_key(name) {
                return Err(RegistryError::Duplicate { name: name.clone() });
            }
        }
        let index = self.entries.len();
        self.entries.push(entry);
        for name in claimed {
            self.names.insert(name, index);
        }
        Ok(())
    }

    /// Resolve one operation for dispatch — computed from the owning
    /// entry, stored nowhere.
    pub fn resolve(&self, op: &OpId) -> Option<Resolved<'_>> {
        let entry = &self.entries[*self.names.get(&op.to_string())?];
        let read = |scope| {
            vec![Authority {
                verb: Verb::Read,
                scope,
            }]
        };
        Some(match (entry, op) {
            (Entry::Resource(r), OpId::Get(_)) => Resolved {
                op: op.clone(),
                category: OpCategory::Get,
                authority: read(r.domain),
                version: r.version,
                input_schema: &r.key_schema,
                output_schema: Some(&r.state_schema),
            },
            (Entry::Resource(r), OpId::List(_) | OpId::Stream(_)) => Resolved {
                op: op.clone(),
                category: op.category(),
                authority: read(r.domain),
                version: r.version,
                input_schema: &r.filter_schema,
                output_schema: Some(&r.state_schema),
            },
            (Entry::Command(c), _) => Resolved {
                op: op.clone(),
                category: OpCategory::DomainVerb,
                authority: vec![c.authority],
                version: c.version,
                input_schema: &c.input_schema,
                // A command's output is a Receipt by construction —
                // its schema is a wire constant, not per-op data.
                output_schema: None,
            },
            (Entry::Report(r), _) => Resolved {
                op: op.clone(),
                category: OpCategory::Report,
                authority: r
                    .reads
                    .iter()
                    .map(|scope| Authority {
                        verb: Verb::Read,
                        scope: *scope,
                    })
                    .collect(),
                version: r.version,
                input_schema: &r.params_schema,
                output_schema: Some(&r.output_schema),
            },
            _ => return None,
        })
    }

    /// Resolve by rendered name — for string-addressed adapters (MCP
    /// tool names, docs routes). The registry is the index; nothing
    /// parses.
    pub fn resolve_named(&self, name: &str) -> Option<Resolved<'_>> {
        // Reconstruct the OpId from the owning entry rather than the
        // string: the name map points at the entry, and the entry
        // knows which of its ops the name denotes.
        let entry = &self.entries[*self.names.get(name)?];
        let op = match entry {
            Entry::Resource(r) => Self::derived_ops(r)
                .into_iter()
                .find(|op| op.to_string() == name)?,
            Entry::Command(c) => c.op(),
            Entry::Report(r) => r.op(),
        };
        self.resolve(&op)
    }

    /// The registered names, in order — the derived surface made
    /// visible.
    pub fn names(&self) -> Vec<&str> {
        self.names.keys().map(String::as_str).collect()
    }

    /// Every registered declaration, in registration order — the
    /// payload of List(Operation): the surface describing itself, in
    /// the model's own halves.
    pub fn describe(&self) -> &[Entry] {
        &self.entries
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}
