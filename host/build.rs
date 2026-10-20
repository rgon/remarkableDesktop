//! Compiles the edge detection shader (WGSL) to SPIR-V for Vulkan, so no
//! shader compiler has to be installed. `naga` is only needed here.

use std::path::Path;

fn main() {
    println!("cargo:rerun-if-changed=src/edges.wgsl");
    let source = std::fs::read_to_string("src/edges.wgsl").expect("src/edges.wgsl");
    let module = naga::front::wgsl::parse_str(&source).unwrap_or_else(|e| panic!("edges.wgsl: {}", e.emit_to_string(&source)));
    let info = naga::valid::Validator::new(naga::valid::ValidationFlags::all(), naga::valid::Capabilities::empty())
        .validate(&module)
        .unwrap_or_else(|e| panic!("edges.wgsl: {e:?}"));
    let words = naga::back::spv::write_vec(&module, &info, &naga::back::spv::Options::default(), None).expect("SPIR-V output");
    let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    let out = Path::new(&std::env::var("OUT_DIR").expect("OUT_DIR")).join("edges.spv");
    std::fs::write(out, bytes).expect("write edges.spv");
}
