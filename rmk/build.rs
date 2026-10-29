#[path = "./build_common.rs"]
mod common;

use std::process::Command;

fn main() {
    let mut cfgs = common::CfgSet::new();
    common::set_target_cfgs(&mut cfgs);

    // `RMK_COMMIT` and `RMK_FEATURES` go into the storage schema hash.
    // When either changes, the storage is erased. The stored values are
    // postcard-encoded `rmk-types` enums, so a commit there counts too.
    // A build from a fork or a submodule can pin `RMK_COMMIT` in its environment,
    // so a fork commit that doesn't change the storage format keeps the storage.
    // The rerun directive only when pinned: any directive turns off the default rerun
    // on package changes, which keeps the `git log` commit current.
    let commit = match std::env::var("RMK_COMMIT") {
        Ok(commit) => {
            println!("cargo:rerun-if-env-changed=RMK_COMMIT");
            commit
        }
        Err(_) => Command::new("git")
            .args(["log", "-1", "--format=%H", "--", ".", "../rmk-types"])
            .output()
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
            .unwrap_or_default(),
    };
    println!("cargo:rustc-env=RMK_COMMIT={commit}");

    let mut features: Vec<String> = std::env::vars()
        .filter_map(|(key, _)| key.strip_prefix("CARGO_FEATURE_").map(str::to_lowercase))
        .collect();
    features.sort();
    println!("cargo:rustc-env=RMK_FEATURES={}", features.join(","));
}
