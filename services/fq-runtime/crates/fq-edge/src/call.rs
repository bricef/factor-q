//! What a handler is given about the call it serves: who made it.
//!
//! The edge authenticates every connection by its token, and knows the
//! principal the token was minted for. It hands that to each handler it
//! calls for `invoke`, as a [`CallContext`], through the registry's handler
//! type. A handler registered with [`EdgeRegistry::command_with_call`]
//! takes it as an argument; every other registration ignores it, so a
//! handler that does not need it is written as before.
//!
//! It is a parameter, not ambient state: a handler that needs its caller
//! cannot be registered without taking it, and receives it where it is
//! called, whatever it does before it awaits anything.
//!
//! It holds the principal, a name signed into the token (`admin` for the
//! daemon's own, and for any token attenuated from it), never the token,
//! which is a bearer credential.
//!
//! [`EdgeRegistry::command_with_call`]: crate::EdgeRegistry::command_with_call

use std::sync::Arc;

use crate::auth::VerifiedToken;

/// Who made the call a handler serves.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CallContext {
    principal: Option<Arc<str>>,
}

impl CallContext {
    /// A call with no caller: a handler called in-process, as tests call
    /// them.
    pub fn none() -> Self {
        Self::default()
    }

    /// The call of a connection whose token the edge verified.
    pub(crate) fn of(token: &VerifiedToken) -> Self {
        Self {
            principal: Some(Arc::from(token.principal.as_str())),
        }
    }

    /// The principal the caller's token was minted for, `None` for a call
    /// with no caller. A token with no principal fact reads `unknown`, as
    /// the edge names it in every other answer.
    pub fn principal(&self) -> Option<&str> {
        self.principal.as_deref()
    }
}
