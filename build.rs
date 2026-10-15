use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    match env::var("CARGO_CFG_TARGET_OS").as_deref() {
        Ok("macos") => build_swift(),
        Ok("windows") => {
            println!("cargo:rerun-if-changed=assets/hop.rc");
            println!("cargo:rerun-if-changed=assets/hop.ico");
            let _ = embed_resource::compile("assets/hop.rc", embed_resource::NONE).manifest_optional();
        }
        _ => {}
    }
}

fn xcrun(args: &[&str]) -> String {
    let out = Command::new("xcrun").args(args).output().expect("xcrun");
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

fn build_swift() {
    let src = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()).join("mac/HopMac.swift");
    println!("cargo:rerun-if-changed={}", src.display());
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let arch = match env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("aarch64") => "arm64",
        _ => "x86_64",
    };
    let target = format!("{arch}-apple-macos13.0");
    let lib = out.join("libhopmac.a");
    let status = Command::new("xcrun")
        .current_dir(&out)
        .args(["swiftc", "-parse-as-library", "-emit-library", "-static", "-module-name", "HopMac"])
        .args(["-swift-version", "5", "-O", "-target", &target])
        .arg(&src)
        .arg("-o")
        .arg(&lib)
        .status()
        .expect("run swiftc");
    assert!(status.success(), "swiftc failed");

    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=hopmac");

    let sdk = xcrun(&["--show-sdk-path"]);
    println!("cargo:rustc-link-search=native={sdk}/usr/lib/swift");
    let swiftc = xcrun(&["--find", "swiftc"]);
    let toolchain_lib = PathBuf::from(swiftc).parent().unwrap().parent().unwrap().join("lib/swift/macosx");
    println!("cargo:rustc-link-search=native={}", toolchain_lib.display());
    println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
    for fw in [
        "ScreenCaptureKit",
        "VideoToolbox",
        "CoreMedia",
        "CoreVideo",
        "CoreGraphics",
        "AppKit",
        "ApplicationServices",
        "IOKit",
        "QuartzCore",
        "Foundation",
    ] {
        println!("cargo:rustc-link-lib=framework={fw}");
    }
}
