//! Scratch: print what volume discovery finds on this machine.
//! `cargo run -p emfit-core --example volumes`
fn main() {
    println!(
        "elevated: {}",
        emfit_core::service::elevation::is_elevated()
    );
    match emfit_core::service::volume::enumerate() {
        Ok(vols) => {
            for v in &vols {
                println!(
                    "\n{}  [{}]  serial {:08X}\n  guid    {}\n  mounts  {:?}\n  label   {:?}\n  sector  {}  cluster {}\n  size    {} total / {} free\n  loc     {:?}\n  raw     {}",
                    v.display_name(),
                    v.filesystem.as_str(),
                    v.serial,
                    v.guid_path,
                    v.mount_points,
                    v.label,
                    v.bytes_per_sector,
                    v.bytes_per_cluster,
                    v.total_bytes,
                    v.free_bytes,
                    v.location,
                    v.supports_raw_scan(),
                );
            }
            println!(
                "\n{} volumes, {} scannable",
                vols.len(),
                vols.iter().filter(|v| v.supports_raw_scan()).count()
            );
        }
        Err(e) => println!("error: {e}"),
    }
}
