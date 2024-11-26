[![MIT licensed][mit-badge]][mit-url]
![CI Build](https://github.com/nrybowski/dune/actions/workflows/rust.yml/badge.svg)
![Documentation](https://github.com/nrybowski/dune/actions/workflows/doc.yml/badge.svg)

[mit-badge]: https://img.shields.io/badge/license-MIT-blue.svg
[mit-url]: https://github.com/nrybowski/dune/blob/master/LICENSE

> **WARNING**: This README is still heavily in construction.

# Distributed Micro Network Emulation (DµNE) Framework

> Orchestrate your emulated networking experiments in a breeze.

`DUNE` is a framework that simplifies the orchestration of distributed emulation of large networks with micro overhead.

## Features

### Emulate everywhere

You want to deploy a small topology on your laptop to quickly test a new feature of your current networked application?
Or you want to deploy a large topology spanning on multiple servers with strict resource allocation?
`DUNE` has your back in both cases. It exposes `Rust` and `Python` APIs to allow easy integration with your own libraries.
You may also directly use the builtin CLI.

### Ressources Allocation

Define your physical nodes (_Phynodes_), e.g., experiment servers, and your virtual nodes (_Nodes_), e.g., routers, in a single configuration file.
Specify the amount of core required for each emulated _Node_ and DUNE will allocate the required ressources on the _Phynodes_. 

> "core_\<X\>" is a reserved keyword specifying that a core must be allocated.
> "core_0" is reserved for the pinned process.

```toml
[infrastructure.node.my_server]
cores = [[1, 2, 3], [4, 5, 6, 7]]

[topology.nodes.r0.pinned]
cmd = "./bird -s /tmp/{{node}}.bird.sk -c /tmp/{{node}}.bird.cfg -P /tmp/{{node}}.bird.pid &"
environ.IO_QUIC_CORE_ID = "{{core_1}}"
```

This simple configuration defines a _phynode_ called "my_server" exposing two NUMA cores, each embedding 4 cores.
Core 0 is not exposed to DUNE as it is reserved for the Linux kernel.

A _node_ named "r0" is also defined.
This node reserves two cores, "core\_0" which is always allocated for the pinned process, and "core_1" whose value is stored in an environment variable "IO_QUIC_CORE_ID".

Note the variable "node". This is a reserved keyword that will be expanded later with the node identifier, here "r0".

### Statically configure your nodes and links

Configure sysctls and one-shot commands on your nodes.

```toml
[topology.nodes.r0]

[topology.nodes.r1]
sysctls = {
    "net.ipv6.conf.default.forwarding" = "1"
}
exec = [
    "echo hello > /tmp/{{node}}.hello"
]

[topology.nodes.r2]
```

Define the links connecting your nodes.

```toml
# Enable by default jumbo frames on each link and set latency to 5ms.
[topology.defaults.links]
mtu = 9500
latency = "5ms"

[topology]
links = {
    # Define endpoints between nodes.
    {endpoints = ["r0:eth0", "r1:eth0"]},
    # Override link properties, e.g., to change the MTU. 
    {endpoints = ["r0:eth1", "r2:eth0"], mtu = 1500},
    # Override interface properties. E.g., this setup emulates an asymetric link.
    {endpoints = ["r1:eth1", "r2:eth1"], "r1:eth1".latency = "25ms"}
}
```

### Defaults and Overrides

Define default values for every resource and override them case-by-case if required.

```toml
# Launch BIRD on each node
[topology.defaults.nodes.pinned]
cmd = "./bird -s /tmp/{{node}}.bird.sk -c /tmp/{{node}}.bird.cfg -P /tmp/{{node}}.bird.pid &"
environ.IO_QUIC_CORE_ID = "{{core_1}}"

# Enable by default ipv6 forwarding on each node
[topology.defaults.nodes.sysctls]
"net.ipv6.conf.default.forwarding" = "1"

# Define some routers
[topology.nodes.r0]

[topology.nodes.r1]

[topology.nodes.r2]

# Disable ipv6 forwarding on a stub node
[topology.nodes.r4.sysctls]
"net.ipv6.conf.default.forwarding" = "0"
```

### Templates Rendering

Automatically render jinja templates with user-provided variables.

```console
protocol kernel {
  learn yes;
    ipv6 {
      export filter {
        if source = RTS_OSPF then accept;
        reject;
      };
    import all;
  };
}

protocol ospf v3 {
  debug all;
  ecmp {{data['ecmp'] if 'ecmp' in data else 'no'}};
  ipv6 {
    import all;
    export all;
  };
  area 0 {
  {% for iface, iface_data in ifaces.items() %}
    interface "{{iface}}" {
      cost {{iface_data.metric}};
      link lsa suppression yes;
      hello 5;
      type ptp;
    };
  {% endfor %}
    interface "lo" {
      stub yes;
    };
  };
}
```

The previous file is a simple OSPFv3 configuration for BIRD.
The variable "ecmp" is a user-defined variable.
Note the "ifaces" variable containing a dictionnary of interface names and interface data as defined in the "links" section.

Define the destination path of the rendered template on the corresponding _phynode_.

```toml
# Launch BIRD on each node
[topology.defaults.nodes.templates]
"./templates/bird.tmpl" = "/tmp/{{node}}/bird.conf"

# R0 does not enable ECMP
[topology.nodes.r0]

# R1 enables ECMP
[topology.nodes.r1]
ecmp = "yes"
```

### Direct integration with the [`mpf`](https://github.com/mpiraux/mpf) framework

At its core design, `DUNE` is able to configure and leverage `mpf` to deploy the emulated infrastructure.

```python
#! /usr/bin/ipython

import dune
from dune import mpf

dune.init("topology.toml")

mpf.add_variable('parallel', range(1,9))
mpf.add_variable('zerocopy', {'': 'disabled', '-Z': 'enabled'})

@mpf.run(role='server')
def start_server(mpf_ctx):
    %ex iperf3 -D -s -1 > /dev/null

@mpf.run(role='client', delay=1)
def start_client(mpf_ctx, parallel, zerocopy):
    result = %ex iperf3 -f k -t 2 -P $parallel $zerocopy -c {mpf_ctx['roles']['server']['interfaces'][0]['ip']} | tail -n 3 | grep -ioE "[0-9.]+ [kmg]bits"
    return {'goodput': result[0]}

df = next(mpf.run_experiment(n_runs=1))
```

### Software Build

> SOON(TM)

### User-defined extensions

> SOON(TM)

## Installation

TODO

## Usage

TODO: configuration file specification

### CLI

TODO

### APIs

TODO

## Concepts

### Node

Virtual topology node.
A node is represented as a Linux network-namespace (netns).
Its processes are explicitely pinned to CPU cores to ensure that the Linux scheduler do not introduce undeeded delays.

### Link

Virtual link in the topology.
If both end nodes lie on the same CPU, the link is represented as a Linux Virtual Ethernet (veth) pair.
If the end nodes do not lie on the same server, it is represented as a VXLAN.

### Physical Node

Physical server on which the node will be executed.

## Related publications

### [OFIQUIC: Leveraging QUIC in OSPF for Seamless Network Topology Changes](https://dial.uclouvain.be/pr/boreal/object/boreal%3A286860)

```console
@INPROCEEDINGS{10619718,
  author={Rybowski, Nicolas and Pelsser, Cristel and Bonaventure, Olivier},
  booktitle={2024 IFIP Networking Conference (IFIP Networking)}, 
  title={OFIQUIC: Leveraging QUIC in OSPF for Seamless Network Topology Changes}, 
  year={2024},
  volume={},
  number={},
  pages={368-376},
  keywords={Transport protocols;Network topology;Web and internet services;Routing;Birds;Routing protocols;Topology;OSPF;IS-IS;routing protocols},
  doi={10.23919/IFIPNetworking62109.2024.10619718}}
}
```
