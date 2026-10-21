//! The decisions contract as a vendor speaks it: [`Decisions`] and the
//! vendor's [`Dialect`] of it, where it departs from the spec. It speaks as
//! the spec, by its name, so a client of the spec is still relayed to it;
//! the format names no vendor, and the vendor writes its dialect.
//!
//! ```ignore
//! formats: &[(&Dialected::new(Decisions, TypeSafe), Some("https://api.typesafe.ai/v1"))]
//! ```

use respan_gateway_core::{Connection, Fault, Format, Provider};
use respan_gateway_decision::{Input, Output};
use respan_gateway_http::{Connect, KeyIn, ProviderIo, Reach, Upstream};

use crate::{Decisions, PATH, bind_provider};

/// How a vendor departs from the decisions contract. What it does not say
/// is the spec's.
pub trait Dialect: Clone + Send + Sync + 'static {
    /// Where the vendor serves the contract, relative to its base URL, if
    /// not at `/decisions`.
    fn path(&self) -> Option<&'static str> {
        None
    }
}

/// [`Decisions`] as a vendor speaks it, in its dialect `D`: a provider
/// format of the spec's name.
#[derive(Debug, Clone, Copy)]
pub struct Dialected<D> {
    spec: Decisions,
    dialect: D,
}

impl<D> Dialected<D> {
    /// `spec` in `dialect`.
    pub const fn new(spec: Decisions, dialect: D) -> Self {
        Self { spec, dialect }
    }
}

/// The spec's.
impl<D: Dialect> Upstream for Dialected<D> {
    fn name(&self) -> &str {
        Decisions::NAME
    }

    fn key(&self) -> KeyIn {
        self.spec.key()
    }

    fn reach(&self) -> Reach {
        self.spec.reach()
    }

    fn at_origin(&self) -> bool {
        self.spec.at_origin()
    }

    fn provider(&self, name: String, connect: Connect) -> Provider {
        Provider::new(name, self.clone(), move |x| connect(x))
    }
}

impl<D: Dialect> Format<ProviderIo, Output, Input> for Dialected<D> {
    fn name(&self) -> &str {
        Decisions::NAME
    }

    /// As the spec's, at the dialect's path.
    fn bind(&self, io: ProviderIo) -> Result<Connection<Output, Input>, Fault> {
        bind_provider(io, self.dialect.path().unwrap_or(PATH))
    }
}
