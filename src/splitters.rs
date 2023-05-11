use std::{
    collections::{BTreeMap, HashMap, HashSet},
    ffi::{OsStr, OsString},
    hash::BuildHasherDefault,
};

use anyhow::{bail, Context};
use indicatif::ProgressBar;
use itertools::Itertools;
use log::{debug, warn};
use procfs::{process::Pfn, Shm};
use rayon::prelude::*;

use crate::{
    filters::{self, Filter},
    processes_group_info, ProcessGroupInfo, ProcessInfo, TheHash,
};
use crate::{process_tree::ProcessTree, ShmsMetadata};
//use snap::{processes_group_info, ProcessGroupInfo, ProcessInfo, ShmsMetadata, TheHash};

pub trait ProcessSplitter<'a> {
    fn name(&self) -> String;
    type GroupIter<'b: 'a>: Iterator<Item = &'a ProcessGroupInfo>
    where
        Self: 'b;
    fn __split(
        &mut self,
        tree: &ProcessTree,
        shm_metadata: &ShmsMetadata,
        processes: Vec<ProcessInfo>,
    );
    fn iter_groups(&self) -> Self::GroupIter<'_>;
    fn collect_processes(self) -> Vec<ProcessInfo>;

    fn split(
        &mut self,
        tree: &ProcessTree,
        shms_metadata: &ShmsMetadata,
        processes: Vec<ProcessInfo>,
    ) {
        let chrono = std::time::Instant::now();
        self.__split(tree, shms_metadata, processes);
        debug!("Split by {}: took {:?}", self.name(), chrono.elapsed());
    }

    fn display(&'a self, shm_metadata: &ShmsMetadata) {
        let chrono = std::time::Instant::now();

        let mut info = Vec::new();
        let pb = ProgressBar::new(self.iter_groups().count() as u64);
        for group_1 in self.iter_groups() {
            let mut other_pfns: HashSet<Pfn, BuildHasherDefault<TheHash>> = HashSet::default();
            let mut other_swap: HashSet<(u64, u64), BuildHasherDefault<TheHash>> =
                HashSet::default();
            let mut other_referenced_shm: HashSet<Shm> = HashSet::new();
            for group_other in self.iter_groups() {
                if group_1 != group_other {
                    other_pfns.par_extend(&group_other.pfns);
                    other_swap.par_extend(&group_other.swap_pages);
                    other_referenced_shm.par_extend(&group_other.referenced_shm);
                }
            }
            for (shm, meta) in shm_metadata {
                match meta {
                    Some((shm_pfns, _swap_pages, _pages_4k, _pages_2M)) => {
                        if other_referenced_shm.contains(shm) {
                            other_pfns.par_extend(shm_pfns);
                        }
                    }
                    None => (),
                }
            }

            let mut group_1_pfns = group_1.pfns.clone();
            for (shm, meta) in shm_metadata {
                match meta {
                    Some((shm_pfns, _swap_pages, _pages_4k, _pages_2M)) => {
                        if group_1.referenced_shm.contains(shm) {
                            group_1_pfns.par_extend(shm_pfns);
                        }
                    }
                    None => (),
                }
            }
            let processes_count = group_1.processes_info.len();
            let mem_rss = group_1_pfns.len() as u64 * procfs::page_size() / 1024 / 1024;
            let mem_uss = group_1_pfns.difference(&other_pfns).count() as u64 * procfs::page_size()
                / 1024
                / 1024;

            let swap_rss = group_1.swap_pages.len() as u64 * procfs::page_size() / 1024 / 1024;
            let swap_uss = group_1.swap_pages.difference(&other_swap).count() as u64
                * procfs::page_size()
                / 1024
                / 1024;

            // TODO: no differences for shm?
            let shm_mem: u64 = group_1
                .referenced_shm
                .iter()
                .map(|shm| shm.rss)
                .sum::<u64>()
                / 1024
                / 1024;
            let shm_swap: u64 = group_1
                .referenced_shm
                .iter()
                .map(|shm| shm.swap)
                .sum::<u64>()
                / 1024
                / 1024;

            info.push((
                group_1.name.clone(),
                processes_count,
                mem_rss,
                mem_uss,
                swap_rss,
                swap_uss,
                shm_mem,
                shm_swap,
            ));
            pb.inc(1);
        }
        pb.finish_and_clear();

        // sort by mem RSS
        info.sort_by(|a, b| b.2.cmp(&a.2));

        println!("Process groups by {} (MiB)", self.name());
        println!("group_name                     #procs         RSS         USS   SWAP RSS   SWAP USS    SHM MEM   SHM SWAP",);
        println!("=========================================================================================================");
        for (name, processes_count, mem_rss, mem_uss, swap_rss, swap_uss, shm_mem, shm_swap) in info
        {
            println!(
                "{:<30}  {:>5}  {:>10}  {:>10} {:>10} {:>10} {:>10} {:>10}",
                name, processes_count, mem_rss, mem_uss, swap_rss, swap_uss, shm_mem, shm_swap
            );
        }
        debug!("Display split by {}: {:?}", self.name(), chrono.elapsed());
        println!("");
    }
}

pub struct ProcessSplitterCustomFilter {
    pub name: String,
    pub filters: Vec<Box<dyn Filter>>,
    pub names: Vec<String>,
    pub groups: HashMap<String, ProcessGroupInfo>,
}
impl ProcessSplitterCustomFilter {
    pub fn new(input: &str) -> anyhow::Result<Self> {
        if !input.is_ascii() {
            bail!("Filter must be ASCII");
        }

        let mut filters: Vec<Box<dyn Filter>> = Vec::new();
        let mut names = Vec::new();
        let groups = HashMap::new();
        let mut counter = 0;

        loop {
            let (filter, ate) = filters::parse(&input[counter..])
                .with_context(|| format!("Invalid filter {:?}", &input[counter..]))?;
            filters.push(filter);
            names.push(input[counter..(counter + ate)].to_string());
            counter += ate;
            if counter + 1 > input.chars().count() {
                break;
            }
            counter += 1;
        }

        if counter < input.chars().count() {
            warn!("Didn't parse full filter {input:?}");
        }

        Ok(Self {
            name: input.to_string(),
            filters,
            names,
            groups,
        })
    }
}
impl<'a> ProcessSplitter<'a> for ProcessSplitterCustomFilter {
    fn name(&self) -> String {
        "Custom splitter".to_string()
    }

    type GroupIter<'b: 'a> = std::collections::hash_map::Values<'a, String, ProcessGroupInfo>;

    fn __split(
        &mut self,
        tree: &ProcessTree,
        shms_metadata: &ShmsMetadata,
        mut processes: Vec<ProcessInfo>,
    ) {
        for (group_name, filter) in self.names.iter().zip(&self.filters) {
            let some_processes = processes
                .drain_filter(|p| filter.eval(&p.process, tree))
                .collect();
            let process_group_info =
                processes_group_info(some_processes, group_name.clone(), shms_metadata);
            self.groups.insert(group_name.clone(), process_group_info);
        }

        // remaining processes not captured by any filter
        let other_info = processes_group_info(processes, "Other".to_string(), shms_metadata);
        self.groups.insert("Other".to_string(), other_info);
    }

    fn iter_groups<'x>(&'a self) -> Self::GroupIter<'a> {
        self.groups.values()
    }

    fn collect_processes(mut self) -> Vec<ProcessInfo> {
        self.groups
            .par_drain()
            .flat_map(|(_k, process_group_info)| process_group_info.processes_info)
            .collect()
    }
}

pub struct ProcessSplitterEnvVariable {
    var: OsString,
    groups: HashMap<Option<OsString>, ProcessGroupInfo>,
}
impl ProcessSplitterEnvVariable {
    pub fn new<S: AsRef<OsStr>>(var: S) -> Self {
        Self {
            groups: HashMap::new(),
            var: var.as_ref().to_os_string(),
        }
    }
}

impl<'a> ProcessSplitter<'a> for ProcessSplitterEnvVariable {
    type GroupIter<'b: 'a> =
        std::collections::hash_map::Values<'a, Option<OsString>, ProcessGroupInfo>;

    fn name(&self) -> String {
        format!("environment variable {}", self.var.to_string_lossy())
    }
    fn __split(
        &mut self,
        _tree: &ProcessTree,
        shms_metadata: &ShmsMetadata,
        mut processes: Vec<ProcessInfo>,
    ) {
        let sids: HashSet<Option<OsString>> = processes
            .par_iter()
            .map(|p| p.environ.get(&self.var).cloned())
            .collect();

        let mut groups: HashMap<Option<OsString>, ProcessGroupInfo> = HashMap::new();
        for sid in sids {
            let some_processes: Vec<ProcessInfo> = processes
                .drain_filter(|p| p.environ.get(&self.var) == sid.as_ref())
                .collect();
            let name = format!(
                "{:?}",
                sid.as_ref().map(|os| os.to_string_lossy().to_string())
            );
            let process_group_info = processes_group_info(some_processes, name, shms_metadata);
            groups.insert(sid, process_group_info);
        }
        self.groups = groups;
    }
    fn iter_groups<'x>(&'a self) -> Self::GroupIter<'a> {
        self.groups.values()
    }
    fn collect_processes(mut self) -> Vec<ProcessInfo> {
        self.groups
            .par_drain()
            .flat_map(|(_k, process_group_info)| process_group_info.processes_info)
            .collect()
    }
}
pub struct ProcessSplitterPids {
    pids: Vec<i32>,
    groups: BTreeMap<u8, ProcessGroupInfo>,
}

impl ProcessSplitterPids {
    pub fn new(pids: &[i32]) -> Self {
        Self {
            pids: pids.to_vec(),
            groups: BTreeMap::new(),
        }
    }
}
impl<'a> ProcessSplitter<'a> for ProcessSplitterPids {
    type GroupIter<'b: 'a> = std::collections::btree_map::Values<'a, u8, ProcessGroupInfo>;

    fn name(&self) -> String {
        "PID list".to_string()
    }
    fn __split(
        &mut self,
        _tree: &ProcessTree,
        shms_metadata: &ShmsMetadata,
        processes: Vec<ProcessInfo>,
    ) {
        let mut processes_info_0: Vec<ProcessInfo> = Vec::new();
        let mut processes_info_1: Vec<ProcessInfo> = Vec::new();

        for p in processes {
            if self.pids.contains(&p.process.pid) {
                processes_info_0.push(p);
            } else {
                processes_info_1.push(p);
            }
        }

        let name_0 = self.pids.iter().map(|pid| pid.to_string()).join(", ");
        let name_1 = "Others PIDs".into();
        let process_group_info_0 = processes_group_info(processes_info_0, name_0, shms_metadata);
        let process_group_info_1 = processes_group_info(processes_info_1, name_1, shms_metadata);

        self.groups.insert(0, process_group_info_0);
        self.groups.insert(1, process_group_info_1);
    }
    fn iter_groups<'x>(&'a self) -> Self::GroupIter<'a> {
        self.groups.values()
    }
    fn collect_processes(self) -> Vec<ProcessInfo> {
        self.groups
            .into_values()
            .flat_map(|group| group.processes_info)
            .collect()
    }
}
pub struct ProcessSplitterUid {
    groups: BTreeMap<u32, ProcessGroupInfo>,
}

impl ProcessSplitterUid {
    pub fn new() -> Self {
        Self {
            groups: BTreeMap::new(),
        }
    }
}
impl<'a> ProcessSplitter<'a> for ProcessSplitterUid {
    type GroupIter<'b: 'a> = std::collections::btree_map::Values<'a, u32, ProcessGroupInfo>;

    fn name(&self) -> String {
        "UID".to_string()
    }
    fn __split(
        &mut self,
        _tree: &ProcessTree,
        shms_metadata: &ShmsMetadata,
        mut processes: Vec<ProcessInfo>,
    ) {
        let uids: HashSet<u32> = processes.iter().map(|p| p.uid).collect();

        for uid in uids {
            let username = users::get_user_by_uid(uid);
            let username = match username {
                Some(username) => username.name().to_string_lossy().to_string(),
                None => format!("{uid}"),
            };
            let processes_info: Vec<ProcessInfo> =
                processes.drain_filter(|p| p.uid == uid).collect();
            let group_info = processes_group_info(processes_info, username, shms_metadata);
            self.groups.insert(uid, group_info);
        }
    }
    fn iter_groups<'x>(&'a self) -> Self::GroupIter<'a> {
        self.groups.values()
    }
    fn collect_processes(self) -> Vec<ProcessInfo> {
        self.groups
            .into_values()
            .flat_map(|group| group.processes_info)
            .collect()
    }
}
