// Detailed memory stats for a single process

use procfs::process::{PageInfo, Pfn, Process};
use std::collections::{HashMap, HashSet};

fn print_info(process: &Process) -> Result<(), Box<dyn std::error::Error>> {
    if process.cmdline()?.is_empty() {
        return Err(String::from("No info for kernel process"))?;
    }

    let mut total_rss = 0;
    let mut total_vsz = 0;
    let mut total_swap = 0;

    // page table size
    let _pte = process
        .status()?
        .vmpte
        .expect("'vmpte' field does not exist");

    // file descriptors
    let _fds = process.fd_count()?;

    let memory_maps = snap::get_memory_maps_for_process(process, false)?;

    for (memory_map, pages) in memory_maps.iter() {
        // physical memory pages
        let mut pfns: Vec<Pfn> = Vec::new();
        // swap type, offset
        let mut swap_pages: Vec<(u64, u64)> = Vec::new();

        println!(
            "0x{:016x}-0x{:016x} {:?} {:?}",
            memory_map.address.0, memory_map.address.1, memory_map.perms, memory_map.pathname,
        );

        for page in pages.iter() {
            match page {
                PageInfo::MemoryPage(memory_page) => {
                    let pfn = memory_page.get_page_frame_number();
                    if pfn.0 != 0 {
                        println!("PFN=0x{pfn:010x} {memory_page:?}");
                        pfns.push(pfn);
                    }
                }
                PageInfo::SwapPage(swap_page) => {
                    let swap_type = swap_page.get_swap_type();
                    let offset = swap_page.get_swap_offset();
                    println!("SWAP={swap_type}: 0x{offset:x} {swap_page:?}");

                    swap_pages.push((swap_type, offset));
                }
            }
        } // end for page

        // kiB
        let vsz = (memory_map.address.1 - memory_map.address.0) / 1024;
        let rss = pfns.len() * 4;
        let swap = swap_pages.len() * 4;

        println!("stats: VSZ={vsz} kiB, RSS={rss} kiB, SWAP={swap} kiB");

        total_rss += rss;
        total_swap += swap;
        total_vsz += vsz;
    } // end for memory_maps

    println!("total stats: VSZ={total_vsz} kiB, RSS={total_rss} kiB, SWAP={total_swap} kiB");

    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();

    let pids: Vec<i32> = args
        .iter()
        .skip(1)
        .map(|s| s.parse().expect("PID arg must be a number"))
        .collect();
    let pid = pids[0];

    // shm (key, id) -> PFNs
    let mut shm_pfns: HashMap<(i32, u64), HashSet<Pfn>> = HashMap::new();
    for shm in procfs::Shm::new().expect("Can't read /dev/sysvipc/shm") {
        let (pfns, _swap_pages) = snap::shm2pfns(&shm, true).unwrap();
        shm_pfns.insert((shm.key, shm.shmid), pfns);
    }

    let process = procfs::process::Process::new(pid)?;

    print_info(&process).unwrap();

    Ok(())
}
