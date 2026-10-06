//! The thin client for the player's local CLI.
//!
//! Finds the running player through the port file its listener published beside the settings, sends
//! one command, prints one JSON answer. All the actual behaviour lives in the player; this binary is
//! deliberately nothing but transport, so a script or an AI driving the player has the same
//! vocabulary a person at the terminal does.
//!
//! ```text
//! mxm-cli dump
//! mxm-cli select 5
//! mxm-cli lock 5 cutoff 0.4
//! mxm-cli export night-bass
//! ```

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: mxm-cli <command> [args…]  — try `mxm-cli help`");
        std::process::exit(2);
    }

    // The same default the player uses, overridable for sandboxed instances.
    let settings_path = std::env::var_os("MXM_SETTINGS")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(mxm_player::settings::Settings::default_path);

    match mxm_player::cli::run_command(&settings_path, &args.join(" ")) {
        Ok(answer) => println!("{answer}"),
        Err(reason) => {
            eprintln!("{reason}");
            std::process::exit(1);
        }
    }
}
