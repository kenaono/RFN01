// Compile the production modules with their existing unit tests, without the Windows app.
#![allow(dead_code)]

#[path = "../../src/comparison.rs"]
mod comparison;
#[path = "../../src/git_graph.rs"]
mod git_graph;
#[path = "../../src/inline_diff.rs"]
mod inline_diff;
#[path = "../../src/kill_ring.rs"]
mod kill_ring;
#[path = "../../src/pane_layout.rs"]
mod pane_layout;
#[path = "../../src/text_blocks.rs"]
mod text_blocks;
#[path = "../../src/workspace_boundary.rs"]
mod workspace_boundary;
