# Distributed Micro Network Emulation (DUNE)

Framework allowing distributed emulation of large networks with micro overhead.

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
