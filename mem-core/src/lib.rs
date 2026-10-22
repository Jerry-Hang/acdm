pub mod models;
pub mod id_gen;
pub mod storage;
pub mod index;
pub mod git;
pub mod resolve;
pub mod render;
pub mod compact;

pub use models::{Event, FileRecord, ProjectMeta};
pub use id_gen::generate_id;
pub use storage::MemStore;
pub use index::{EventPointer, MemIndex};
pub use resolve::{resolve_conflicts, ResolveReport};
pub use render::{render_project_readme, write_project_readme};
pub use compact::{compact_project_files, CompactReport};
