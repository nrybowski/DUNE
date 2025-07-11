#![feature(let_chains)]
#![doc = include_str!("../../README.md")]

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;

use cfg::{Config, Interface, Link, Node, Phynodes};
// use graphrs::{Graph, GraphSpecs};
use serde::{Deserialize, Serialize};

use tracing::{info, span, warn, Level};
use tracing_appender::rolling::{self};
use tracing_subscriber::fmt::writer::MakeWriterExt;

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
        let logfile = rolling::never("/tmp", "dune.log");
        let stdout = std::io::stdout.with_min_level(tracing::Level::TRACE);
        tracing_subscriber::fmt()
            .with_writer(stdout.and(logfile))
            .init();
        info!("Tracing and logging enabled!");
        let mut dune = Self::new(cfg);
        dune.stats();
        dune.allocate();
        dune
    }

    pub fn stats(&self) {
        info!(
            "Collected <{}> nodes on <{}> phynodes",
            self.nodes.len(),
            self.infra.nodes.len()
        );
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
                let ifname = link.endpoints[idx].interface.clone();
                let interfaces = node.interfaces.get_or_insert_with(HashMap::new);
                let ifindex = interfaces.len() + 2;
                let mut iface =
                    Interface::new(&cfg.topology.defaults.links, &link, idx, ifindex as u32);

                // Load interface's addresse(s), if any
                if let Some(addrs) = &node.addrs
                    && let Some(addrs) = addrs.get(&ifname)
                {
                    iface.addrs = Some(addrs.clone());
                }

                // Only insert the interface if it is not already defined to avoid duplicates
                interfaces.entry(ifname).or_insert(iface);
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

        // Load Node's files, if any
        nodes.iter_mut().for_each(|(_, node)| node.configure());

        Self {
            nodes,
            // topo,
            infra: cfg.infrastructure,
            allocated: false,
        }
    }

    /// Allocate requested cores to physical cores, if possible given the provided infrastructure.
    pub fn allocate(&mut self) {
        // FIXME: Detect  and report unallocated nodes
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
                                if let Some(cores) = &mut node.cores {
                                    cores.iter_mut().for_each(|(_, core)| {
                                        *core = Some(available.pop().unwrap())
                                    });
                                    node.phynode = Some(name.clone());
                                    break;
                                }
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
        let _span = span!(Level::INFO, "phynode", name = phynode).entered();
        // FIXME: cleaner filter

        info!("Filtering nodes for current phynode.");

        let nodes = self
            .nodes
            .iter()
            .filter_map(|(name, node)| {
                if let Some(node_phynode) = &node.phynode
                    && node_phynode == &phynode
                {
                    Some(node)
                } else {
                    warn!(
                        "Skipped node <{name}>: registered phynode <{:#?}>\n{node:#?}",
                        node.phynode
                    );
                    None
                }
            })
            .collect::<Vec<&Node>>();

        info!("Got <{}> nodes to install on <{phynode}>", nodes.len());

        // Instanciate nodes
        let span = span!(Level::INFO, "step", name = "nodes").entered();
        nodes.iter().for_each(|node| node.init());
        span.exit();

        // Configure interfaces
        let span = span!(Level::INFO, "step", name = "interfaces").entered();
        nodes.iter().for_each(|node| node.setup());
        span.exit();
    }
}
