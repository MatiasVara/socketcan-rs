// build.rs
//
// Build for socketcan-rs simply checks that the target system is Linux
// and issues a clear error message if not.
//

#[cfg(not(target_os = "linux"))]
compile_error!("SocketCAN is only supported on Linux targets.");

fn main() {}
