# Volume quotas

Every persistent volume has a size (`volume_size`, default 1g). Until this
change the size was only bookkeeping: the controller stored it and counted it
against the plan, but the node created a plain directory on its own disk, so
`df` inside a container showed the whole node and one tenant could fill it.

The node now enforces the size with an **XFS project quota** per volume.

## How it works

- Every volume directory is one XFS project: the agent gives it a project id
  with the inherit flag, and gives the project a hard block limit of the
  requested size. The kernel enforces it. The id and the limit are stored in
  the filesystem and survive reboots.
- `df` inside the container shows the limit as the filesystem size. XFS
  reports a project quota through `statfs`, and virtiofsd (Kata) passes
  `statfs` through.
- A write past the limit fails with `ENOSPC` inside that volume only. The
  node, the agent and other tenants are not affected.
- Next to each volume's `data` directory the agent keeps a `quota.json`
  (project id, size, container). It is outside the bind mount. At start the
  agent re-applies every limit from these sidecars, which also restores
  quotas after a copy to a new filesystem (rsync and tar do not carry project
  ids).
- The size travels with the deploy: `ContainerAssignment` carries
  `volume_size`, and an upgrade re-applies the size from the stored config.
  Growing a volume is `nordkraft upgrade` with a larger `volume_size`.
  Shrinking below current usage is allowed but writes fail until usage fits.

## Requirements

`/var/lib/nordkraft` must sit on an **XFS** filesystem mounted with
**`prjquota`**, and `xfsprogs` must be installed (`xfs_quota`, `xfs_io`).
`findmnt -T /var/lib/nordkraft -o FSTYPE,OPTIONS` should show `xfs` and
`prjquota`.

`VOLUME_QUOTA` in `/etc/nordkraft/config.env` decides what the agent does
when that is not the case:

| value | behaviour |
|---|---|
| `auto` (default) | enforce where possible; otherwise log a warning per volume and deploy without a limit |
| `required` | refuse any deploy with persistence that cannot be limited |
| `off` | never touch quotas (dev nodes) |

Production nodes run `required` once migrated. Self-hosted nodes keep `auto`.

## Migrating a node to XFS

One maintenance window per node. Containers with volumes are down while the
data is copied; nothing else is touched. Take a backup of the Postgres
database and of the volumes (`nordkraft backup` per container, or a tar of
`/var/lib/nordkraft`) before you start.

### Node with LVM (dellopt1)

```bash
# 0. Room in the volume group?
vgs

# 1. Stop workloads and the agent
systemctl stop nordkraft-node
nerdctl ps -q | xargs -r nerdctl stop

# 2. New logical volume, XFS
lvcreate -L 200G -n nordkraft <vg>
mkfs.xfs -L nordkraft /dev/<vg>/nordkraft

# 3. Copy the data (preserve everything the files carry)
mkdir -p /mnt/nk-new
mount -o prjquota /dev/<vg>/nordkraft /mnt/nk-new
rsync -aHAXS --numeric-ids /var/lib/nordkraft/ /mnt/nk-new/
umount /mnt/nk-new

# 4. Swap the mount
mv /var/lib/nordkraft /var/lib/nordkraft.old
mkdir /var/lib/nordkraft
echo "LABEL=nordkraft /var/lib/nordkraft xfs defaults,prjquota 0 2" >> /etc/fstab
mount /var/lib/nordkraft
findmnt -T /var/lib/nordkraft -o SOURCE,FSTYPE,OPTIONS   # xfs ... prjquota

# 5. Packages and config
apt-get install -y xfsprogs
sed -i 's/^VOLUME_QUOTA=.*/VOLUME_QUOTA=required/' /etc/nordkraft/config.env \
  || echo VOLUME_QUOTA=required >> /etc/nordkraft/config.env

# 6. Start, and watch the reconcile
systemctl start nordkraft-node
journalctl -u nordkraft-node -f | grep -E "Volume quota"
```

Keep `/var/lib/nordkraft.old` until the volumes have been checked; then
remove it.

The LVM snapshot path of `nordkraft backup` keeps working: it snapshots the
new logical volume and mounts it with `nouuid`.

### Node without LVM (Raspberry Pi on an SD card or USB disk)

Same steps, with a partition or a whole disk in place of the logical volume:
`mkfs.xfs -L nordkraft /dev/sdX1`. If there is no spare partition, move the
data to a USB disk formatted XFS. Do not reformat the root filesystem in
place.

### Volumes that predate this change

Volumes created before the agent knew about quotas have no `quota.json`, so
the start-up reconcile leaves them alone. They get their limit the first
time their container is upgraded (`nordkraft upgrade` resends the stored
config, including `volume_size`). Run that once per legacy container after
the migration; the container restarts, the data stays.

## Checking

```bash
# On the node: every project, used vs. hard limit (KiB)
xfs_quota -x -c "report -p -h" /var/lib/nordkraft

# Which project a volume belongs to
xfs_io -r -c lsproj /var/lib/nordkraft/users/<slot>/volumes/<container>/data

# From inside a container
nordkraft shell <container> -- df -h /data
```

## Out of scope

Placement. Quotas cap each volume, but the scheduler does not yet check a
node's free disk against the sum of the volumes it holds. That is a separate
change on the controller.
