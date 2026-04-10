// WHPX boot test: Boots a Linux kernel under WHPX with optional disk and network.
// Usage: whpx-boot-test <vmlinux> [initrd] [--disk <path>] [--ram <MiB>]

use std::env;
use std::path::PathBuf;
use std::process;

use polly::event_manager::EventManager;
use utils::eventfd::EventFd;
use vmm::resources::VmResources;
use vmm::vmm_config::external_kernel::{ExternalKernel, KernelFormat};
use vmm::vmm_config::machine_config::VmConfig;

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: {} <vmlinux> [initrd] [--disk <path>] [--ram <MiB>]", args[0]);
        process::exit(1);
    }

    // Parse arguments
    let kernel_path = PathBuf::from(&args[1]);
    let mut initrd_path: Option<PathBuf> = None;
    let mut disk_path: Option<String> = None;
    let mut ram_mib: usize = 1024;

    let mut i = 2;
    while i < args.len() {
        match args[i].as_str() {
            "--disk" => {
                i += 1;
                disk_path = Some(args[i].clone());
            }
            "--ram" => {
                i += 1;
                ram_mib = args[i].parse().expect("Invalid RAM size");
            }
            _ => {
                if initrd_path.is_none() {
                    initrd_path = Some(PathBuf::from(&args[i]));
                }
            }
        }
        i += 1;
    }

    println!("WHPX Boot Test");
    println!("Kernel: {}", kernel_path.display());
    if let Some(ref p) = initrd_path {
        println!("Initrd: {}", p.display());
    }
    if let Some(ref p) = disk_path {
        println!("Disk: {}", p);
    }
    println!("RAM: {} MiB", ram_mib);

    let mut vm_resources = VmResources::default();

    // VM config
    let vm_config = VmConfig {
        vcpu_count: Some(1),
        mem_size_mib: Some(ram_mib),
        ht_enabled: Some(false),
        cpu_template: None,
    };
    let _ = vm_resources.set_vm_config(&vm_config);

    // Kernel cmdline: root on /dev/vda if disk provided, otherwise just serial console
    // Same cmdline for both disk and no-disk modes.
    // initrd handles rootfs switch when root= is in cmdline.
    let mut cmdline = "earlyprintk=ttyS0 console=ttyS0 reboot=t panic=-1 nohpet nolapic noapic tsc=reliable lpj=7200000 8250.nr_uarts=1".to_string();
    if disk_path.is_some() {
        // Don't add root= to kernel cmdline - let initrd handle rootfs mount
        // Pass it as a custom parameter that initrd's init script will parse
        cmdline.push_str(" krun_root=/dev/vda");
    }

    // External kernel
    let format = if kernel_path.to_str().map_or(false, |s| s.contains("vmlinux")) {
        KernelFormat::Elf
    } else {
        KernelFormat::Raw
    };
    // Always load initrd - it handles rootfs switch when disk is provided
    let external_kernel = ExternalKernel {
        path: kernel_path,
        format,
        initramfs_path: initrd_path.clone(),
        initramfs_size: initrd_path
            .as_ref()
            .and_then(|p| std::fs::metadata(p).ok())
            .map(|m| m.len())
            .unwrap_or(0),
        cmdline: Some(cmdline),
    };
    vm_resources.set_external_kernel(external_kernel);

    // Add block device if --disk specified
    #[cfg(target_os = "windows")]
    if let Some(ref path) = disk_path {
        use vmm::vmm_config::block::BlockDeviceConfig;
        let block_config = BlockDeviceConfig {
            block_id: "root".to_string(),
            cache_type: devices::virtio::CacheType::Unsafe,
            disk_image_path: path.clone(),
            disk_image_format: devices::virtio::block::ImageType::Raw,
            is_disk_read_only: false,
            direct_io: false,
            sync_mode: devices::virtio::block::SyncMode::Full,
        };
        vm_resources
            .add_block_device(block_config)
            .expect("Failed to add block device");
        println!("Block device added: {}", path);
    }

    // Add slirp network device on Windows
    #[cfg(target_os = "windows")]
    {
        use vmm::vmm_config::net::NetworkInterfaceConfig;
        use devices::virtio::net::device::VirtioNetBackend;
        let net_config = NetworkInterfaceConfig {
            iface_id: "eth0".to_string(),
            backend: VirtioNetBackend::Slirp {
                port_forwards: vec![(9222, 9222)],
            },
            mac: [0x52, 0x54, 0x00, 0x12, 0x34, 0x56],
            features: 0,
        };
        vm_resources.add_network_interface(net_config).expect("Failed to add net device");
        println!("Slirp network with hostfwd 9222:9222 added");
    }

    // Event manager + shutdown
    let mut event_manager = EventManager::new().expect("Failed to create event manager");
    let shutdown_efd =
        EventFd::new(utils::eventfd::EFD_NONBLOCK).expect("Failed to create shutdown eventfd");
    let (sender, _receiver) = crossbeam_channel::unbounded();

    println!("Building microVM...");

    match vmm::builder::build_microvm(&vm_resources, &mut event_manager, Some(shutdown_efd), sender)
    {
        Ok(_vmm) => {
            println!("MicroVM running. CDP should be available at http://127.0.0.1:9222/json/version");
            loop {
                match event_manager.run_with_timeout(1000) {
                    Ok(_) => {}
                    Err(e) => {
                        eprintln!("Event manager error: {:?}", e);
                        break;
                    }
                }
            }
        }
        Err(e) => {
            eprintln!("Failed to build microVM: {}", e);
            process::exit(1);
        }
    }
}
