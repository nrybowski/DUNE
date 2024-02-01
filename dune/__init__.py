from copy import deepcopy
from enum import StrEnum
from re import match, sub
import socket
import yaml
import sys
import os

from jinja2 import Template, meta, Environment, FileSystemLoader
from jinja2.nodes import Template as NodeTemplate
import docker

from dune.infrastructure import Infra
from dune.topology import Topo, Pinned


class ConfigSection(StrEnum):
    Pre = 'PreSetup'
    Nodes = 'Nodes'
    Links = 'Links'
    Post = 'PostSetup'
    Processes = 'Processes'
    PreDown = 'PreDown'
    Down = 'Down'

# def lo_from_id(nid: int) -> str:
#     # TODO: make prefix configurable
#     TODO: move in plugin
#     lo = bytes.fromhex(hex(((0xfc00 << 48) + (1 << 32) + (nid << 16)) << 64)[2:])
#     lo = socket.inet_ntop(socket.AF_INET6, lo)
#     return lo


_keywords = ['fun']
_expr = '|'.join([f'^%{k} ' for k in _keywords])

def _expand_env(plugins: dict, env: dict):

    def _apply(entry: str):
        """ Remove keyword form entry """
        if m := match(f'({_expr})', entry):
            raw_keyword = m.group(0)
            entry = sub(raw_keyword, "", entry)
            keyword = raw_keyword[1:-1]

            """ Apply keyword function on entry """
            if keyword == 'fun':
                entry = eval(entry, plugins, {})

        return entry

    return {_apply(k): _expand_env(plugins, v) if isinstance(v, dict) else _apply(v) for k, v in env.items()}

class Dune:

    def __init__(self, base: str, topo: str):

        path = os.path.join(base, topo)
        self.base = base
        self.name = topo

        self._plugins = {}
        self._load_plugins()

        # TODO: do not load twice if all config in a single file.
        self.topo = Topo(path)
        self.infra = Infra(path)
        self._allocation = None
        self._configs = {}
        self._docker = None

        if self.topo._total_cores > self.infra._total_cores:
            print('Specified infrastructure has not enough cores to allocate each process.')
            exit(1)

    def _load_plugins(self):
        plugins_dir = os.path.join(self.base, 'plugins')
        if not os.path.isdir(plugins_dir): return
        import importlib
        import pkgutil
        sys.path.append(plugins_dir)
        for _, name, _ in pkgutil.iter_modules([plugins_dir]):
            module = importlib.import_module(name)
            self._plugins[name] = module

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
        node_idx = list(self.topo.nodes).index(nid)

        """ Add a netns with ID @p nid on the corresponding phynode """
        self._phynode_exec(phynode, section, f'ip netns add {nid}')

        """ Set 'lo' addresses if specified or required. """
        lo = node._addresses.get('lo')
        if lo is not None:
            for address in lo:
                self._ip(section, f'a add {address} dev lo', nid)

        # # TODO: Check if auto-generation is requested with prefixes
        # TODO: move this in a plugin
        # """ Generate the ULA based on the node ID @p node_id """
        # idx = list(self.topo.nodes).index(nid)
        # gen_lo = lo_from_id(idx)
        # if lo is not None:
        #     lo.append(gen_lo)
        # else:
        #     node._addresses['lo'] = [gen_lo]
        # self._ip(section, f'a add {gen_lo} dev lo', nid)

        self._ip(section, 'l set dev lo up', nid)

        """ Apply execs if any. """
        if node.execs is not None:
            for cmd in node.execs:
                self._node_exec(nid, section, Template(cmd).render(dict(node=nid, **node.env)))

        """ Apply sysctls, if any. """
        if node.sysctls is not None:
            for sysctl, value in node.sysctls.items():
                self._sysctl(nid, section, sysctl, value)

        """ Apply pinned processes, if any. """
        if node.pinned is not None:
            for idx, process in enumerate(node.pinned):
                self._node_pinned(nid, process, idx)

        """ Generate files specified by templates, if any. """
        if node.templates is not None:

            """ First expansion of the node environment. """
            nenv = yaml.safe_load(Template(str(node.env)).render(dict(node=nid, addrs=node._addresses)))

            """ Apply plugin function calls, if any. """
            nenv = _expand_env(self._plugins, nenv)

            for template, data in node.templates.items():
                ifaces = {iface: dict(peer=peer, **data) for (_, peer, (iface, _), data) in self.topo.edges(nid, data=True, keys=True)}
                renv = {
                    'rid': node.env['rid'] if 'rid' in node.env else socket.inet_ntoa(socket.inet_aton(str(node_idx+1))),
                    'ifaces': ifaces,
                    'node': nid,
                }
                renv.update(nenv)

                """ Template rendering with final expanded environment. """
                data['content'] = self._generate_template(template, renv)
                data['dst'] = Template(data['dst']).render({'node': nid})


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

    def _generate_template(self, template: str, data: dict) -> str:
        env = Environment(loader=FileSystemLoader(os.path.join(self.base, 'templates'), followlinks=True))
        return env.get_template(template).render(data)

        # with open(os.path.join(self.base, template), 'r') as fd:
            # template = fd.read()
        # return Template(template).render(data)


        # builder = data.get('builder')
        # if builder is None:
        #     print(f'Builder not specified for template <{template}>')
        #     exit(1)

        # builder = self._get_builder(builder)
        # src_path = f'/data/{data["src"]}'
        # volumes = {
        #     data['src']: {
        #         'bind': src_path,
        #         'mode': 'ro'
        #     },
        # }
        # self._docker.containers.run(builder, volumes=volumes)


    def _add_link(self, head: str, tail: str, ifaces: tuple[str, str], data: dict):

        section = ConfigSection.Links

        head_phynode = self._node_to_phynode(head)
        tail_phynode = self._node_to_phynode(tail)
        head_iface, tail_iface = ifaces

        if head_phynode == tail_phynode:

            """ Both ends of the link lie on the same phynode, link is a veth pair. """
            self._phynode_exec(head_phynode, section, f'ip l add dev {head_iface} netns {head} type veth peer name {tail_iface} netns {tail}')

        else:

            """ Both nodes are on separate phynodes, we create vlan-defined links """
            # TODO: create VLAN and interfaces
            head_idx = self._get_node_idx(head)
            tail_idx = self._get_node_idx(tail)
            vlan_id = f'0x{head_idx :02x}{tail_idx :02x}'
            print(vlan_id)

        """ Set link properties """
        delay = data['latency'] if 'latency' in data else '0ms'
        bw = data['bw'] if 'bw' in data else '1gbit'
        self._node_exec(head, section, f'tc qdisc add dev {head_iface} root netem delay {delay} rate {bw}')
        if (mtu := data.get('mtu')) is not None:
            self._ip(section, f'l set dev {head_iface} mtu {mtu}', head)
            self._ip(section, f'l set dev {tail_iface} mtu {mtu}', tail)

        if (addrs := self.topo.nodes[head]['cfg']._addresses.get(head_iface)):
            for addr in addrs:
                self._ip(section, f'a add {addr} dev {head_iface}', head)

        if (addrs := self.topo.nodes[tail]['cfg']._addresses.get(tail_iface)):
            for addr in addrs:
                self._ip(section, f'a add {addr} dev {tail_iface}', tail)

        self._ip(section, f'l set dev {head_iface} up', head)
        self._ip(section, f'l set dev {tail_iface} up', tail)

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

        base = os.path.join(self.base, '.dune')
        if not os.path.exists(base): os.mkdir(base)

        """ Dump phynodes configs """
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

        """ Dump templates """
        nodes_dir = os.path.join(base, 'nodes')
        if not os.path.exists(nodes_dir): os.mkdir(nodes_dir)
        for node, cfg in self.topo.nodes(data=True):
            cfg = cfg['cfg']
            targets = {}
            node_dir = os.path.join(nodes_dir, node)
            if not os.path.exists(node_dir): os.mkdir(node_dir)
            for template, data in cfg.templates.items():
                local = os.path.basename(data['dst'])
                targets[local] = data['dst']
                print(node, template, data)
                dst = os.path.join(node_dir, local)
                with open(dst, 'w') as fp:
                    fp.write(data['content'])
            with open(os.path.join(node_dir, 'targets.yml'), 'w') as fd:
                yaml.dump(targets, fd)

        """ Dump roles for mpf """
        seen = []
        roles = {}
        for head, tail, (head_iface, tail_iface),  in self.topo.edges(keys=True):
            forward = f'{head}:{head_iface}-{tail}:{tail_iface}'
            reverse = f'{tail}:{tail_iface}-{head}:{head_iface}'
            if reverse in seen: continue
            seen.append(forward)

            """ Add forward direction """
            a = (forward, 'forward')
            try:
                roles[head][head_iface] = a
            except KeyError:
                roles[head] = {head_iface: a}

            """ Add reverse direction """
            b = (forward, 'backward')
            try:
                roles[tail][tail_iface] = b
            except KeyError:
                roles[tail] = {tail_iface: b}

        r = [{'role': role, 'namespace': role, 'interfaces': [{'name': iface, 'link': name, 'direction': ord} for iface, (name, ord) in ifaces.items()]} for role, ifaces in roles.items()]
        name = sub('\.dune\.yml', '', self.name)
        with open(os.path.join(self.base, f'{name}.mpf.yml'), 'w') as fd:
            yaml.dump(r, fd)

def cli():
    from pathlib import Path
    import argparse

    parser = argparse.ArgumentParser()
    parser.add_argument('-t', '--topology', type=Path, required=True, help='Topology definition file')
    parser.add_argument('-b', '--backend', type=str, default='mpf', choices=['mpf', 'shell'], help="""
    Backend used to apply the topology.

    mpf: Leverage mpf to deploy an ipyarallel cluster defined by the provided infrastructure.

    shell: Produce a shell script per phynode that users have to manually transfer and execute.
    """)
    args = parser.parse_args()

    base = args.topology.parent
    topo = args.topology.name

    dune = Dune(base, topo)
    dune.build()
    dune.dump(format='json')

if __name__ == 'dune' or __name__ == '__main__':
    cli()
