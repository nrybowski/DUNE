use std::net::IpAddr;
use std::{collections::HashMap, path::PathBuf};

use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyString};
use serde::{Deserialize, Serialize, de::Error};
use serde_json;

use dune_core::{Dune, cfg::Phynode};
use tracing::{Level, info, span};

use tracing_appender::rolling::{self};
use tracing_subscriber::fmt::writer::MakeWriterExt;

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
        let dune = Dune::new(cfg);
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
                let interfaces = if let Some(interfaces) = &node.interfaces {
                    interfaces
                        .iter()
                        .filter_map(|(ifname, iface)| {
                            if let Some(peer) = &iface.peer {
                                Some(Interface::ExplicitInterface(ExplicitInterface {
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
                                }))
                            } else {
                                None
                            }
                        })
                        .collect::<Vec<Interface>>()
                } else {
                    vec![]
                };
                let ns = Namespace {
                    role: name.clone(),
                    namespace: name.clone(),
                    interfaces,
                };

                if let Some(phynode) = &node.phynode {
                    match namespaces.get_mut(phynode) {
                        Some(entry) => entry.push(ns),
                        None => {
                            let _ = namespaces.insert(phynode.clone(), vec![ns]);
                        }
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

impl Config {
    pub fn dump(&self) -> String {
        toml::to_string(&self).unwrap()
    }
}

// ==== Python FFI ====

#[pyclass(module = "dune_mpf")]
#[derive(Serialize, Deserialize)]
struct MpfDune(Dune);

#[pymethods]
impl MpfDune {
    fn phynodes(&self) -> Vec<String> {
        self.0.phynodes()
    }

    fn setup(&self, phynode: String) {
        let logfile = rolling::never("/tmp", "dune.log");
        let stdout = std::io::stdout.with_min_level(tracing::Level::TRACE);
        tracing_subscriber::fmt()
            .with_writer(stdout.and(logfile))
            .init();
        // tracing_subscriber::fmt().init();
        let _ = span!(Level::INFO, "mpf");
        info!("phynode <{phynode}> setup");
        self.0.phynode_setup(phynode);
    }

    fn dump(&self) {
        println!("{:#?}", self.0);
    }

    fn dumps(&self, py: Python<'_>) -> PyResult<PyObject> {
        Ok(PyString::new(py, toml::ser::to_string(&self).unwrap().as_str()).to_object(py))
    }

    // Some black-magic to make MpfDune picklable. DO NOT TOUCH.
    // See https://github.com/PyO3/pyo3/issues/100#issuecomment-2244769044
    #[staticmethod]
    pub fn deserialize(data: Vec<u8>) -> Self {
        Self(serde_json::from_slice(&data).unwrap())
    }

    pub fn __reduce__(&self, py: Python<'_>) -> PyResult<(PyObject, PyObject)> {
        py.run_bound("import dune_mpf", None, None).unwrap();
        let cls = py
            .eval_bound("dune_mpf.MpfDune.deserialize", None, None)
            .unwrap();
        let data = PyBytes::new(py, &serde_json::to_vec(&self).unwrap()).to_object(py);
        Ok((cls.to_object(py), (data,).to_object(py)))
    }
}

#[pyclass]
struct MpfConfig(Config);

#[pymethods]
impl MpfConfig {
    fn dump(&self) -> String {
        self.0.dump()
    }
}

#[pyfunction]
fn load(cfg: PathBuf) -> (MpfDune, MpfConfig) {
    let dune = Dune::init(&cfg);
    let config = Config::try_from(&dune).unwrap();
    (MpfDune(dune), MpfConfig(config))
}

#[pymodule]
fn dune_mpf(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(load, m)?)?;
    m.add_class::<MpfDune>()?;
    m.add_class::<MpfConfig>()?;
    Ok(())
}
