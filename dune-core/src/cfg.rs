use std::collections::HashMap;
use std::fs;
use std::net::IpAddr;
use std::process::Command;
use std::str::{self, FromStr};
use std::vec::Vec;

use futures::executor::block_on;
use ipnetwork::IpNetwork;
use minijinja::Environment;
use regex::Regex;
use rtnetlink::NetworkNamespace;
use serde::{de::Visitor, Deserialize, Serialize, Serializer};

use crate::NodeId;

fn expand<T: std::iter::IntoIterator<Item = U> + std::iter::Extend<U> + Clone, U>(
    node: &mut Option<T>,
    cfg: &Option<T>,
) {
    if let Some(entry) = cfg {
        match node {
            Some(node_cfg) => node_cfg.extend(entry.clone()),
            None => *node = Some(entry.clone()),
        }
    }
}

// ==== Phynode ====

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Phynode {
    pub cores: Vec<Vec<u64>>,
    #[serde(default, flatten)]
    pub _additional_fields: Option<HashMap<String, toml::Value>>,
}

impl Phynode {
    pub fn cores(&self) -> usize {
        self.cores.iter().fold(0, |acc, cores| acc + cores.len())
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Phynodes {
    pub nodes: HashMap<String, Phynode>,
    #[serde(default, flatten)]
    pub _additional_fields: Option<HashMap<String, toml::Value>>,
}

impl Phynodes {
    pub fn cores(&self) -> usize {
        self.nodes
            .iter()
            .fold(0, |acc, (_, phynode)| acc + phynode.cores())
    }
}

// ==== Configuration ====

#[derive(Serialize, Deserialize, Debug)]
pub struct Config {
    pub infrastructure: Phynodes,
    pub topology: Topology,
}

impl Config {
    pub fn new(path: &str) -> Self {
        // TODO: handle I/O Errors
        let content = fs::read(path).unwrap();
        let cfg: Config = toml::from_str(str::from_utf8(&content).unwrap()).unwrap();
        cfg
    }
}

/// Map core name with core id, e.g., core named "core_0" is mapped as follows: ("core_0", 0).
pub type CoreId = String;
pub type Cores = HashMap<CoreId, u64>;
pub type Sysctl = HashMap<String, String>;
pub type Templates = HashMap<String, String>;
pub type Exec = Vec<String>;

// ==== Pinned process ====

#[derive(Serialize, Deserialize, Debug, Clone)]
/// Pinned process informations.
pub struct Pinned {
    /// Command representing the Pinned process.
    pub cmd: String,
    /// Environment variables required to launch the process.
    pub environ: Option<HashMap<String, String>>,
    /// Instruction required to properly shutdown the process.
    pub down: Option<String>,
    /// Set of instructions launched before properly shutting down the process.
    pub pre_down: Option<Vec<String>>,
    #[serde(skip)]
    cores: Cores,
}

impl Pinned {
    /// Lazyly collect cores list required for the current process.
    pub fn cores(&mut self) -> Cores {
        let re = Regex::new("^core_\\d+$").unwrap();
        if self.cores.len() == 0
            && let Some(environ) = &self.environ
        {
            self.cores.insert("core_0".to_string(), 0);
            let env = Environment::new();
            environ.iter().for_each(|(_var, value)| {
                let tmpl = env.template_from_str(value).unwrap();
                for value in tmpl.undeclared_variables(true) {
                    if let Some(_m) = re.find(&value) {
                        self.cores
                            .insert(value.clone(), u64::from_str(&value[5..]).unwrap());
                    }
                }
            });
        }
        self.cores.clone()
    }

    /// Lazyly get the number of cores required for the current process.
    pub fn n_cores(&mut self) -> usize {
        self.cores().len()
    }
}

// ==== Default elements ====

#[derive(Serialize, Deserialize, Debug)]
pub struct Defaults {
    pub links: Option<LinksDefaults>,
    pub nodes: Option<NodesDefaults>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct NodesDefaults {
    pub sysctls: Option<Sysctl>,
    pub templates: Option<Templates>,
    pub exec: Option<Exec>,
    pub pinned: Option<Vec<Pinned>>,
    #[serde(default, flatten)]
    _additional_fields_: Option<HashMap<String, toml::Value>>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct LinksDefaults {
    pub latency: String,
    pub metric: u64,
    pub mtu: u32,
    pub bw: String,
    #[serde(default, flatten)]
    _additional_fields: Option<HashMap<String, toml::Value>>,
}

// ==== Interface ====

#[derive(Serialize, Deserialize, Debug, Default, Clone)]
pub struct Interface {
    /// Name of the Interface
    pub name: String,
    /// Latency of the Link
    pub latency: String,
    /// Metric of the Interface
    pub metric: u64,
    /// Bandwidth of the Link
    pub bandwidth: String,
    /// MTU of the Link
    pub mtu: u32,
    /// Index of the current Endpoint in the Endpoints list defined in the configuration
    pub idx: usize,
    /// Peer Endpoint
    pub peer: Option<Endpoint>,
}

impl Interface {
    fn set_from_field(&mut self, name: &str, field: &toml::Value) {
        match name {
            "latency" => {
                if let Some(latency) = field.as_str() {
                    self.latency = latency.to_string();
                }
            }
            "metric" => {
                if let Some(metric) = field.as_integer() {
                    self.metric = metric as u64;
                }
            }
            "mtu" => {
                if let Some(mtu) = field.as_integer() {
                    self.mtu = mtu as u32;
                }
            }
            "bw" => {
                if let Some(bw) = field.as_str() {
                    self.bandwidth = bw.to_string();
                }
            }
            _ => {}
        }
    }

    pub fn new(dflt: &Option<LinksDefaults>, config: &Link, idx: usize) -> Self {
        assert!(idx == 0 || idx == 1, "Index should be 0 or 1");

        // Expand Endpoint configuration from Defaults
        let mut iface = match dflt {
            Some(dflt) => Interface::from(dflt),
            None => Interface::default(),
        };

        let name = &config.endpoints[idx].interface;

        // Override default values if any specified
        config._additional_fields.iter().for_each(|(idx, field)| {
            let idx = idx.as_str();
            if let Some(endpoint) = Endpoint::try_from(idx).ok()
                && &endpoint.interface == name
            {
                if let Some(table) = field.as_table() {
                    table.iter().for_each(|(idx, field)| {
                        // Latency and MTU are bidirectionnal and should not be modified
                        // TODO: log warning
                        if idx != "latency" && idx != "mtu" {
                            iface.set_from_field(idx, field);
                        }
                    })
                }
            } else {
                iface.set_from_field(idx, field);
            }
        });

        // Set interface name
        iface.name = name.clone();
        iface.peer = Some(config.endpoints[1 - idx].clone());
        iface.idx = idx;

        iface
    }

    pub fn setup(&self, node: &NodeId, addrs: Option<&Vec<IpNetwork>>) {
        // Configure link.
        // If the peer interface is on the same node, the link is created with
        // a pair of virtual interfaces (veth).
        // If both interfaces are not on the same phynode, create a vlan.
        if let Some(endpoint) = &self.peer {
            // e.g., ip l add eth0 netns r0 type veth peer name eth0 netns r1
            let _ = Command::new("ip")
                .arg("l")
                .arg("add")
                .arg(&self.name)
                .arg("netns")
                .arg(node)
                .arg("type")
                .arg("veth")
                .arg("peer")
                .arg("name")
                .arg(&endpoint.interface)
                .arg("netns")
                .arg(&endpoint.node)
                .output();
        } else if &self.name != "lo" {
            // TODO
        }

        if let Some(addrs) = addrs {
            addrs.iter().for_each(|addr| {
                let _ = Command::new("ip")
                    .arg("-n")
                    .arg(node)
                    .arg("a")
                    .arg("add")
                    .arg(addr.to_string())
                    .arg("dev")
                    .arg(&self.name)
                    .output();
            });
        }

        // Set interface up
        let _ = Command::new("ip")
            .arg("-n")
            .arg(node)
            .arg("l")
            .arg("set")
            .arg("dev")
            .arg(&self.name)
            .arg("up")
            .output();
    }
}

// ==== Node ====

#[derive(Serialize, Deserialize, Debug, Default, Clone)]
pub struct Node {
    // ==== Fields provided in the configuration ====
    pub sysctls: Option<Sysctl>,
    pub templates: Option<Templates>,
    pub exec: Option<Exec>,
    pub pinned: Option<Vec<Pinned>>,
    pub addrs: Option<HashMap<String, Vec<IpNetwork>>>,
    #[serde(default, flatten)]
    _additional_fields: Option<HashMap<String, toml::Value>>,

    // ==== DUNE's internal fields ====
    // Some fields should not be deserialized from the DUNE's configuration file but
    // they have to be serializable to send DUNE context to phynodes.
    // Hence, they are wrapped in Option so that they are None upon configuration parsing
    /// Node's name
    pub name: Option<String>,
    /// Mapping of core identifier and real core number
    #[serde(skip)]
    pub cores: HashMap<CoreId, Option<u64>>,
    /// Phynode to which the current Node is attached
    pub phynode: Option<String>,
    // #[serde(skip)]
    pub interfaces: Option<HashMap<String, Interface>>,
}

impl Node {
    pub fn new(dflt: &Option<NodesDefaults>, config: &Self, name: &String) -> Self {
        // Expand Node configuration from Defaults
        let mut node = match dflt {
            Some(dflt) => Node::from(dflt),
            None => Node::default(),
        };

        // Explicit Node configuration overrides Defaults
        expand(&mut node.sysctls, &config.sysctls);
        expand(&mut node.templates, &config.templates);
        expand(&mut node.exec, &config.exec);
        expand(&mut node.pinned, &config.pinned);
        node.addrs = config.addrs.clone();
        node.name = Some(name.clone());

        // TODO: sanity check: core_id defined in a single Pinned process unless duplicate entries are explicitely allowed
        // FIXME: What happens if multiple Pinned process use undertone core_0 ?

        // Collect requested cores. They are currently not allocated.
        if let Some(pinned) = &mut node.pinned {
            node.cores = pinned
                .iter_mut()
                .flat_map(|pinned| pinned.cores())
                .map(|core_id| (core_id.0.clone(), None))
                .collect();
        }

        node
    }

    pub fn cores(&self) -> usize {
        self.cores.len()
    }

    pub fn setup(&self) {
        // FIXME: Use rtnetlink rather than Command calls

        // TODO: Log errors if any
        if let Some(netns) = &self.name {
            // 1. Create netns
            let _ = block_on(NetworkNamespace::add(netns.clone()));

            if let Some(addrs) = &self.addrs
                && let Some(addrs) = addrs.get("lo")
            {
                addrs.iter().for_each(|addr| {
                    println!("lo {}", addr.to_string());
                    let _ = Command::new("ip")
                        .arg("-n")
                        .arg(netns)
                        .arg("a")
                        .arg("add")
                        .arg(addr.to_string())
                        .arg("dev")
                        .arg("lo")
                        .output();
                });
            }

            // Set interface up
            let _ = Command::new("ip")
                .arg("-n")
                .arg(netns)
                .arg("l")
                .arg("set")
                .arg("dev")
                .arg("lo")
                .arg("up")
                .output();

            // 2. Setup links: create veth pairs or vlan interfaces, if required
            if let Some(interfaces) = &self.interfaces {
                interfaces.iter().for_each(|(ifname, iface)| {
                    let addrs = self.addrs.as_ref().and_then(|a| a.get(ifname));
                    iface.setup(netns, addrs);
                });
            }

            // 4. Apply sysctls to nodes
            if let Some(sysctls) = &self.sysctls {
                sysctls.iter().for_each(|(sysctl, value)| {
                    let _ = Command::new("ip")
                        .arg("netns")
                        .arg("exec")
                        .arg(netns)
                        .arg("sysctl -w")
                        .arg(sysctl)
                        .arg(value)
                        .output();
                })
            }

            // 5. Apply execs to nodes
            if let Some(execs) = &self.exec {
                execs.iter().for_each(|exec| {
                    let _ = Command::new("ip")
                        .arg("netns")
                        .arg("exec")
                        .arg(netns)
                        .arg(exec)
                        .output();
                });
            }

            // 6. Apply pinned to nodes
            // TODO
        }
    }
}

// trait NodeSetup {
//     /// Initialize a Node.
//     /// 1. Create the nework namespace
//     /// 2. Initialize the loopback addresses, if any.
//     fn setup(&mut self);
// }

impl From<&NodesDefaults> for Node {
    fn from(dflt: &NodesDefaults) -> Self {
        let mut node = Self::default();
        node.pinned = dflt.pinned.clone();
        node.sysctls = dflt.sysctls.clone();
        node.exec = dflt.exec.clone();
        node.templates = dflt.templates.clone();
        node
    }
}

// ==== Endpoint ====

#[derive(Debug, Default, Clone)]
pub struct Endpoint {
    pub node: String,
    pub interface: String,
}

impl Serialize for Endpoint {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(format!("{}:{}", self.node, self.interface).as_str())
    }
}

impl<'de> Deserialize<'de> for Endpoint {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_str(EndpointVisitor)
    }
}

struct EndpointVisitor;

impl<'de> Visitor<'de> for EndpointVisitor {
    type Value = Endpoint;

    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(
            formatter,
            "an endpoint formatted as \"<node_id>:<interface_name>\", e.g., \"r0:eth0\"."
        )
    }

    fn visit_str<E>(self, s: &str) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Endpoint::try_from(s)
            .map_err(|_err| serde::de::Error::custom("Can not convert &str to endpoint"))
    }
}

impl TryFrom<&str> for Endpoint {
    type Error = ();
    fn try_from(value: &str) -> Result<Self, ()> {
        // TODO: Return useful error
        let endpoint: [&str; 2] = value
            .split(":")
            .collect::<Vec<&str>>()
            .try_into()
            .map_err(|_err| ())?;
        Ok(Endpoint {
            node: endpoint[0].to_string(),
            interface: endpoint[1].to_string(),
        })
    }
}

// ==== Link ====

#[derive(Serialize, Deserialize, Debug, Default)]
pub struct Link {
    pub endpoints: [Endpoint; 2],
    #[serde(default, flatten)]
    _additional_fields: HashMap<String, toml::Value>,
}

impl From<&LinksDefaults> for Interface {
    fn from(dflt: &LinksDefaults) -> Self {
        let mut iface = Interface::default();
        iface.latency = dflt.latency.clone();
        iface.bandwidth = dflt.bw.clone();
        iface.mtu = dflt.mtu;
        iface.metric = dflt.metric;
        iface
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Topology {
    pub defaults: Defaults,
    pub nodes: HashMap<String, Node>,
    pub links: Vec<Link>,
}
