#![feature(let_chains)]

use std::net::IpAddr;
use std::{collections::HashMap, path::PathBuf};

use pyo3::prelude::*;
use serde::{de::Error, Deserialize, Serialize};

use dune_core::{cfg::Phynode, Dune};

// ==== Interface ====

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub enum Interface {
    SimpleInterface(SimpleInterface),
    ExplicitInterface(ExplicitInterface),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SimpleInterface {
    pub name: String,
    pub ip: IpAddr,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ExplicitInterface {
    pub link: String,
    pub direction: Direction,
    pub name: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Forward,
    Backward,
}

// ==== Namespace ====

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Namespace {
    pub role: String,
    pub namespace: String,
    pub interfaces: Vec<Interface>,
}

// ==== Machine ====

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Machine {
    pub hostname: Option<String>,
    pub user: String,
    pub role: String,
    pub interfaces: Vec<Interface>,
    pub namespaces: Option<Vec<Namespace>>,
}

impl TryFrom<&Phynode> for Machine {
    type Error = toml::de::Error;
    fn try_from(phynode: &Phynode) -> Result<Self, Self::Error> {
        match phynode._additional_fields.as_ref().unwrap().get("mpf") {
            Some(cfg) => cfg.clone().try_into(),
            None => Err(Error::missing_field("mpf")),
        }
    }
}

// ==== Global ====

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Global {
    pub python_path: String,
}

// ==== Controller ====

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Controller {
    pub ports: String,
    pub control_ip: Option<IpAddr>,
    pub hostname: String,
    pub user: String,
    pub role: String,
    pub interfaces: Vec<Interface>,
}

// ==== Config ====

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Config {
    pub global: Global,
    pub machines: Option<Vec<Machine>>,
    pub controller: Controller,
}

impl TryFrom<&PathBuf> for Config {
    type Error = ();
    fn try_from(cfg: &PathBuf) -> Result<Self, Self::Error> {
        let mut dune = Dune::new(cfg);
        dune.allocate();
        Self::try_from(&dune).map_err(|_err| ())
    }
}

impl TryFrom<&Dune> for Config {
    type Error = toml::de::Error;
    fn try_from(dune: &Dune) -> Result<Self, Self::Error> {
        if let Some(additional) = &dune.infra._additional_fields
            && let Some(mpf) = additional.get("mpf")
        {
            let mut cfg: Config = mpf.clone().try_into().unwrap();

            // Collect namespaces for each Phynode
            let mut namespaces: HashMap<String, Vec<Namespace>> = HashMap::new();
            dune.nodes.iter().for_each(|(name, node)| {
                let ns = Namespace {
                    role: name.clone(),
                    namespace: name.clone(),
                    interfaces: node
                        .interfaces
                        .iter()
                        .map(|(ifname, iface)| {
                            let peer = iface.peer.as_ref().unwrap();
                            Interface::ExplicitInterface(ExplicitInterface {
                                link: format!(
                                    "{}:{}-{}:{}",
                                    name.clone(),
                                    ifname,
                                    peer.node,
                                    peer.interface
                                ),
                                direction: if iface.idx == 0 {
                                    Direction::Forward
                                } else {
                                    Direction::Backward
                                },
                                name: ifname.clone(),
                            })
                        })
                        .collect(),
                };

                match namespaces.get_mut(&node.phynode) {
                    Some(entry) => entry.push(ns),
                    None => {
                        let _ = namespaces.insert(node.phynode.clone(), vec![ns]);
                    }
                }
            });

            // Collect Phynodes informations
            cfg.machines = Some(
                dune.infra
                    .nodes
                    .iter()
                    .map(|(name, phynode)| {
                        let mut m = Machine::try_from(phynode).unwrap();
                        m.hostname = Some(name.clone());
                        m.namespaces = namespaces.get(name).cloned();
                        m
                    })
                    .collect::<Vec<Machine>>(),
            );

            Ok(cfg)
        } else {
            Err(Error::missing_field("mpf"))
        }
    }
}

// ==== Python FFI ====

#[pyfunction]
fn load_mpf_cfg(cfg: PathBuf) -> String {
    let cfg = Config::try_from(&cfg).unwrap();
    toml::to_string(&cfg).unwrap()
}

// #[pyfunction]
#[pymodule]
fn dune_mpf(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(load_mpf_cfg, m)?)?;
    Ok(())
}
