# regia_steam

Steam integration for Regia Engine (`docs/specs/24-steam.md`). Desktop only, opt-in. The umbrella
crate's `steam` feature (`regia = { features = ["steam"] }`) re-exports this crate as
`regia::steam`; the real client is a separate switch, this crate's `steamworks` feature (add
`regia_steam` as a dependency of the game with `features = ["steamworks"]`), which links Valve's Steamworks SDK through the open-source `steamworks` 0.13
bindings (MIT/Apache-2.0).

## Where the Steamworks SDK goes (never committed to this repository)

`steamworks-sys` compiles and links against Valve's **Steamworks SDK**, which is distributed under
the Steamworks SDK Access Agreement — not under an open-source licence. The SDK and its
redistributable libraries (`libsteam_api.so`, `steam_api64.dll`, `libsteam_api.dylib`) must
**never be committed to this repository or any project built on it**.

To develop or build with the `steam` feature:

1. Download the Steamworks SDK from <https://partner.steamgames.com/> (a Steam partner account is
   required). Any 1.58+ SDK works; `steamworks` 0.13 was generated against SDK 1.64.
2. Unzip it somewhere outside the repository, e.g. `~/sdk/steamworks_1.64/`.
3. Point `STEAM_SDK_LOCATION` at that directory when building:

   ```sh
   STEAM_SDK_LOCATION=~/sdk/steamworks_1.64 cargo build -p my_game --features regia_steam/steamworks
   ```

   `steamworks-sys`'s build script copies the redistributable from
   `<STEAM_SDK_LOCATION>/redistributable_bin/{linux64,win64,osx}` into its build output and links
   against it; the pre-generated bindings it ships need no headers.

Without `STEAM_SDK_LOCATION` the build of the `steamworks` feature fails with a copy error from
`steamworks-sys` — that is expected; the feature stays off by default for exactly this reason.

## Redistribution

- The `steamworks`/`steamworks-sys` *crate code* is MIT/Apache-2.0 and appears in
  `THIRD_PARTY_NOTICES.md`. The **SDK header files inside `steamworks-sys/lib/steam/` are excluded
  from that licence** (the crate's own README states this) and remain under Valve's agreement.
- The SDK redistributable ships **only inside Steam builds**: `regia package --steam` copies
  `libsteam_api.so` / `steam_api64.dll` next to the game binary in the depot when
  `STEAM_SDK_LOCATION` is set, and prints instructions otherwise.
- The runtime `libsteam_api.so` must be findable next to the Linux game binary; the launcher
  should link with an `$ORIGIN` rpath (`-C link-arg=-Wl,-rpath,$ORIGIN`) so "next to the binary"
  works on Linux (on Windows the executable's directory is already on the loader path).

## Licence note for games

Valve's "Distributing Open Source Applications on Steam" guidance: permissive licences
(MIT, BSD, Apache-2.0) combine cleanly with the Steamworks SDK; strong copyleft (GPL) generally
does not. Games that enable this feature must be licensable accordingly.
