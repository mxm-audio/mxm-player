//! MXM Player - a CLAP host for testing and playing the MXM Synth Collection.

use mxm_player::ui::{MIN_SIZE, PlayerApp, REFERENCE_SIZE};

/// The graphics backends the player will accept, and the reason it is a restricted set.
///
/// The player renders through `wgpu` so that it shares nothing with a plugin editor rendering
/// through OpenGL in the same process — see `docs/known-issues.md` in mxm-kit. **But wgpu has its
/// own GL backend**, so asking for wgpu is not by itself asking for something that is not OpenGL:
/// if it fell back to GL the original conflict would return with no visible sign that anything had
/// changed. Naming the three explicitly is what makes the separation a property of the build
/// rather than a hope about adapter selection.
const BACKENDS: eframe::wgpu::Backends = eframe::wgpu::Backends::DX12
    .union(eframe::wgpu::Backends::VULKAN)
    .union(eframe::wgpu::Backends::METAL);

fn main() -> eframe::Result<()> {
    // A copy of this program started to read a VST3 module's plugins does that and exits.
    mxm_vst3_host::serve_probe();
    let mut wgpu_options = eframe::egui_wgpu::WgpuConfiguration::default();
    if let eframe::egui_wgpu::WgpuSetup::CreateNew(setup) = &mut wgpu_options.wgpu_setup {
        setup.instance_descriptor.backends = BACKENDS;
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size(REFERENCE_SIZE)
            .with_min_inner_size([MIN_SIZE.0, MIN_SIZE.1])
            // The first window never exceeds the monitor it opens on. The remembered size is
            // applied a frame later and cannot be clamped here — egui reports no monitor size —
            // so `remember_geometry` refuses to record a maximised or fullscreen one instead.
            .with_clamp_size_to_monitor_size(true)
            .with_title("MXM Player"),
        wgpu_options,
        ..Default::default()
    };

    let result = eframe::run_native(
        "MXM Player",
        options,
        Box::new(|cc| {
            // Which adapter was actually chosen, once, at startup. Without this there is no way to
            // tell D3D12 from GL by looking — and telling them apart is the entire point of
            // `BACKENDS`, so the restriction needs something that reports whether it took.
            if let Some(state) = &cc.wgpu_render_state {
                let info = state.adapter.get_info();
                eprintln!(
                    "MXM Player: rendering on {:?} via {} ({})",
                    info.backend, info.name, info.driver
                );
            }
            Ok(Box::new(PlayerApp::new(cc)))
        }),
    );

    // A machine with no D3D12, Vulkan or Metal adapter cannot run the player, where the old OpenGL
    // backend would have limped along on a software implementation. That is an accepted cost of not
    // sharing a graphics API with plugin editors — but it must **say so**, because eframe's own
    // failure here is opaque and looks like a crash rather than an unmet requirement.
    if let Err(error) = &result {
        eprintln!(
            "MXM Player could not start: {error}\n\
             \n\
             It needs a Direct3D 12, Vulkan or Metal capable graphics adapter. OpenGL is\n\
             deliberately not used: a plugin's editor renders through OpenGL in this same process,\n\
             and two OpenGL renderers corrupt each other's output.\n\
             \n\
             On a virtual machine or a system without a working graphics driver, installing or\n\
             enabling a Vulkan runtime is usually what is missing."
        );
    }

    result
}
