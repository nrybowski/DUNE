from infrastructure import Infra
from topology import Topo

class Dune:
    
    def __init__(self, path: str):

        # TODO: do not load twice if all config in a single file.
        self.topo = Topo(path)
        self.infra = Infra(path)

        if self.topo._total_cores > self.infra._total_cores:
            print('Specified infrastructure has not enough cores to allocate each process.')
            exit(1)
            
    def allocate(self):
        # TODO: clever way with buckets and CP
        # Current way is dumb, we fill each phynode in the order of nodes

        for nid, data in self.topo.nodes(data=True):
            node = data['cfg']
            print(nid, node)

    def build(self):
        pass


if __name__ == '__main__':
    path = 'house.yml'
    dune = Dune(path)
    print(dune.infra._cores)
    dune.allocate()
