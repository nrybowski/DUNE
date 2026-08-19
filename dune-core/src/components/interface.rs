use std::process::Command;
use std::thread;
use std::{collections::HashMap, net::IpAddr};

use futures::TryStreamExt;
use ipnetwork::IpNetwork;
use netns_rs::NetNs;
use nix::{self, fcntl::OFlag, sys::stat::Mode};
use rtnetlink::packet_route::link::{
    InfoData, InfoKind, InfoVeth, LinkAttribute, LinkFlags, LinkInfo, LinkMessage,
};
use serde::{Deserialize, Serialize};
use tracing::{Level, error, info, span, warn};

use crate::cfg::{Endpoint, Link, LinksDefaults};
use crate::components::sriov::Vf;
use crate::{NetlinkHandler, NodeId};

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
    pub latency: Option<f64>,
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
    pub peer_name: Option<String>,
    //// Interface index
    pub ifindex: u32,
    pub vlan: Option<u32>,
    #[serde(default, flatten)]
    pub ctx: Option<HashMap<String, toml::Value>>,
}

impl From<&LinksDefaults> for Interface {
    fn from(dflt: &LinksDefaults) -> Self {
        let mut iface = Interface::default();
        iface.latency = dflt.latency.clone();
        iface.bandwidth = dflt.bw.clone();
        iface.mtu = dflt.mtu;
        iface.metric = dflt.metric;
        iface.ctx = dflt._additional_fields.clone();
        iface
    }
}

impl Interface {
    fn set_from_field(&mut self, name: &str, field: &toml::Value) {
        match name {
            "latency" => {
                if let Some(latency) = field.as_float() {
                    self.latency = Some(latency);
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

    pub fn new(
        dflt: &Option<LinksDefaults>,
        config: &Link,
        idx: usize,
        ifindex: u32,
        vlan: Option<u32>,
    ) -> Self {
        assert!(idx == 0 || idx == 1, "Index should be 0 or 1");

        // Expand Endpoint configuration from Defaults
        let mut iface = match dflt {
            Some(dflt) => Interface::from(dflt),
            None => Interface::default(),
        };

        let name = &config.endpoints[idx].interface;
        let node = &config.endpoints[idx].node;

        // Override default values, if any specified
        config._additional_fields.iter().for_each(|(idx, field)| {
            let idx = idx.as_str();
            if let Ok(endpoint) = Endpoint::try_from(idx)
                && &endpoint.interface == name
                && &endpoint.node == node
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
        iface.peer_name = iface.peer.as_ref().and_then(|peer| Some(peer.node.clone()));
        iface.idx = idx;
        iface.ifindex = ifindex;
        iface.vlan = vlan;

        iface
    }

    pub fn setup(
        &self,
        node: &NodeId,
        addrs: Option<&Vec<IpNetwork>>,
        net: &NetlinkHandler,
        vf: &mut Option<&mut Vf>,
    ) {
        let _span = span!(Level::INFO, "interface", name = self.name).entered();
        info!("Interface setup");

        let mut open_flags = OFlag::empty();
        open_flags.insert(OFlag::O_RDONLY);
        open_flags.insert(OFlag::O_CLOEXEC);

        if self.name == "lo" {
            // Nothing to do.
        } else if let Ok(fd1) = nix::fcntl::open(
            format!("/run/netns/{node}").as_str(),
            open_flags,
            Mode::empty(),
        ) {
            // Configure link.
            // If the peer interface is on the same node, the link is created with
            // a pair of virtual interfaces (veth).
            // If both interfaces are not on the same phynode, create a vlan.
            if let Some(vlanid) = self.vlan {
                // We need to create VLAN interfaces.
                if let Some(vf) = vf {
                    // If we have SR-IOV interfaces, create VLAN on VF.
                    if let Err(e) = vf.add_vlan_if(&self.name, vlanid as u16) {
                        error!("{e:?}");
                    }
                } else {
                    unimplemented!("Plain VLAN without HW offload not yet supported.");
                }
            } else {
                // We create a pair of veth.
                if let Some(endpoint) = &self.peer
                    && let Ok(fd2) = nix::fcntl::open(
                        format!("/run/netns/{}", endpoint.node).as_str(),
                        open_flags,
                        Mode::empty(),
                    )
                {
                    let mut msg = LinkMessage::default();
                    let mut peer_msg = LinkMessage::default();

                    // msg.attributes
                    //     .push(LinkAttribute::LinkInfo(vec![LinkInfo::Kind(
                    //         InfoKind::Veth,
                    //     )]));

                    // Both interfaces have the same MTU
                    if let Some(mtu) = self.mtu {
                        info!("Setting interfaces MTU <{mtu}>");
                        msg.attributes.push(LinkAttribute::Mtu(mtu));
                        peer_msg.attributes.push(LinkAttribute::Mtu(mtu));
                    }

                    if let Some(mac) = &self.mac {
                        info!("Setting MAC address <{:x?}>", self.mac);
                        msg.attributes.push(LinkAttribute::Address(mac.clone()));
                    }

                    // TODO: Set peer mac ?

                    // Set correct netns for both ifaces.
                    msg.attributes.push(LinkAttribute::NetNsFd(fd1));
                    msg.attributes
                        .push(LinkAttribute::IfName(self.name.clone()));
                    // msg.attributes
                    // .push(LinkAttribute::NewIfIndex(self.ifindex as i32));

                    peer_msg.attributes.push(LinkAttribute::NetNsFd(fd2));
                    peer_msg
                        .attributes
                        .push(LinkAttribute::IfName(endpoint.interface.clone()));

                    msg.attributes.push(LinkAttribute::LinkInfo(vec![
                        LinkInfo::Kind(InfoKind::Veth),
                        LinkInfo::Data(InfoData::Veth(InfoVeth::Peer(peer_msg))),
                    ]));

                    net.rt.block_on(async {
                        if let Err(e) = net.handle.link().add(msg).execute().await {
                            error!("{:?}", e);
                        }
                    });
                } else {
                    warn!("Failed to open peer netns");
                }
            }
        }

        // Finalise interfaces configuration from within netns.
        let _ = thread::scope(|s| {
            let _ = s
                .spawn(move || {
                    if let Ok(ns) = NetNs::get(node) {
                        if let Ok(_) = ns.enter() {
                            let handle = net.handle.clone();
                            net.rt.block_on(async move {
                                if let Ok(Some(msg)) = handle
                                    .link()
                                    .get()
                                    .match_name(self.name.clone())
                                    .execute()
                                    .try_next()
                                    .await
                                {
                                    let ifindex = msg.header.index;
                                    // info!("{msg:?}");

                                    // Add interface addresses, if any.
                                    if let Some(addrs) = addrs {
                                        for addr in addrs {
                                            // TODO: Build message.
                                            if let Err(e) = handle
                                                .address()
                                                .add(ifindex, addr.ip(), addr.prefix())
                                                .execute()
                                                .await
                                            {
                                                error!("{e:?}");
                                            }
                                        }
                                    }

                                    // Set interface up
                                    let mut msg = LinkMessage::default();
                                    msg.header.index = ifindex;
                                    msg.header.flags = LinkFlags::Up;
                                    if let Some(mac) = &self.mac {
                                        info!("Setting MAC address <{:x?}>", self.mac);
                                        msg.attributes.push(LinkAttribute::Address(mac.clone()));
                                    }
                                    if let Err(e) = handle.link().set(msg).execute().await {
                                        error!("{e:?}");
                                    }
                                }

                                // Configure the maximum bandwidth of the link, if specified
                                // TODO: https://docs.rs/rtnetlink/latest/rtnetlink/struct.QDiscNewRequest.html

                                // Configure the latency of the link, if specified
                                // TODO: https://docs.rs/rtnetlink/latest/rtnetlink/struct.QDiscNewRequest.html
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
                                        .arg(format!("{latency}ms"))
                                        .output();
                                }

                                //     // FIXME: Use netlink to issue all the commands below
                                //     // TODO: should only up remote end of the link
                                //     let _ = Command::new("ip")
                                //         .arg("-n")
                                //         .arg(node)
                                //         .arg("l")
                                //         .arg("set")
                                //         .arg("dev")
                                //         .arg(&self.name)
                                //         .arg("up")
                                //         .output();
                            });
                        }
                    }
                })
                .join();
        });

        // Add addresses to the interface, if specified
        // if let Some(addrs) = addrs {
        //     // let mut req =
        //     //     handle
        //     //         .address()
        //     //         .add(0, IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 24);
        //     // let msg = req.message_mut();
        //     // println!("{msg:#?}");

        //     // FIXME: Use netlink to issue all the commands below
        //     addrs.iter().for_each(|addr| {
        //         let _ = Command::new("ip")
        //             .arg("-n")
        //             .arg(node)
        //             .arg("a")
        //             .arg("add")
        //             .arg(addr.to_string())
        //             .arg("dev")
        //             .arg(&self.name)
        //             .output();
        //     });
        // }

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
        // });
    }
}
