//! The registry: the catalogue of promises, self-describing.
//!
//! Resources register once and receive their derived read surface
//! (Get + List, Stream for atoms, Create where opted in) with derived
//! authority; the declared surface (domain verbs, reports, machinery
//! reads) registers per definition with declared authority. Every
//! entry becomes an [`OpDescriptor`] — the payload of `Operation`'s
//! own List (the surface describing itself) and the input to
//! client-wrapper codegen.

use std::collections::BTreeMap;

use schemars::{Schema, schema_for};

use crate::catalogue::{AtomResource, CreatableResource, Nature, ResourceId, ResourceType};
use crate::declared::{Command, MetaRead, Report};
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
/// entry defines the types once; these strings let `describe` say what
/// each derived op means for *this* resource.
#[derive(Debug, Clone, Copy)]
pub struct ResourceDocs {
    pub stability: Stability,
    /// Description for Get; List/Stream/Create descriptions derive
    /// from it mechanically.
    pub summary: &'static str,
    /// Caveats shared by the resource's whole read surface (retention
    /// bounds, fold semantics). Empty means "none".
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

    fn insert_read_surface<R: ResourceType>(
        &mut self,
        nature: Nature,
        docs: ResourceDocs,
    ) -> Result<(), RegistryError> {
        let read = Authority {
            verb: Verb::Read,
            scope: R::ID,
        };
        self.insert(OpDescriptor {
            op: OpId::Get(R::ID),
            name: OpId::Get(R::ID).to_string(),
            category: OpCategory::Get,
            nature: Some(nature),
            version: R::VERSION,
            authority: vec![read],
            description: docs.summary,
            stability: docs.stability,
            caveats: docs.caveats,
            input_schema: schema_for!(R::Key),
            output_schema: schema_for!(R::State),
        })?;
        self.insert(OpDescriptor {
            op: OpId::List(R::ID),
            name: OpId::List(R::ID).to_string(),
            category: OpCategory::List,
            nature: Some(nature),
            version: R::VERSION,
            authority: vec![read],
            description: docs.summary,
            stability: docs.stability,
            caveats: docs.caveats,
            input_schema: schema_for!(R::Filter),
            output_schema: schema_for!(R::State),
        })
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
        self.insert(OpDescriptor {
            op: OpId::Stream(R::ID),
            name: OpId::Stream(R::ID).to_string(),
            category: OpCategory::Stream,
            nature: Some(Nature::Atom),
            version: R::VERSION,
            authority: vec![Authority {
                verb: Verb::Read,
                scope: R::ID,
            }],
            description: docs.summary,
            stability: docs.stability,
            caveats: docs.caveats,
            input_schema: schema_for!(R::Filter),
            output_schema: schema_for!(R::State),
        })
    }

    /// Register Create for an opted-in resource (additive to its read
    /// surface; authority derives to Write on the resource).
    pub fn register_create<R: CreatableResource>(
        &mut self,
        docs: ResourceDocs,
    ) -> Result<(), RegistryError> {
        self.insert(OpDescriptor {
            op: OpId::Create(R::ID),
            name: OpId::Create(R::ID).to_string(),
            category: OpCategory::Create,
            nature: None,
            version: R::VERSION,
            authority: vec![Authority {
                verb: Verb::Write,
                scope: R::ID,
            }],
            description: docs.summary,
            stability: docs.stability,
            caveats: docs.caveats,
            input_schema: schema_for!(R::CreateInput),
            output_schema: schema_for!(crate::wire::Receipt),
        })
    }

    /// Register a domain verb. Output is always a receipt (D3) — the
    /// trait has no output type to get wrong.
    pub fn register_command<C: Command>(&mut self) -> Result<(), RegistryError> {
        self.insert(OpDescriptor {
            op: OpId::Verb(C::ID),
            name: OpId::Verb(C::ID).to_string(),
            category: OpCategory::DomainVerb,
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
        self.insert(OpDescriptor {
            op: OpId::Report(R::ID),
            name: OpId::Report(R::ID).to_string(),
            category: OpCategory::Report,
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

    /// Register a machinery read. Authority derives to Read on the
    /// synthetic `Control` resource.
    pub fn register_meta_read<M: MetaRead>(&mut self) -> Result<(), RegistryError> {
        self.insert(OpDescriptor {
            op: OpId::MetaRead(M::ID),
            name: OpId::MetaRead(M::ID).to_string(),
            category: OpCategory::MetaRead,
            nature: None,
            version: M::VERSION,
            authority: vec![Authority {
                verb: Verb::Read,
                scope: ResourceId::Control,
            }],
            description: M::META.description,
            stability: M::META.stability,
            caveats: M::META.caveats,
            input_schema: schema_for!(()),
            output_schema: schema_for!(M::Output),
        })
    }

    pub fn get(&self, op: OpId) -> Option<&OpDescriptor> {
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
