//! What the two plugin menus hold: the status bar's instrument picker and the rail's `+`.
//!
//! **Columns before a scroll bar** (the owner, 2026-10-09: "make the menu have columns with the
//! plugin. and if that isnt enough then add the scroll bar"). With VST3 the scan finds hundreds of
//! plugins, and one column ran off the screen with no way to reach its end. So the rows flow down
//! a column and on into the next, as many columns as the screen's height needs and its width
//! allows; only past that does the menu scroll.
//!
//! **A search field heads the menu** and has the keyboard as it opens, so typing a few letters
//! finds a plugin among hundreds; `Enter` takes the first match. While it has focus the computer
//! keyboard is not an instrument (`keyboard.rs`), so typing plays nothing.
//!
//! **MXM's own plugins come first**, under their own heading, then everything else by name.
//!
//! **Refusals do not disappear, they move**, here one step further: into a submenu at the foot,
//! named with their count, each still disabled with its own reason. Listing a hundred refused rows
//! under the choices buried the choices.

use crate::discovery::Found;
use egui::{Align, Key, Layout, RichText, vec2};
use std::path::PathBuf;

/// One column's width: room for most plugin names; a longer one is cut short, and its hover says
/// it in full.
const COLUMN_WIDTH: f32 = 210.0;
const COLUMN_GAP: f32 = 12.0;
/// What the menu needs besides its rows: the search field, separators, trailing rows and the
/// window's own margins.
const RESERVED_HEIGHT: f32 = 150.0;
/// The id prefix every MXM plugin's CLAP id carries (mxm-kit's plugin conventions,
/// *Permanent identifiers*).
const MXM_ID_PREFIX: &str = "dk.mxm.";

/// What one menu says in its own words.
pub(super) struct Words<'a> {
    /// Shown when nothing at all can be chosen.
    pub nothing: &'a str,
    /// After the refused count: "cannot be loaded", "cannot be an effect".
    pub refused: &'a str,
}

/// One row of a column.
enum Row<'a> {
    Heading(&'static str),
    Plugin { found: &'a Found, text: String },
}

/// The menu's contents: a search field, the plugins `offered` admits in columns, `trailing`, then
/// the refused ones in a submenu. Returns the bundle and id of the plugin chosen this frame.
pub(super) fn contents(
    ui: &mut egui::Ui,
    salt: &str,
    found: &[Found],
    offered: impl Fn(&Found) -> bool,
    refusal: impl Fn(&Found) -> Option<String>,
    words: &Words<'_>,
    trailing: impl FnOnce(&mut egui::Ui),
) -> Option<(PathBuf, String)> {
    let id = search_id(salt);
    let mut query = ui.data_mut(|d| d.get_temp::<String>(id).unwrap_or_default());
    let field = ui.add(
        egui::TextEdit::singleline(&mut query)
            .hint_text("Search")
            .desired_width(COLUMN_WIDTH),
    );
    if ui.memory(|m| m.focused().is_none()) {
        field.request_focus();
    }
    let entered = field.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
    ui.data_mut(|d| d.insert_temp(id, query.clone()));
    let needle = query.trim().to_lowercase();

    let duplicates = crate::discovery::duplicated_ids(found);
    let mut choices: Vec<&Found> = found
        .iter()
        .filter(|f| offered(f) && matches(f, &needle))
        .collect();
    choices.sort_by_cached_key(|f| (!is_mxm(f), f.name.to_lowercase()));

    let mut chosen = None;
    if entered && let Some(first) = choices.first() {
        chosen = Some((first.bundle.clone(), first.id.clone()));
    }

    ui.separator();
    if choices.is_empty() {
        if needle.is_empty() {
            ui.weak(words.nothing);
        } else {
            ui.weak(format!("Nothing matches “{}”.", query.trim()));
        }
    } else {
        let mut rows = Vec::with_capacity(choices.len() + 2);
        let mxm = choices.iter().filter(|f| is_mxm(f)).count();
        if mxm > 0 && mxm < choices.len() {
            rows.push(Row::Heading("MXM"));
        }
        for (index, found) in choices.iter().copied().enumerate() {
            if index == mxm && mxm > 0 {
                rows.push(Row::Heading("Other"));
            }
            // Two bundles with one id are two builds of the same plugin, and one is usually stale.
            // The location is the only thing that tells them apart, so it is shown for exactly that
            // case rather than for every row.
            let text = if duplicates.contains(&found.id) {
                format!("{}  ·  {}", found.name, found.location())
            } else {
                found.name.clone()
            };
            rows.push(Row::Plugin { found, text });
        }
        if let Some(found) = columns(ui, &rows, true, |found| match found.hover().as_str() {
            "" => found.name.clone(),
            hover => format!("{}\n{hover}", found.name),
        }) {
            chosen = Some((found.bundle.clone(), found.id.clone()));
        }
    }

    trailing(ui);

    let mut refused: Vec<&Found> = found
        .iter()
        .filter(|f| !offered(f) && matches(f, &needle))
        .collect();
    refused.sort_by_cached_key(|f| f.name.to_lowercase());
    if !refused.is_empty() {
        ui.separator();
        ui.menu_button(format!("{} {}", refused.len(), words.refused), |ui| {
            let rows: Vec<_> = refused
                .iter()
                .map(|&found| Row::Plugin {
                    found,
                    text: found.name.clone(),
                })
                .collect();
            columns(ui, &rows, false, |found| match refusal(found) {
                Some(reason) => format!("{}\n{}\n{reason}", found.name, found.location()),
                None => format!("{}\n{}", found.name, found.location()),
            });
        });
    }

    if chosen.is_some() {
        ui.close();
    }
    chosen
}

/// Forgets a menu's search once it has closed, so the next opening starts with every plugin.
pub(super) fn forget_search(ctx: &egui::Context, salt: &str) {
    ctx.data_mut(|d| d.remove::<String>(search_id(salt)));
}

fn search_id(salt: &str) -> egui::Id {
    egui::Id::new(("plugin-menu-search", salt))
}

fn is_mxm(found: &Found) -> bool {
    found.id.starts_with(MXM_ID_PREFIX)
}

/// Whether a plugin matches a lower-cased search: in its name, its maker or its id.
fn matches(found: &Found, needle: &str) -> bool {
    needle.is_empty()
        || found.name.to_lowercase().contains(needle)
        || found.vendor.to_lowercase().contains(needle)
        || found.id.to_lowercase().contains(needle)
}

/// Lays `rows` out in columns, and returns the plugin clicked. Disabled rows carry their hover
/// as the reason they are disabled.
fn columns<'a>(
    ui: &mut egui::Ui,
    rows: &[Row<'a>],
    enabled: bool,
    hover: impl Fn(&Found) -> String,
) -> Option<&'a Found> {
    let row_height = ui.spacing().interact_size.y + ui.spacing().item_spacing.y;
    let screen = ui.ctx().content_rect();
    let rows_fit = ((screen.height() - RESERVED_HEIGHT) / row_height)
        .floor()
        .max(1.0) as usize;
    let max_columns = ((screen.width() - COLUMN_GAP) / (COLUMN_WIDTH + COLUMN_GAP))
        .floor()
        .max(1.0) as usize;
    let layout = column_layout(rows.len(), rows_fit, max_columns);

    let mut clicked = None;
    let mut draw = |ui: &mut egui::Ui| {
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = COLUMN_GAP;
            for column in rows.chunks(layout.rows.max(1)) {
                ui.allocate_ui_with_layout(
                    vec2(COLUMN_WIDTH, 0.0),
                    Layout::top_down_justified(Align::LEFT),
                    |ui| {
                        ui.set_width(COLUMN_WIDTH);
                        for row in column {
                            match row {
                                Row::Heading(text) => {
                                    ui.label(RichText::new(*text).weak().small());
                                }
                                Row::Plugin { found, text } => {
                                    let button = egui::Button::new(text).truncate();
                                    let response = ui.add_enabled(enabled, button);
                                    let tip = hover(found);
                                    if enabled {
                                        if response.on_hover_text(tip).clicked() {
                                            clicked = Some(*found);
                                        }
                                    } else {
                                        response.on_disabled_hover_text(tip);
                                    }
                                }
                            }
                        }
                    },
                );
            }
        });
    };
    if layout.scroll {
        egui::ScrollArea::vertical()
            .max_height(rows_fit as f32 * row_height)
            .show(ui, |ui| draw(ui));
    } else {
        draw(ui);
    }
    clicked
}

/// How a menu's rows are laid out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ColumnLayout {
    columns: usize,
    /// Rows in each column but the last.
    rows: usize,
    scroll: bool,
}

/// As few columns as hold `n` rows at `rows_fit` a column, the rows shared out evenly; when even
/// `max_columns` cannot hold them, `max_columns` of them and a scroll bar.
fn column_layout(n: usize, rows_fit: usize, max_columns: usize) -> ColumnLayout {
    let (rows_fit, max_columns) = (rows_fit.max(1), max_columns.max(1));
    if n == 0 {
        return ColumnLayout {
            columns: 1,
            rows: 0,
            scroll: false,
        };
    }
    let columns = n.div_ceil(rows_fit);
    if columns <= max_columns {
        ColumnLayout {
            columns,
            rows: n.div_ceil(columns),
            scroll: false,
        }
    } else {
        ColumnLayout {
            columns: max_columns,
            rows: n.div_ceil(max_columns),
            scroll: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_list_is_one_column() {
        assert_eq!(
            column_layout(12, 40, 6),
            ColumnLayout {
                columns: 1,
                rows: 12,
                scroll: false
            }
        );
    }

    #[test]
    fn a_long_list_takes_as_few_columns_as_fit_and_shares_the_rows_out() {
        // 130 rows at 40 a column is four columns; 33, 33, 33 and 31 rather than 40, 40, 40, 10.
        assert_eq!(
            column_layout(130, 40, 6),
            ColumnLayout {
                columns: 4,
                rows: 33,
                scroll: false
            }
        );
    }

    #[test]
    fn past_the_widest_the_screen_allows_it_scrolls() {
        assert_eq!(
            column_layout(500, 40, 6),
            ColumnLayout {
                columns: 6,
                rows: 84,
                scroll: true
            }
        );
    }

    #[test]
    fn nothing_and_degenerate_sizes_still_lay_out() {
        assert_eq!(column_layout(0, 40, 6).rows, 0);
        assert_eq!(column_layout(5, 0, 0).columns, 1);
    }
}
