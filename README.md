> **WARNING**: This README is still in construction.

# Distributed Micro Network Emulation (DµNE) Framework

Orchestrate your emulated networking experiments in a breeze.

> Framework allowing distributed emulation of large networks with micro overhead.

## Features

### Ressources Allocation

Define your physical nodes (_Phynodes_), e.g., experiment servers, and your virtual nodes (_Nodes_), e.g., routers, in a single configuration file.
Specify the amount of core required for each emulated _Node_ and DUNE will allocate the required ressources on the _Phynodes_. 

> "core_<X>" is a reserved keyword specifying that a core must be allocated.

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

### Software Build

## Install

TODO

## Quick Start

DUNE requires two main files: (i) infra.yml and (ii) topo.yml.
The first one describes the physical infrastructure used for the emulation.
It can range from a single server to a cluster.
The second one describes the topology to emulate.

See [infra.sample.yml](infra.sample.yml) and [topo.sample.yml](topo.sample.yml) for illustration.

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
