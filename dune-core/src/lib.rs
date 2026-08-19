#![doc = include_str!("../../README.md")]

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::channel;

use rtnetlink::{Handle, new_connection};
use serde::{Deserialize, Serialize};

use tracing::{Level, debug, error, info, span, warn};
use tracing_appender::rolling::{self};
use tracing_subscriber::fmt::writer::MakeWriterExt;

pub mod cfg;
pub mod components;

use crate::cfg::{Config, Link, Node, Phynodes};
use crate::components::sriov::Pf;
use crate::components::{
    interface::Interface,
    sriov::{PhynodeVfs, PhynodesVfs},
};

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
    pub infra: Phynodes,
}

fn allocate(nodes: &mut HashMap<NodeId, Node>, infra: &mut Phynodes) {
    info!("Allocating CPU resources");
    // Sort nodes by decreasing number of cores to allocate
    let mut cores: BTreeMap<usize, BTreeSet<NodeId>> = BTreeMap::new();
    nodes.iter_mut().for_each(|(node_id, node)| {
        cores
            .entry(node.cores())
            .and_modify(|entry| {
                let _ = entry.insert(node_id.clone());
            })
            .or_insert(BTreeSet::from([node_id.clone()]));
    });

    // Sanity check on requested core count.
    assert!(
        cores.iter().fold(0, |acc, (cores, _)| acc + cores) < infra.cores(),
        "More core booked than available in the defined infrastructure. Please, fix your configuration file."
    );

    // Allocate cores in decreasing order.
    cores.iter_mut().rev().for_each(|(_, core_nodes)| {
        // For each node, reserve the necessary amount of cores then allocate them.
        core_nodes.iter().for_each(|node_id| {
            if let Some(node) = nodes.get_mut(node_id) {
                let n = node.cores();
                // Search for at least n cores located on the same NUMA node for locality.
                // This ensures that every Pinned processes of a Node are located on the same NUMA node.
                // The strategy is dummy: we fill servers in order.
                for (name, phynode) in infra.nodes.iter_mut() {
                    info!("{n} {name} {:?}", phynode.cores);
                    if let Some((cpu, available)) = phynode
                        .cores
                        .iter_mut()
                        .enumerate()
                        .find(|(_, available)| available.len() >= n)
                        && let Some(pids) = &mut node.pinned
                    {
                        pids.iter_mut().for_each(|pinned| {
                            if let Some(cores) = &mut pinned.cores {
                                cores
                                    .iter_mut()
                                    .for_each(|(_id, core)| *core = available.pop().unwrap());
                            }
                        });
                        // Node is allocated to the specified CPU of phynode.
                        info!(
                            "Node {:?} allocated to CPU{cpu} of <{phynode:?}>",
                            node.name
                        );
                        node.phynode = Some(name.clone());
                        node.cpu = Some(cpu);
                        break;
                    }
                }
            }
        });
    });
    info!("{cores:#?}");
}

impl Dune {
    pub fn init(cfg: &PathBuf) -> Self {
        let logfile = rolling::never("/tmp", "dune.log");
        let stdout = std::io::stdout;
        tracing_subscriber::fmt()
            .with_writer(stdout.and(logfile))
            .init();
        info!("Tracing and logging enabled!");
        let dune = Self::new(cfg, true);
        dune.stats();
        dune
    }

    pub fn stats(&self) {
        // TODO: add core count
        info!(
            "Collected <{}> nodes on <{}> phynodes.",
            self.nodes.len(),
            self.infra.nodes.len()
        );
    }

    pub fn from(mut cfg: Config, load: bool) -> Self {
        info!("Creating DUNE object from configuration file.");
        fn load_interface(
            nodes: &mut HashMap<String, Node>,
            pfs: &mut PhynodesVfs,
            vlanid: u32,
            link: &Link,
            cfg: &Config,
            idx: usize,
        ) -> bool {
            // We only emulated p2p links, hence a link connects only two endpoints, i.e., two interfaces.
            assert!(idx == 0 || idx == 1);

            let peer = nodes.get(&link.endpoints[(idx + 1) % 2].node).cloned();

            // We get the first endpoint's node.
            if let Some(node) = nodes.get_mut(&link.endpoints[idx].node) {
                let mut share_phynode = true;
                if let Some(p1) = &node.phynode
                    && let Some(peer) = peer
                    && let Some(p2) = &peer.phynode
                {
                    share_phynode = p1 == p2;
                    // Keep track of allocated VF per PF, as well as outer VLAN id.
                    // Inner VLAN id is the ifindex.
                    if !share_phynode {
                        // We need an external interface, if not already allocated.
                        if node.pf_ifname.is_none() {
                            // Get a pair (PF, VF) according to our allocated CPU.
                            if let Some(pfs) = pfs.get_mut(p1)
                                && let Some(cpu) = node.cpu
                                && let Ok((pf_name, vf_id)) = pfs.get_vf(cpu)
                            {
                                node.pf_ifname = Some(pf_name);
                                node.vf_idx = Some(vf_id as u32);
                            } else {
                                // Something went terribly wrong, we can not allocate a VF to our node.
                                error!("Could not allocate a VF to current node.");
                            }
                        }
                    }
                }

                let ifname = link.endpoints[idx].interface.clone();

                // Get or initialises the interfaces list of the node.
                let interfaces = node.interfaces.get_or_insert_with(HashMap::new);

                // Skip the loopback ifindex if the list is empty, otherwise ensure that we do not duplicate an ifindex.
                let ifindex = interfaces.len() + 2;
                // If both nodes are not on the same phynode, we configure the vlan id of the link.
                let vlan = (!share_phynode).then_some(vlanid);
                let mut iface = Interface::new(
                    &cfg.topology.defaults.links,
                    &link,
                    idx,
                    ifindex as u32,
                    vlan,
                );

                // Load interface's addresse(s), if any
                if let Some(addrs) = &node.addrs
                    && let Some(addrs) = addrs.get(&ifname)
                {
                    iface.addrs = Some(addrs.clone());
                }

                // Only insert the interface if it is not already defined to avoid duplicates
                interfaces.entry(ifname).or_insert(iface);
                return !share_phynode;
            }
            false
        }

        // Collect and expand Nodes data
        let mut nodes = cfg
            .topology
            .nodes
            .iter()
            .map(|(name, config)| {
                (
                    name.clone(),
                    Node::new(&cfg.topology.defaults.nodes, &config, name),
                )
            })
            .collect::<HashMap<String, Node>>();

        // Allocate cores to Pinned processes.
        allocate(&mut nodes, &mut cfg.infrastructure);

        // Collect SR-IOV interfaces.
        let mut pfs = cfg
            .infrastructure
            .nodes
            .iter()
            .map(|(name, phynode)| (name.clone(), PhynodeVfs::from(phynode)))
            .collect::<PhynodesVfs>();
        let mut vlanid = 1;

        // Collect and expand Links data. Thanks to the previous allocation,
        // we know whether both endpoints are on the same phynode or not.
        cfg.topology.links.iter().for_each(|link| {
            let mut bump_vlan = false;
            // If necessary, we provide a common vlanid for both endpoints of a link crossing
            // different phynodes.
            (0..2).into_iter().for_each(|idx| {
                bump_vlan = load_interface(&mut nodes, &mut pfs, vlanid, link, &cfg, idx);
            });
            if bump_vlan {
                vlanid += 1;
            }
        });

        let mut ret = Self {
            nodes: nodes.clone(),
            infra: cfg.infrastructure,
        };

        // Load Node's files, if any.
        ret.nodes
            .iter_mut()
            .for_each(|(_, node)| node.configure(&nodes, load));

        ret
    }

    pub fn new(cfg: &PathBuf, load: bool) -> Self {
        // Load DUNE's configuration
        let cfg = Config::new(cfg.to_str().unwrap());
        Self::from(cfg, load)
    }

    pub fn phynodes(&self) -> Vec<NodeId> {
        self.infra
            .nodes
            .iter()
            .map(|(phynode, _)| phynode.clone())
            .collect::<Vec<NodeId>>()
    }

    pub fn phynode_setup(&self, phynode: NodeId) -> Result<(), ()> {
        let _span = span!(Level::INFO, "phynode", name = phynode).entered();

        // 1. Collect nodes to init on the current phynode.
        info!("Filtering nodes for current phynode.");
        // TODO: Maybe keep references towards the node?
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

        // 2. Spawn a netlink socket.
        info!("Spawning a netlink socket.");
        let rt = Arc::new(tokio::runtime::Runtime::new().unwrap());
        let rt0 = rt.clone();
        let (tx, rx) = channel();
        rt.block_on(async move {
            if let Ok((conn, handle, _rx)) = new_connection() {
                rt0.spawn(conn);
                let _ = tx.send(handle);
            } else {
                error!("Failed to spawn a netlink socket. Aborting phynode setup!");
            }
        });

        // We provide the tokio runtime for async netlink calls as well as the
        // netlink socket handle to the setup functions.
        let nethandle = if let Ok(h) = rx.try_recv() {
            NetlinkHandler {
                rt,
                handle: Arc::new(h),
            }
        } else {
            return Err(());
        };

        // 3. Initialise the SR-IOV interfaces, if any.
        let pfs = self.infra.nodes[&phynode]
            .interfaces
            .iter()
            .filter_map(|iface| {
                if let Some(sriov) = &iface.sriov
                    && let Ok(mut pf) = Pf::new(&iface.name, Some(&nethandle))
                {
                    if let Err(e) = pf.reset() {
                        error!("{e:?}");
                    }

                    // TODO: Only init selected VFs.
                    if let Err(e) = pf.create_vfs(sriov.max_vfs) {
                        error!("{e:?}");
                    }
                    Some((&iface.name, pf))
                } else {
                    None
                }
            })
            .collect::<HashMap<&String, Pf>>();

        // 4. Initialise and setup nodes.
        info!("Got <{}> nodes to install on <{phynode}>", nodes.len());

        // Instanciate nodes
        let span = span!(Level::INFO, "step", name = "nodes").entered();
        nodes.iter().for_each(|node| node.init());
        span.exit();

        // Configure interfaces
        let span = span!(Level::INFO, "step", name = "interfaces").entered();
        nodes.iter().for_each(|node| {
            let mut vf = if let Some(pf) = &node.pf_ifname
                && let Some(id) = node.vf_idx
                && let Some(pf) = pfs.get(pf)
            {
                pf.get_vf(id as usize).ok()
            } else {
                None
            };

            node.setup(&nethandle, &mut vf.as_mut());
        });
        span.exit();

        Ok(())
    }
}

pub struct NetlinkHandler {
    pub handle: Arc<Handle>,
    pub rt: Arc<tokio::runtime::Runtime>,
}
