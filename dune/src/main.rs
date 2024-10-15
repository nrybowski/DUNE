use dune::cfg::Config;
use dune::cfg::Cores;
use dune::Node;

use graphrs::{self, Graph, GraphSpecs};

fn main() {
    let cfg = Config::new("src/test.toml");

    println!("{:#?}", cfg);

    // TODO: unpack defaults
    let node_dflt = cfg.topology.defaults;

    let mut graph = Graph::<String, Node>::new(GraphSpecs::directed());
    for (name, config) in cfg.topology.nodes {
        let mut node = Node {
            sysctls: None,
            pinned: None,
            cores: Cores::new(),
            execs: None,
            templates: None,
        };
        println!("{} {:#?}", name, node);
        let node = graphrs::Node {
            name,
            attributes: Some(node),
        };
        graph.add_node(node);
        // println!("{:#?} {:#?} {:#?}", node, config, node_dflt);
    }

    // if let Some(pinned) = cfg.topology.defaults.nodes.pinned {
    // for mut process in pinned {
    // println!("{:#?}", process.n_cores());
    // }
    // }
}
