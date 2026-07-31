mod home;
pub use home::Home;

mod blog;
pub use blog::Blog;

// Agent 视图（Phase 6.2）经 #[path] 复用 web 端实现：视图仅依赖
// `crate::agent::*` 与 `use_context::<Client>()`，双端同构。
#[path = "../../../web/src/views/agent_chat.rs"]
mod agent_chat;
pub use agent_chat::AgentChat;

#[path = "../../../web/src/views/skills.rs"]
mod skills;
pub use skills::Skills;

#[path = "../../../web/src/views/memory.rs"]
mod memory;
pub use memory::Memory;

#[path = "../../../web/src/views/tools.rs"]
mod tools;
pub use tools::Tools;
