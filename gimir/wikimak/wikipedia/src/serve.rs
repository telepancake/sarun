//! `wikimak serve <root> [addr]` — the local browse window (plan §5).
//! Routes: `/wiki/<title>` (+ `?asof=<ts>` date picker), `/history/<title>`,
//! `/special/allpages`; every internal link carries asof through.
//! Renders through wikimak-wikitext with the LuaInvoker and
//! BlobMediaResolver wired in; red/blue links via existence-at-τ.
//! OWNED BY: the serve agent. Skeleton compiles, serves nothing yet.

use crate::Instance;

pub struct ServeConfig {
    pub addr: String,
}

pub fn serve(_inst: Instance, _cfg: ServeConfig) -> Result<(), String> {
    Err("serve: not yet implemented (B1)".into())
}
