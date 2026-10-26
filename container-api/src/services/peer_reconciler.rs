// services/peer_reconciler.rs — WireGuard peers + nftables from the database.
//
// One declarative, two-way reconciler for everything the controller's VPN
// hub needs to know about who may talk to what:
//
//   desired  = owners   (users: active, slot set, real WireGuard key)
//            ∪ children (tenant_peers: active, owner active)   — see CHILD_PEERS.md
//   observed = `wg show <iface> allowed-ips`
//
// WireGuard: `wg set` for every desired peer that is missing or has the wrong
// allowed-ips, `wg set … remove` for every peer on the interface that the DB
// does not know (a suspended owner, a deleted child, a hand-added test peer).
//
// nftables: the COMPLETE desired ruleset is rendered from the DB and applied
// with one `nft -f`, which is one atomic transaction. No `nft insert`, no
// grepping for rule handles, no "does this rule already exist". The
// controller owns `inet filter forward` (the chain the installer creates and
// production has carried by hand so far) and `inet filter nordkraft_input`.
// Anything added to those two chains by hand is overwritten within one
// interval; everything else in the table is left alone.
//
// Runs once at start (boot recovery), every 5 minutes, and immediately when
// kicked by a write path (peer or container change, tenant purge, admin
// provision). Replaces the add-only WgReconciler and boot_recovery_from_db.

use sqlx::{PgPool, Row};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::net::Ipv4Addr;
use std::sync::Arc;
use tokio::process::Command;
use tokio::sync::{Mutex, Notify, RwLock};
use tokio::time::{interval, Duration};
use tracing::{debug, error, info, warn};

use crate::services::tenant_purge::is_wireguard_key;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

// ============= ADDRESSING =============
// 172.20.0.<slot>          owner of slot S
// 172.20.<slot>.<index>    child peer of slot S, index 2..254
// 172.21.<slot>.0/24       containers of slot S

/// The whole VPN range. Owners use its first /24, children the rest.
pub const VPN_NET: &str = "172.20.0.0/16";

/// True for every address a child peer can have: inside 172.20.0.0/16 but
/// outside the owners' 172.20.0.0/24. Used by the auth guard (children never
/// get an API identity) and by the generated input rule.
pub fn is_child_peer_ip(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    o[0] == 172 && o[1] == 20 && o[2] != 0
}

/// What happens to VPN traffic no rule accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DenyMode {
    /// Production: chain policy drop, final `counter drop`.
    Drop,
    /// Soak mode for a node migrating from the installer's accept-by-default
    /// chain: policy accept, final rule only logs and counts, so the journal
    /// shows what *would* be dropped.
    Audit,
}

impl DenyMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "drop" => Some(Self::Drop),
            "audit" | "log" => Some(Self::Audit),
            _ => None,
        }
    }
}

// ============= DESIRED STATE =============

/// An IPv4 network in CIDR form, as stored in
/// `garage_container_allocations.container_subnet` ("172.21.5.0/24").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Subnet {
    pub network: Ipv4Addr,
    pub prefix: u8,
}

impl Subnet {
    pub fn parse(s: &str) -> Option<Self> {
        let (addr, len) = s.trim().split_once('/')?;
        let addr: Ipv4Addr = addr.parse().ok()?;
        let prefix: u8 = len.parse().ok()?;
        if prefix > 32 {
            return None;
        }
        let mask = Self::mask(prefix);
        Some(Self {
            network: Ipv4Addr::from(u32::from(addr) & mask),
            prefix,
        })
    }

    /// What every tenant has had so far: 172.21.<slot>.0/24.
    pub fn legacy_for_slot(slot: u8) -> Self {
        Self {
            network: Ipv4Addr::new(172, 21, slot, 0),
            prefix: 24,
        }
    }

    fn mask(prefix: u8) -> u32 {
        if prefix == 0 {
            0
        } else {
            u32::MAX << (32 - prefix)
        }
    }

    pub fn contains(&self, ip: Ipv4Addr) -> bool {
        (u32::from(ip) & Self::mask(self.prefix)) == u32::from(self.network)
    }
}

impl std::fmt::Display for Subnet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.network, self.prefix)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Owner {
    pub slot: u8,
    pub public_key: String,
    /// The tenant's container range, from its allocation row. Not derived from
    /// the slot, so a garage that hands out /20s instead of /24s needs no
    /// change here.
    pub container_subnet: Subnet,
}

impl Owner {
    pub fn vpn_ip(&self) -> Ipv4Addr {
        Ipv4Addr::new(172, 20, 0, self.slot)
    }
    /// Server-side allowed-ips. The container subnet is what every owner peer
    /// has carried since day one; it is kept so nothing is rewritten on
    /// existing peers.
    pub fn allowed_ips(&self) -> Vec<String> {
        vec![
            format!("{}/32", self.vpn_ip()),
            self.container_subnet.to_string(),
        ]
    }
}

/// One container a child may reach. `ip` is None while no non-deleted
/// container of that name has an address: the target is "dangling" and
/// produces no rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub container_name: String,
    pub ip: Option<Ipv4Addr>,
    /// None = every port, else accepted TCP destination ports.
    pub ports: Option<Vec<u16>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    AllContainers,
    Containers(Vec<Target>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildPeer {
    pub slot: u8,
    pub index: u8,
    pub name: String,
    pub public_key: String,
    /// The owner's container range (see Owner::container_subnet).
    pub container_subnet: Subnet,
    pub scope: Scope,
}

impl ChildPeer {
    pub fn vpn_ip(&self) -> Ipv4Addr {
        Ipv4Addr::new(172, 20, self.slot, self.index)
    }
    pub fn allowed_ips(&self) -> Vec<String> {
        vec![format!("{}/32", self.vpn_ip())]
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DesiredState {
    pub owners: Vec<Owner>,
    pub children: Vec<ChildPeer>,
}

impl DesiredState {
    /// public key → allowed-ips, for the WireGuard side.
    fn wg_peers(&self) -> BTreeMap<String, BTreeSet<String>> {
        let mut m = BTreeMap::new();
        for o in &self.owners {
            m.insert(o.public_key.clone(), o.allowed_ips().into_iter().collect());
        }
        for c in &self.children {
            m.insert(c.public_key.clone(), c.allowed_ips().into_iter().collect());
        }
        m
    }
}

/// Load the desired state. Rows that cannot be a peer (invite placeholders,
/// slots outside 1..=255, odd indexes) are skipped with a warning rather than
/// failing the whole run.
pub async fn load_desired(pool: &PgPool) -> Result<DesiredState, BoxError> {
    // user_id → container subnet, preferring the primary garage's allocation.
    let mut subnets: BTreeMap<String, Subnet> = BTreeMap::new();
    for row in sqlx::query(
        "SELECT a.user_id, a.container_subnet
         FROM garage_container_allocations a
         JOIN users u ON u.id = a.user_id
         ORDER BY a.user_id, (a.garage_id = u.primary_garage_id) DESC, a.created_at",
    )
    .fetch_all(pool)
    .await?
    {
        let user_id: String = row.get("user_id");
        let subnet: String = row.get("container_subnet");
        if subnets.contains_key(&user_id) {
            continue;
        }
        match Subnet::parse(&subnet) {
            Some(s) => {
                subnets.insert(user_id, s);
            }
            None => warn!(
                "peer reconciler: allocation for {} has unparsable subnet {:?}, ignored",
                user_id, subnet
            ),
        }
    }
    let subnet_for = |user_id: &str, slot: u8| -> Subnet {
        subnets.get(user_id).copied().unwrap_or_else(|| {
            warn!(
                "peer reconciler: no allocation row for slot {}, assuming 172.21.{}.0/24",
                slot, slot
            );
            Subnet::legacy_for_slot(slot)
        })
    };

    let mut owners = Vec::new();
    for row in sqlx::query(
        "SELECT id, wireguard_public_key, user_slot FROM users
         WHERE account_status = 'active' AND user_slot IS NOT NULL
         ORDER BY user_slot",
    )
    .fetch_all(pool)
    .await?
    {
        let user_id: String = row.get("id");
        let key: String = row.get("wireguard_public_key");
        let slot: i32 = row.get("user_slot");
        if !is_wireguard_key(&key) {
            // Unclaimed invite (NKINVITE-…) or garbage: no peer yet.
            continue;
        }
        let Ok(slot) = u8::try_from(slot) else {
            warn!("peer reconciler: slot {} out of range, skipped", slot);
            continue;
        };
        if slot == 0 {
            warn!("peer reconciler: slot 0 is not a tenant, skipped");
            continue;
        }
        owners.push(Owner {
            slot,
            public_key: key,
            container_subnet: subnet_for(&user_id, slot),
        });
    }

    let mut children_by_id: BTreeMap<String, ChildPeer> = BTreeMap::new();
    let mut scoped: BTreeSet<String> = BTreeSet::new();
    for row in sqlx::query(
        "SELECT p.id, p.user_id, p.name, p.peer_index, p.public_key, p.scope, u.user_slot
         FROM tenant_peers p
         JOIN users u ON u.id = p.user_id
         WHERE p.status = 'active' AND u.account_status = 'active' AND u.user_slot IS NOT NULL
         ORDER BY u.user_slot, p.peer_index",
    )
    .fetch_all(pool)
    .await?
    {
        let id: String = row.get("id");
        let user_id: String = row.get("user_id");
        let name: String = row.get("name");
        let index: i16 = row.get("peer_index");
        let key: String = row.get("public_key");
        let scope: String = row.get("scope");
        let slot: i32 = row.get("user_slot");
        if !is_wireguard_key(&key) {
            warn!(
                "peer reconciler: child peer {} has no valid key, skipped",
                name
            );
            continue;
        }
        let (Ok(slot), Ok(index)) = (u8::try_from(slot), u8::try_from(index)) else {
            warn!(
                "peer reconciler: child peer {} has slot/index out of range, skipped",
                name
            );
            continue;
        };
        if slot == 0 || !(2..=254).contains(&index) {
            warn!(
                "peer reconciler: child peer {} has slot/index out of range, skipped",
                name
            );
            continue;
        }
        let scope = match scope.as_str() {
            "all" => Scope::AllContainers,
            "containers" => {
                scoped.insert(id.clone());
                Scope::Containers(Vec::new())
            }
            other => {
                warn!(
                    "peer reconciler: child peer {} has unknown scope {:?}, skipped",
                    name, other
                );
                continue;
            }
        };
        children_by_id.insert(
            id,
            ChildPeer {
                slot,
                index,
                name,
                public_key: key,
                container_subnet: subnet_for(&user_id, slot),
                scope,
            },
        );
    }

    if !scoped.is_empty() {
        for row in sqlx::query(
            "SELECT t.peer_id, t.container_name, t.ports,
                    (SELECT c.internal_ip FROM containers c
                      WHERE c.user_id = p.user_id AND c.container_name = t.container_name
                        AND c.status != 'deleted' AND c.internal_ip IS NOT NULL
                      ORDER BY c.created_at DESC LIMIT 1) AS internal_ip
             FROM tenant_peer_targets t
             JOIN tenant_peers p ON p.id = t.peer_id
             ORDER BY t.peer_id, t.container_name",
        )
        .fetch_all(pool)
        .await?
        {
            let peer_id: String = row.get("peer_id");
            let Some(peer) = children_by_id.get_mut(&peer_id) else {
                continue;
            };
            let Scope::Containers(targets) = &mut peer.scope else {
                continue;
            };
            let name: String = row.get("container_name");
            let ports: Option<Vec<i32>> = row.get("ports");
            let ip: Option<String> = row.get("internal_ip");
            let ip = ip.and_then(|s| s.split('/').next()?.parse::<Ipv4Addr>().ok());
            let ports = ports.map(|v| {
                v.into_iter()
                    .filter_map(|p| u16::try_from(p).ok())
                    .filter(|p| *p > 0)
                    .collect::<Vec<u16>>()
            });
            targets.push(Target {
                container_name: name,
                ip,
                ports,
            });
        }
    }

    Ok(DesiredState {
        owners,
        children: children_by_id.into_values().collect(),
    })
}

// ============= RULESET RENDERING (pure) =============

/// nft comments are quoted strings; keep them to a safe alphabet and length.
fn comment(s: &str) -> String {
    let clean: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ' ' | ':') {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("\"{}\"", clean.chars().take(120).collect::<String>())
}

fn port_set(ports: &[u16]) -> String {
    let mut sorted: Vec<u16> = ports.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    let list: Vec<String> = sorted.iter().map(u16::to_string).collect();
    format!("{{ {} }}", list.join(", "))
}

/// Render the complete nftables ruleset for the controller. Deterministic:
/// owners by slot, children by (slot, index), targets by name.
pub fn render_ruleset(state: &DesiredState, wg_interface: &str, mode: DenyMode) -> String {
    let mut owners = state.owners.clone();
    owners.sort_by_key(|o| o.slot);
    let mut children = state.children.clone();
    children.sort_by_key(|c| (c.slot, c.index));

    let wg = format!("\"{}\"", wg_interface);
    let policy = match mode {
        DenyMode::Drop => "drop",
        DenyMode::Audit => "accept",
    };

    let mut out = String::new();
    out.push_str("#!/usr/sbin/nft -f\n");
    out.push_str("# Generated by container-api (PeerReconciler) from the database.\n");
    out.push_str("# Do not edit: the forward and nordkraft_input chains are rewritten\n");
    out.push_str("# on every reconcile. See setup/CHILD_PEERS.md.\n");
    let _ = writeln!(
        out,
        "# {} owner(s), {} child peer(s), deny mode: {}",
        owners.len(),
        children.len(),
        match mode {
            DenyMode::Drop => "drop",
            DenyMode::Audit => "audit (log only)",
        }
    );
    out.push('\n');

    // Create-if-missing, then flush, then fill. Declaring an existing base
    // chain again is a no-op apart from (re)setting its policy.
    out.push_str("table inet filter {\n");
    let _ = writeln!(
        out,
        "\tchain forward {{\n\t\ttype filter hook forward priority filter; policy {};\n\t}}",
        policy
    );
    out.push_str(
        "\tchain nordkraft_input {\n\t\ttype filter hook input priority -10; policy accept;\n\t}\n",
    );
    out.push_str("}\n");
    out.push_str("flush chain inet filter forward\n");
    out.push_str("flush chain inet filter nordkraft_input\n\n");

    out.push_str("table inet filter {\n");
    out.push_str("\tchain forward {\n");
    out.push_str("\t\tct state established,related accept\n");

    out.push_str("\t\t# owners: 172.20.0.<slot> -> the tenant's container subnet\n");
    for o in &owners {
        let _ = writeln!(
            out,
            "\t\tiifname {} ip saddr {} ip daddr {} accept comment {}",
            wg,
            o.vpn_ip(),
            o.container_subnet,
            comment(&format!("tenant-{}", o.slot))
        );
    }

    out.push_str("\t\t# child peers: 172.20.<slot>.<index> -> owner's containers in scope\n");
    for c in &children {
        let label = format!("peer-{}-{} {}", c.slot, c.index, c.name);
        match &c.scope {
            Scope::AllContainers => {
                let _ = writeln!(
                    out,
                    "\t\tiifname {} ip saddr {} ip daddr {} accept comment {}",
                    wg,
                    c.vpn_ip(),
                    c.container_subnet,
                    comment(&format!("{} all", label))
                );
            }
            Scope::Containers(targets) => {
                let mut targets = targets.clone();
                targets.sort_by(|a, b| a.container_name.cmp(&b.container_name));
                for t in &targets {
                    let Some(ip) = t.ip else {
                        let _ = writeln!(
                            out,
                            "\t\t# {} {}: no running container with that name (dangling)",
                            label, t.container_name
                        );
                        continue;
                    };
                    // A container of another tenant can never be a target: the
                    // DB query joins on the owner, and this check keeps a bad
                    // row from ever widening the rule.
                    if !c.container_subnet.contains(ip) {
                        let _ = writeln!(
                            out,
                            "\t\t# {} {}: {} is outside {}, refused",
                            label, t.container_name, ip, c.container_subnet
                        );
                        continue;
                    }
                    let ports = match &t.ports {
                        Some(p) if !p.is_empty() => format!(" tcp dport {}", port_set(p)),
                        _ => String::new(),
                    };
                    let _ = writeln!(
                        out,
                        "\t\tiifname {} ip saddr {} ip daddr {}{} accept comment {}",
                        wg,
                        c.vpn_ip(),
                        ip,
                        ports,
                        comment(&format!("{} {}", label, t.container_name))
                    );
                }
            }
        }
    }

    out.push_str("\t\t# anything else arriving from the VPN\n");
    match mode {
        DenyMode::Drop => {
            let _ = writeln!(
                out,
                "\t\tiifname {} counter drop comment \"vpn-default-deny\"",
                wg
            );
        }
        DenyMode::Audit => {
            let _ = writeln!(
                out,
                "\t\tiifname {} counter log prefix \"nk-vpn-deny \" comment \"vpn-default-deny-audit\"",
                wg
            );
        }
    }
    out.push_str("\t}\n");

    out.push_str("\tchain nordkraft_input {\n");
    out.push_str("\t\t# child peers never reach the controller host itself (API, SSH, NATS)\n");
    let _ = writeln!(
        out,
        "\t\tiifname {} ip saddr 172.20.1.0-172.20.255.255 counter drop comment \"children-never-reach-host\"",
        wg
    );
    out.push_str("\t}\n");
    out.push_str("}\n");
    out
}

// ============= OBSERVED STATE =============

/// `wg show <iface> allowed-ips` → public key → set of allowed-ips.
fn parse_allowed_ips(stdout: &str) -> BTreeMap<String, BTreeSet<String>> {
    let mut m = BTreeMap::new();
    for line in stdout.lines() {
        let mut parts = line.split('\t');
        let Some(key) = parts.next().map(str::trim) else {
            continue;
        };
        if key.is_empty() {
            continue;
        }
        let ips: BTreeSet<String> = parts
            .next()
            .unwrap_or("")
            .split_whitespace()
            .filter(|s| *s != "(none)")
            .map(str::to_string)
            .collect();
        m.insert(key.to_string(), ips);
    }
    m
}

async fn observed_wg_peers(
    wg_interface: &str,
) -> Result<BTreeMap<String, BTreeSet<String>>, BoxError> {
    let out = Command::new("wg")
        .args(["show", wg_interface, "allowed-ips"])
        .output()
        .await?;
    if !out.status.success() {
        return Err(format!(
            "wg show {} allowed-ips failed: {}",
            wg_interface,
            String::from_utf8_lossy(&out.stderr).trim()
        )
        .into());
    }
    Ok(parse_allowed_ips(&String::from_utf8_lossy(&out.stdout)))
}

async fn wg_set_peer(
    wg_interface: &str,
    key: &str,
    allowed: &BTreeSet<String>,
) -> Result<(), BoxError> {
    let allowed_ips = allowed.iter().cloned().collect::<Vec<_>>().join(",");
    let out = Command::new("wg")
        .args([
            "set",
            wg_interface,
            "peer",
            key,
            "allowed-ips",
            &allowed_ips,
        ])
        .output()
        .await?;
    if !out.status.success() {
        return Err(format!(
            "wg set peer {}… failed: {}",
            &key[..8.min(key.len())],
            String::from_utf8_lossy(&out.stderr).trim()
        )
        .into());
    }
    Ok(())
}

pub async fn wg_remove_peer(wg_interface: &str, key: &str) -> Result<(), BoxError> {
    let out = Command::new("wg")
        .args(["set", wg_interface, "peer", key, "remove"])
        .output()
        .await?;
    if !out.status.success() {
        return Err(format!(
            "wg set peer {}… remove failed: {}",
            &key[..8.min(key.len())],
            String::from_utf8_lossy(&out.stderr).trim()
        )
        .into());
    }
    Ok(())
}

/// `ip` needs root; the route manager reads ROUTE_USE_SUDO for the same reason.
fn ip_cmd(args: &[&str]) -> Command {
    let use_sudo = std::env::var("ROUTE_USE_SUDO")
        .map(|v| v == "true" || v == "1")
        .unwrap_or(false);
    if use_sudo {
        let mut c = Command::new("sudo");
        c.arg("ip").args(args);
        c
    } else {
        let mut c = Command::new("ip");
        c.args(args);
        c
    }
}

/// Make sure the whole VPN range is routed to the WireGuard interface.
///
/// Owners sit inside the /24 that is the interface address, so they work
/// with nothing else. Children live in 172.20.<slot>.0/24: on a controller
/// whose wg0 carries only a /24 (rasp2's netplan did) their packets fail the
/// reverse-path check before any nft rule sees them, and replies leave via
/// the default gateway. One link route fixes both directions. Re-asserted on
/// every pass, so a netplan apply or reboot cannot take it away for long.
/// Returns true when the route had to be added.
async fn ensure_vpn_route(wg_interface: &str) -> Result<bool, BoxError> {
    let out = ip_cmd(&["route", "show", VPN_NET, "dev", wg_interface])
        .output()
        .await?;
    let present = String::from_utf8_lossy(&out.stdout)
        .lines()
        .any(|l| l.trim_start().starts_with(VPN_NET));
    if present {
        return Ok(false);
    }
    let out = ip_cmd(&["route", "replace", VPN_NET, "dev", wg_interface])
        .output()
        .await?;
    if !out.status.success() {
        return Err(format!(
            "ip route replace {} dev {} failed: {}",
            VPN_NET,
            wg_interface,
            String::from_utf8_lossy(&out.stderr).trim()
        )
        .into());
    }
    Ok(true)
}

/// Apply a rendered ruleset atomically and read it back.
async fn apply_ruleset(ruleset: &str) -> Result<(), BoxError> {
    let path = std::env::temp_dir().join("nordkraft-ruleset.nft");
    tokio::fs::write(&path, ruleset).await?;
    let out = Command::new("nft")
        .args(["-f", &path.to_string_lossy()])
        .output()
        .await?;
    if !out.status.success() {
        return Err(format!(
            "nft -f failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )
        .into());
    }
    // Read-back: the deny rule is the one line every ruleset has.
    let check = Command::new("nft")
        .args(["list", "chain", "inet", "filter", "forward"])
        .output()
        .await?;
    let listed = String::from_utf8_lossy(&check.stdout);
    if !listed.contains("vpn-default-deny") {
        return Err("nft read-back: forward chain does not carry the generated rules".into());
    }
    Ok(())
}

// ============= PROCESS-WIDE HANDLE =============
// Deep write paths (deploy, delete, reaper, purge) kick the reconciler without
// plumbing it through every struct. None in dev and agent modes.

static GLOBAL: std::sync::OnceLock<Arc<PeerReconciler>> = std::sync::OnceLock::new();

pub fn install_global(rec: Arc<PeerReconciler>) {
    if GLOBAL.set(rec).is_err() {
        warn!("peer reconciler: install_global called twice, ignored");
    }
}

pub fn global() -> Option<Arc<PeerReconciler>> {
    GLOBAL.get().cloned()
}

/// Ask the reconciler for a pass soon. No-op where there is none.
pub fn kick() {
    if let Some(rec) = GLOBAL.get() {
        rec.kick();
    }
}

// ============= RECONCILER =============

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ReconcileReport {
    pub at: String,
    pub owners: usize,
    pub children: usize,
    pub wg_added: Vec<String>,
    pub wg_updated: Vec<String>,
    pub wg_removed: Vec<String>,
    /// The 172.20.0.0/16 link route on the interface had to be (re)added.
    pub route_added: bool,
    pub nft_applied: bool,
    pub errors: Vec<String>,
}

fn short(key: &str) -> String {
    format!("{}…", &key[..8.min(key.len())])
}

pub struct PeerReconciler {
    pool: PgPool,
    wg_interface: String,
    mode: DenyMode,
    /// Leave WireGuard peers the DB does not know alone (default false).
    keep_stray_peers: bool,
    notify: Notify,
    run_lock: Mutex<()>,
    last: RwLock<Option<ReconcileReport>>,
}

impl PeerReconciler {
    pub fn new(
        pool: PgPool,
        wg_interface: &str,
        mode: DenyMode,
        keep_stray_peers: bool,
    ) -> Arc<Self> {
        Arc::new(Self {
            pool,
            wg_interface: wg_interface.to_string(),
            mode,
            keep_stray_peers,
            notify: Notify::new(),
            run_lock: Mutex::new(()),
            last: RwLock::new(None),
        })
    }

    /// Ask for a reconcile soon (coalesced). Never blocks.
    pub fn kick(&self) {
        self.notify.notify_one();
    }

    pub async fn last_report(&self) -> Option<ReconcileReport> {
        self.last.read().await.clone()
    }

    /// Render what the DB says right now without touching anything.
    pub async fn render_current(&self) -> Result<String, BoxError> {
        let desired = load_desired(&self.pool).await?;
        Ok(render_ruleset(&desired, &self.wg_interface, self.mode))
    }

    /// One full pass: DB → wg set/remove → nft -f. Serialised; a second caller
    /// waits for the running pass and then runs its own.
    pub async fn reconcile_now(&self) -> ReconcileReport {
        let _guard = self.run_lock.lock().await;
        let mut report = ReconcileReport {
            at: chrono::Utc::now().to_rfc3339(),
            ..Default::default()
        };

        let desired = match load_desired(&self.pool).await {
            Ok(d) => d,
            Err(e) => {
                report.errors.push(format!("load desired state: {}", e));
                error!("peer reconciler: {}", report.errors.last().unwrap());
                *self.last.write().await = Some(report.clone());
                return report;
            }
        };
        report.owners = desired.owners.len();
        report.children = desired.children.len();
        let wanted = desired.wg_peers();

        match observed_wg_peers(&self.wg_interface).await {
            Ok(observed) => {
                for (key, allowed) in &wanted {
                    match observed.get(key) {
                        Some(have) if have == allowed => {}
                        Some(_) => match wg_set_peer(&self.wg_interface, key, allowed).await {
                            Ok(()) => report.wg_updated.push(short(key)),
                            Err(e) => report.errors.push(e.to_string()),
                        },
                        None => match wg_set_peer(&self.wg_interface, key, allowed).await {
                            Ok(()) => report.wg_added.push(short(key)),
                            Err(e) => report.errors.push(e.to_string()),
                        },
                    }
                }
                for key in observed.keys() {
                    if wanted.contains_key(key) {
                        continue;
                    }
                    if self.keep_stray_peers {
                        warn!(
                            "peer reconciler: peer {} is not in the DB (kept: WG_KEEP_STRAY_PEERS)",
                            short(key)
                        );
                        continue;
                    }
                    match wg_remove_peer(&self.wg_interface, key).await {
                        Ok(()) => {
                            warn!(
                                "peer reconciler: removed peer {} (not in the DB)",
                                short(key)
                            );
                            report.wg_removed.push(short(key));
                        }
                        Err(e) => report.errors.push(e.to_string()),
                    }
                }
            }
            Err(e) => report.errors.push(e.to_string()),
        }

        match ensure_vpn_route(&self.wg_interface).await {
            Ok(added) => {
                report.route_added = added;
                if added {
                    warn!(
                        "peer reconciler: added route {} dev {} (child peers need it)",
                        VPN_NET, self.wg_interface
                    );
                }
            }
            Err(e) => report.errors.push(e.to_string()),
        }

        let ruleset = render_ruleset(&desired, &self.wg_interface, self.mode);
        match apply_ruleset(&ruleset).await {
            Ok(()) => report.nft_applied = true,
            Err(e) => report.errors.push(e.to_string()),
        }

        let changed = !(report.wg_added.is_empty()
            && report.wg_updated.is_empty()
            && report.wg_removed.is_empty())
            || report.route_added;
        if !report.errors.is_empty() {
            error!(
                "peer reconciler: {} owner(s), {} child peer(s); errors: {}",
                report.owners,
                report.children,
                report.errors.join("; ")
            );
        } else if changed {
            info!(
                "🔄 peer reconciler: {} owner(s), {} child peer(s); wg +{} ~{} -{}; nft applied",
                report.owners,
                report.children,
                report.wg_added.len(),
                report.wg_updated.len(),
                report.wg_removed.len()
            );
        } else {
            debug!(
                "peer reconciler: converged ({} owner(s), {} child peer(s))",
                report.owners, report.children
            );
        }

        *self.last.write().await = Some(report.clone());
        report
    }

    /// Boot pass now, then every `interval_secs` or on `kick()`.
    pub fn start(self: &Arc<Self>, interval_secs: u64) {
        let me = Arc::clone(self);
        tokio::spawn(async move {
            let boot = me.reconcile_now().await;
            info!(
                "✅ Boot recovery (peer reconciler): {} owner(s), {} child peer(s), wg +{} ~{} -{}, nft {}{}",
                boot.owners,
                boot.children,
                boot.wg_added.len(),
                boot.wg_updated.len(),
                boot.wg_removed.len(),
                if boot.nft_applied { "applied" } else { "NOT applied" },
                if boot.errors.is_empty() {
                    String::new()
                } else {
                    format!("; errors: {}", boot.errors.join("; "))
                }
            );

            let mut tick = interval(Duration::from_secs(interval_secs));
            tick.tick().await; // the boot pass was the first tick
            loop {
                tokio::select! {
                    _ = tick.tick() => {}
                    _ = me.notify.notified() => {
                        // Coalesce bursts (deploy + route + status writes).
                        tokio::time::sleep(Duration::from_millis(250)).await;
                    }
                }
                me.reconcile_now().await;
            }
        });
        info!(
            "🔄 Peer reconciler started (interval {}s, deny mode {:?}, stray peers {})",
            interval_secs,
            self.mode,
            if self.keep_stray_peers {
                "kept"
            } else {
                "removed"
            }
        );
    }
}

// ============= TESTS =============

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> DesiredState {
        DesiredState {
            owners: vec![
                Owner {
                    slot: 12,
                    public_key: "OOOOOOOOOOOOOOOOOOOOOOOOOOOOOOOOOOOOOOOOOOA=".into(),
                    container_subnet: Subnet::legacy_for_slot(12),
                },
                Owner {
                    slot: 5,
                    public_key: "OOOOOOOOOOOOOOOOOOOOOOOOOOOOOOOOOOOOOOOOOOB=".into(),
                    container_subnet: Subnet::legacy_for_slot(5),
                },
            ],
            children: vec![
                ChildPeer {
                    slot: 5,
                    index: 3,
                    name: "ci-runner".into(),
                    public_key: "CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCA=".into(),
                    container_subnet: Subnet::legacy_for_slot(5),
                    scope: Scope::Containers(vec![
                        Target {
                            container_name: "web".into(),
                            ip: Some(Ipv4Addr::new(172, 21, 5, 17)),
                            ports: Some(vec![443, 80, 80]),
                        },
                        Target {
                            container_name: "db".into(),
                            ip: Some(Ipv4Addr::new(172, 21, 5, 20)),
                            ports: None,
                        },
                        Target {
                            container_name: "gone".into(),
                            ip: None,
                            ports: None,
                        },
                        Target {
                            container_name: "evil".into(),
                            ip: Some(Ipv4Addr::new(172, 21, 12, 2)),
                            ports: None,
                        },
                    ]),
                },
                ChildPeer {
                    slot: 5,
                    index: 2,
                    name: "anna's laptop".into(),
                    public_key: "CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCB=".into(),
                    container_subnet: Subnet::legacy_for_slot(5),
                    scope: Scope::AllContainers,
                },
            ],
        }
    }

    fn snapshot_path(name: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../tests/snapshots")
            .join(name)
    }

    /// Golden file. Rewrite with UPDATE_SNAPSHOTS=1 cargo test.
    fn assert_snapshot(name: &str, actual: &str) {
        let path = snapshot_path(name);
        if std::env::var("UPDATE_SNAPSHOTS").is_ok() {
            std::fs::write(&path, actual).unwrap();
            return;
        }
        let expected = std::fs::read_to_string(&path).unwrap_or_else(|e| {
            panic!(
                "missing snapshot {}: {} (UPDATE_SNAPSHOTS=1 to create)",
                path.display(),
                e
            )
        });
        if expected != actual {
            panic!(
                "ruleset differs from {}\n--- expected\n{}\n--- actual\n{}",
                path.display(),
                expected,
                actual
            );
        }
    }

    #[test]
    fn ruleset_drop_mode_matches_snapshot() {
        assert_snapshot(
            "nft-ruleset-drop.nft",
            &render_ruleset(&sample(), "wg0", DenyMode::Drop),
        );
    }

    #[test]
    fn ruleset_audit_mode_matches_snapshot() {
        assert_snapshot(
            "nft-ruleset-audit.nft",
            &render_ruleset(&sample(), "wg0", DenyMode::Audit),
        );
    }

    #[test]
    fn ruleset_empty_still_denies() {
        let r = render_ruleset(&DesiredState::default(), "wg0", DenyMode::Drop);
        assert!(r.contains("policy drop"));
        assert!(r.contains("iifname \"wg0\" counter drop comment \"vpn-default-deny\""));
        assert!(r.contains("172.20.1.0-172.20.255.255 counter drop"));
        assert!(!r.contains("tenant-"));
    }

    #[test]
    fn ruleset_is_deterministic_regardless_of_input_order() {
        let a = render_ruleset(&sample(), "wg0", DenyMode::Drop);
        let mut s = sample();
        s.owners.reverse();
        s.children.reverse();
        if let Scope::Containers(t) = &mut s.children[1].scope {
            t.reverse();
        }
        assert_eq!(a, render_ruleset(&s, "wg0", DenyMode::Drop));
    }

    #[test]
    fn child_rule_never_leaves_the_owners_subnet() {
        let r = render_ruleset(&sample(), "wg0", DenyMode::Drop);
        assert!(!r.contains("ip daddr 172.21.12.2 accept"));
        assert!(r.contains("172.21.12.2 is outside 172.21.5.0/24, refused"));
        // and a dangling target produces a comment, not a rule
        assert!(r.contains("gone: no running container"));
    }

    #[test]
    fn child_peer_addressing() {
        assert!(is_child_peer_ip("172.20.5.2".parse().unwrap()));
        assert!(is_child_peer_ip("172.20.255.254".parse().unwrap()));
        assert!(!is_child_peer_ip("172.20.0.5".parse().unwrap()));
        assert!(!is_child_peer_ip("172.20.0.254".parse().unwrap()));
        assert!(!is_child_peer_ip("172.21.5.2".parse().unwrap()));
        assert!(!is_child_peer_ip("10.0.0.36".parse().unwrap()));
    }

    #[test]
    fn wg_allowed_ips_parse() {
        let m = parse_allowed_ips(
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=\t172.20.0.5/32 172.21.5.0/24\n\
             BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB=\t(none)\n",
        );
        assert_eq!(m.len(), 2);
        let a = &m["AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="];
        assert!(a.contains("172.20.0.5/32") && a.contains("172.21.5.0/24"));
        assert!(m["BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB="].is_empty());
        // What the reconciler wants for the same owner compares equal.
        let want: BTreeSet<String> = Owner {
            slot: 5,
            public_key: String::new(),
            container_subnet: Subnet::legacy_for_slot(5),
        }
        .allowed_ips()
        .into_iter()
        .collect();
        assert_eq!(&want, a);
    }

    #[test]
    fn subnet_parse_and_contains() {
        let s = Subnet::parse("172.21.16.0/20").unwrap();
        assert_eq!(s.to_string(), "172.21.16.0/20");
        assert!(s.contains("172.21.16.1".parse().unwrap()));
        assert!(s.contains("172.21.31.254".parse().unwrap()));
        assert!(!s.contains("172.21.32.1".parse().unwrap()));
        assert!(!s.contains("172.21.15.255".parse().unwrap()));
        // host bits are masked off
        assert_eq!(
            Subnet::parse("172.21.20.5/20").unwrap().network,
            "172.21.16.0".parse::<Ipv4Addr>().unwrap()
        );
        assert_eq!(Subnet::legacy_for_slot(7).to_string(), "172.21.7.0/24");
        assert!(Subnet::parse("172.21.1.0/33").is_none());
        assert!(Subnet::parse("172.21.1.0").is_none());
        assert!(Subnet::parse("nope/24").is_none());
    }

    /// A garage that allocates /20s per tenant: the rules follow the
    /// allocation row, not the slot number.
    #[test]
    fn ruleset_follows_a_wider_allocation() {
        let big = Subnet::parse("172.21.16.0/20").unwrap();
        let state = DesiredState {
            owners: vec![Owner {
                slot: 2,
                public_key: "OOOOOOOOOOOOOOOOOOOOOOOOOOOOOOOOOOOOOOOOOOC=".into(),
                container_subnet: big,
            }],
            children: vec![
                ChildPeer {
                    slot: 2,
                    index: 2,
                    name: "all".into(),
                    public_key: "CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC=".into(),
                    container_subnet: big,
                    scope: Scope::AllContainers,
                },
                ChildPeer {
                    slot: 2,
                    index: 3,
                    name: "one".into(),
                    public_key: "CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCD=".into(),
                    container_subnet: big,
                    scope: Scope::Containers(vec![
                        Target {
                            container_name: "inside".into(),
                            ip: Some("172.21.29.7".parse().unwrap()),
                            ports: None,
                        },
                        Target {
                            container_name: "outside".into(),
                            ip: Some("172.21.2.7".parse().unwrap()),
                            ports: None,
                        },
                    ]),
                },
            ],
        };
        let r = render_ruleset(&state, "wg0", DenyMode::Drop);
        assert!(
            r.contains("ip saddr 172.20.0.2 ip daddr 172.21.16.0/20 accept comment \"tenant-2\"")
        );
        assert!(r.contains("ip saddr 172.20.2.2 ip daddr 172.21.16.0/20 accept"));
        assert!(r.contains("ip saddr 172.20.2.3 ip daddr 172.21.29.7 accept"));
        // 172.21.2.7 would have been "slot 2's" under the old slot-derived rule
        assert!(!r.contains("ip daddr 172.21.2.7 accept"));
        assert!(r.contains("172.21.2.7 is outside 172.21.16.0/20, refused"));
        // children stay in the VPN range whatever the container subnet is
        assert!(!r.contains("172.20.16."));
    }

    #[test]
    fn deny_mode_parse() {
        assert_eq!(DenyMode::parse("drop"), Some(DenyMode::Drop));
        assert_eq!(DenyMode::parse(" Audit "), Some(DenyMode::Audit));
        assert_eq!(DenyMode::parse("yes"), None);
    }
}
