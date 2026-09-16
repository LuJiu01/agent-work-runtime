use crate::{operations::parse, project::database};
use awr_core::*;
use awr_store::Store;
use rmcp::model::CallToolResult;
use serde_json::Value;
use std::path::Path;

pub(crate) const NAMES: [&str; 3] = [
    "awr_compaction_observe",
    "awr_compaction_get",
    "awr_compaction_defer",
];
pub(crate) fn call(root: &Path, name: &str, args: Value) -> Result<CallToolResult> {
    let value = if name == "awr_compaction_get" {
        let store = Store::read_snapshot(&database(root)?, 256 * 1024 * 1024)?;
        awr_runtime::inspect_compaction(&store, root, &parse(args)?)?
    } else {
        let mut store = Store::open_existing(&database(root)?)?;
        match name {
            "awr_compaction_observe" => {
                awr_runtime::observe_compaction(&mut store, root, &parse(args)?)?
            }
            "awr_compaction_defer" => {
                awr_runtime::defer_compaction(&mut store, root, &parse(args)?)?
            }
            _ => return Err(Error::Unsupported(name.into())),
        }
    };
    Ok(CallToolResult::structured(value))
}
