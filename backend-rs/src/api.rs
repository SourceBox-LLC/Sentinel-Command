//! Ported HTTP routes.
//!
//! One module per Python router. A route lands here only once it has been
//! diffed against the Python it replaces; until then it stays in the
//! proxy fallback in `app.rs`.

pub mod cameras;
