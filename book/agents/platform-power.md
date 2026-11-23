# Platform power state

The host power supply (ADR-3997). Spec 11 names `PowerState` and does not define the fields. This page does.

## Reading it

`platform.power_state` takes an optional `dir` string (at most 4096 bytes). Omit it or pass `null` to read the host. The call returns one synchronous reading with no timestamp.

| Field | Meaning |
|---|---|
| `source` | `battery`, `ac`, or `unknown`. |
| `percent` | An integer from 0 through 100, or `null` when the platform does not report a percent or the value is outside that range. |
| `low_power` | `true` only when `source` is `battery` and `percent` is a number below 20 (`LOW_POWER_BELOW_PERCENT`). |

Linux, with `dir` omitted, reads `/sys/class/power_supply` (`LINUX_POWER_SUPPLY_CLASS`). A directory that contains a `type` file is one supply. Otherwise each child directory is a supply, and symlinks are followed. `type` text `Battery` and `UPS` are `battery`. `Mains`, `USB`, and `Wireless` are `ac`. Any other text is `unknown`. `capacity` is the percent when the trimmed text is ASCII digits and the integer is 0 through 100. `status` (`Unknown`, `Charging`, `Discharging`, `Not charging`, `Full`) is read and does not change the three fields. A supply whose `scope` is `Device` (a peripheral's battery: a wireless mouse, a gamepad) or whose `present` is `0` (an empty battery slot) is not the host supply and reads as `unknown`. A battery supply is kept over an AC supply. Among supplies of the same kind, the lowest directory name is kept.

Android reads the sticky `ACTION_BATTERY_CHANGED` intent (`BatteryManager` extras `EXTRA_PRESENT`, `EXTRA_PLUGGED`, `EXTRA_LEVEL`, `EXTRA_SCALE`, `EXTRA_STATUS`). A present battery stays `battery` while the device is plugged in. Percent is `level * 100 / scale` when that integer is 0 through 100. Absent and plugged is `ac` with a null percent. A JNI failure answers `unknown` and is not `power_unavailable`.

Windows, macOS, and other hosts answer `source` `unknown`, `percent` null, and `low_power` false. They do not open a device.

Passing `dir` reads that directory on every host and does not open the host class path. Tests use a temporary directory. `dir` must lie under the allowed roots of [Safety](safety.md) (the project, the temp directory or an `output_roots` entry; no `..` or symlink leading out, no `\\host\share` form), and only regular files in it are read, so a FIFO cannot block the call (ADR-5869). An empty `dir` or one holding NUL still answers `power_unavailable`.

## Errors

| Code | When |
|---|---|
| `power_unavailable` | The directory cannot be opened, or the path is not a directory. Missing attribute files are not this error: they answer `unknown`. An attribute that is not a regular file (a FIFO or a device) reads as missing and is never opened. |
| `F0002` | `dir` is non-null and is not a string, or `dir` is longer than 4096 bytes, or `dir` lies outside the allowed roots (`data.reason` `path_outside_project`). |

## Rust

`regia_platform::PowerState`, `PowerSource`, `read_power_supply_dir`, and `power_state`. `PowerSource` is `Unknown`, `Battery`, or `Ac`.
