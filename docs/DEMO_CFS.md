# Demo: a stock NASA cFS bundle streaming into zenith

What you get: the six core apps' housekeeping charting live in
zenith, 87 named channels decoded from dictionaries pulled out of
the flight binary's own debug info, with zenith arming the downlink
the moment it connects. Commanding a cFS target beyond that arm step
is not in this release; the command pages answer with "unsupported
for this protocol".

Everything below was followed as written to produce the bundled
`targets/cfs-cpu1` directory and the live acceptance of this
release, against the cFS bundle at its main branch of 2026-09.

## 1. Build the stock bundle

Any Linux host with CMake, a C compiler and gdb (the dictionary
generator reads debug info through it). A container keeps the
toolchain off your machine; this is the one the release was verified
with:

```dockerfile
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential cmake git python3 gdb ca-certificates \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /cfs
CMD ["bash"]
```

cFS backs its software bus with POSIX message queues, and the kernel
default queue depth is below what the core apps create at boot, so
the container raises it. Host networking lets the bundle talk to
zenith on the loopback:

```bash
docker build -t cfs-rig .
git clone --recurse-submodules https://github.com/nasa/cFS.git cfs
docker run -it --rm --network host --sysctl fs.mqueue.msg_max=1024 \
  -v "$PWD/cfs:/cfs" cfs-rig bash
```

Inside, the bundle's own build steps, unchanged. The bundle ships
its Makefile and `sample_defs/` at the top level with named
configurations; `native_std` is the native simulation build:

```bash
make native_std.prep      # configures the build tree
make native_std.install   # compiles and stages the executable
```

The bundle leaves the executive at
`build-native_std/exe/cpu1/core-cpu1`, with the core apps as shared
objects beside it and full debug info (DWARF), which is what the
next step reads.

## 2. Generate the zenith target directory

The generator walks the debug info of the binaries you just built
and writes the target directory: one struct dictionary per app with
exact payload layouts, the manifest, plot layouts, and the connect-
time step that enables the downlink. A spec file beside your
checkout names the apps, their telemetry structs and APIDs; the one
that produced the bundled directory is the shape to copy:

```json
{
  "application": "cFS cpu1",
  "on_connect": [
    {
      "name": "Enable TO downlink",
      "cmd_mid": "0x1880",
      "cc": 6,
      "payload_ascii": "127.0.0.1",
      "pad_to": 16
    }
  ],
  "entries": [
    {
      "component": "cfe_es",
      "name": "CFE_ES",
      "elf": "cfs/build-native_std/exe/cpu1/core-cpu1",
      "type": "CFE_ES_HousekeepingTlm_t",
      "apid": "0x000",
      "uid": "0x00C00000"
    }
  ]
}
```

```bash
python3 tools/cfs-dictgen/dictgen.py dictgen-cpu1.json targets/cfs-cpu1
```

It prints each app's payload size and field count, and the
`apid_map` block to paste into the target definition. Rerun it after
any rebuild: the dictionaries come from the binaries, so they cannot
drift from the wire.

## 3. Declare the target

```toml
[[targets]]
name = "cFS cpu1"
host = "127.0.0.1"                 # CI_LAB: commands and the arm step go here
port = 1234
protocol = "ccsds-spp"
carrier = "udp"
listen_port = 2234                 # TO_LAB pushes telemetry here
auto_connect = true
manifest = "/data/targets/cfs-cpu1/app_manifest.json"
structs_dir = "/data/targets/cfs-cpu1/structs"
telemetry_config = "/data/targets/cfs-cpu1/telemetry.json"
connect_init = "/data/targets/cfs-cpu1/on_connect.json"
[targets.apid_map]
"0x000" = "0x00C00000" # CFE_ES
"0x001" = "0x00C00001" # CFE_EVS
"0x003" = "0x00C00003" # CFE_SB
"0x005" = "0x00C00005" # CFE_TIME
"0x080" = "0x00C00080" # TO_LAB
"0x084" = "0x00C00084" # CI_LAB
```

The bundled image already carries this target and its directory, so
with the published image there is nothing to write.

## 4. Run

Start the bundle from its executable directory (it reads its
startup script from there):

```bash
cd build-native_std/exe/cpu1 && ./core-cpu1
```

It logs "Awaiting enable command": a stock cFS is silent until the
ground asks. Then zenith:

```bash
docker run -d --name zenith --network host -v zenith-data:/var/lib/zenith \
  ghcr.io/apexedgesystems/zenith:latest
```

On connect, zenith sends the generated step to CI_LAB, the bundle
logs "TO telemetry output enabled for IP 127.0.0.1", and the
Telemetry page's default layout charts mission time, the ground
link counters, event services and the software bus. The dashboard's
component list shows all six apps green while their housekeeping
arrives.

## What to expect

- Housekeeping arrives at 1 Hz per app; the first samples land
  within a second of the arm.
- Apps the bundle subscribes to TO_LAB but that are not in the
  dictionary spec count as unroutable units in the log, at a low
  rate. Add them to the spec to name them.
