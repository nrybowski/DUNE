import yaml

class Infra:

    def __init__(self, path: str):
        with open(path, 'r') as fd:
            cfg = yaml.load(fd, yaml.Loader) 

        """ Get mandatory sections. """
        infra = cfg.get('infrastructure')
        if infra is None:
            print('\'infrastructure\' section not found in config.')
            exit(1)

        nodes = infra.get('nodes')
        if nodes is None:
            print('\'nodes\' section not found in infrastructure.')
            exit(1)

        self._total_cores = 0
        self._cores = {}

        """ Parse mandatory sections. """
        if self._load_nodes(nodes) != 0: exit(1)

        self.pre = None
        """ List of commands to launch at phynode initialization. """
        self.post = None
        """ List of commands to launch after phynode initialization. """

        """ Get setup commands, if any. """
        setup = infra.get('setup')
        if setup is not None:
            self.pre = setup.get('pre')
            self.post = setup.get('post')

        self.builders = infra.get('builders')
        """ Build environments. """

    def _load_nodes(self, nodes: dict) -> int:

        if len(nodes.keys()) == 0:
            print('Infrastructure should contain at least one node.')
            return 1
        
        for node, cfg in nodes.items():

            """ Sanity checks. """
            cores = cfg.get('cores')
            if cores is None:
                print('\'cores\' not found in node cfg')
                return 1

            if node in self._cores:
                print(f'node <{node}> redifined.')
                return 1
            
            """ Collect cores available in specified infrastructure. """
            t = type(cores)
            
            if t == int:
                self._cores[node] = list(range(1, cores))
                self._total_cores += cores
            elif t == list:
                self._cores[node] = cores
                self._total_cores += len([c for l in cores for c in l])
            else:
                print('\'cores\' should be either an integer or a list of list of integers.')
                return 1
                
        return 0
    
if __name__ == '__main__':
    infra = Infra('house.yml')
    print(infra._cores)
    print(infra._total_cores)
