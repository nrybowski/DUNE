from copy import deepcopy
from enum import StrEnum
import socket
import os

from jinja2 import Template, meta, Environment
from jinja2.nodes import Template as NodeTemplate
import docker

from infrastructure import Infra
from topology import Topo, Pinned


class ConfigSection(StrEnum):
    Pre = 'PreSetup'
    Nodes = 'Nodes'
    Links = 'Links'
    Post = 'PostSetup'
    Processes = 'Processes'
    PreDown = 'PreDown'
    Down = 'Down'
    
def lo_from_id(nid: int) -> str:
    # TODO: make prefix configurable
    lo = bytes.fromhex(hex(((0xfc00 << 48) + (1 << 32) + (nid << 16)) << 64)[2:])
    lo = socket.inet_ntop(socket.AF_INET6, lo)
    return lo

class Dune:

    def __init__(self, path: str):

        # TODO: do not load twice if all config in a single file.
        self.topo = Topo(path)
        self.infra = Infra(path)
        self._allocation = None
        self._configs = {}
        self._docker = None

        if self.topo._total_cores > self.infra._total_cores:
            print('Specified infrastructure has not enough cores to allocate each process.')
            exit(1)
       
    def allocate(self) -> dict:
        # TODO: clever way with buckets and CP, fill gaps if any

        if self._allocation is not None: return self._allocation

        self._allocation = {}

        available = deepcopy(self.infra._cores)

        for node, count in sorted(
                {nid: node['cfg']._get_n_cores() for nid, node in self.topo.nodes(data=True)}.items(),
                key=lambda item: item[1],
                reverse=True):

            alloc = []
            phynode0 = None
            for process in self.topo.nodes[node]['cfg']._get_cores():
                b = False
                for phynode, cores in available.items():
                    if b: break
                    for numa in cores:
                        # TODO: use list:
                        if type(numa) != list: continue
                        if len(numa) >= count:
                            alloc.append([numa.pop() for _ in process])
                            b = True
                            phynode0 = phynode
                            break
            self._allocation[node] = (phynode0, alloc)

        return self._allocation

    def _node_to_phynode(self, nid: str) -> str:

        """ Return the corresponding phynode for a given node id.
            @param[in]   nid The node ID for which we ask the corresponding phynode.
            @return      The corresponding phynode ID.
        """
        
        try:
            ret = self._allocation[nid][0]
        # TODO: catch exception if nid not in allocation matrix even if allocated
        except TypeError:
            self.allocate()
            ret = self._allocation[nid][0]
        return ret

    def _phynode_exec(self, pid: str, section: ConfigSection, cmd: str):
        
        """ Run an arbitrary command on a given phynode.
            @param[in]  pid         The target phynode.
            @param[in]  cmd         The command to run on the phynode whose ID is @p phynode_id.
            @post                   The command has been successfully added to the XML output file.
            @todo                   Check the post-condition.
        """
        try:
            self._configs[pid][section].append(cmd)
        except KeyError:
            try:
                self._configs[pid][section] = [cmd]
            except KeyError:
                self._configs[pid] = {section: [cmd]}

    def _node_exec(self, nid: str, section: ConfigSection, cmd: str, environ: dict = None):
        """ Execute @p cmd in the netns corresponding to the node @p node_id 
            @param      nid The ID of the node on which @p cmd has to be executed.
            @param      cmd     The command to execute on node @p node_id.
            @post                   The command has been successfully added to the XML output file.
            @todo                   Check the post-condition.
        """
        phynode = self._node_to_phynode(nid)
        environ = '' if environ is None else ' '.join([f'{k}={v}' for k,v in environ.items()]) + ' '
        self._phynode_exec(phynode, section, f'{environ}ip netns exec {nid} bash -c "{cmd}"')

    def _node_pinned(self, nid: str, pinned: Pinned, idx: int):
        _, cores = self._allocation[nid]
        cores = cores[idx]
        env = Environment()
        ast = env.parse(pinned.cmd)
        cores = {k: cores[idx] for idx,(k, v) in enumerate(pinned._get_cores().items())}
        environ = None if pinned.environ is None else {k: Template(v).render(**cores) for k, v in pinned.environ.items()}

        renv = {'node': nid, **cores}
        cmd = Template(pinned.cmd).render(renv)
            
        self._node_exec(nid, ConfigSection.Processes, f'taskset -c {cores["core_0"]} {cmd}', environ)

        """ Add down instruction. """
        if pinned.down is not None:
            self._node_exec(nid, ConfigSection.Down, Template(pinned.down).render(renv))

        # TODO: add PreDown

            
    def _ip(self, section: ConfigSection, cmd: str, nid: str = None):
        """ Run an ip-based command on a phynode.
            @param[in]  cmd     The ip subcommand to run.
            @param[in]  node_id The ID of the phynode on which the command has to be executed.
            @post                   The command has been successfully added to the XML output file.
            @todo                   Check the post-condition.
        """
        phynode = self._node_to_phynode(nid)
        nid = f'-n {nid} ' if nid is not None else ''
        self._phynode_exec(phynode, section, f'ip {nid}{cmd}')

    def _sysctl(self, nid: str, section: ConfigSection, sysctl: str, value: str):
        # TODO: check if sysctl is valid
        # TODO: check if value is valid
        phynode = self._node_to_phynode(nid)
        self._node_exec(nid, section, f'sysctl -w {sysctl}={value}')

    def _get_node_id(self, nid: str) -> int:
        # TODO: handle errors
        return list(self.topo.nodes).index(nid)

    def _add_node(self, nid: str):

        """Add a node with @p node_id to the topology.
            @param      node_id The ID of the node to add.
            @post               A node with @p node_id ID has been added to the corresponding
                                physical host. Its loopback interface has been configured with a
                                correct link-local IPv6 address to quick fix the BIRD IPv6 bug and 
                                a ULA in the range fc00:1::/64.
            @return     the config of the node added to the topology
        """

        section = ConfigSection.Nodes
        phynode = self._node_to_phynode(nid)
        node = self.topo.nodes[nid]['cfg']
        
        """ Add a netns with ID @p nid on the corresponding phynode """
        self._phynode_exec(phynode, section, f'ip netns add {nid}')

        """ Set 'lo' addresses if specified or required. """
        lo = node._addresses.get('lo')
        if lo is not None:
            for address in lo:
                self._ip(section, f'a add {address} dev lo', nid)

        # TODO: Check if auto-generation is requested with prefixes
        """ Generate the ULA based on the node ID @p node_id """
        idx = list(self.topo.nodes).index(nid)
        gen_lo = lo_from_id(idx)
        if lo is not None:
            lo.append(gen_lo)
        else:
            node._addresses['lo'] = [gen_lo]
        self._ip(section, f'a add {gen_lo} dev lo', nid)

        self._ip(section, 'l set dev lo up', nid)

        """ Apply execs if any. """
        if node.execs is not None:
            for cmd in node.execs:
                self._node_exec(nid, section, cmd)

        """ Apply sysctls, if any. """
        if node.sysctls is not None:
            for sysctl, value in node.sysctls.items():
                self._sysctl(nid, section, sysctl, value)

        """ Apply pinned processes, if any. """
        if node.pinned is not None:
            for idx, process in enumerate(node.pinned):
                self._node_pinned(nid, process, idx)

        """ Generate files specified by templates, if any. """
        # if node.templates is not None:
        #     for template, data in node.templates.items():
        #         self._generate_template(template, data)

    def _get_builder(self, builder: str):

        builder_cfg = self.infra.builders.get(builder)

        if builder_cfg is None:
            print(f'Builder <{builder}> not defined.')
            exit(1)

        # TODO: sanity check on builder_cfg
        
        if self._docker is None: self._docker = docker.from_env()
            
        img = self._docker.images.list(name=builder, filters={'label': ['dune.builder']})
        path = os.path.join(os.getcwd(), builder_cfg['context'])
        return img[0] if len(img) > 0 else self._docker.images.build(
            path=path,
            dockerfile=builder_cfg['containerfile'],
            tag=builder,
            labels={'dune.builder': '1'}
        )
        
    def _generate_template(self, template: str, data: dict):
        
        builder = data.get('builder')
        if builder is None:
            print(f'Builder not specified for template <{template}>')
            exit(1)

        builder = self._get_builder(builder)
        src_path = f'/data/{data["src"]}'        
        volumes = {
            data['src']: {
                'bind': src_path,
                'mode': 'ro'
            },
        }
        self._docker.containers.run(builder, volumes=volumes)

        
    def _add_link(self, head: str, tail: str, ifaces: tuple[str, str], data: dict):

        section = ConfigSection.Links
        
        head_phynode = self._node_to_phynode(head)
        tail_phynode = self._node_to_phynode(tail)
        head_iface, tail_iface = ifaces
        print(data)
        
        if head_phynode == tail_phynode:
            
            """ Both ends of the link lie on the same phynode, link is a veth pair. """
            self._ip(section, f'l add dev {head_iface} type veth peer name {tail_iface} netns {tail}', head)

            # TODO: allocate ips if any

            # TODO: set link attributes if any
            if 'latency' in data:
                # TODO
                pass

            mtu = data.get('mtu')
            if mtu is not None:
                self._ip(section, f'l set dev {head_iface} mtu {mtu}', head)
                self._ip(section, f'l set dev {tail_iface} mtu {mtu}', tail)
            
            self._ip(section, f'l set dev {head_iface} up', head)
            self._ip(section, f'l set dev {tail_iface} up', tail)
            
        else:

            """ Both nodes are on separate phynodes, we create vlan-defined links """
            head_idx = self._get_node_idx(head)
            tail_idx = self._get_node_idx(tail)
            vlan_id = f'0x{head_idx :02x}{tail_idx :02x}'
            print(vlan_id)


    def _add_setup(self, section: ConfigSection):
        if section not in [ConfigSection.Pre, ConfigSection.Post]: return
        setup = self.infra.pre if section == ConfigSection.Pre else self.infra.post

        if setup is not None:
            phynodes = self._configs.keys()
            for cmd in setup:
                k,v = list(cmd.items())[0]

                # TODO: support script loading
                if k == "inline": cmd = v
                else: continue

                for phynode in phynodes:
                    try:
                        self._configs[phynode][section].append(cmd)
                    except KeyError:
                        self._configs[phynode][section] = [cmd]

    def build(self):

        """ Nodes and Processes hook. """
        for nid in self.topo.nodes(): self._add_node(nid)

        """ Add links. """
        iface_set = {}
        for head, tail, ifaces, link in self.topo.edges(keys=True, data=True):
            if head not in iface_set: iface_set[head] = []
            if tail not in iface_set: iface_set[tail] = []
            if ifaces[0] not in iface_set[head] and ifaces[1] not in iface_set[tail]:
                self._add_link(head, tail, ifaces, link)
                try:
                    iface_set[head].append(ifaces[0])
                except KeyError:
                    ifaces_set[head] = [ifaces[0]]

                try:
                    iface_set[tail].append(ifaces[1])
                except KeyError:
                    ifaces_set[tail] = [ifaces[1]]
                    
        """ Pre-setup hook. """
        self._add_setup(ConfigSection.Pre)
                
        """ Post-setup hook. """
        self._add_setup(ConfigSection.Post)

    def dump(self, format: str = 'text'):
        import os

        dir = '.dune'
        cwd = os.getcwd()
        base = os.path.join(cwd, dir)
        if not os.path.exists(base): os.mkdir(dir)

        for phynode, config in self._configs.items():
            print(phynode, config)
            with open(os.path.join(base, phynode), 'w') as fd:
                
                if format == 'text':
                    for k, v in [
                        ('# PreSetup', ConfigSection.Pre),
                        ('# Nodes', ConfigSection.Nodes),
                        ('# Links', ConfigSection.Links),
                        ('# PostSetup', ConfigSection.Post),
                        ('# Processes', ConfigSection.Processes)
                    ]:
                        fd.write(f'{k}\n')
                        for cmd in config[v]: fd.write(f'{cmd}\n')

                elif format == 'json':
                    import json
                    json.dump(config, fd)


if __name__ == '__main__':
    # path = 'house.yml'
    path = 'abilene.yml'
    dune = Dune(path)
    dune.build()
    # dune.dump()
    dune.dump(format='json')
    
    # for phynode, cfg in dune._configs.items():
    #     for section, data in cfg.items():
    #         print(phynode, section, '\n', data)

    # for nid, node in dune.topo.nodes(data=True):
    #     print(nid, node['cfg']._addresses)
