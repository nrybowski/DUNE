use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::net::IpAddr;
use std::str;
use std::str::FromStr;
use std::vec::Vec;

use minijinja::Environment;
use regex::Regex;

#[derive(Serialize, Deserialize, Debug)]
pub struct Phynode {
    pub cores: Vec<Vec<u32>>,
    pub iface: String,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Phynodes {
    pub nodes: HashMap<String, Phynode>,
}

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

pub type Cores = HashMap<String, u64>;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Pinned {
    pub cmd: String,
    pub environ: Option<HashMap<String, String>>,
    pub down: Option<String>,
    pub pre_down: Option<Vec<String>>,
    #[serde(default)]
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
            environ.iter().for_each(|(var, value)| {
                let tmpl = env.template_from_str(value).unwrap();
                for value in tmpl.undeclared_variables(true) {
                    if let Some(m) = re.find(&value) {
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

pub type Sysctl = HashMap<String, String>;
pub type Templates = HashMap<String, String>;
pub type Exec = Vec<String>;

#[derive(Serialize, Deserialize, Debug)]
pub struct NodesDefaults {
    pub sysctls: Option<Sysctl>,
    pub templates: Option<Templates>,
    pub exec: Option<Exec>,
    pub pinned: Option<Vec<Pinned>>,
    #[serde(default)]
    #[serde(flatten)]
    _additional_fields_: Option<HashMap<String, toml::Value>>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct LinksDefaults {
    pub latency: String,
    pub metric: u64,
    pub mtu: u64,
    pub bw: String,
    #[serde(default)]
    #[serde(flatten)]
    _additional_fields_: Option<HashMap<String, toml::Value>>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Defaults {
    pub links: Option<LinksDefaults>,
    pub nodes: Option<NodesDefaults>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Node {
    pub addrs: Option<HashMap<String, Vec<IpAddr>>>,
    #[serde(default)]
    #[serde(flatten)]
    _additional_fields_: Option<HashMap<String, toml::Value>>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Link {
    pub endpoints: [String; 2],
    #[serde(default)]
    #[serde(flatten)]
    _additional_fields_: Option<HashMap<String, toml::Value>>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Topology {
    pub defaults: Defaults,
    pub nodes: HashMap<String, Node>,
    pub links: Vec<Link>,
}
