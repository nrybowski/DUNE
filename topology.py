from copy import deepcopy
from sys import argv
import yaml

from jinja2 import Environment, meta, Template

import networkx

RESERVED_KEYS = ['pinned', 'sysctls', 'exec', 'templates', 'addrs']

def is_var(token: str) -> str:
    return None if len(token) <=3 or token[0] != '$' or token[1] != '{' or token[-1] != '}' else token[2:-1]

class Pinned:
    """
    Pinned process representation.
    A pinned process may require multiple cores, e.g., if the process spawns sub-processes.
    """
    def __init__(self, cmd: str, environ: dict = None, pre_down: list = None, down: str = None):
        self.cmd = cmd
        """ The shell command to be pinned. """
        self.environ = environ
        """ Optionnal environment variables required by the pinned process. """
        self._cores = {}
        """ IDs of cores required by the process """
        self.pre_down = pre_down
        """ List of instructions to launch before stopping the current process """
        self.down = down
        """ One line instruction to launch to stop the current process """

    def __str__(self):
        return f"cmd <{self.cmd}>\nenviron <{self.environ}>"

    def from_dict(cfg: dict):
        cmd = cfg.get('cmd')
        if cmd is None:
            print("Malformed pinned: 'cmd' not found")
            return None
        return Pinned(cmd, environ=cfg.get('environ'), pre_down=cfg.get('pre_down'), down=cfg.get('down'))
        
    def _get_cores(self) -> list:
        """ Lazyly collect cores list required for the current process """

        if len(self._cores) == 0: 
            self._cores['core_0'] = 0
            if self.environ is not None:
                for var, value in self.environ.items():
                    env = Environment()
                    ast = env.parse(value)
                    for value in meta.find_undeclared_variables(ast):
                        if len(value) > 5 and value[0:5] == 'core_':
                            core = int(value[5:])
                            if core not in self._cores:
                                self._cores[value] = core
                        
        return self._cores

    def _get_n_cores(self) -> int:

        """ Lazyly get the number of cores required for the current process """ 
        return len(self._get_cores())
                            

class Node:
    """ Represent an emulated node configuration. """

    def __init__(self, pinned:list=None, addrs:dict=None, sysctls:dict=None, execs:list=None, templates:dict=None, env:dict=None):
        
        # TODO: use classical constructor instead ?
        self.pinned = None if pinned is None else list(filter(lambda x: x is not None, [Pinned.from_dict(entry) for entry in pinned]))
        """ List of pinned processes, if any, for the current Node. """
        
        self._cores = []
        """ List of core IDs required for each pinned process, if any. """
        
        self.sysctls = sysctls
        """ List of sysctls, if any, to apply upon node initialization. """
        
        self._addresses = addrs
        """ 
        Dict of addresses in the node. They can be auto-generated or hardcoded.
        The key is the interface and the value a list of addresses regardless of the family.
        """
        
        self.execs = execs
        """ List of one-shot commands, if any, to launch upon node startup. """
        
        self.templates = templates
        """ Dict of templates to generate, if any. """

        self.env = env
        """ Dict with additional user-defied data"""

    def __str__(self):
        ret = f"pinned:\n"
        for pinned in self.pinned:
            ret += f"{pinned}\n\n"

        return ret

    def from_cfg(cfg: dict):
        
        # TODO: check sysctls syntax

        templates = cfg.get('templates')
        if templates is not None:
            templates = {k: {'dst': v, 'content': None} for k, v in templates.items()}
        env = {k: v for k, v in cfg.items() if k not in RESERVED_KEYS}

        return Node(
            pinned = cfg.get('pinned'),
            sysctls = cfg.get('sysctls'),
            execs = cfg.get('exec'),
            addrs = cfg.get('addrs'),
            templates = templates,
            env = env
        )

    def _get_cores(self) -> list:
        if len(self._cores) == 0:
            for pinned in self.pinned:
                self._cores.append(pinned._get_cores())
        return self._cores

    def _get_n_cores(self) -> int:
        counter = 0
        for cores in self._get_cores():
            counter += len(cores)
        return counter

class Topo(networkx.MultiDiGraph):

    def __init__(self, path: str):
        super().__init__()
        self._total_cores = 0
        self._load_topo(path)

    def _load_topo(self, path: str):

        # TODO: check that path exists

        with open(path) as fd:
            cfg = yaml.load(fd, yaml.Loader)

        """ Check that mandatory sections are present. """
        try:
            topo = cfg['topology']
        except KeyError:
            print('No topology found in the configuration')
            exit(1)

        if 'links' not in topo:
            print("No links found")
            exit(1)
    
        if 'nodes' not in topo:
            print("No nodes found")
            exit(1)

        """ Get defaults, if any. """
        links_defaults = None
        nodes_defaults = None
        if 'defaults' in topo:
            links_defaults = topo['defaults'].get('links')
            nodes_defaults = topo['defaults'].get('nodes')
        
        """ Parse mandatory sections. """
        if self._parse_links(topo['links'], links_defaults) != 0: exit(1)
        if self._parse_nodes(topo['nodes'], nodes_defaults) != 0: exit(1)
    
    def _parse_links(self, links: list, defaults: dict = None) -> int:

        def parse_endpoint(v: str) -> tuple[str, str]:
            return v.split(':')

        for link in links:
            try:
                endpoints = link['endpoints']
                if len(endpoints) != 2:
                    print('Unexpected number of entries in endpoint')
                    return 1
                head, tail = endpoints
                head_node, head_iface = parse_endpoint(head)
                tail_node, tail_iface = parse_endpoint(tail)
                del link['endpoints']

                for def_key, def_val in defaults.items():
                    if def_key not in link:
                        link[def_key] = def_val

                self.add_edge(head_node, tail_node, key=(head_iface, tail_iface), **link)
                self.add_edge(tail_node, head_node, key=(tail_iface, head_iface), **link)
          
            except KeyError:
                print('No endpoint defined in link')
                return 1

        return 0
        
    def _parse_nodes(self, nodes: dict, defaults: dict = None) -> int:
        
        for node, config in nodes.items():
            """ For each node, expand config from defaults, if any. """
            node_cfg = deepcopy(defaults) if defaults is not None else {}
            
            if config is not None:
                for key, value in config.items():
                    if key in node_cfg:
                        t = type(node_cfg[key])
                        if t == dict:
                            node_cfg[key].update(config[key])                        
                        elif t == list:
                            node_cfg[key].extend(config[key])
                        else:
                            node_cfg[key] = config[key]
                    else:
                        node_cfg[key] = config[key]
        

            n = Node.from_cfg(node_cfg)
            self._total_cores += n._get_n_cores()
            self.add_node(node, cfg=n)

        return 0

if __name__ == "__main__":
    topo = Topo("house.yml")
    print(topo._total_cores)
