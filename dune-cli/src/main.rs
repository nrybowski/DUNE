use std::path::PathBuf;

use clap::Parser;

use dune_core::Dune;
use dune_mpf::Config as MpfConfig;

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    #[arg(short, long, value_name = "CFG")]
    cfg: PathBuf,
    #[arg(short, long, value_name = "NTF")]
    ntf: Option<PathBuf>,
}

fn main() {
    let cli = Cli::parse();
    // TODO: verify path
    let dune = Dune::new(&cli.cfg);

    let cfg = MpfConfig::try_from(&dune).unwrap();
    println!("{:#?}", cfg);
    println!("{:#?}", dune.infra);
    println!("{:#?}", dune.nodes);
}
