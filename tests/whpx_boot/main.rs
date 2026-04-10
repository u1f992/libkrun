// WHPX boot test: Directly uses vmm crate to boot a Linux kernel under WHPX.
// Usage: whpx-boot-test <bzImage> [initrd]

use std::env;
use std::path::PathBuf;
use std::process;

use polly::event_manager::EventManager;
use utils::eventfd::EventFd;
use vmm::resources::VmResources;
use vmm::vmm_config::external_kernel::{ExternalKernel, KernelFormat};
use vmm::vmm_config::machine_config::VmConfig;

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("debug")).init();

    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: {} <bzImage> [initrd]", args[0]);
        process::exit(1);
    }

    let kernel_path = PathBuf::from(&args[1]);
    let initrd_path = args.get(2).map(PathBuf::from);

    println!("WHPX Boot Test");
    println!("Kernel: {}", kernel_path.display());
    if let Some(ref p) = initrd_path {
        println!("Initrd: {}", p.display());
    }

    // Create VM resources
    let mut vm_resources = VmResources::default();

    // Set VM config
    let vm_config = VmConfig {
        vcpu_count: Some(1),
        mem_size_mib: Some(256),
        ht_enabled: Some(false),
        cpu_template: None,
    };
    vm_resources.set_vm_config(&vm_config);

    // Set external kernel
    let format = if kernel_path.to_str().map_or(false, |s| s.contains("vmlinux")) {
        KernelFormat::Elf
    } else {
        KernelFormat::Raw
    };
    let external_kernel = ExternalKernel {
        path: kernel_path,
        format,
        initramfs_path: initrd_path.clone(),
        initramfs_size: initrd_path
            .as_ref()
            .and_then(|p| std::fs::metadata(p).ok())
            .map(|m| m.len())
            .unwrap_or(0),
        cmdline: Some("console=ttyS0 earlyprintk=ttyS0 reboot=t panic=-1 tsc=reliable lpj=7200000 nohpet lapic ip=dhcp".to_string()),
    };
    vm_resources.set_external_kernel(external_kernel);

    // Add slirp network device on Windows
    #[cfg(target_os = "windows")]
    {
        use vmm::vmm_config::net::NetworkInterfaceConfig;
        use devices::virtio::net::device::VirtioNetBackend;
        let net_config = NetworkInterfaceConfig {
            iface_id: "eth0".to_string(),
            backend: VirtioNetBackend::Slirp,
            mac: [0x52, 0x54, 0x00, 0x12, 0x34, 0x56],
            features: 0,
        };
        vm_resources.add_network_interface(net_config).expect("Failed to add net device");
        println!("Slirp network device added");
    }

    // Create event manager
    let mut event_manager = EventManager::new().expect("Failed to create event manager");

    // Create shutdown event
    let shutdown_efd =
        EventFd::new(utils::eventfd::EFD_NONBLOCK).expect("Failed to create shutdown eventfd");

    let (sender, _receiver) = crossbeam_channel::unbounded();

    println!("Building microVM...");

    match vmm::builder::build_microvm(&vm_resources, &mut event_manager, Some(shutdown_efd), sender)
    {
        Ok(_vmm) => {
            println!("MicroVM built successfully! Running event loop...");
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
