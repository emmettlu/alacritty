//! Native graphics backends and shared rendering primitives.
use std::fmt;
pub(crate) mod rects;
pub(crate) mod native;
pub(crate) use native::GlyphCache;
#[derive(Debug)]
pub(crate) struct Error(pub String);
impl std::error::Error for Error {}
impl fmt::Display for Error { fn fmt(&self,f:&mut fmt::Formatter<'_>)->fmt::Result { self.0.fmt(f) } }
impl From<std::io::Error> for Error { fn from(err:std::io::Error)->Self { Self(err.to_string()) } }
