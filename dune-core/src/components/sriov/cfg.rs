use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::{NodeId, cfg::Phynode};

/*
    [infrastructure.nodes.node0]
    // We have two NUMA nodes, each with 74 cores.
    cores = [[5, 79], [85, 159]]
    // We have two x710-da2, each connected to one CPU.
    // Each NIC has two ports, hence two PF.
    // Each PF supports up to 32 VFs.
    // These NICs are slightly under-dimensioned as we have a total of 64 VFs for 74 cores.
    // We allocate the VFs to nodes according to their underlying CPU.
    // Nodes allocated in range [5, 79] will request VFs from PF0 and PF1, while
    // nodes allocated in range [85, 159] will request VFs from PF2 and PF3.
    // VFs are allocated in requesting order, i.e., if Node 6 is the first to
    // request a VF, it receives VF0 from PF0.
    // Then, if Node 3 requests in turn a VF, it receives VF1 from PF0.
    interfaces = [
      { name = "enP1p2s0f0np0", cpu = 0, sriov = { max_vfs = 32}},
      { name = "enP1p2s0f1np1", cpu = 0, sriov = { max_vfs = 32}},
      { name = "enP2p2s0f0np0", cpu = 1, sriov = { max_vfs = 32}},
      { name = "enP2p2s0f1np1", cpu = 1, sriov = { max_vfs = 32}},
    ]

*/

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PfEntry {
    pf_ifname: String,
    max: usize,
    available: usize,
}

impl PfEntry {
    fn new(pf_ifname: String, max: usize, available: usize) -> Self {
        Self {
            // id,
            pf_ifname,
            max,
            available,
        }
    }
}

pub enum SrIovError {
    NoVfForCpu(usize),
}

type _PhynodeVfs = HashMap<usize, Vec<PfEntry>>;

#[derive(Default, Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PhynodeVfs(_PhynodeVfs);

pub type PhynodesVfs = HashMap<NodeId, PhynodeVfs>;

impl FromIterator<(usize, Vec<PfEntry>)> for PhynodeVfs {
    fn from_iter<T: IntoIterator<Item = (usize, Vec<PfEntry>)>>(iter: T) -> Self {
        Self(_PhynodeVfs::from_iter(iter))
    }
}

impl From<&Phynode> for PhynodeVfs {
    fn from(phynode: &Phynode) -> Self {
        let mut ret: Self = Default::default();
        phynode
            .interfaces
            .iter()
            // .enumerate()
            .filter_map(|iface| {
                if let Some(sriov) = &iface.sriov {
                    Some((
                        iface.cpu,
                        PfEntry::new(iface.name.clone(), sriov.max_vfs, sriov.max_vfs),
                    ))
                } else {
                    None
                }
            })
            .rev()
            .for_each(|(cpu, pf)| ret.0.entry(cpu).or_insert(vec![]).push(pf));
        ret
    }
}

impl PhynodeVfs {
    pub fn get_vf(&mut self, cpu: usize) -> Result<(String, usize), SrIovError> {
        // Get the available PFs for <cpu>.
        if let Some(pfs_cpu) = self.0.get_mut(&cpu)
            && let Some(pf) = pfs_cpu.last_mut()
        {
            let ids = (pf.pf_ifname.clone(), pf.max - pf.available);
            pf.available -= 1;

            // If no VF remaining on the PF, we remove it from the list.
            if pf.available == 0 {
                pfs_cpu.pop();
            }
            // If there is no PF available for <cpu>, we remove it from the map.
            if pfs_cpu.len() == 0 {
                self.0.remove(&cpu);
            }
            return Ok(ids);
        } else {
            // We requested a VF for <cpu>, but we do not have any available...
            return Err(SrIovError::NoVfForCpu(cpu));
        }
    }
}

#[cfg(test)]
mod test_get_vf {

    use super::*;

    /// Return a simple SR-IOV cfg:
    /// 2 CPUs, for each CPU, 2 PF supporting each 2 VF.
    fn simple_cfg() -> PhynodeVfs {
        PhynodeVfs::from_iter(vec![
            (
                0,
                vec![
                    PfEntry::new("pf1".into(), 2, 2),
                    PfEntry::new("pf0".into(), 2, 2),
                ],
            ),
            (
                1,
                vec![
                    PfEntry::new("pf3".into(), 2, 2),
                    PfEntry::new("pf2".into(), 2, 2),
                ],
            ),
        ])
    }

    fn check_ids(
        expected: Result<(String, usize), SrIovError>,
        got: Result<(String, usize), SrIovError>,
    ) {
        if expected.is_ok() {
            assert!(got.is_ok());
        } else {
            assert!(got.is_err());
        }

        if let Ok((exp_pf, exp_vf)) = expected
            && let Ok((got_pf, got_vf)) = got
        {
            assert_eq!(got_pf, exp_pf, "PF mismatch");
            assert_eq!(got_vf, exp_vf, "VF mismatch");
        }
    }

    /// Use exactly all the available ressources.
    #[test]
    fn exact_ressources() {
        let mut pfs = simple_cfg();

        // Request 3 VFs on cpu0.
        for i in 0..3 {
            let exp = Result::Ok((format!("pf{:?}", i / 2).into(), i % 2));
            let got = pfs.get_vf(0);
            check_ids(exp, got);
        }

        // Request 1 VF on cpu1.
        let exp = Result::Ok(("pf2".into(), 0));
        let got = pfs.get_vf(1);
        check_ids(exp, got);

        // Request 1 VF on cpu0.
        let exp = Result::Ok(("pf1".into(), 1));
        let got = pfs.get_vf(0);
        check_ids(exp, got);

        // No more Pf available on cpu0.
        assert!(!pfs.0.contains_key(&0));

        // Request 3 VFs on cpu1.
        for i in 1..4 {
            let exp = Result::Ok((format!("pf{}", i / 2 + 2).into(), i % 2));
            let got = pfs.get_vf(1);
            check_ids(exp, got);
        }

        // No more Pf available on cpu1.
        assert!(!pfs.0.contains_key(&1));

        // All ressources are allocated.
        assert!(pfs.0.len() == 0);
    }

    /// Try to allocate more VFs on CPU0 than available.
    #[test]
    fn overflow() {
        let mut pfs = simple_cfg();

        // Empty all ressources.
        for i in 0..4 {
            let exp = Result::Ok((format!("pf{}", i / 2).into(), i % 2));
            let got = pfs.get_vf(0);
            check_ids(exp, got);
        }

        // Query an additional VF on CPU0.
        let exp = Result::Err(SrIovError::NoVfForCpu(0));
        let got = pfs.get_vf(0);
        check_ids(exp, got);

        // Ensure that ressources for CPU1 are left untouched.
        assert_eq!(pfs.0.len(), 1);
        assert_eq!(pfs.0[&1].len(), 2);
        assert_eq!(pfs.0[&1][0].pf_ifname, "pf3".to_string());
        assert_eq!(pfs.0[&1][0].max, 2);
        assert_eq!(pfs.0[&1][0].available, 2);
        assert_eq!(pfs.0[&1][1].pf_ifname, "pf2".to_string());
        assert_eq!(pfs.0[&1][1].max, 2);
        assert_eq!(pfs.0[&1][1].available, 2);
    }
}

#[cfg(test)]
mod test_from_phynode {
    use crate::cfg::{PhynodeInterface, SrIOVInterface};

    use super::*;

    /// Ensure that PhynodeVfs is empty if no SR-IOV is specified.
    #[test]
    fn from_phynode_no_sriov() {
        let mut phy = Phynode::default();
        phy.interfaces.push(PhynodeInterface {
            name: "pf0".into(),
            cpu: 0,
            sriov: None,
        });

        let got = PhynodeVfs::from(&phy);

        // Ensure no PF.
        assert_eq!(got.0.len(), 0);
    }

    /// Ensure that PhynodeVfs is correctly populated for a single core, single PF.
    #[test]
    fn from_phynode_single_core_single_pf() {
        let core_id = 18;
        let max_vfs = 2;
        let pf_name = "pf0".to_string();

        let mut phy = Phynode::default();
        phy.interfaces.push(PhynodeInterface {
            name: pf_name.clone(),
            cpu: core_id,
            sriov: Some(SrIOVInterface { max_vfs }),
        });

        let got = PhynodeVfs::from(&phy);

        // Ensure a single core and correct core id.
        assert_eq!(got.0.len(), 1);
        assert!(got.0.keys().collect::<Vec<&usize>>().contains(&&core_id));

        // Ensure correct number of PF on dedicated core.
        assert_eq!(got.0[&core_id].len(), 1, "Number of PF mismatch");

        // Ensure correct PF name.
        assert_eq!(got.0[&core_id][0].pf_ifname, pf_name);

        // Ensure correct max_vfs.
        assert_eq!(got.0[&core_id][0].max, max_vfs);

        // Ensure correct available.
        assert_eq!(got.0[&core_id][0].available, max_vfs);
    }

    /// Ensure that PhynodeVfs is correctly populated for a single core, single PF.
    #[test]
    fn from_phynode_dual_core_dual_pf() {
        let core_id0 = 25;
        let core_id1 = 8;
        let max_vfs = 64;
        let pf0_name = "pf3".to_string();
        let pf1_name = "pf2".to_string();
        let pf2_name = "pf1".to_string();
        let pf3_name = "pf0".to_string();

        let mut phy = Phynode::default();
        phy.interfaces.push(PhynodeInterface {
            name: pf0_name.clone(),
            cpu: core_id0,
            sriov: Some(SrIOVInterface { max_vfs }),
        });
        phy.interfaces.push(PhynodeInterface {
            name: pf1_name.clone(),
            cpu: core_id0,
            sriov: Some(SrIOVInterface { max_vfs }),
        });
        phy.interfaces.push(PhynodeInterface {
            name: pf2_name.clone(),
            cpu: core_id1,
            sriov: Some(SrIOVInterface { max_vfs }),
        });
        phy.interfaces.push(PhynodeInterface {
            name: pf3_name.clone(),
            cpu: core_id1,
            sriov: Some(SrIOVInterface { max_vfs }),
        });

        let got = PhynodeVfs::from(&phy);

        // Ensure a two cores and correct core ids.
        assert_eq!(got.0.len(), 2);
        assert!(got.0.keys().collect::<Vec<&usize>>().contains(&&core_id0));
        assert!(got.0.keys().collect::<Vec<&usize>>().contains(&&core_id1));

        // Ensure correct number of PF on dedicated core.
        assert_eq!(got.0[&core_id0].len(), 2, "Number of PF mismatch");
        assert_eq!(got.0[&core_id1].len(), 2, "Number of PF mismatch");

        // Ensure correct PF name.
        assert_eq!(got.0[&core_id0][0].pf_ifname, pf1_name);
        assert_eq!(got.0[&core_id0][1].pf_ifname, pf0_name);
        assert_eq!(got.0[&core_id1][0].pf_ifname, pf3_name);
        assert_eq!(got.0[&core_id1][1].pf_ifname, pf2_name);

        for core in vec![core_id0, core_id1] {
            for i in 0..2 {
                // Ensure correct max_vfs.
                assert_eq!(got.0[&core][i].max, max_vfs);

                // Ensure correct available.
                assert_eq!(got.0[&core][i].available, max_vfs);
            }
        }
    }
}
