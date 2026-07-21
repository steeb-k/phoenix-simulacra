//! Phoenix Simulacra restore planner and execution.

pub mod bootrepair;
pub mod grow;
pub mod layout_edit;
pub mod partition_table;
pub mod plan;
pub mod relocation;
pub mod restore;
pub mod winadmin;
pub mod winhello;

pub use plan::{
    build_full_disk_plan, build_partial_plan, default_plan_from_backup, partition_allows_resize,
    RestoreMode, RestorePlan, RestorePlanEntry,
};
pub use restore::{run_restore, verify_backup, verify_backup_with_progress};
