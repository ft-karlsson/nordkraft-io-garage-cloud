# Energy transparency: watts and CO₂ per container

Every node is measured at the wall by a real meter. The agent on the node
reports how much CPU time each container used. The controller splits the
measured watts between the containers, prices each minute with Energinet's
grid CO₂ intensity, and the CLI shows the result:

```bash
nordkraft energy                 # account: watts now, kWh and CO₂ last 24 h, per container
nordkraft energy campfire --since 7d
nordkraft ps                     # "Power: 12.3 W" on each container
```

The per-container number is an attributed share, and the CLI says so in
every output. Nothing is estimated when the meter is silent: a node without
a fresh reading simply has no rows for that minute.

## Pieces

| piece | runs where | does |
| --- | --- | --- |
| `nordkraft-meterd` | wherever the meters are reachable (the controller for a Bluetooth plug, any LAN host for a PDU) | reads each meter every minute, publishes `nordkraft.energy.node` |
| agent (`container-api`, every node that runs containers) | the node | reads containerd task metrics every minute, publishes `nordkraft.energy.cpu` |
| controller (`container-api`) | the controller | joins the two, attributes, polls Energinet, rolls up hourly, serves `/api/energy` |

Wire format on `nordkraft.energy.node` (plain JSON, anything can produce it):

```json
{"node_id":"ry-optiplex-1","ts":"2026-11-02T21:40:00Z","watts":27.3,"energy_wh":12345.6,"source":"shelly-ble"}
```

## Attribution

```text
idle_w      = nodes.power_idle_watts        (what the box draws with no containers)
dynamic_w   = node_w − idle_w
container_w = idle_w × vcpu_i / Σ vcpu  +  dynamic_w × cpu_time_i / Σ cpu_time
```

The idle part is split by reserved vCPU (`--cpus` at deploy), so a sleeping
container still carries its slice of the box being on. The dynamic part
follows the CPU time actually used in the interval. For a Kata container the
CPU time is the sandbox cgroup, VMM included, so the isolation's own cost
lands on the tenant that asked for it. Containers the controller does not
know (a registry, a leftover) take part in the split but get no rows: that
is platform overhead and is nobody's.

Without a baseline (`power_idle_watts` NULL) everything is split by CPU
time; the method is stored with every row and the API reports it.

### Measuring the idle baseline

Once per node, with no tenant containers running (stop them, or do it
before the node joins), read the meter for a few minutes and take the
average. Then:

```sql
UPDATE nodes SET power_idle_watts = 11.5 WHERE node_id = 'ry-optiplex-1';
```

Redo it after a hardware change. A WiFi plug that cannot join the LAN is
still fine for this: put the node on it at the bench and read the value from
the Shelly app.

## Meters

`nordkraft-meterd` reads `/etc/nordkraft/meters.toml` (see
`examples/meters.toml`):

```toml
nats_url = "nats://10.0.0.200:4222"
interval_seconds = 60

[[meter]]
node_id = "ry-optiplex-1"
kind = "shelly-ble"
address = "AA:BB:CC:DD:EE:FF"      # bluetoothctl shows it; `nordkraft-meterd scan` too
```

| kind | device | fields |
| --- | --- | --- |
| `shelly-ble` | Shelly Gen2/Gen3 plug or Pro over Bluetooth (no network at all) | `address` or `name`, `component` (`switch`/`em1`), `component_id` |
| `shelly-http` | Shelly Gen2/Gen3 on the LAN (Pro line has a wired port) | `url`, `password` (device auth, user is `admin`), `component`, `component_id` |
| `netio` | NETIO PowerBOX / PowerPDU, one outlet per node | `url`, `outlet` (1-based), `username`, `password` |

`component = "switch"` reads `Switch.GetStatus` (`apower`, `aenergy.total`);
`component = "em1"` reads `EM1.GetStatus` and `EM1Data.GetStatus` (Pro EM,
Pro 3EM in monophase profile: one clamp per node, `component_id` 0..2).

Commands:

```bash
nordkraft-meterd probe                  # read every meter once, print the samples, no NATS
nordkraft-meterd probe ry-optiplex-1    # one node
nordkraft-meterd scan                   # Bluetooth: what advertises the Shelly RPC service
nordkraft-meterd                        # run (the service)
```

`probe` also builds and runs on a Mac, so a Bluetooth plug can be checked
from a laptop before anything is installed on the controller.

### Shelly over Bluetooth, step by step

The plug never joins a network. It talks to one bonded host, and that host
reads it once a minute.

1. On the plug (Shelly app, or `http://<plug>/rpc/...` while it is still
   on its setup access point): turn WiFi station mode off, Shelly Cloud off,
   and Bluetooth RPC on (`BLE.SetConfig {"config":{"rpc":{"enable":true}}}`).
   Set device authentication if the plug will ever see a network again.
2. On the reader host (the controller): `apt install bluez`, then
   `systemctl enable --now bluetooth`.
3. Open a pairing window on the plug (`BLE.StartPairing`, 180 s; the Shelly
   app has a button for it), then on the host:

   ```bash
   bluetoothctl scan on          # note the plug's address, name ShellyPlugSG3-...
   bluetoothctl pair AA:BB:CC:DD:EE:FF
   bluetoothctl trust AA:BB:CC:DD:EE:FF
   ```

4. `nordkraft-meterd probe` prints the sample, or says what is missing.
5. Install the service:

   ```ini
   # /etc/systemd/system/nordkraft-meterd.service
   [Unit]
   Description=Nordkraft meter reader (watts per node)
   After=network-online.target bluetooth.service
   Wants=network-online.target

   [Service]
   ExecStart=/usr/local/bin/nordkraft-meterd
   Restart=always
   RestartSec=10
   Environment=RUST_LOG=info

   [Install]
   WantedBy=multi-user.target
   ```

Range: Bluetooth Low Energy wants the plug within a few metres of the host
with no closed metal cabinet between them. A dropped link shows up as a
missing minute, never as a wrong number.

### Scaling to more nodes

One `meters.toml` entry per node. A Bluetooth reader handles a handful of
plugs in one room; a PDU handles as many outlets as it has; a second site
runs its own `nordkraft-meterd` next to its meters and publishes to the same
NATS. The controller only sees `node_id` and watts.

## Grid CO₂

The controller polls Energinet's Energi Data Service every five minutes,
no key needed:

- `CO2Emis`: measured 5-minute intensity, gram CO₂ per kWh, per price area,
  a few minutes behind the clock;
- `CO2EmisProg`: the forecast, used for a minute whose measured value has
  not arrived yet.

Each node has a `price_area` (`DK1` west of Storebælt, `DK2` east; default
`DK1`). If Energinet is unreachable the minute is stored without CO₂ and
the CLI says how many intervals were left unpriced.

## Tables

`setup/migration_energy.sql`, applied by the controller on start:

| table | holds | kept |
| --- | --- | --- |
| `node_power_samples` | one row per meter reading | 30 days |
| `container_energy_minutes` | attributed watts, Wh and CO₂ per container per interval | 7 days |
| `container_energy_hourly` | rollup of the minute rows; the invoice-grade record | forever |
| `grid_co2` | Energinet values per price area | forever (forecasts a day) |

The per-container tables reference `users` with `ON DELETE CASCADE`, so an
account purge takes its energy history with it.

## Configuration

Both roles (`/etc/nordkraft/controller.env`, the agent's env):

| variable | default | meaning |
| --- | --- | --- |
| `ENERGY_ENABLED` | `true` | `false` turns the sampler, collector and poller off |
| `ENERGY_SAMPLE_INTERVAL` | `60` | seconds between the agent's CPU samples (min 10) |

Controller only:

| variable | default | meaning |
| --- | --- | --- |
| `ENERGY_PRICE_AREA` | `DK1` | area for the "grid now" line and for nodes without one |
| `ENERGINET_API_URL` | `https://api.energidataservice.dk` | |

## API

| route | returns |
| --- | --- |
| `GET /api/energy?since=24h` | account: `now.watts`, `total.wh`, `total.co2_g`, per container, `grid`, `method` |
| `GET /api/containers/<name>/energy?since=24h` | one container, plus `hourly[]` |
| `GET /api/usage` | gains an `energy` block (live watts, 24 h totals, grid now) |
| `GET /api/containers` | each container gains `watts` when its node is metered |

Windows: `90m`, `24h`, `7d`, `30d` (up to 400 days; over seven days the
hourly table is read).

## Rollout order

1. Controller: new binary (applies the migration, starts the collector and
   the Energinet poller). Nothing shows yet.
2. Agents: new binary. They start publishing CPU samples at once; the
   controller ignores them until a meter reports.
3. Set `power_idle_watts` per node.
4. Meter reader: `nordkraft-meterd` with its `meters.toml` and unit on the
   host that reaches the meters. From the first reading on, `nordkraft
   energy` fills in within two minutes.
5. New CLI.

## Verifying

```bash
# controller journal: both subscriptions, then one line per interval at debug
journalctl -u container-api -f | grep -i energy

# readings arriving
sudo -u postgres psql garage_cloud -c "select * from node_power_samples order by ts desc limit 5"
sudo -u postgres psql garage_cloud -c "select container_name, ts, watts, wh, co2_g, method from container_energy_minutes order by ts desc limit 10"
sudo -u postgres psql garage_cloud -c "select * from grid_co2 where not forecast order by ts desc limit 3"
```

The sum of a node's container rows for one minute is at most the node's
reading for that minute; the difference is platform overhead and
containers the controller does not know.
