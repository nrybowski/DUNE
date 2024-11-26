#![feature(let_chains)]
#![doc = include_str!("../../README.md")]

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;

use cfg::{Config, Endpoint, Interface, Link, Node, Phynodes};
// use graphrs::{Graph, GraphSpecs};
use serde::{Deserialize, Serialize};

pub mod cfg;

type NodeId = String;

pub enum SetupStep {
    Pre(Vec<String>),
    Nodes,
    Links,
    Post,
    Processes,
    PreDown,
    Down,
}

pub struct ExperimentalSetup {
    pub pre: Vec<String>,
    pub nodes: Vec<String>,
    pub links: Vec<String>,
    pub post: Vec<String>,
    pub processes: Vec<String>,
    pub pre_down: Vec<String>,
    pub down: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Dune {
    pub nodes: HashMap<NodeId, Node>,
    // topo: Graph<NodeId, ()>,
    pub infra: Phynodes,
    allocated: bool,
}

impl Dune {
    pub fn init(cfg: &PathBuf) -> Self {
        let mut dune = Self::new(cfg);
        dune.allocate();
        dune
    }

    pub fn new(cfg: &PathBuf) -> Self {
        fn load_interface(
            nodes: &mut HashMap<String, Node>,
            link: &Link,
            cfg: &Config,
            idx: usize,
        ) {
            assert!(idx == 0 || idx == 1);
            if let Some(node) = nodes.get_mut(&link.endpoints[idx].node) {
                let iface = Interface::new(&cfg.topology.defaults.links, &link, idx);
                // TODO: check that interface is not defined multiple times
                node.interfaces
                    .get_or_insert_with(HashMap::new)
                    .insert(link.endpoints[idx].interface.clone(), iface);
            }
        }

        // Load DUNE's configuration
        let cfg = Config::new(cfg.to_str().unwrap());
        // let mut topo = Graph::<NodeId, _>::new(GraphSpecs::multi_directed());

        // Collect and expand Nodes data
        let mut nodes = cfg
            .topology
            .nodes
            .iter()
            .map(|(name, config)| {
                // topo.add_node(graphrs::Node::from_name(name.clone()));
                (
                    name.clone(),
                    Node::new(&cfg.topology.defaults.nodes, &config, name),
                )
            })
            .collect::<HashMap<String, Node>>();

        // Collect and expand Links data
        cfg.topology.links.iter().for_each(|link| {
            (0..2).into_iter().for_each(|idx| {
                load_interface(&mut nodes, link, &cfg, idx);
            })
        });

        Self {
            nodes,
            // topo,
            infra: cfg.infrastructure,
            allocated: false,
        }
    }

    /// Allocate requested cores to physical cores, if possible given the provided infrastructure.
    pub fn allocate(&mut self) {
        if !self.allocated {
            self.allocated = true;
            // Sort nodes by decreasing number of cores to allocate
            let mut cores: BTreeMap<usize, BTreeSet<NodeId>> = BTreeMap::new();
            self.nodes.iter().for_each(|(node_id, node)| {
                cores
                    .entry(node.cores())
                    .and_modify(|entry| {
                        let _ = entry.insert(node_id.clone());
                    })
                    .or_insert(BTreeSet::from([node_id.clone()]));
            });

            assert!(
                cores.iter().fold(0, |acc, (cores, _)| acc + cores) < self.infra.cores(),
                "More core booked than available in the defined infrastructure. Please, fix your configuration file."
            );

            let mut core_pool = self.infra.clone();

            cores.iter().rev().for_each(|(_, nodes)| {
                // For each node, reserve the necessary amount of cores then allocate them
                nodes.iter().for_each(|node_id| {
                    if let Some(node) = self.nodes.get_mut(node_id) {
                        let n = node.cores();
                        // Search for at least n cores located on the same NUMA node for locality.
                        // This ensures that every Pinned processes of a Node are located on the same NUMA node.
                        // The strategy is dummy: we fill servers in order.
                        for (name, phynode) in core_pool.nodes.iter_mut() {
                            if let Some(available) = phynode
                                .cores
                                .iter_mut()
                                .find(|available| available.len() >= n)
                            {
                                node.cores
                                    .iter_mut()
                                    .for_each(|(_, core)| *core = Some(available.pop().unwrap()));
                                node.phynode = Some(name.clone());
                                break;
                            }
                        }
                    }
                });
            });
        }
    }

    pub fn phynodes(&self) -> Vec<NodeId> {
        self.infra
            .nodes
            .iter()
            .map(|(phynode, _)| phynode.clone())
            .collect::<Vec<NodeId>>()
    }

    pub fn phynode_setup(&self, phynode: NodeId) {
        self.nodes.iter().for_each(|(name, node)| {
            if let Some(node_phynode) = &node.phynode
                && node_phynode == &phynode
            {
                node.setup();
            }
        })
    }

    // fn phynode(&mut self, node_id: NodeId) -> &String {
    //     if !self.allocated {
    //         self.allocate();
    //     }
    //     &self.nodes[&node_id].phynode
    // }

    // fn phynode_exec(&self, pynode: String, step: SetupStep, cmd: String) {}

    // fn node_exec(&mut self, node_id: NodeId, step: SetupStep, cmd: String) {
    //     let phynode = self.phynode(node_id);
    // }
}
