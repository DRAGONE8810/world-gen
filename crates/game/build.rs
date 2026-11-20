//! Compiles the GLSL shaders in `shaders/` to SPIR-V at build time using
//! naga (pure Rust), so building the game needs no Vulkan SDK.

use naga::back::spv;
use naga::front::glsl;
use naga::valid::{Capabilities, ValidationFlags, Validator};
use naga::ShaderStage;
use std::path::PathBuf;

fn main() {
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    for (file, stage) in [("terrain.vert", ShaderStage::Vertex), ("terrain.frag", ShaderStage::Fragment)] {
        let path = PathBuf::from("shaders").join(file);
        println!("cargo:rerun-if-changed={}", path.display());
        let src = std::fs::read_to_string(&path).unwrap();
        let module = glsl::Frontend::default()
            .parse(&glsl::Options::from(stage), &src)
            .unwrap_or_else(|e| panic!("{file}: {}", e.emit_to_string(&src)));
        let info = Validator::new(ValidationFlags::all(), Capabilities::IMMEDIATES)
            .validate(&module)
            .unwrap_or_else(|e| panic!("{file}: {}", e.emit_to_string(&src)));
        // naga's default flips gl_Position.y (wgpu's convention). We write
        // plain Vulkan GLSL, so turn that off.
        let mut options = spv::Options::default();
        options.flags.remove(spv::WriterFlags::ADJUST_COORDINATE_SPACE);
        let words = spv::write_vec(
            &module,
            &info,
            &options,
            Some(&spv::PipelineOptions { shader_stage: stage, entry_point: "main".into() }),
        )
        .unwrap_or_else(|e| panic!("{file}: {e}"));
        let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        std::fs::write(out.join(format!("{file}.spv")), bytes).unwrap();
    }
}
