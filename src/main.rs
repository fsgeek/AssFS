use std::path::PathBuf;

use fuser::MountOption;

use assfs::fuse_ops::AssFs;

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args: Vec<String> = std::env::args().collect();

    if args.len() < 2 {
        eprintln!("Usage: {} <mountpoint> [--data-dir <path>]", args[0]);
        eprintln!();
        eprintln!("  <mountpoint>          Where to mount the filesystem");
        eprintln!("  --data-dir <path>     Where to store data (default: ~/.assfs)");
        std::process::exit(1);
    }

    let mountpoint = PathBuf::from(&args[1]);

    // Parse --data-dir option.
    let data_dir = if let Some(pos) = args.iter().position(|a| a == "--data-dir") {
        if pos + 1 < args.len() {
            PathBuf::from(&args[pos + 1])
        } else {
            eprintln!("--data-dir requires a path argument");
            std::process::exit(1);
        }
    } else {
        dirs_or_default()
    };

    log::info!("AssFS starting");
    log::info!("  mountpoint: {}", mountpoint.display());
    log::info!("  data dir:   {}", data_dir.display());

    // Create mountpoint if it doesn't exist.
    if !mountpoint.exists() {
        if let Err(e) = std::fs::create_dir_all(&mountpoint) {
            eprintln!("Failed to create mountpoint: {}", e);
            std::process::exit(1);
        }
    }

    let fs = match AssFs::open(&data_dir) {
        Ok(fs) => fs,
        Err(e) => {
            eprintln!("Failed to open AssFS: {}", e);
            std::process::exit(1);
        }
    };

    let options = vec![
        MountOption::AutoUnmount,
        MountOption::AllowOther,
        MountOption::FSName("assfs".to_string()),
        MountOption::DefaultPermissions,
    ];

    log::info!("Mounting filesystem...");
    if let Err(e) = fuser::mount2(fs, &mountpoint, &options) {
        // AllowOther may fail without /etc/fuse.conf. Retry without it.
        log::warn!("Mount with AllowOther failed ({}), retrying without...", e);
        let options = vec![
            MountOption::AutoUnmount,
            MountOption::FSName("assfs".to_string()),
            MountOption::DefaultPermissions,
        ];
        if let Err(e) = fuser::mount2(
            AssFs::open(&data_dir).expect("reopen failed"),
            &mountpoint,
            &options,
        ) {
            eprintln!("Failed to mount: {}", e);
            std::process::exit(1);
        }
    }

    log::info!("AssFS unmounted cleanly");
}

fn dirs_or_default() -> PathBuf {
    if let Some(home) = std::env::var_os("HOME") {
        PathBuf::from(home).join(".assfs")
    } else {
        PathBuf::from("/tmp/assfs-data")
    }
}
