//! The host's view of a plugin's parameters.
//!
//! A snapshot, taken on the GUI thread, of what the `params` extension reports: name, range,
//! value, formatted text. The panel draws from this; it never reaches into the plugin mid-frame.
//!
//! **After a player-initiated state load the panel requeries every parameter**, rather than
//! waiting for a rescan callback. That is not belt-and-braces: `docs/known-issues.md` records
//! that nice-plug 0.3.0 never issues `params.rescan(VALUES)` after a host state load, so a
//! *correct* host that trusts the callback would show a stale panel with our own plugins.

use clack_extensions::params::{ParamInfoBuffer, ParamInfoFlags, PluginParams};
use clack_host::prelude::*;
use std::ffi::CString;

/// How much room a formatted value is given.
const TEXT_BUFFER: usize = 256;

/// One parameter, as the host currently understands it.
#[derive(Clone, Debug, PartialEq)]
pub struct ParamSnapshot {
    pub id: u32,
    pub name: String,
    pub module: String,
    pub min: f64,
    pub max: f64,
    pub default: f64,
    pub value: f64,
    /// The plugin's own formatting, which is what the label and the unit must come from.
    pub text: String,
    pub is_stepped: bool,
    pub is_hidden: bool,
    pub is_read_only: bool,
    pub is_bypass: bool,
    /// Whether the plugin accepts `CLAP_EVENT_PARAM_MOD` for this parameter.
    ///
    /// The sequencer sends a step's deviation as modulation, so a parameter that does not advertise
    /// it cannot be sequenced — see `PlayerApp::sequenceable`. The player hosts any CLAP, and
    /// sending modulation to a plugin that never claimed to support it is out of spec, not merely
    /// unlikely to work.
    pub is_modulatable: bool,
}

impl ParamSnapshot {
    /// Where the value sits in its range, for a slider.
    pub fn normalised(&self) -> f64 {
        if self.max <= self.min {
            return 0.0;
        }
        ((self.value - self.min) / (self.max - self.min)).clamp(0.0, 1.0)
    }

    pub fn denormalise(&self, normalised: f64) -> f64 {
        self.min + normalised.clamp(0.0, 1.0) * (self.max - self.min)
    }
}

/// Everything the panel needs, requeried whenever the plugin says it changed — and
/// unconditionally after a state load.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ParamSet {
    pub params: Vec<ParamSnapshot>,
}

impl ParamSet {
    pub fn get(&self, id: u32) -> Option<&ParamSnapshot> {
        self.params.iter().find(|p| p.id == id)
    }

    pub fn get_mut(&mut self, id: u32) -> Option<&mut ParamSnapshot> {
        self.params.iter_mut().find(|p| p.id == id)
    }

    /// Reads every parameter's metadata and value from the plugin.
    pub fn read<H: HostHandlers>(instance: &mut PluginInstance<H>) -> Self {
        let Some(ext): Option<PluginParams> = instance.plugin_shared_handle().get_extension()
        else {
            return Self::default();
        };

        let count = {
            let mut handle = instance.plugin_handle();
            ext.count(&mut handle)
        };

        let mut params = Vec::with_capacity(count as usize);
        let mut info_buffer = ParamInfoBuffer::new();

        for index in 0..count {
            let described = {
                let mut handle = instance.plugin_handle();
                ext.get_info(&mut handle, index, &mut info_buffer)
                    .map(|info| {
                        (
                            info.id,
                            String::from_utf8_lossy(info.name)
                                .trim_end_matches('\0')
                                .to_owned(),
                            String::from_utf8_lossy(info.module)
                                .trim_end_matches('\0')
                                .to_owned(),
                            info.min_value,
                            info.max_value,
                            info.default_value,
                            info.flags,
                        )
                    })
            };

            let Some((id, name, module, min, max, default, flags)) = described else {
                continue;
            };

            let value = {
                let mut handle = instance.plugin_handle();
                ext.get_value(&mut handle, id).unwrap_or(default)
            };
            let text = format_value(instance, &ext, id, value);

            params.push(ParamSnapshot {
                id: id.get(),
                name,
                module,
                min,
                max,
                default,
                value,
                text,
                is_stepped: flags.contains(ParamInfoFlags::IS_STEPPED),
                is_hidden: flags.contains(ParamInfoFlags::IS_HIDDEN),
                is_read_only: flags.contains(ParamInfoFlags::IS_READONLY),
                is_bypass: flags.contains(ParamInfoFlags::IS_BYPASS),
                is_modulatable: flags.contains(ParamInfoFlags::IS_MODULATABLE),
            });
        }

        Self { params }
    }

    /// Requeries values and formatted text only, leaving metadata alone.
    ///
    /// What a `rescan(VALUES)` asks for, and what a state load gets unconditionally.
    pub fn refresh_values<H: HostHandlers>(&mut self, instance: &mut PluginInstance<H>) {
        let Some(ext): Option<PluginParams> = instance.plugin_shared_handle().get_extension()
        else {
            return;
        };

        for param in &mut self.params {
            let Some(id) = ClapId::from_raw(param.id) else {
                continue;
            };
            let value = {
                let mut handle = instance.plugin_handle();
                ext.get_value(&mut handle, id)
            };
            if let Some(value) = value {
                param.value = value;
            }
            param.text = format_value(instance, &ext, id, param.value);
        }
    }
}

/// Asks the plugin to format a value, falling back to a plain number if it will not.
pub fn format_value<H: HostHandlers>(
    instance: &mut PluginInstance<H>,
    ext: &PluginParams,
    id: ClapId,
    value: f64,
) -> String {
    let mut buffer = [0u8; TEXT_BUFFER];
    let mut handle = instance.plugin_handle();

    match ext.value_to_text(&mut handle, id, value, &mut buffer) {
        Ok(written) => String::from_utf8_lossy(written)
            .trim_end_matches('\0')
            .to_owned(),
        Err(_) => format!("{value:.3}"),
    }
}

/// Parses typed-in text through the plugin, so direct entry means the same thing the display does.
pub fn parse_text<H: HostHandlers>(
    instance: &mut PluginInstance<H>,
    id: ClapId,
    text: &str,
) -> Option<f64> {
    let ext: PluginParams = instance.plugin_shared_handle().get_extension()?;
    let text = CString::new(text).ok()?;
    let mut handle = instance.plugin_handle();
    ext.text_to_value(&mut handle, id, &text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(min: f64, max: f64, value: f64) -> ParamSnapshot {
        ParamSnapshot {
            id: 0,
            name: "Cutoff".to_owned(),
            module: String::new(),
            min,
            max,
            default: value,
            value,
            text: String::new(),
            is_stepped: false,
            is_hidden: false,
            is_read_only: false,
            is_modulatable: true,
            is_bypass: false,
        }
    }

    #[test]
    fn normalisation_round_trips() {
        let param = snapshot(20.0, 20_000.0, 12_000.0);
        let normalised = param.normalised();
        assert!((param.denormalise(normalised) - 12_000.0).abs() < 1e-6);
    }

    #[test]
    fn a_degenerate_range_does_not_divide_by_zero() {
        let param = snapshot(1.0, 1.0, 1.0);
        assert_eq!(param.normalised(), 0.0);
        assert_eq!(param.denormalise(0.5), 1.0);
    }
}
