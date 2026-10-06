# Trademarks

**MXM™** and the MXM logo are trademarks of mxm ([mxm.dk](https://mxm.dk)). They name the
instruments, effects and software published by mxm.

The code in this repository is free software under the GNU General Public License, version 3 or
later (`LICENSE`). That licence covers the code. **It grants no rights to the MXM name or logo**
(GPL-3.0 §7(e) allows exactly this).

## What you may do

- Build, use, change and redistribute this code, and binaries you build from it, under the GPL.
- Say truthfully what your work is based on or compatible with: "based on MXM Mono-01", "loads
  MXM control maps", "compatible with MXM Player".
- Use the names of the MIT-licensed kit crates (`mxm-ui`, `mxm-preset` and the others in
  [mxm-kit](https://github.com/mxm-audio/mxm-kit)) when you build with them.

## What you must do when you distribute builds

When you distribute binaries — changed or not — that you built yourself:

- **Give them your own names.** Not `mxm-…`, not "MXM Player", not "MXM Synth Collection", and
  nothing that could be mistaken for them.
- **Change the vendor, the IDs and the contact details.** The vendor (`mxm`), the CLAP plugin IDs
  (`dk.mxm.*`, built from mxm's own domain, which would also clash with the official plugins in a
  host), the URL (`mxm.dk`) and the email (`plugins@mxm.dk`). In each plugin they sit together at
  the top of its `src/lib.rs`.
- **Leave out the MXM logo and artwork.**
- **Don't suggest that mxm made, endorses or supports your version.**

The official builds are the ones published by mxm. Questions about the name: plugins@mxm.dk.
