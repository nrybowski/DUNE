use std::collections::HashMap;
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr};
use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use std::str::{self, FromStr};
use std::thread;
use std::vec::Vec;
use std::{fs, io};

use core_affinity::{self, CoreId as CaCoreId};
use futures::executor::block_on;
use futures::future::Inspect;
use futures::AsyncWriteExt;
use ipnetwork::IpNetwork;
use netns_rs::NetNs;
use nix::NixPath;
use regex::Regex;
use rtnetlink::NetworkNamespace;
use serde::de::IntoDeserializer;
use serde::{de::Visitor, Deserialize, Serialize, Serializer};
use tracing::{debug, error, event, info, instrument, span, warn, Level};

use minijinja::{context, path_loader, Environment};
use netlink_packet_route::link::{
    self,
    LinkAttribute::{self, LinkInfo},
    LinkFlag, State,
};
use nix::{self, fcntl::OFlag, sys::stat::Mode};
use rtnetlink::{new_connection, LinkHandle};
use tokio;

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

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Phynode {
    pub cores: Vec<Vec<u64>>,
    pub binds: Option<Binds>,
    #[serde(default, flatten)]
    pub _additional_fields: Option<HashMap<String, toml::Value>>,
}

impl Phynode {
    pub fn cores(&self) -> usize {
        self.cores.iter().fold(0, |acc, cores| acc + cores.len())
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
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
pub type Binds = Vec<DuneFile>;
pub type Exec = Vec<String>;

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct DuneFile {
    pub src: String,
    pub dst: String,
    pub content: Option<Vec<u8>>,
    pub exec: bool,
}

impl From<(String, String)> for DuneFile {
    fn from(value: (String, String)) -> Self {
        Self {
            src: value.0,
            dst: value.1,
            content: None,
            exec: false,
        }
    }
}

impl DuneFile {
    #[instrument]
    pub fn load(&mut self) {
        match fs::read(&self.src) {
            Ok(content) => {
                self.content = Some(content);
            }
            Err(_e) => {
                // TODO: handle I/O errors
                event!(Level::ERROR, "Failed to load file <{}> content", self.src);
            }
        }
    }
}

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
    /// Set of instructions launched before starting the process.
    pub post_up: Option<Vec<String>>,
    // #[serde(skip)]
    cores: Option<Cores>,
}

impl Pinned {
    /// Lazyly collect cores list required for the current process.
    pub fn cores(&mut self) -> Cores {
        let re = Regex::new("^core_\\d+$").unwrap();
        if let None = self.cores {
            let mut cores = Cores::new();
            cores.insert("core_0".to_string(), 0);

            if let Some(environ) = &self.environ {
                let env = Environment::new();
                environ.iter().for_each(|(_var, value)| {
                    let tmpl = env.template_from_str(value).unwrap();
                    for value in tmpl.undeclared_variables(true) {
                        if let Some(_m) = re.find(&value) {
                            cores.insert(value.clone(), u64::from_str(&value[5..]).unwrap());
                        }
                    }
                });
            }

            self.cores = Some(cores);
        }
        self.cores.as_ref().unwrap().clone()
    }

    /// Lazyly get the number of cores required for the current process.
    pub fn n_cores(&mut self) -> usize {
        self.cores().len()
    }

    pub fn expand<T: Serialize>(&mut self, ctx: T) {
        let env = Environment::new();

        // Expand pre_down commands
        if let Some(pre_down) = &mut self.pre_down {
            self.pre_down = Some(
                pre_down
                    .iter()
                    .filter_map(|cmd| {
                        if let Ok(res) = env.render_str(cmd, &ctx) {
                            Some(res)
                        } else {
                            None
                        }
                    })
                    .collect(),
            );
        }

        // Expand command
        if let Ok(res) = env.render_str(&self.cmd, &ctx) {
            self.cmd = res;
        } else {
            error!("Failed to expand cmd.");
        }

        // Expand post_up commands
        if let Some(post_up) = &mut self.post_up {
            self.post_up = Some(
                post_up
                    .iter()
                    .filter_map(|cmd| {
                        if let Ok(res) = env.render_str(cmd, &ctx) {
                            Some(res)
                        } else {
                            None
                        }
                    })
                    .collect(),
            );
        }
    }
}

// ==== Default elements ====

#[derive(Serialize, Deserialize, Debug)]
pub struct Defaults {
    pub links: Option<LinksDefaults>,
    pub nodes: Option<NodesDefaults>,
}

// FIXME: type NodeDefaults = Node;
#[derive(Serialize, Deserialize, Debug)]
pub struct NodesDefaults {
    pub sysctls: Option<Sysctl>,
    pub binds: Option<Binds>,
    pub templates: Option<Templates>,
    pub exec: Option<Exec>,
    pub pinned: Option<Vec<Pinned>>,
    #[serde(default, flatten)]
    _additional_fields_: Option<HashMap<String, toml::Value>>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct LinksDefaults {
    pub latency: Option<String>,
    pub metric: Option<u64>,
    pub mtu: Option<u32>,
    pub bw: Option<String>,
    #[serde(default, flatten)]
    _additional_fields: Option<HashMap<String, toml::Value>>,
}

// ==== Interface ====
//
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Addr {
    prefix: IpAddr,
    plen: u8,
}

impl From<&IpNetwork> for Addr {
    fn from(value: &IpNetwork) -> Self {
        Self {
            prefix: value.ip(),
            plen: value.prefix(),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Default, Clone)]
pub struct Interface {
    /// Name of the Interface
    pub name: String,
    /// Latency of the Link
    pub latency: Option<String>,
    /// Metric of the Interface
    pub metric: Option<u64>,
    /// Bandwidth of the Link
    pub bandwidth: Option<String>,
    /// MTU of the Link
    pub mtu: Option<u32>,
    /// MAC address of the interface
    pub mac: Option<Vec<u8>>,
    /// Index of the current Endpoint in the Endpoints list defined in the configuration
    pub idx: usize,
    /// Peer Endpoint
    pub peer: Option<Endpoint>,
    /// Interface's addresses
    pub addrs: Option<Vec<IpNetwork>>,
    /// Interface's addresses for rendering context
    pub ctx_addrs: Option<Vec<Addr>>,
    /// Interface's MAC addresse for rendering context
    pub ctx_mac: Option<String>,
    //// Interface index
    pub ifindex: u32,
}

impl Interface {
    fn set_from_field(&mut self, name: &str, field: &toml::Value) {
        match name {
            "latency" => {
                if let Some(latency) = field.as_str() {
                    self.latency = Some(latency.to_string());
                }
            }
            "metric" => {
                if let Some(metric) = field.as_integer() {
                    self.metric = Some(metric as u64);
                }
            }
            "mtu" => {
                if let Some(mtu) = field.as_integer() {
                    self.mtu = Some(mtu as u32);
                }
            }
            "bw" => {
                if let Some(bw) = field.as_str() {
                    self.bandwidth = Some(bw.to_string());
                }
            }
            "mac" => {
                if let Some(mac) = field.as_str() {
                    // Ugly cast from textual byte representation to actual bytes
                    let mac = mac
                        .chars()
                        .filter_map(|e| {
                            if e != ':'
                                && let Some(digit) = e.to_digit(16)
                            {
                                Some(digit as u8)
                            } else {
                                None
                            }
                        })
                        .enumerate()
                        .fold(Vec::new(), |mut acc, (idx, x)| {
                            if idx & 1 == 1 {
                                let byte = (acc.pop().unwrap() << 4 | x as u8) as u8;
                                acc.push(byte);
                            } else {
                                acc.push(x);
                            }
                            acc
                        });
                    self.mac = Some(mac);
                }
            }
            _ => {}
        }
    }

    pub fn new(dflt: &Option<LinksDefaults>, config: &Link, idx: usize, ifindex: u32) -> Self {
        assert!(idx == 0 || idx == 1, "Index should be 0 or 1");

        // Expand Endpoint configuration from Defaults
        let mut iface = match dflt {
            Some(dflt) => Interface::from(dflt),
            None => Interface::default(),
        };

        let name = &config.endpoints[idx].interface;

        // Override default values, if any specified
        config._additional_fields.iter().for_each(|(idx, field)| {
            let idx = idx.as_str();
            if let Ok(endpoint) = Endpoint::try_from(idx)
                && &endpoint.interface == name
            {
                if let Some(table) = field.as_table() {
                    table.iter().for_each(|(idx, field)| {
                        // MTU is bidirectionnal and should not be modified
                        if idx != "mtu" {
                            iface.set_from_field(idx, field);
                        } else {
                            warn!("Skipped unidirectionnal MTU setup.");
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
        iface.ifindex = ifindex;

        iface
    }

    pub fn setup(&self, node: &NodeId, addrs: Option<&Vec<IpNetwork>>) {
        let _span = span!(Level::INFO, "interface", name = self.name).entered();
        info!("Interface setup");

        // Configure link.
        // If the peer interface is on the same node, the link is created with
        // a pair of virtual interfaces (veth).
        // If both interfaces are not on the same phynode, create a vlan.

        let mut open_flags = OFlag::empty();
        open_flags.insert(OFlag::O_RDONLY);
        open_flags.insert(OFlag::O_CLOEXEC);

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        rt.block_on(async {
            let (connection, handle, _) = new_connection().unwrap();
            tokio::spawn(connection);

            if let Ok(fd1) = nix::fcntl::open(
                format!("/run/netns/{node}").as_str(),
                open_flags,
                Mode::empty(),
            ) {
                if let Some(endpoint) = &self.peer
                    && let Ok(fd2) = nix::fcntl::open(
                        format!("/run/netns/{}", endpoint.node).as_str(),
                        open_flags,
                        Mode::empty(),
                    )
                {
                    let mut req = handle
                        .link()
                        .add()
                        .veth(self.name.clone(), endpoint.interface.clone());
                    let msg = req.message_mut();
                    for attr in &mut msg.attributes {
                        if let LinkInfo(info) = attr {
                            for attr in info {
                                if let link::LinkInfo::Data(data) = attr
                                    && let link::InfoData::Veth(veth) = data
                                    && let link::InfoVeth::Peer(peer) = veth
                                {
                                    // FIXME: Seems unsupported by the kernel
                                    // peer.header.flags.push(LinkFlag::Up);
                                    if let Some(mtu) = self.mtu {
                                        info!("Setting MTU <{mtu}>");
                                        peer.attributes.push(LinkAttribute::Mtu(mtu));
                                    }
                                    info!("MAC: <{:x?}>", self.mac);
                                    if let Some(mac) = &self.mac {
                                        info!("Setting MAC address <{:x?}>", self.mac);
                                        peer.attributes.push(LinkAttribute::Address(mac.clone()));
                                    }
                                    info!("Setting ifindex <{}>", self.ifindex);
                                    peer.header.index = self.ifindex;
                                    peer.attributes.push(LinkAttribute::NetNsFd(fd1));
                                }
                            }
                        }
                    }

                    if let Some(mtu) = self.mtu {
                        info!("Setting peer MTU <{mtu}>");
                        msg.attributes.push(LinkAttribute::Mtu(mtu));
                    }

                    info!("{:#?}", self.peer);

                    // if let Some(mac) = remote_mac {
                    // info!("Setting peer MAC <{mac:x?}>");
                    // msg.attributes.push(LinkAttribute::Address(mac));
                    // }
                    // msg.header.index = ifindex;
                    msg.attributes.push(LinkAttribute::NetNsFd(fd2));
                    if let Err(e) = req.execute().await.map_err(|e| format!("{}", e)) {
                        error!("{e}");
                    }

                    // Set MAC address (if any) and interface up
                    // let mut req = handle.link().set(self.ifindex.unwrap() as u32);
                    // let msg = req.message_mut();
                    // }
                    // msg.header.flags.push(LinkFlag::Up);

                    // if let Err(e) = req.execute().await.map_err(|e| format!("{}", e)) {
                    // warn!("{e}");
                    // }
                } else if self.name == "lo" {
                    // Nothing to do, just skip error message
                } else {
                    warn!("Failed to open peer netns");
                }

                // Add addresses to the interface, if specified
                if let Some(addrs) = addrs {
                    // let mut req =
                    //     handle
                    //         .address()
                    //         .add(0, IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 24);
                    // let msg = req.message_mut();
                    // println!("{msg:#?}");

                    // FIXME: Use netlink to issue all the commands below
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

                // info!("Spawning thread for interface configuration");
                // let _ = thread::scope(|scope| {
                //     let _ = scope
                //         .spawn(move || {
                //             if let Ok(ns) = NetNs::get(node) {
                //                 if let Ok(_) = ns.enter() {
                //                     // Set MAC address if specified
                //                     block_on(async {
                //                         if let Some(mac) = &self.mac {
                //                             info!("Setting MAC <{mac:x?}>");
                //                             if let Err(e) = handle
                //                                 .link()
                //                                 .set(self.ifindex)
                //                                 .address(mac.clone())
                //                                 .execute()
                //                                 .await
                //                                 .map_err(|e| format!("{}", e))
                //                             {
                //                                 warn!("{e}");
                //                             }
                //                         }
                //                     })

                //                     // Set interface up
                //                     // if let Err(e) = handle
                //                     //     .link()
                //                     //     .set(self.ifindex)
                //                     //     .up()
                //                     //     .execute()
                //                     //     .await
                //                     //     .map_err(|e| format!("{}", e))
                //                     // {
                //                     //     warn!("{e}");
                //                     // }
                //                     // })
                //                     // });
                //                 } else {
                //                     error!("Failde to enter <{node}> netns.");
                //                 }
                //             } else {
                //                 error!("Failed to open <{node}> netns to configure interfaces.");
                //             }
                //         })
                //         .join();
                // });
            }
        });
        // });

        // Configure the maximum bandwidth of the link, if specified
        // TODO
        // https://docs.rs/rtnetlink/latest/rtnetlink/struct.QDiscNewRequest.html

        // Configure the latency of the link, if specified
        // TODO
        // https://docs.rs/rtnetlink/latest/rtnetlink/struct.QDiscNewRequest.html
        //
        // FIXME: use netlink only
        info!("Mac {:x?}", self.mac);
        if let Some(mac) = &self.mac {
            let mut cmd = Command::new("ip");
            cmd.arg("-n")
                .arg(node)
                .arg("l")
                .arg("set")
                .arg("dev")
                .arg(&self.name)
                .arg("address")
                .arg({
                    let last = mac.len() - 1;
                    mac.iter()
                        .enumerate()
                        .fold(String::new(), |mut acc, (idx, byte)| {
                            let formatted =
                                format!("{byte:x}{}", if idx == last { "" } else { ":" });
                            acc.push_str(&formatted);
                            acc
                        })
                });
            info!("Setting MAC <{cmd:#?}>");
            if let Err(e) = cmd.output() {
                warn!("{e:#?}");
            }
        }

        // Set interface up
        // FIXME: Use netlink to issue all the commands below
        // TODO: should only up remote end of the link
        let _ = Command::new("ip")
            .arg("-n")
            .arg(node)
            .arg("l")
            .arg("set")
            .arg("dev")
            .arg(&self.name)
            .arg("up")
            .output();

        if let Some(latency) = &self.latency {
            // tc qdisc add dev eth2 root netem delay 1ms
            let _ = Command::new("ip")
                .arg("netns")
                .arg("exec")
                .arg(node)
                .arg("tc")
                .arg("qdisc")
                .arg("add")
                .arg("dev")
                .arg(self.name.clone())
                .arg("root")
                .arg("netem")
                .arg("delay")
                .arg(latency)
                .output();
        }
    }
}

// ==== Node ====

#[derive(Serialize, Deserialize, Debug, Default, Clone)]
pub struct Node {
    // ==== Fields provided in the configuration ====
    pub sysctls: Option<Sysctl>,
    pub templates: Option<Templates>,
    pub binds: Option<Vec<DuneFile>>,
    pub exec: Option<Exec>,
    pub pinned: Option<Vec<Pinned>>,
    pub addrs: Option<HashMap<String, Vec<IpNetwork>>>,

    // ==== DUNE's internal fields ====
    // Some fields should not be deserialized from the DUNE's configuration file but
    // they have to be serializable to send DUNE context to phynodes.
    // Hence, they are wrapped in Option so that they are None upon configuration parsing
    /// Node's name
    pub name: Option<String>,
    /// Mapping of core identifier and real core number
    // #[serde(skip)]
    pub cores: Option<HashMap<CoreId, Option<u64>>>,
    /// Phynode to which the current Node is attached
    pub phynode: Option<String>,
    // #[serde(skip)]
    pub interfaces: Option<HashMap<String, Interface>>,
    // FIXME: make this cleaner by dividing Node in NodeCfg and Node and calling load() upon Node::from(NodeCfg)
    pub tmpls: Option<Vec<DuneFile>>,

    #[serde(default, flatten)]
    _additional_fields: Option<HashMap<String, toml::Value>>,
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
        expand(&mut node.binds, &config.binds);
        expand(&mut node.templates, &config.templates);
        expand(&mut node.exec, &config.exec);
        expand(&mut node.pinned, &config.pinned);
        expand(&mut node._additional_fields, &config._additional_fields);
        node.addrs = config.addrs.clone();
        node.name = Some(name.clone());

        // Add Loopback interface if any address has to be configured
        let loopback = "lo".to_string();
        if let Some(addrs) = node.addrs.as_ref().and_then(|a| a.get(&loopback)) {
            let mut lo = Interface::default();
            lo.name = loopback.clone();
            lo.addrs = Some(addrs.clone());
            node.interfaces
                .get_or_insert_with(HashMap::new)
                .insert(loopback, lo);
        }

        // TODO: sanity check: core_id defined in a single Pinned process unless duplicate entries are explicitely allowed
        // FIXME: What happens if multiple Pinned process use undertone core_0 ?

        // Collect requested cores. They are currently not allocated.
        if let Some(pinned) = &mut node.pinned {
            node.cores = Some(
                pinned
                    .iter_mut()
                    .flat_map(|pinned| pinned.cores())
                    .map(|core_id| (core_id.0.clone(), None))
                    .collect(),
            );
        }

        // Load files, if any
        // node.load();

        node
    }

    pub fn cores(&self) -> usize {
        if let Some(cores) = &self.cores {
            cores.len()
        } else {
            0
        }
    }

    pub fn load(&mut self) {
        // Load Binds, if any
        if let Some(binds) = &mut self.binds {
            info!("Loading <{}> binds", binds.len());
            binds.iter_mut().for_each(|bind| bind.load());
        }

        // Load and render Templates, if any
        if let Some(templates) = &mut self.templates
            && templates.len() > 0
        {
            info!("Loading <{}> templates", templates.len());
            let mut env = Environment::new();
            env.set_loader(path_loader("."));
            let templates = templates
                .iter()
                .filter_map(|(template, path)| {
                    if let Ok(tmpl) = env.get_template(template) {
                        // Make IpNetworks Serializable to be used in minijinja context
                        if let Some(ifaces) = &mut self.interfaces {
                            // FIXME: Use custom accessor abstracting such operations rather than encoding more info in the structure
                            // see: https://github.com/mitsuhiko/minijinja/discussions/338
                            ifaces.iter_mut().for_each(|(_name, iface)| {
                                if let Some(addrs) = &iface.addrs {
                                    iface.ctx_addrs = Some(
                                        addrs
                                            .iter()
                                            .map(|addr| Addr::from(addr))
                                            .collect::<Vec<Addr>>(),
                                    );
                                }
                                if let Some(mac) = &iface.mac {
                                    let last = mac.len() - 1;
                                    let mac = mac.iter().enumerate().fold(
                                        String::new(),
                                        |mut acc, (idx, byte)| {
                                            let formatted = format!(
                                                "{byte:x}{}",
                                                if idx == last { "" } else { ":" }
                                            );
                                            acc.push_str(&formatted);
                                            acc
                                        },
                                    );
                                    iface.ctx_mac = Some(mac);
                                }
                            });
                        }
                        // Render template
                        match tmpl.render(context! {
                            node => self.name,
                            ifaces => self.interfaces.as_ref().unwrap(),
                            ctx => self._additional_fields
                        }) {
                            Ok(rendered) => {
                                let dst =
                                    env.render_str(path, context! {node => self.name}).unwrap();
                                let mut file = DuneFile::from((template.clone(), dst));
                                file.content = Some(rendered.into());
                                Some(file)
                            }
                            Err(e) => {
                                warn!("Failed to render <{template}>: {e}");
                                None
                            }
                        }
                    } else {
                        warn!("Failed to retrieve template <{template}>");
                        None
                    }
                })
                .collect::<Vec<DuneFile>>();
            self.tmpls = Some(templates);
        }
    }

    pub fn dump_files(&self) {
        fn _dump_file(file: &DuneFile) {
            match fs::File::create(&file.dst) {
                Ok(mut f) => {
                    if let Err(e) = f.write_all(&file.content.as_ref().unwrap()) {
                        warn!("Failed to write <{}>: {e:#?}", file.dst);
                        return;
                    }
                    info!("File <{}> written", file.dst);
                    if file.exec
                        && let Ok(perms) = f.metadata()
                    {
                        let mut perms = perms.permissions();
                        perms.set_mode(0o744);
                        if let Err(_e) = f.set_permissions(perms) {
                            // TODO: handler permissions error
                        }
                    }
                }
                Err(e) => match e.kind() {
                    // Path error, try to create parent directories if they do not exist.
                    io::ErrorKind::NotFound => {
                        let dst = std::path::Path::new(&file.dst);
                        if let Some(dst_parent) = dst.parent() {
                            if !dst_parent.is_dir() {
                                match fs::DirBuilder::new().recursive(true).create(dst_parent) {
                                    Ok(_) => {
                                        _dump_file(file);
                                    }
                                    Err(e) => {
                                        // FIXME: Handler the error
                                        println!("{:#?}", e);
                                    }
                                }
                            }
                            // FIXME: The error was something else, too bad.
                        }
                    }
                    _ => {
                        // FIXME: Handler the error
                        println!("{:#?}", e);
                        return;
                    }
                },
            }
        }

        // Dump Binds, if any
        if let Some(binds) = &self.binds {
            info!("Dumping <{}> bind(s)", binds.len());
            binds.iter().for_each(|file| {
                _dump_file(file);
            });
        }

        // Dump Templates if any
        if let Some(templates) = &self.tmpls {
            info!("Dumping <{}> templates(s)", templates.len());
            templates.iter().for_each(|file| {
                _dump_file(file);
            });
        }
    }

    pub fn init(&self) {
        if let Some(netns) = &self.name {
            info!("Adding netns <{netns}>");
            if let Err(e) = block_on(NetworkNamespace::add(netns.clone())) {
                warn!("Failed to add netns <{netns}>: {e}");
            }
        }
    }

    pub fn configure(&mut self) {
        let ctx = context! {
        node => self.name,
        ifaces => self.interfaces.as_ref().unwrap(),
        ctx => self._additional_fields
        };
        self.expand(&ctx);
        self.load();
    }

    pub fn expand<T: Serialize>(&mut self, ctx: T) {
        // Expand pinned processes
        if let Some(pinned) = &mut self.pinned {
            pinned.iter_mut().for_each(|pinned| pinned.expand(&ctx))
        }
    }

    pub fn setup(&self) {
        let _span = span!(Level::INFO, "node", name = self.name).entered();
        /// Must be called in the correct netns
        fn _async_exec(exec: &String) {
            let out = Command::new("bash").arg("-c").arg(exec).spawn();
            debug!("{:#?}", out);
        }

        fn _sync_exec(exec: &String) {
            let out = Command::new("bash").arg("-c").arg(exec).output();
            debug!("{:#?}", out);
        }

        // 0. Write binds, if any
        self.dump_files();

        if let Some(netns) = &self.name {
            let _span = span!(Level::DEBUG, "node {self.name}").entered();

            // 2. Setup interfaces: create veth pairs or vlan interfaces, if required
            if let Some(interfaces) = &self.interfaces {
                interfaces.iter().for_each(|(ifname, iface)| {
                    let addrs = self.addrs.as_ref().and_then(|a| a.get(ifname));
                    iface.setup(netns, addrs);
                });
            }

            // Enter netns
            if let Ok(ns) = NetNs::get(netns) {
                let _ = ns.run(|_| {
                    // 3. Apply sysctls to nodes
                    if let Some(sysctls) = &self.sysctls {
                        info!("Applying <{}> sysctls.", sysctls.len());
                        sysctls.iter().for_each(|(sysctl, value)| {
                            let cmd = Command::new("sysctl")
                                .arg("-w")
                                .arg(format!("{sysctl}={value}"))
                                .output();
                            if let Err(e) = cmd {
                                warn!("{e:#?}");
                            }
                        });
                    }

                    // 4. Apply execs to nodes
                    if let Some(execs) = &self.exec {
                        info!("Applying <{}> execs.", execs.len());
                        execs.iter().for_each(|exec| {
                            _sync_exec(exec);
                        });
                    }

                    // 6. Apply pinned to nodes
                    if let Some(pinned) = &self.pinned {
                        info!("Applying <{}> pinned processes.", pinned.len());
                        pinned.iter().for_each(|pinned| {
                            if let Some(cores) = &self.cores
                                && let Some(core_id) = cores.get("core_0")
                            {
                                let _ = thread::scope(|scope| {
                                    let _ = scope
                                        .spawn(move || {
                                            if core_affinity::set_for_current(CaCoreId {
                                                id: core_id.unwrap() as usize,
                                            }) {
                                                let mut cmd = pinned.cmd.split_whitespace();
                                                let _out = Command::new(cmd.next().unwrap())
                                                    .args(cmd)
                                                    .spawn();
                                                // _exec(&pinned.cmd);
                                            }
                                        })
                                        .join();
                                });

                                // Launch post_up commands, if any.
                                if let Some(post_ups) = &pinned.post_up {
                                    let _span = span!(Level::INFO, "pinned");
                                    info!("Launching <{}> post_up commands", post_ups.len());
                                    post_ups.iter().for_each(|post_up| {
                                        let _ = thread::scope(|scope| {
                                            let _ = scope
                                                .spawn(move || {
                                                    _async_exec(&post_up);
                                                })
                                                .join();
                                        });
                                    });
                                }
                            }
                        });
                    }
                });
            }
        }
    }
}

impl From<&NodesDefaults> for Node {
    fn from(dflt: &NodesDefaults) -> Self {
        let mut node = Self::default();
        node.pinned = dflt.pinned.clone();
        node.binds = dflt.binds.clone();
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

#[cfg(test)]
mod phynodes {

    use super::*;

    #[test]
    fn phynode_ser() {
        let phynode = Phynode {
            cores: vec![vec![1, 2, 3], vec![4, 5]],
            _additional_fields: Some(HashMap::new()),
        };

        let serialized = toml::to_string(&phynode).expect("Serialization failed");
        let expected = "cores = [[1, 2, 3], [4, 5]]\n";
        assert_eq!(serialized, expected);
    }

    #[test]
    fn phynode_de() {
        let expected = Phynode {
            cores: vec![vec![1, 2, 3], vec![4, 5]],
            _additional_fields: Some(HashMap::new()),
        };

        let cfg = "cores = [[1, 2, 3], [4, 5]]";

        let deserialized: Phynode = toml::de::from_str(&cfg).expect("Deserialization failed");
        assert_eq!(deserialized, expected);
    }

    #[test]
    fn phynode_ser_additional_fields() {
        let mut additional_fields = HashMap::new();
        additional_fields.insert(
            "extra_field".to_string(),
            toml::Value::String("some_value".to_string()),
        );

        let phynode = Phynode {
            cores: vec![vec![1, 2], vec![3, 4]],
            _additional_fields: Some(additional_fields),
        };

        let serialized = toml::to_string(&phynode).expect("Serialization failed");
        let expected = "cores = [[1, 2], [3, 4]]\nextra_field = \"some_value\"\n";

        assert_eq!(serialized, expected);
    }

    #[test]
    fn phynode_de_additional_fields() {
        let mut additional_fields = HashMap::new();
        additional_fields.insert(
            "extra_field".to_string(),
            toml::Value::String("some_value".to_string()),
        );

        let expected = Phynode {
            cores: vec![vec![1, 2], vec![3, 4]],
            _additional_fields: Some(additional_fields),
        };

        let cfg = "cores = [[1, 2], [3, 4]]\nextra_field = \"some_value\"";

        let deserialized: Phynode = toml::de::from_str(&cfg).expect("Deserialization failed");
        assert_eq!(deserialized, expected);
    }

    #[test]
    fn phynode_ser_default() {
        let phynode = Phynode {
            cores: Vec::new(),
            _additional_fields: Some(HashMap::new()),
        };

        let serialized = toml::to_string(&phynode).expect("Serialization failed");
        let expected = "cores = []\n";
        assert_eq!(serialized, expected);
    }

    #[test]
    fn phynode_de_default() {
        let expected = Phynode {
            cores: Vec::new(),
            _additional_fields: Some(HashMap::new()),
        };
        let cfg = "cores = []\n";

        let deserialized: Phynode = toml::de::from_str(&cfg).expect("Deserialization failed");
        assert_eq!(deserialized, expected);
    }

    #[test]
    fn phynodes_ser() {
        let phynode1 = Phynode {
            cores: vec![vec![1, 2], vec![3, 4]],
            _additional_fields: Some(HashMap::new()),
        };

        let phynode2 = Phynode {
            cores: vec![vec![5, 6], vec![7, 8]],
            _additional_fields: Some(HashMap::new()),
        };

        let mut nodes = HashMap::new();
        nodes.insert("node1".to_string(), phynode1);
        nodes.insert("node2".to_string(), phynode2);

        let phynodes = Phynodes {
            nodes,
            _additional_fields: Some(HashMap::new()),
        };

        let serialized = toml::to_string(&phynodes).expect("Serialization failed");
        let expected1 =
            "[nodes.node2]\ncores = [[5, 6], [7, 8]]\n\n[nodes.node1]\ncores = [[1, 2], [3, 4]]\n";
        let expected2 =
            "[nodes.node1]\ncores = [[1, 2], [3, 4]]\n\n[nodes.node2]\ncores = [[5, 6], [7, 8]]\n";
        assert!(serialized == expected1 || serialized == expected2);
    }

    #[test]
    fn phynodes_de() {
        let phynode1 = Phynode {
            cores: vec![vec![1, 2], vec![3, 4]],
            _additional_fields: Some(HashMap::new()),
        };

        let phynode2 = Phynode {
            cores: vec![vec![5, 6], vec![7, 8]],
            _additional_fields: Some(HashMap::new()),
        };

        let mut nodes = HashMap::new();
        nodes.insert("node1".to_string(), phynode1);
        nodes.insert("node2".to_string(), phynode2);

        let expected = Phynodes {
            nodes,
            _additional_fields: Some(HashMap::new()),
        };

        let cfg =
            "[nodes.node1]\ncores = [[1, 2], [3, 4]]\n[nodes.node2]\ncores = [[5, 6], [7, 8]]\n";

        let deserialized: Phynodes = toml::de::from_str(&cfg).expect("Deserialization failed");
        assert_eq!(deserialized, expected);
    }

    #[test]
    fn phynodes_de_additional_fields() {
        let mut additional_fields = HashMap::new();
        additional_fields.insert(
            "extra_field".to_string(),
            toml::Value::String("some_value".to_string()),
        );

        let phynode = Phynode {
            cores: vec![vec![1, 2], vec![3, 4]],
            _additional_fields: Some(HashMap::new()),
        };

        let mut nodes = HashMap::new();
        nodes.insert("node1".to_string(), phynode);

        let phynodes = Phynodes {
            nodes,
            _additional_fields: Some(additional_fields),
        };

        let cfg = "extra_field = \"some_value\"\n[nodes.node1]\ncores = [[1, 2], [3, 4]]\n";

        let deserialized: Phynodes = toml::de::from_str(&cfg).expect("Deserialization failed");
        assert_eq!(phynodes, deserialized);
    }

    #[test]
    fn phynodes_se_additional_fields() {
        let mut additional_fields = HashMap::new();
        additional_fields.insert(
            "extra_field".to_string(),
            toml::Value::String("some_value".to_string()),
        );

        let phynode = Phynode {
            cores: vec![vec![1, 2], vec![3, 4]],
            _additional_fields: Some(HashMap::new()),
        };

        let mut nodes = HashMap::new();
        nodes.insert("node1".to_string(), phynode);

        let phynodes = Phynodes {
            nodes,
            _additional_fields: Some(additional_fields),
        };

        let expected = "extra_field = \"some_value\"\n\n[nodes.node1]\ncores = [[1, 2], [3, 4]]\n";

        let serialized = toml::ser::to_string(&phynodes).expect("Serialized failed");

        assert_eq!(serialized, expected);
    }
}
