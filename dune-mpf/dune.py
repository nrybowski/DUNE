import ipyparallel

import pytoml
import yaml

import dune_mpf
import mpf


def init(path: str):
    """ Load DUNE context and configure mpf """

    global dune

    (dune, cfg) = dune_mpf.load("../test.toml")
    cfg = yaml.dump(pytoml.loads(cfg.dump()))
    with open("/dev/shm/mpf_cfg.yml", "r+") as f:
        f.write(cfg)
        f.seek(0)
        mpf.setup(f)
    setup()

def setup():
    """ Deploy synchronously the experimental setup. """
    
    global dune

    # TODO: Send dune_mpf wheel and install it in venv

    # Import DUNE module on every phynode
    with mpf.client[:].sync_imports(quiet=False):
        import dune_mpf

    # Send DUNE context to every phynode
    mpf.client[:].push({'dune': dune})

    @ipyparallel.interactive
    def _phynode_init(phynode):
        dune.setup(phynode)
  
    for phynode in dune.phynodes():
        print(f"Installing phynode <{phynode}>")
        mpf.client[mpf.roles[phynode].machine_id].apply_sync(_phynode_init, phynode)
