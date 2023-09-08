use std::{
    ffi::{c_void, CString},
    fs::File,
    os::unix::prelude::{AsRawFd, FromRawFd},
    sync::mpsc::{self, Sender},
};

use nix::{
    poll::{poll, PollFd, PollFlags},
    sys::{
        memfd::{memfd_create, MemFdCreateFlag},
        mman,
    },
};
use userfaultfd::{FaultKind, FeatureFlags, RegisterMode, Uffd, UffdBuilder};

fn main() {
    let mem_size = 4096 * 4;
    let (file, mem_addr) = create_uffd_mapping(mem_size);
    let vm_addr = create_vm_mapping(file.as_raw_fd(), mem_size as _);

    let uffd = UffdBuilder::new()
        .user_mode_only(false)
        .non_blocking(false)
        .require_features(
            FeatureFlags::EVENT_REMOVE
                | FeatureFlags::EVENT_REMAP
                | FeatureFlags::EVENT_FORK
                | FeatureFlags::EVENT_UNMAP
                | FeatureFlags::MISSING_SHMEM
                | FeatureFlags::MINOR_SHMEM
                | FeatureFlags::PAGEFAULT_FLAG_WP,
        )
        .create()
        .unwrap();

    uffd.register_with_mode(
        vm_addr,
        mem_size as _,
        RegisterMode::MISSING | RegisterMode::MODE_MINOR | RegisterMode::WRITE_PROTECT,
    )
    .unwrap();

    let (tx, rx) = mpsc::channel();
    std::thread::spawn({
        let uffd_copy = unsafe { Uffd::from_raw_fd(uffd.as_raw_fd()) };
        let vm_addr = vm_addr as u64;

        move || {
            handle_uffd_events(uffd_copy, mem_size, vm_addr, tx);
        }
    });
    let time = std::time::Instant::now();

    // First write to the _underlying_ memory
    write_to_pointer(mem_addr, &[1, 2, 3]);

    // Then read from VM armed memory, this should trigger a minor fault
    read_from_pointer(vm_addr, 3);

    println!("Done, took {:?}", time.elapsed());
}

fn create_uffd_mapping(size: u64) -> (File, *mut c_void) {
    let name = CString::new("mapping").unwrap();
    let memfd = memfd_create(&name, MemFdCreateFlag::empty()).unwrap();
    let file = unsafe { File::from_raw_fd(memfd) };
    file.set_len(size).unwrap();

    let mapping = unsafe {
        mman::mmap(
            std::ptr::null_mut(),
            size as usize,
            mman::ProtFlags::PROT_READ | mman::ProtFlags::PROT_WRITE,
            mman::MapFlags::MAP_SHARED,
            memfd,
            0,
        )
        .unwrap()
    };

    (file, mapping)
}

fn create_vm_mapping(fd: i32, size: usize) -> *mut c_void {
    unsafe {
        mman::mmap(
            std::ptr::null_mut(),
            size,
            mman::ProtFlags::PROT_READ | mman::ProtFlags::PROT_WRITE,
            mman::MapFlags::MAP_SHARED,
            fd,
            0,
        )
        .unwrap()
    }
}

fn handle_uffd_events(uffd: Uffd, mem_size: u64, vm_addr: u64, tx: Sender<()>) {
    let (file, mem_addr) = create_uffd_mapping(mem_size);
    // Loop, handling incoming events on the userfaultfd file descriptor.
    let pollfd = PollFd::new(uffd.as_raw_fd(), PollFlags::POLLIN);

    loop {
        println!("Checking");
        // Wait for fd to become available
        poll(&mut [pollfd], -1).unwrap();
        let revents = pollfd.revents().unwrap();

        if revents.contains(PollFlags::POLLERR) {
            panic!("poll returned POLLERR");
        }

        // Read an event from the userfaultfd.
        let event = uffd.read_event().expect("Failed to read uffd_msg");

        match event {
            Some(userfaultfd::Event::Pagefault { kind, rw, addr }) => {
                println!("Pagefault event: {:?}", event);
                let relative_addr = (addr as u64) - vm_addr;

                if kind == FaultKind::Missing {
                    // Missing event
                    unsafe { uffd.zeropage(addr, mem_size as _, true).unwrap() };
                } else if kind == FaultKind::Minor {
                    // Minor event
                    while let Err(err) = uffd.uffd_continue(addr, mem_size as _, true) {
                        println!("uffd_continue failed: {:?}", err);
                    }
                }
            }
            Some(userfaultfd::Event::Remove { .. }) => {
                println!("Remove event: {:?}", event);
            }
            ev => {
                panic!("Unexpected event: {:?}", ev);
            }
        }
    }
}

fn write_to_pointer(ptr: *mut c_void, data: &[u8]) {
    unsafe {
        std::ptr::copy_nonoverlapping(data.as_ptr(), ptr as *mut u8, data.len());
    }
}

fn read_from_pointer(ptr: *mut c_void, size: usize) -> Vec<u8> {
    let mut data = vec![0; size];
    unsafe {
        std::ptr::copy_nonoverlapping(ptr as *const u8, data.as_mut_ptr(), size);
    }
    data
}
