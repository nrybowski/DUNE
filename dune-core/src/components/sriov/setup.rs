use nix::{self, sys::stat::Mode};
use std::{
    fs::{read, read_dir, write},
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    str::FromStr,
    sync::{
        Arc,
        mpsc::{Receiver, Sender, channel},
    },
};

use rtnetlink::{
    Error, Handle, new_connection,
    packet_route::link::{
        InfoData, InfoKind, InfoVlan, LinkAttribute, LinkFlags, LinkInfo, LinkMessage, LinkVfInfo,
        VfInfo, VfInfoVlan,
    },
};

use crate::NetlinkHandler;

const SYS_BASE: &str = "/sys/class/net/";

pub struct Fun {
    path: PathBuf,
}

impl Fun {
    fn new(path: PathBuf) -> Result<Self, PfError> {
        // Ensure that path in sysfs exists.
        let err = std::io::Error::new(std::io::ErrorKind::NotFound, "{path} not found.");
        let _ = path
            .try_exists()
            .map_err(|e| PfError::Io(e))?
            .ok_or(PfError::Io(err))?;

        // Ensure that path is a directory.
        let _ = path.is_dir().ok_or(PfError::SysFs(SysFs::NotDir))?;
        Ok(Self { path })
    }

    fn write(&self, subpath: &str, content: &str) -> Result<(), PfError> {
        write(&self.path.join(subpath), content).map_err(|e| PfError::Io(e))
    }

    fn read(&self, subpath: &str) -> Result<Vec<u8>, PfError> {
        let path = self.path.join(subpath);
        println!("Reading {path:?}");
        read(&path).map_err(|e| PfError::Io(e))
    }

    pub fn ifindex(&self) -> Result<u32, PfError> {
        self.read("ifindex").and_then(|ifindex| {
            let mut ifindex =
                str::from_utf8(&ifindex).map_err(|e| PfError::ParseError(e.to_string()))?;
            ifindex = &ifindex[..ifindex.len() - 1];
            u32::from_str(ifindex).map_err(|e| PfError::ParseError(e.to_string()))
        })
    }
}

pub struct Pf {
    pub name: String,
    fun: Fun,
    vfs: usize,
    rt: Arc<tokio::runtime::Runtime>,
    hdl: Arc<Handle>,
}

#[derive(Debug)]
pub enum VfError {
    Overflow,
}

#[derive(Debug)]
pub enum SysFs {
    BasePath,
    NotDir,
}

#[derive(Debug)]
pub enum PfError {
    Io(std::io::Error),
    SysFs(SysFs),
    VfError(VfError),
    ParseError(String),
    Netlink(Error),
}

impl<'a> Pf {
    pub fn new(name: &str, net: Option<&NetlinkHandler>) -> Result<Self, PfError> {
        let path = PathBuf::from_str(&format!("{SYS_BASE}{name}/"))
            .map_err(|_| PfError::SysFs(SysFs::BasePath))?;

        let fun = Fun::new(path)?;

        let (rt, hdl) = if let Some(net) = net {
            (net.rt.clone(), net.handle.clone())
        } else {
            let rt = Arc::new(tokio::runtime::Runtime::new().unwrap());
            let rt0 = rt.clone();
            let (tx, rx): (Sender<Handle>, Receiver<Handle>) = channel();
            rt.block_on(async move {
                if let Ok((conn, handle, _rx)) = new_connection() {
                    rt0.spawn(conn);
                    let _ = tx.send(handle);
                }
            });
            let hdl = Arc::new(rx.recv().unwrap());
            (rt, hdl)
        };

        Ok(Self {
            name: name.into(),
            fun,
            vfs: 0,
            rt,
            hdl,
        })
    }

    fn write_vfs(&self, content: &str) -> Result<(), PfError> {
        self.fun.write("device/sriov_numvfs", content)
    }

    pub fn reset(&self) -> Result<(), PfError> {
        self.write_vfs("0")
    }

    pub fn create_vfs(&mut self, n: usize) -> Result<(), PfError> {
        // FIXME: Sanity check on max number of vfs or let user handle PfError?
        let res = self.write_vfs(&format!("{n}"));
        if res.is_ok() {
            self.vfs = n;
        }
        res
    }

    pub fn ifindex(&self) -> Result<u32, PfError> {
        self.fun.ifindex()
    }

    pub fn get_vf(&'a self, id: usize) -> Result<Vf<'a>, PfError> {
        if id >= self.vfs {
            return Err(PfError::VfError(VfError::Overflow));
        }
        Vf::new(self, id)
    }
}

pub struct Vf<'a> {
    _id: u32,
    _dev: Fun,
    net: Fun,
    _ifname: String,
    pf: &'a Pf,
    sub: Vec<String>,
}

impl<'a> Vf<'a> {
    fn new(pf: &'a Pf, id: usize) -> Result<Self, PfError> {
        let dev_path = pf.fun.path.join(format!("device/virtfn{id}/"));
        let net_path = dev_path.join("net/");

        let dev = Fun::new(dev_path)?;

        // If we call this function too quickly after the initialization of the VFs,
        // the sysfs may not be ready yet.
        // Hence, we slightly poll the sysfs before giving up.
        let attemps = 3;
        for i in 0..attemps {
            match Fun::new(net_path.clone()) {
                Ok(_fun) => {
                    break;
                }
                Err(e) => {
                    if i == attemps - 1 {
                        // Last iteration, we failed.
                        return Err(e);
                    } else {
                        std::thread::sleep(std::time::Duration::from_secs(1));
                    }
                }
            }
        }

        // Look for interface name so that we can access its sysfs.
        let _ifname = read_dir(&net_path)
            .map_err(|e| PfError::Io(e))?
            .next()
            .ok_or(PfError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "Ifname not found.",
            )))?
            .map_err(|e| PfError::Io(e))?
            .file_name()
            .to_string_lossy()
            .to_string();
        let net = Fun::new(net_path.join(_ifname.clone()))?;

        Ok(Self {
            _id: id as u32,
            _ifname,
            _dev: dev,
            net,
            pf,
            sub: Default::default(),
        })
    }

    pub fn id(&self) -> u32 {
        self._id
    }

    pub fn ifname(&self) -> &String {
        &self._ifname
    }

    pub fn ifindex(&self) -> Result<u32, PfError> {
        self.net.ifindex()
    }

    pub fn set_vlan(&self, id: u32) {
        let mut msg = LinkMessage::default();
        msg.header.index = self.pf.ifindex().unwrap();
        msg.attributes
            .push(LinkAttribute::VfInfoList(vec![LinkVfInfo(vec![
                VfInfo::Vlan(VfInfoVlan::new(self._id, id, 0)),
            ])]));

        let vfid = self._id;
        let handle = self.pf.hdl.clone();

        self.pf.rt.block_on(async move {
            let link_handle = handle.link();
            if let Err(e) = link_handle.set(msg).execute().await {
                println!("Failed to set VLAN {id} on VF {}: {e}", vfid);
            }
        });
    }

    pub fn add_vlan_if(&mut self, ifname: &str, id: u16) -> Result<(), PfError> {
        let mut msg = LinkMessage::default();
        msg.attributes.push(LinkAttribute::Link(self.ifindex()?));
        msg.attributes.push(LinkAttribute::IfName(ifname.into()));
        msg.attributes.push(LinkAttribute::LinkInfo(vec![
            LinkInfo::Kind(InfoKind::Vlan),
            LinkInfo::Data(InfoData::Vlan(vec![
                InfoVlan::Id(id),
                // InfoVlan::Protocol(VlanProtocol::Ieee8021Q),
            ])),
        ]));

        let handle = self.pf.hdl.clone();
        let (tx, rx): (Sender<Error>, Receiver<Error>) = channel();
        self.pf.rt.block_on(async move {
            if let Err(e) = handle.link().add(msg).execute().await {
                println!("Failed to add VLAN iface: {e}");
                let _ = tx.send(e);
            }
        });
        match rx.try_recv() {
            Err(_) => {
                self.sub.push(ifname.into());
                Ok(())
            }
            Ok(e) => Err(PfError::Netlink(e)),
        }
    }

    pub fn set_netns(&self, netns: &str, name: Option<&str>) -> Result<(), PfError> {
        let path = format!("{}{netns}", rtnetlink::NETNS_PATH);

        let mut flags = nix::fcntl::OFlag::empty();
        flags.insert(nix::fcntl::OFlag::O_RDONLY);
        flags.insert(nix::fcntl::OFlag::O_CLOEXEC);
        let fd = nix::fcntl::open(Path::new(&path), flags, Mode::empty())
            .map_err(|e| PfError::ParseError(e.to_string()))?;

        let mut msg = LinkMessage::default();
        msg.header.index = self.ifindex()?;
        msg.header.flags = LinkFlags::Up;

        msg.attributes.push(LinkAttribute::NetNsFd(fd.as_raw_fd()));
        if let Some(name) = name {
            msg.attributes.push(LinkAttribute::IfName(name.into()));
        }

        let handle = self.pf.hdl.clone();
        let (tx, rx): (Sender<Error>, Receiver<Error>) = channel();
        let txc = tx.clone();
        self.pf.rt.block_on(async move {
            if let Err(e) = handle.link().set(msg).execute().await {
                println!("Failed to move VF to netns: {e:x?}");
                let _ = txc.send(e);
            }
        });
        if let Ok(e) = rx.try_recv() {
            return Err(PfError::Netlink(e));
        }

        for iface in &self.sub {
            let path = PathBuf::from_str(&format!("{SYS_BASE}{iface}/"))
                .map_err(|_| PfError::SysFs(SysFs::BasePath))?;
            let fun = Fun::new(path)?;
            let mut msg = LinkMessage::default();
            msg.header.index = fun.ifindex()?;
            msg.header.flags = LinkFlags::Up;
            msg.attributes.push(LinkAttribute::NetNsFd(fd.as_raw_fd()));
            msg.attributes.push(LinkAttribute::IfName(iface.into()));

            let handle = self.pf.hdl.clone();
            let txc = tx.clone();
            self.pf.rt.block_on(async move {
                if let Err(e) = handle.link().set(msg).execute().await {
                    println!("Failed to move VF to netns: {e:x?}");
                    let _ = txc.send(e);
                }
            });
            if let Ok(e) = rx.try_recv() {
                return Err(PfError::Netlink(e));
            }
        }

        match rx.try_recv() {
            Err(_) => Ok(()),
            Ok(e) => Err(PfError::Netlink(e)),
        }
    }
}

// fn main() -> Result<(), PfError> {
//     // let iface = "enP1p2s0f0np0";
//     let iface = "enP1p1s0f0np0";

//     let netns = String::from_str("test").unwrap();
//     let n_vfs = 2;

//     let mut pf = Pf::new(iface)?;

//     // Reset PF.
//     pf.reset()?;
//     println!("PF has been reset.");

//     // Create n_vfs VFs.
//     pf.create_vfs(n_vfs)?;
//     println!("{n_vfs} VFs have been created.");

//     // Get VF0.
//     let mut vf0 = pf.get_vf(0)?;

//     // Enable VLAN offloading on VF0 for VLAN100.
//     vf0.set_vlan(100);
//     vf0.add_vlan_if("eth0", 1)?;
//     vf0.add_vlan_if("eth1", 2)?;
//     vf0.set_netns(&netns, Some("phy0"))?;

//     // let _ = NetworkNamespace::add(netns.clone()).await;
//     // let _ = NetworkNamespace::add(netns).await;
//     Ok(())
// }
