#![feature(let_chains)]

use cfg::{Cores, Exec, Pinned, Sysctl, Templates};

pub mod cfg;

#[derive(Debug, Clone)]
pub struct Node {
    pub pinned: Option<Vec<Pinned>>,
    pub cores: Cores,
    pub sysctls: Option<Sysctl>,
    pub execs: Option<Exec>,
    pub templates: Option<Templates>,
    // pub environ:
}

// impl From<cfg::Node> for Node {
// fn from(node: cfg::Node) -> Self {
// Node { pinned: None,
// }
// }
// }
