//! Extractor types

pub(crate) mod form;
pub(crate) mod json;
mod path;
pub(crate) mod payload;
mod query;

pub use self::form::{Form, FormConfig};
pub use self::json::{Json, JsonConfig};
pub use self::path::Path;
pub use self::payload::{Payload, PayloadConfig};
pub use self::query::Query;
