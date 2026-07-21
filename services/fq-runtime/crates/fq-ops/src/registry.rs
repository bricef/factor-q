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
//! guarantee the declared leaf strings owe us), as [`RegistryError::Duplicate`].

use std::collections::BTreeMap;

use schemars::{Schema, schema_for};

use crate::catalogue::{AtomResource, CreatableResource, Nature, ResourceType};
use crate::declared::{Command, Report};
use crate::meta::{Authority, Stability, Verb};
use crate::opid::{OpCategory, OpId};

/// One registered promise, described. `authority` is a list because a
/// report reads several scopes; generic operations and verbs carry
/// exactly one entry.
#[derive(Debug, Clone, serde::Serialize)]
pub struct OpDescriptor {
    pub op: OpId,
    pub name: String,
    pub category: OpCategory,
    /// The resource's nature, for the generic surface (`None` for the
    /// declared surface).
    pub nature: Option<Nature>,
    pub version: u32,
    pub authority: Vec<Authority>,
    pub description: &'static str,
    pub stability: Stability,
    pub caveats: &'static str,
    pub input_schema: Schema,
    pub output_schema: Schema,
}

/// Why a registration was refused — a defect in the registering code,
/// not a runtime condition to retry.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RegistryError {
    #[error("`{name}` is already registered — one registry, one description per operation (D1)")]
    Duplicate { name: String },
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

    fn insert_generic<R: ResourceType>(
        &mut self,
        op: OpId,
        nature: Option<Nature>,
        docs: ResourceDocs,
        input_schema: Schema,
        output_schema: Schema,
    ) -> Result<(), RegistryError> {
        let authority = match op.category() {
            OpCategory::Create => Verb::Write,
            _ => Verb::Read,
        };
        self.insert(OpDescriptor {
            name: op.to_string(),
            category: op.category(),
            op,
            nature,
            version: R::VERSION,
            authority: vec![Authority {
                verb: authority,
                scope: R::ID,
            }],
            description: docs.summary,
            stability: docs.stability,
            caveats: docs.caveats,
            input_schema,
            output_schema,
        })
    }

    fn insert_read_surface<R: ResourceType>(
        &mut self,
        nature: Nature,
        docs: ResourceDocs,
    ) -> Result<(), RegistryError> {
        self.insert_generic::<R>(
            OpId::Get(R::ID),
            Some(nature),
            docs,
            schema_for!(R::Key),
            schema_for!(R::State),
        )?;
        self.insert_generic::<R>(
            OpId::List(R::ID),
            Some(nature),
            docs,
            schema_for!(R::Filter),
            schema_for!(R::State),
        )
    }

    /// Register a view: Get + List derive, answering as of a
    /// watermark. Views are never streamed — stream their atoms.
    pub fn register_view<R: ResourceType>(
        &mut self,
        docs: ResourceDocs,
    ) -> Result<(), RegistryError> {
        self.insert_read_surface::<R>(Nature::View, docs)
    }

    /// Register an atom: Get + List + Stream derive. The
    /// [`AtomResource`] bound makes "only atoms stream" a compile-time
    /// fact.
    pub fn register_atom<R: AtomResource>(
        &mut self,
        docs: ResourceDocs,
    ) -> Result<(), RegistryError> {
        self.insert_read_surface::<R>(Nature::Atom, docs)?;
        self.insert_generic::<R>(
            OpId::Stream(R::ID),
            Some(Nature::Atom),
            docs,
            schema_for!(R::Filter),
            schema_for!(R::State),
        )
    }

    /// Register a synthetic resource: Get alone — the machinery
    /// describing itself. Nothing else derives (no atoms behind it,
    /// nothing to list or stream); its verbs register separately as
    /// commands.
    pub fn register_synthetic<R: ResourceType>(
        &mut self,
        docs: ResourceDocs,
    ) -> Result<(), RegistryError> {
        self.insert_generic::<R>(
            OpId::Get(R::ID),
            Some(Nature::Synthetic),
            docs,
            schema_for!(R::Key),
            schema_for!(R::State),
        )
    }

    /// Register Create for an opted-in resource (additive to its read
    /// surface; authority derives to Write on the resource).
    pub fn register_create<R: CreatableResource>(
        &mut self,
        docs: ResourceDocs,
    ) -> Result<(), RegistryError> {
        self.insert_generic::<R>(
            OpId::Create(R::ID),
            None,
            docs,
            schema_for!(R::CreateInput),
            schema_for!(crate::wire::Receipt),
        )
    }

    /// Register a domain verb. Output is always a receipt (D3) — the
    /// trait has no output type to get wrong. Identity comes from the
    /// impl itself; a leaf that collides with anything already
    /// registered (including a derived generic name) is refused here.
    pub fn register_command<C: Command>(&mut self) -> Result<(), RegistryError> {
        let op = C::op();
        self.insert(OpDescriptor {
            name: op.to_string(),
            category: OpCategory::DomainVerb,
            op,
            nature: None,
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
            nature: None,
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
