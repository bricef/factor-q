//! The registry: the catalogue of promises, self-describing.
//!
//! Resources register once and receive their derived surface: views
//! Get + List, atoms Get + List + Stream, synthetic resources Get
//! alone, Create where opted in — all with derived authority. The
//! declared surface (domain verbs, reports) registers per definition
//! with declared authority and identity, so adding a verb touches its
//! impl and one register call, nothing else. Every entry becomes an
//! [`OpDescriptor`] — the payload of List(Operation) (the surface
//! describing itself) and the input to client-wrapper codegen.
//!
//! Registration is where identity collisions surface (the one
//! guarantee the declared verb strings owe us), as [`RegistryError::Duplicate`].

use std::collections::BTreeMap;

use schemars::{Schema, schema_for};

use crate::model::{Authority, Stability, Verb};
use crate::model::{Command, Report};
use crate::model::{Domain, Nature, Resource};
use crate::opid::{OpCategory, OpId};

/// One registered promise, described. Operations have categories;
/// natures belong to resources ([`ResourceDescriptor`]) — an op's
/// semantics follow from its category plus, for generic ops, the
/// nature of the domain it reads. `authority` is a list because a
/// report reads several scopes; generic operations and verbs carry
/// exactly one entry.
#[derive(Debug, Clone, serde::Serialize)]
pub struct OpDescriptor {
    pub op: OpId,
    pub name: String,
    pub category: OpCategory,
    pub version: u32,
    pub authority: Vec<Authority>,
    pub description: &'static str,
    pub stability: Stability,
    pub caveats: &'static str,
    pub input_schema: Schema,
    pub output_schema: Schema,
}

/// One catalogue entry, described: the resource-level half of the
/// registry's payload, recorded once per registered resource — where
/// nature lives, alongside the docs shared by the resource's whole
/// derived surface.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ResourceDescriptor {
    pub domain: Domain,
    pub nature: Nature,
    pub version: u32,
    pub stability: Stability,
    pub description: &'static str,
    pub caveats: &'static str,
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

#[derive(Debug, Default)]
pub struct Registry {
    resources: BTreeMap<&'static str, ResourceDescriptor>,
    ops: BTreeMap<String, OpDescriptor>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    fn insert(&mut self, descriptor: OpDescriptor) -> Result<(), RegistryError> {
        if self.ops.contains_key(&descriptor.name) {
            return Err(RegistryError::Duplicate {
                name: descriptor.name,
            });
        }
        self.ops.insert(descriptor.name.clone(), descriptor);
        Ok(())
    }

    /// Record the resource-level catalogue entry — a pure projection
    /// of the impl.
    fn insert_resource<R: Resource>(&mut self) -> Result<(), RegistryError> {
        let segment = R::DOMAIN.segment();
        if self.resources.contains_key(segment) {
            return Err(RegistryError::DuplicateResource { domain: R::DOMAIN });
        }
        self.resources.insert(
            segment,
            ResourceDescriptor {
                domain: R::DOMAIN,
                nature: R::NATURE,
                version: R::VERSION,
                description: R::DESCRIPTION,
                stability: R::STABILITY,
                caveats: R::CAVEATS,
            },
        );
        Ok(())
    }

    fn insert_generic<R: Resource>(
        &mut self,
        op: OpId,
        input_schema: Schema,
        output_schema: Schema,
    ) -> Result<(), RegistryError> {
        // The generic surface is read-only: every mutation on the
        // whole surface is a declared command. Derived authority is
        // therefore always Read.
        self.insert(OpDescriptor {
            name: op.to_string(),
            category: op.category(),
            op,
            version: R::VERSION,
            authority: vec![Authority {
                verb: Verb::Read,
                scope: R::DOMAIN,
            }],
            description: R::DESCRIPTION,
            stability: R::STABILITY,
            caveats: R::CAVEATS,
            input_schema,
            output_schema,
        })
    }

    fn insert_read_surface<R: Resource>(&mut self) -> Result<(), RegistryError> {
        self.insert_generic::<R>(
            OpId::Get(R::DOMAIN),
            schema_for!(R::Key),
            schema_for!(R::State),
        )?;
        self.insert_generic::<R>(
            OpId::List(R::DOMAIN),
            schema_for!(R::Filter),
            schema_for!(R::State),
        )
    }

    /// Register a catalogue entry. Everything derives from the impl —
    /// there is no registration-time choice to get wrong: atoms get
    /// Get + List + Stream, views Get + List (stream their atoms),
    /// synthetics Get alone (their verbs register as commands).
    pub fn register_resource<R: Resource>(&mut self) -> Result<(), RegistryError> {
        self.insert_resource::<R>()?;
        match R::NATURE {
            Nature::Atom => {
                self.insert_read_surface::<R>()?;
                self.insert_generic::<R>(
                    OpId::Stream(R::DOMAIN),
                    schema_for!(R::Filter),
                    schema_for!(R::State),
                )
            }
            Nature::View => self.insert_read_surface::<R>(),
            Nature::Synthetic => self.insert_generic::<R>(
                OpId::Get(R::DOMAIN),
                schema_for!(R::Key),
                schema_for!(R::State),
            ),
        }
    }

    /// Register a domain verb. Output is always a receipt (D3) — the
    /// trait has no output type to get wrong. Identity comes from the
    /// impl itself; a verb that collides with anything already
    /// registered (including a derived generic name) is refused here.
    pub fn register_command<C: Command>(&mut self) -> Result<(), RegistryError> {
        let op = C::op();
        self.insert(OpDescriptor {
            name: op.to_string(),
            category: OpCategory::DomainVerb,
            op,
            version: C::VERSION,
            authority: vec![C::AUTHORITY],
            description: C::DESCRIPTION,
            stability: C::STABILITY,
            caveats: C::CAVEATS,
            input_schema: schema_for!(C::Input),
            output_schema: schema_for!(crate::wire::Receipt),
        })
    }

    /// Register a report. Authority derives to Read on each consumed
    /// scope.
    pub fn register_report<R: Report>(&mut self) -> Result<(), RegistryError> {
        let op = R::op();
        self.insert(OpDescriptor {
            name: op.to_string(),
            category: OpCategory::Report,
            op,
            version: R::VERSION,
            authority: R::READS
                .iter()
                .map(|scope| Authority {
                    verb: Verb::Read,
                    scope: *scope,
                })
                .collect(),
            description: R::DESCRIPTION,
            stability: R::STABILITY,
            caveats: R::CAVEATS,
            input_schema: schema_for!(R::Params),
            output_schema: schema_for!(R::Output),
        })
    }

    pub fn get(&self, op: &OpId) -> Option<&OpDescriptor> {
        self.ops.get(&op.to_string())
    }

    /// The catalogue entry for a registered domain — where its nature
    /// and resource-level docs live.
    pub fn get_resource(&self, domain: Domain) -> Option<&ResourceDescriptor> {
        self.resources.get(domain.segment())
    }

    /// Every registered resource, in segment order — the catalogue
    /// half of the registry's payload.
    pub fn describe_resources(&self) -> Vec<&ResourceDescriptor> {
        self.resources.values().collect()
    }

    /// Lookup by rendered name — for string-addressed adapters (MCP
    /// tool names, docs routes). The registry is the index; nothing
    /// parses.
    pub fn get_named(&self, name: &str) -> Option<&OpDescriptor> {
        self.ops.get(name)
    }

    /// Every registered promise, in rendered-name order — the payload
    /// of List(Operation), the surface describing itself.
    pub fn describe(&self) -> Vec<&OpDescriptor> {
        self.ops.values().collect()
    }

    pub fn len(&self) -> usize {
        self.ops.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }
}
