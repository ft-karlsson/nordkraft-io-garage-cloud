# Self-Hosting nordkraft.io Garage Cloud

> **Status:** Self-hosting installer coming in MARK II. This guide covers manual setup for contributors and early self-hosters. If you'd rather skip the setup, [cloud.nordkraft.io](https://cloud.nordkraft.io) runs the same stack on dedicated Danish hardware.

**Requirements:** Ubuntu 24.04, x86_64, CPU with VT-x/AMD-V, 4GB+ RAM.

---

## 1. PostgreSQL

```bash
sudo apt install -y postgresql postgresql-contrib

# Create user and database
sudo -u postgres psql <<EOF
CREATE USER garage_user WITH PASSWORD 'changeme';
CREATE DATABASE garage_cloud OWNER garage_user;
GRANT ALL PRIVILEGES ON DATABASE garage_cloud TO garage_user;
EOF

# Apply schema
sudo -u postgres psql -d garage_cloud -f schema.sql
```

The schema creates all tables, functions, and indexes. It expects `garage_user` to own most objects — the `psql` session above handles that.

Set your connection string:

```bash
export DATABASE_URL=postgresql://garage_user:changeme@localhost:5432/garage_cloud
```

---

## 2. NATS

```bash
# Download NATS server
curl -L -o /tmp/nats-server.zip \
  https://github.com/nats-io/nats-server/releases/download/v2.10.18/nats-server-v2.10.18-linux-amd64.zip
unzip /tmp/nats-server.zip -d /tmp/
sudo mv /tmp/nats-server-v2.10.18-linux-amd64/nats-server /usr/local/bin/

# Run (or add to systemd — see below)
nats-server -p 4222 &
```

For production, create a systemd unit:

```bash
sudo tee /etc/systemd/system/nats.service > /dev/null <<EOF
[Unit]
Description=NATS Server
After=network.target

[Service]
ExecStart=/usr/local/bin/nats-server -p 4222
Restart=always
User=nobody

[Install]
WantedBy=multi-user.target
EOF

sudo systemctl enable --now nats
```

---

## 3. Kata Containers

Check that your CPU supports hardware virtualisation before starting:

```bash
egrep -c '(vmx|svm)' /proc/cpuinfo   # must be > 0
ls /dev/kvm                           # must exist
```

Install:

```bash
sudo apt install -y zstd containerd

# Download and extract Kata 3.25.0
curl -L -o /tmp/kata-static.tar.zst \
  https://github.com/kata-containers/kata-containers/releases/download/3.25.0/kata-static-3.25.0-amd64.tar.zst
sudo tar -I zstd -xf /tmp/kata-static.tar.zst -C /

# Symlinks
sudo ln -sf /opt/kata/bin/kata-runtime /usr/local/bin/
sudo ln -sf /opt/kata/bin/containerd-shim-kata-v2 /usr/local/bin/

# Kernel modules (persistent)
sudo modprobe vhost vhost_net vhost_vsock
echo -e "vhost\nvhost_net\nvhost_vsock" | sudo tee /etc/modules-load.d/kata.conf
```

Configure containerd to use Kata:

```bash
sudo mkdir -p /etc/containerd
sudo containerd config default | sudo tee /etc/containerd/config.toml

sudo tee -a /etc/containerd/config.toml > /dev/null <<EOF

[plugins."io.containerd.grpc.v1.cri".containerd.runtimes.kata]
  runtime_type = "io.containerd.kata.v2"
  [plugins."io.containerd.grpc.v1.cri".containerd.runtimes.kata.options]
    ConfigPath = "/opt/kata/share/defaults/kata-containers/configuration.toml"
EOF

sudo systemctl restart containerd
sudo systemctl enable containerd
```

Install nerdctl:

```bash
curl -L -o /tmp/nerdctl.tar.gz \
  https://github.com/containerd/nerdctl/releases/download/v2.0.3/nerdctl-2.0.3-linux-amd64.tar.gz
sudo tar -xzf /tmp/nerdctl.tar.gz -C /usr/local/bin/
```

Verify everything works:

```bash
sudo /opt/kata/bin/kata-runtime kata-check

# Volume test — should print VOLUME_WORKS
mkdir -p /tmp/kata-test && echo "VOLUME_WORKS" > /tmp/kata-test/check.txt
sudo nerdctl run --rm --runtime io.containerd.kata.v2 \
  -v /tmp/kata-test:/mnt busybox cat /mnt/check.txt
```

If the volume test prints nothing, `virtio-fs` is misconfigured and persistent storage will silently lose data. Check:

```bash
grep shared_fs /opt/kata/share/defaults/kata-containers/configuration.toml
# Expected: shared_fs = "virtio-fs"
```

---

## 4. Build and run `container-api`

```bash
git clone https://github.com/nordkraft/container-api
cd container-api
cargo build --release

# Hybrid mode runs both controller and agent on one machine
export NORDKRAFT_MODE=hybrid
export NODE_ID=my-node
export DATABASE_URL=postgresql://garage_user:changeme@localhost:5432/garage_cloud
export NATS_URL=nats://127.0.0.1:4222
export BIND_ADDRESS=127.0.0.1
export BIND_PORT=8001
export USE_KATA=true
export CONTAINER_RUNTIME=nerdctl

./target/release/nordkraft serve
```

`DEV_MODE=true` skips WireGuard authentication — useful for local development without a VPN setup.

---

## 5. Verify

```bash
curl http://127.0.0.1:8001/api/status
```

Should return a JSON status object with node info.

---

## What's next

- **[SETUP_WIREGUARD.md](SETUP_WIREGUARD.md)** — Configure the WireGuard interface, port forwarding, and understand the security model
- **[INGRESS_PFSENSE.md](INGRESS_PFSENSE.md)** — Set up public HTTPS ingress via pfSense + HAProxy + ACME wildcard cert. Only tested on Netgate hardware. If you don't already run pfSense, wait for the Caddy ingress coming in MARK II.
- Add more nodes: set `NORDKRAFT_MODE=agent` and point `NATS_URL` at your controller
