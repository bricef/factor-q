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

use crate::catalogue::{Atom, Domain, Nature, Resource, Synthetic, View};
use crate::declared::{Command, Report};
use crate::meta::{Authority, Stability, Verb};
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
    pub summary: &'static str,
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

/// Descriptions for a resource's derived operations. The catalogue
/// entry defines the types once; these strings let List(Operation)
/// say what each derived op means for *this* resource.
#[derive(Debug, Clone, Copy)]
pub struct ResourceDocs {
    pub stability: Stability,
    /// Description for the derived surface.
    pub summary: &'static str,
    /// Caveats shared by the resource's whole derived surface
    /// (retention bounds, fold semantics). Empty means "none".
    pub caveats: &'static str,
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

    /// Record the resource-level catalogue entry — where nature lives.
    fn insert_resource<R: Resource>(
        &mut self,
        nature: Nature,
        docs: ResourceDocs,
    ) -> Result<(), RegistryError> {
        let segment = R::DOMAIN.segment();
        if self.resources.contains_key(segment) {
            return Err(RegistryError::DuplicateResource { domain: R::DOMAIN });
        }
        self.resources.insert(
            segment,
            ResourceDescriptor {
                domain: R::DOMAIN,
                nature,
                version: R::VERSION,
                stability: docs.stability,
                summary: docs.summary,
                caveats: docs.caveats,
            },
        );
        Ok(())
    }

    fn insert_generic<R: Resource>(
        &mut self,
        op: OpId,
        docs: ResourceDocs,
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
            description: docs.summary,
            stability: docs.stability,
            caveats: docs.caveats,
            input_schema,
            output_schema,
        })
    }

    fn insert_read_surface<R: Resource>(
        &mut self,
        docs: ResourceDocs,
    ) -> Result<(), RegistryError> {
        self.insert_generic::<R>(
            OpId::Get(R::DOMAIN),
            docs,
            schema_for!(R::Key),
            schema_for!(R::State),
        )?;
        self.insert_generic::<R>(
            OpId::List(R::DOMAIN),
            docs,
            schema_for!(R::Filter),
            schema_for!(R::State),
        )
    }

    /// Register a view: Get + List derive, answering as of a
    /// watermark. Views are never streamed — stream their atoms.
    pub fn register_view<R: View>(&mut self, docs: ResourceDocs) -> Result<(), RegistryError> {
        self.insert_resource::<R>(Nature::View, docs)?;
        self.insert_read_surface::<R>(docs)
    }

    /// Register an atom: Get + List + Stream derive. The
    /// [`Atom`] bound makes "only atoms stream" a compile-time
    /// fact.
    pub fn register_atom<R: Atom>(&mut self, docs: ResourceDocs) -> Result<(), RegistryError> {
        self.insert_resource::<R>(Nature::Atom, docs)?;
        self.insert_read_surface::<R>(docs)?;
        self.insert_generic::<R>(
            OpId::Stream(R::DOMAIN),
            docs,
            schema_for!(R::Filter),
            schema_for!(R::State),
        )
    }

    /// Register a synthetic resource: Get alone — the machinery
    /// describing itself. Nothing else derives (no atoms behind it,
    /// nothing to list or stream); its verbs register separately as
    /// commands.
    pub fn register_synthetic<R: Synthetic>(
        &mut self,
        docs: ResourceDocs,
    ) -> Result<(), RegistryError> {
        self.insert_resource::<R>(Nature::Synthetic, docs)?;
        self.insert_generic::<R>(
            OpId::Get(R::DOMAIN),
            docs,
            schema_for!(R::Key),
            schema_for!(R::State),
        )
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
            description: C::META.description,
            stability: C::META.stability,
            caveats: C::META.caveats,
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
            description: R::META.description,
            stability: R::META.stability,
            caveats: R::META.caveats,
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
