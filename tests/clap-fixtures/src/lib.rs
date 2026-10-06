//! CLAP plugins that exist only to be refused, to misbehave, or to emit things a well-behaved
//! plugin never would — and one that behaves: `dk.mxm.fixture.effect`, the reference effect the
//! player's effect hosting is proved against precisely because it shares no code with the
//! collection.
//!
//! One `cdylib` exposing several plugin IDs from a single factory: one Cargo package with one
//! `cdylib` cannot otherwise produce several libraries. Fixtures are loaded **by path** by the
//! player's tests, never through its scanner, so a deliberately hostile fixture cannot end up in
//! an ordinary scan.
//!
//! Build and stage with `cargo xtask fixtures`.

use clack_plugin::entry::prelude::*;
use clack_plugin::prelude::*;
use std::ffi::CStr;

mod fixture;
mod spec;

pub use fixture::{ECHO_DELAY_FRAMES, ECHO_FEEDBACK, EFFECT_TAIL_FRAMES, PARAM_ID};
pub use spec::{Behaviour, FIXTURES, FixtureSpec, find};

/// The entry point exposed by the built library.
pub struct FixtureEntry {
    plugin_factory: PluginFactoryWrapper<FixtureFactory>,
}

impl Entry for FixtureEntry {
    fn new(_bundle_path: Option<&CStr>) -> Result<Self, EntryLoadError> {
        Ok(Self {
            plugin_factory: PluginFactoryWrapper::new(FixtureFactory::new()),
        })
    }

    fn declare_factories<'a>(&'a self, builder: &mut EntryFactories<'a>) {
        builder.register_factory(&self.plugin_factory);
    }
}

/// Exposes every entry of [`spec::FIXTURES`], in table order.
pub struct FixtureFactory {
    descriptors: Vec<PluginDescriptor>,
}

impl FixtureFactory {
    fn new() -> Self {
        Self {
            descriptors: spec::FIXTURES
                .iter()
                .map(|spec| {
                    PluginDescriptor::new(spec.id, spec.name)
                        .with_vendor("MXM")
                        .with_version(env!("CARGO_PKG_VERSION"))
                        .with_description(spec.description)
                        .with_features(spec.features().iter().copied())
                })
                .collect(),
        }
    }
}

impl PluginFactoryImpl for FixtureFactory {
    fn plugin_count(&self) -> u32 {
        self.descriptors.len() as u32
    }

    fn plugin_descriptor(&self, index: u32) -> Option<&PluginDescriptor> {
        self.descriptors.get(index as usize)
    }

    fn create_plugin<'a>(
        &'a self,
        host_info: HostInfo<'a>,
        plugin_id: &CStr,
    ) -> Option<PluginInstance<'a>> {
        let index = self
            .descriptors
            .iter()
            .position(|d| d.id() == Some(plugin_id))?;
        let spec = &spec::FIXTURES[index];
        let descriptor = &self.descriptors[index];

        Some(PluginInstance::new::<fixture::Fixture>(
            host_info,
            descriptor,
            move |_host| Ok(fixture::new_shared(spec)),
            |host, shared| Ok(fixture::new_main_thread(host, shared)),
        ))
    }
}

clack_export_entry!(FixtureEntry);
