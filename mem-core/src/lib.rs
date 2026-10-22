pub mod compact;
pub mod git;
pub mod id_gen;
pub mod index;
pub mod models;
pub mod render;
pub mod resolve;
pub mod storage;
pub mod util;

pub use compact::{compact_project_files, CompactReport};
pub use git::{
    git_has_conflicts, git_has_upstream, git_pull_rebase, git_rebase_continue, sync_store,
    SyncOutcome,
};
pub use id_gen::generate_id;
pub use index::{EventPointer, MemIndex};
pub use models::{compute_sha256, hash_file, hash_reader, Event, FileRecord, ProjectMeta};
pub use render::{render_project_readme, write_project_readme};
pub use resolve::{resolve_conflicts, ResolveReport};
pub use storage::{lock_store, validate_project_name, MemStore, StoreWriteLock};
pub use util::{
    default_device_name, event_cmp, find_store_root, is_store_root, normalize_file_name,
    now_rfc3339, parse_ts, resolve_store_root, sort_events, ts_cmp, STORE_ENV_VAR, STORE_MARKER,
};
