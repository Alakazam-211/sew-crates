//! Search: the pure query parser shared by the server, K2 and clients, so a query can be
//! validated before it is sent. Wire bodies live in [`crate::api::search`]; the syntax is
//! what this module's parser accepts.

pub mod query;

pub use query::{
    Clause, ClauseKind, DateFilter, FilterName, HasValue, InRef, IsValue, MemberRef, ParsedQuery,
    QueryError, SpaceRefQ, parse,
};
