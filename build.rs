use std::process::Command;
use std::env;
use std::fs;
use std::path::Path;
use sha2::{Sha256, Digest};

fn calculate_src_hash() -> String {
    let mut hasher = Sha256::new();
    let src_dir = Path::new("src");
    
    if let Ok(entries) = fs::read_dir(src_dir) {
        let mut files: Vec<_> = entries
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.path().extension().map_or(false, |ext| ext == "rs"))
            .collect();
        
        // Sort files to ensure consistent hashing
        files.sort_by_key(|entry| entry.path());
        
        for entry in files {
            if let Ok(content) = fs::read(entry.path()) {
                hasher.update(&content);
            }
        }
    }
    
    format!("{:x}", hasher.finalize())
}

fn main() {
    // Get source files hash
    let src_hash = calculate_src_hash();

    // Get build date
    let build_date = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();

    println!("cargo:rustc-env=SRC_HASH={}", src_hash);
    println!("cargo:rustc-env=BUILD_DATE={}", build_date);
} 