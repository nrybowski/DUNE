[![MIT licensed][mit-badge]][mit-url]
![CI Build](https://github.com/nrybowski/dune/actions/workflows/rust.yml/badge.svg)

[mit-badge]: https://img.shields.io/badge/license-MIT-blue.svg
[mit-url]: https://github.com/nrybowski/dune/blob/master/LICENSE

> **WARNING**: This README is still heavily in construction.

# Distributed Micro Network Emulation (DµNE) Framework

> Orchestrate your emulated networking experiments in a breeze.

`DUNE` is a framework that simplifies the orchestration of distributed emulation of large networks with micro overhead.

## Features

### Ressources Allocation

Define your physical nodes (_Phynodes_), e.g., experiment servers, and your virtual nodes (_Nodes_), e.g., routers, in a single configuration file.
Specify the amount of core required for each emulated _Node_ and DUNE will allocate the required ressources on the _Phynodes_. 

> "core_\<X\>" is a reserved keyword specifying that a core must be allocated.

```toml
TODO
```

### Defaults and Overrides

Define default values for every resource and override them case-by-case if required. 

```toml
[topology.defaults.nodes.sysctls]
"net.ipv6.conf.default.forwarding" = "1"

[topolofy.nodes.r0]

[topolofy.nodes.r1]
"net.ipv6.conf.default.forwarding" = "0"

[topolofy.nodes.r2]
```

### Templates Rendering

> TODO

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

## Install

TODO

## Features
- Automatically balance the virtual node on the physical infrastructure based on user constraints.
- (Non-)Interactively configure the physical infrastructure with netns, veth pairs and vxlans.
- Build node's configuration files based on templates and topology data.
- Build binaries to run on nodes based on the physical environment requirements.
- Offer extensability in topology configuration with user-provided plugins.

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
