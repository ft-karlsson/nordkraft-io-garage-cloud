// services/tenant_purge.rs — remove one tenant completely (admin only).
//
// Called from POST /api/admin/tenant/purge. The order mirrors the account
// deletion design: suspend → ingress + custom domains → IPv6 firewall rules →
// containers + data on every node → WireGuard peer + nft rule (done by the
// caller, which owns those helpers) → DB rows → controller routes.
//
// Safety rules:
// - Dry run unless the caller explicitly asks otherwise; the dry run returns
//   the full inventory and changes nothing.
// - The caller must repeat the user's email; an id typo cannot hit someone else.
// - The user is suspended first, so they cannot deploy while the purge runs and
//   boot recovery will not restore their VPN peer.
// - Every external step is idempotent (missing objects count as removed), so a
//   failed purge is simply run again.
// - DB rows are deleted only when every step reported success and every node
//   that may hold the tenant's containers confirmed it converged. Until then
//   the rows stay, because they are what the next run needs to find things.
// - One purge at a time.

use crate::routes::ingress::teardown_ingress;
use crate::services::container_manager::ContainerManager;
use crate::services::domain_reconciler::teardown_custom_domain;
use crate::services::haproxy_client::HAProxyClientTrait;
use crate::services::nats_service::{NatsService, TenantPurgeRequest};
use crate::services::pfsense_client::PfSenseClientTrait;
use crate::services::route_manager::StaticRouteManager;
use crate::storage;
use serde::Serialize;
use sqlx::{PgPool, Row};
use std::sync::Arc;
use tracing::{error, info, warn};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

static PURGE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A WireGuard public key is 32 bytes in base64: 43 characters plus '='.
/// Checked before the key is used as a container label filter, where an
/// empty or odd value could match containers that belong to someone else.
pub fn is_wireguard_key(key: &str) -> bool {
    key.len() == 44
        && key.ends_with('=')
        && key[..43]
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/')
}

/// The user being purged, as loaded from the DB.
#[derive(Debug, Clone, Serialize)]
pub struct PurgeTarget {
    pub user_id: String,
    pub email: String,
    pub full_name: String,
    #[serde(skip)]
    pub wireguard_public_key: String,
    pub user_slot: u32,
    pub account_status: String,
}

/// Everything the purge will touch. Returned as-is by a dry run.
#[derive(Debug, Serialize)]
pub struct PurgeInventory {
    pub user: PurgeTarget,
    /// (container_name, node_id, status, internal_ip)
    pub containers: Vec<(String, String, String, Option<String>)>,
    pub ingress_routes: Vec<String>,
    pub custom_domains: Vec<String>,
    pub ipv6_firewall_rules: Vec<String>,
    /// Nodes that must confirm over NATS (the local hybrid node is purged directly).
    pub nodes_to_confirm: Vec<String>,
    pub local_node: Option<String>,
}

#[derive(Debug, Default, Serialize)]
pub struct PurgeReport {
    pub done: Vec<String>,
    pub errors: Vec<String>,
}

impl PurgeReport {
    fn ok(&mut self, msg: impl Into<String>) {
        let msg = msg.into();
        info!("🧹 purge: {}", msg);
        self.done.push(msg);
    }

    fn fail(&mut self, msg: impl Into<String>) {
        let msg = msg.into();
        error!("❌ purge: {}", msg);
        self.errors.push(msg);
    }
}

pub struct PurgeContext<'a> {
    pub pool: &'a PgPool,
    pub haproxy: &'a Arc<dyn HAProxyClientTrait>,
    pub pfsense: &'a Arc<dyn PfSenseClientTrait>,
    pub nats: Option<&'a NatsService>,
    /// Set on a hybrid controller: this process also runs containers.
    pub local: Option<(&'a ContainerManager, &'a str)>,
}

/// Held for the whole purge; a second concurrent purge is refused.
pub fn try_lock() -> Option<tokio::sync::MutexGuard<'static, ()>> {
    PURGE_LOCK.try_lock().ok()
}

/// Load the user and check the invariants the purge relies on.
pub async fn load_target(pool: &PgPool, user_id: &str) -> Result<PurgeTarget, BoxError> {
    let row = sqlx::query(
        "SELECT id, email, full_name, wireguard_public_key, user_slot, account_status
         FROM users WHERE id = $1",
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| format!("no user with id {}", user_id))?;

    let slot: Option<i32> = row.get("user_slot");
    let slot = match slot {
        Some(s) if (1..=255).contains(&s) => s as u32,
        other => return Err(format!("user has no valid slot ({:?})", other).into()),
    };

    let target = PurgeTarget {
        user_id: row.get("id"),
        email: row.get("email"),
        full_name: row.get("full_name"),
        wireguard_public_key: row.get("wireguard_public_key"),
        user_slot: slot,
        account_status: row
            .get::<Option<String>, _>("account_status")
            .unwrap_or_default(),
    };

    if !is_wireguard_key(&target.wireguard_public_key) {
        return Err("user's WireGuard key is malformed; purge by hand".into());
    }

    // Slot and key identify the tenant on nodes and in nft/WireGuard. If either
    // is shared with another user, a purge would hit that user too.
    let shared: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM users
         WHERE id != $1 AND (user_slot = $2 OR wireguard_public_key = $3)",
    )
    .bind(&target.user_id)
    .bind(slot as i32)
    .bind(&target.wireguard_public_key)
    .fetch_one(pool)
    .await?;
    if shared > 0 {
        return Err(format!(
            "slot {} or WireGuard key is shared with another user; refusing",
            slot
        )
        .into());
    }

    Ok(target)
}

pub async fn inventory(
    pool: &PgPool,
    target: PurgeTarget,
    local_node: Option<&str>,
) -> Result<PurgeInventory, BoxError> {
    let containers = sqlx::query(
        "SELECT container_name, node_id, COALESCE(status, '') AS status, internal_ip
         FROM containers WHERE user_id = $1 ORDER BY created_at",
    )
    .bind(&target.user_id)
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|r| {
        (
            r.get("container_name"),
            r.get("node_id"),
            r.get("status"),
            r.get("internal_ip"),
        )
    })
    .collect();

    let ingress_routes =
        sqlx::query_scalar("SELECT full_domain FROM ingress_routes WHERE user_id = $1 ORDER BY id")
            .bind(&target.user_id)
            .fetch_all(pool)
            .await?;

    let custom_domains =
        sqlx::query_scalar("SELECT domain FROM custom_domains WHERE user_id = $1 ORDER BY id")
            .bind(&target.user_id)
            .fetch_all(pool)
            .await?;

    let ipv6_firewall_rules = sqlx::query_scalar(
        "SELECT container_name FROM ipv6_firewall_rules WHERE user_id = $1 ORDER BY id",
    )
    .bind(&target.user_id)
    .fetch_all(pool)
    .await?;

    // Every registered node, plus any node a live container row points at.
    // Containers can be gone while their volumes and backups remain, so all
    // nodes are asked, not only the ones that run something now.
    let nodes: Vec<String> = sqlx::query_scalar(
        "SELECT node_id FROM nodes
         UNION
         SELECT DISTINCT node_id FROM containers WHERE user_id = $1 AND status != 'deleted'
         ORDER BY 1",
    )
    .bind(&target.user_id)
    .fetch_all(pool)
    .await?;

    Ok(PurgeInventory {
        user: target,
        containers,
        ingress_routes,
        custom_domains,
        ipv6_firewall_rules,
        nodes_to_confirm: nodes
            .into_iter()
            .filter(|n| Some(n.as_str()) != local_node)
            .collect(),
        local_node: local_node.map(str::to_string),
    })
}

/// Block the account: auth, boot recovery and the peer resolver all require
/// 'active', so the tenant can no longer use the API or get a peer back.
pub async fn suspend(pool: &PgPool, target: &PurgeTarget, report: &mut PurgeReport) {
    match sqlx::query(
        "UPDATE users SET account_status = 'suspended', updated_at = NOW() WHERE id = $1",
    )
    .bind(&target.user_id)
    .execute(pool)
    .await
    {
        Ok(_) => report.ok("account suspended"),
        Err(e) => report.fail(format!("could not suspend account: {}", e)),
    }
}

/// Everything outside this host's WireGuard/nft: pfSense, HAProxy, nodes,
/// controller routes. Errors are collected; each step still runs so one
/// failure does not hide the next.
pub async fn purge_external(
    ctx: &PurgeContext<'_>,
    inv: &PurgeInventory,
    report: &mut PurgeReport,
) {
    let user = &inv.user;

    // 1. Platform ingress (active rows carry the HAProxy/pfSense objects).
    match storage::get_user_ingress_routes(ctx.pool, &user.user_id).await {
        Ok(routes) => {
            for route in &routes {
                let errors = teardown_ingress(ctx.pool, ctx.haproxy, ctx.pfsense, route).await;
                if errors.is_empty() {
                    report.ok(format!("ingress {} removed", route.subdomain));
                } else {
                    report.fail(format!(
                        "ingress {}: {}",
                        route.subdomain,
                        errors.join("; ")
                    ));
                }
            }
        }
        Err(e) => report.fail(format!("could not list ingress routes: {}", e)),
    }

    // 2. Custom domains (routing, certificate, static route). Disabled first:
    //    the domain reconciler skips disabled rows, so it cannot re-create what
    //    is being torn down while the rows still exist.
    if let Err(e) = sqlx::query(
        "UPDATE custom_domains SET status = 'disabled', updated_at = NOW() WHERE user_id = $1",
    )
    .bind(&user.user_id)
    .execute(ctx.pool)
    .await
    {
        report.fail(format!("could not disable custom domains: {}", e));
        return;
    }
    match storage::list_custom_domains_for_user(ctx.pool, &user.user_id).await {
        Ok(domains) => {
            for d in &domains {
                let warnings = teardown_custom_domain(ctx.pool, ctx.haproxy, ctx.pfsense, d).await;
                if warnings.is_empty() {
                    report.ok(format!("custom domain {} removed", d.domain));
                } else {
                    report.fail(format!(
                        "custom domain {}: {}",
                        d.domain,
                        warnings.join("; ")
                    ));
                }
            }
        }
        Err(e) => report.fail(format!("could not list custom domains: {}", e)),
    }

    // 3. IPv6 firewall rules on pfSense.
    match sqlx::query(
        "SELECT container_name, pfsense_rule_id FROM ipv6_firewall_rules
         WHERE user_id = $1 AND pfsense_rule_id IS NOT NULL",
    )
    .bind(&user.user_id)
    .fetch_all(ctx.pool)
    .await
    {
        Ok(rows) => {
            for row in rows {
                let name: String = row.get("container_name");
                let rule_id: String = row.get("pfsense_rule_id");
                match ctx.pfsense.remove_rule(&rule_id).await {
                    Ok(_) => report.ok(format!("IPv6 firewall rule for {} removed", name)),
                    Err(e) => report.fail(format!("IPv6 firewall rule for {}: {}", name, e)),
                }
            }
        }
        Err(e) => report.fail(format!("could not list IPv6 firewall rules: {}", e)),
    }

    // 4. Containers, volumes and volume backups on every node.
    if let Some((cm, node_id)) = ctx.local {
        match cm
            .purge_owner(&user.wireguard_public_key, user.user_slot)
            .await
        {
            Ok(removed) => report.ok(format!(
                "node {} (local): {} container(s) removed, data deleted",
                node_id,
                removed.len()
            )),
            Err(e) => report.fail(format!("node {} (local): {}", node_id, e)),
        }
    }

    if !inv.nodes_to_confirm.is_empty() {
        let Some(nats) = ctx.nats else {
            report.fail("NATS is not connected; remote nodes cannot be purged");
            return;
        };
        let request = TenantPurgeRequest {
            owner_pubkey: user.wireguard_public_key.clone(),
            user_slot: user.user_slot,
        };
        for node_id in &inv.nodes_to_confirm {
            match nats.request_tenant_purge(node_id, &request).await {
                Ok(reply) => match reply.error {
                    None => report.ok(format!(
                        "node {}: {} container(s) removed, data deleted",
                        node_id,
                        reply.removed.len()
                    )),
                    Some(e) => report.fail(format!("node {}: {}", node_id, e)),
                },
                Err(e) => report.fail(e.to_string()),
            }
        }
    }
}

/// Controller host routes to the tenant's container IPs. Run after the rows
/// are gone, otherwise the route reconciler re-adds routes for containers
/// still marked running. A leftover /32 into the tenant's own subnet is
/// harmless, so failures only warn.
pub async fn remove_controller_routes(route_manager: &StaticRouteManager, inv: &PurgeInventory) {
    for (name, _, _, ip) in &inv.containers {
        if let Some(ip) = ip {
            if let Err(e) = route_manager.remove_container_route(ip).await {
                warn!("purge: route for {} ({}) not removed: {}", name, ip, e);
            }
        }
    }
}

/// Final step, only after everything else succeeded: delete the tenant's rows
/// in dependency order in one transaction. The user row must still match the
/// email and key the purge started with, otherwise nothing is deleted.
pub async fn delete_rows(pool: &PgPool, target: &PurgeTarget) -> Result<(), BoxError> {
    let mut tx = pool.begin().await?;
    let id = &target.user_id;

    // TCP ingress ports go back to the pool. (release_ingress_port() points at
    // a table that no longer exists, so this is done directly.)
    sqlx::query(
        "UPDATE ingress_port_pool SET is_allocated = false, allocated_at = NULL
         WHERE port IN (SELECT public_port FROM ingress_routes
                        WHERE user_id = $1 AND public_port IS NOT NULL)",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?;

    for sql in [
        "DELETE FROM container_volumes WHERE container_id IN
            (SELECT container_id FROM containers WHERE user_id = $1)",
        "DELETE FROM ipv6_firewall_rules WHERE user_id = $1",
        "DELETE FROM ingress_routes WHERE user_id = $1",
        "DELETE FROM custom_domains WHERE user_id = $1",
        "DELETE FROM container_config WHERE user_id = $1",
        "DELETE FROM deploy_events WHERE user_id = $1",
        "DELETE FROM containers WHERE user_id = $1",
        "DELETE FROM garage_container_allocations WHERE user_id = $1",
    ] {
        sqlx::query(sql).bind(id).execute(&mut *tx).await?;
    }

    let deleted =
        sqlx::query("DELETE FROM users WHERE id = $1 AND email = $2 AND wireguard_public_key = $3")
            .bind(id)
            .bind(&target.email)
            .bind(&target.wireguard_public_key)
            .execute(&mut *tx)
            .await?
            .rows_affected();

    if deleted != 1 {
        tx.rollback().await?;
        return Err(format!(
            "user row changed during the purge ({} rows matched); nothing deleted",
            deleted
        )
        .into());
    }

    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::is_wireguard_key;

    #[test]
    fn wireguard_key_shape() {
        assert!(is_wireguard_key(
            "q0Jx3vX9P2mXQyY0b4n7m0u0q7v5b9r3s5t7u9w1y3A="
        ));
        assert!(!is_wireguard_key(""));
        assert!(!is_wireguard_key("abc="));
        // A label filter value with '=' or ',' in the middle could change the filter.
        assert!(!is_wireguard_key(
            "q0Jx3vX9P2mXQyY0b4n7m0u0q7v5b9r3s5t7u9w1y3=="
        ));
        assert!(!is_wireguard_key(
            "q0Jx3vX9P2mXQyY0b4n7m0u0q7v5b9r3s5t7u9w,y3A="
        ));
    }

    // Live DB test: needs a scratch Postgres loaded with setup/schema.sql and
    // setup/migration_custom_domains.sql. Never point it at a real database.
    //   PURGE_TEST_DATABASE_URL=postgres://... cargo test live_purge -- --ignored
    #[tokio::test]
    #[ignore]
    async fn live_purge_rows_touch_only_one_user() {
        use super::{delete_rows, inventory, load_target};

        let url = std::env::var("PURGE_TEST_DATABASE_URL").expect("PURGE_TEST_DATABASE_URL");
        let pool = sqlx::PgPool::connect(&url).await.unwrap();
        let key_a = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
        let key_b = "BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB=";

        for sql in [
            "TRUNCATE users, containers, container_volumes, container_config, deploy_events,
                      ingress_routes, custom_domains, ipv6_firewall_rules,
                      garage_container_allocations, ingress_port_pool, nodes, garages, plans CASCADE",
            "INSERT INTO plans (id, name, display_name, cpu, memory, storage, price)
                VALUES ('p', 'p', 'p', '1', '1', '1', '0')",
            "INSERT INTO garages (garage_id, name, location, vpn_endpoint, container_subnet_base)
                VALUES ('ry', 'ry', 'ry', 'x', '172.21.0.0/16')",
            "INSERT INTO nodes (node_id, name, location, ip_range, garage_id)
                VALUES ('ry-rpi4-1', 'a', 'ry', '10.0.0.0/24', 'ry'),
                       ('ry-optiplex-1', 'b', 'ry', '10.0.0.0/24', 'ry')",
            "INSERT INTO ingress_port_pool (port, is_allocated) VALUES (10001, true), (10002, true)",
        ] {
            sqlx::query(sql).execute(&pool).await.unwrap();
        }

        // Every statement gets the same binds: $1 id, $2 slot, $3 key,
        // $4 container id, $5 container name, $6 TCP port (unused ones are fine).
        for (id, slot, key, port) in [("ua", 6, key_a, 10001), ("ub", 60, key_b, 10002)] {
            for sql in [
                "INSERT INTO users (id, email, full_name, address, wireguard_public_key,
                    client_ip, plan_id, user_slot)
                    VALUES ($1, $1 || '@x.dk', 'n', 'a', $3, '1.1.1.1', 'p', $2)",
                "INSERT INTO garage_container_allocations (user_id, garage_id, user_slot, container_subnet)
                    VALUES ($1, 'ry', $2, '172.21.0.0/24')",
                "INSERT INTO containers (container_id, user_id, node_id, container_name, image, internal_ip)
                    VALUES ($4, $1, 'ry-optiplex-1', $5, 'img', '172.21.' || $2 || '.10')",
                "INSERT INTO container_volumes (container_id, volume_name, host_path, container_path)
                    VALUES ($4, 'data', '/x', '/data')",
                "INSERT INTO container_config (container_name, user_id, config) VALUES ($5, $1, '{}')",
                "INSERT INTO deploy_events (user_id, container_name, node_id, phase, message)
                    VALUES ($1, $5, 'n', 'p', 'm')",
                "INSERT INTO ingress_routes (user_id, container_id, subdomain, full_domain, mode,
                    target_ip, target_port, public_port)
                    VALUES ($1, $4, $1, $1 || '.x.dk', 'tcp', '172.21.0.10', 80, $6)",
                "INSERT INTO custom_domains (user_id, container_id, domain, verification_token)
                    VALUES ($1, $4, $1 || '.example.com', 't')",
                "INSERT INTO ipv6_firewall_rules (container_name, user_id) VALUES ($5, $1)",
            ] {
                sqlx::query(sql)
                    .bind(id)
                    .bind(slot)
                    .bind(key)
                    .bind(format!("{}-cid", id))
                    .bind(format!("{}-web", id))
                    .bind(port)
                    .execute(&pool)
                    .await
                    .unwrap();
            }
        }

        let target = load_target(&pool, "ua").await.unwrap();
        assert_eq!(target.user_slot, 6);
        let inv = inventory(&pool, target.clone(), Some("ry-rpi4-1"))
            .await
            .unwrap();
        assert_eq!(inv.nodes_to_confirm, vec!["ry-optiplex-1".to_string()]);
        assert_eq!(inv.containers.len(), 1);
        assert_eq!(inv.custom_domains, vec!["ua.example.com".to_string()]);

        // A user row that changed since the purge started is never deleted.
        let mut stale = target.clone();
        stale.email = "someone-else@x.dk".into();
        assert!(delete_rows(&pool, &stale).await.is_err());
        let still: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM containers WHERE user_id = 'ua'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(still, 1, "failed delete must roll back");

        delete_rows(&pool, &target).await.unwrap();

        for table in [
            "users WHERE id",
            "containers WHERE user_id",
            "container_config WHERE user_id",
            "deploy_events WHERE user_id",
            "ingress_routes WHERE user_id",
            "custom_domains WHERE user_id",
            "ipv6_firewall_rules WHERE user_id",
            "garage_container_allocations WHERE user_id",
        ] {
            for (user, want) in [("ua", 0), ("ub", 1)] {
                let n: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {} = $1", table))
                    .bind(user)
                    .fetch_one(&pool)
                    .await
                    .unwrap();
                assert_eq!(n, want, "{} for {}", table, user);
            }
        }
        let volumes: Vec<String> =
            sqlx::query_scalar("SELECT container_id FROM container_volumes ORDER BY 1")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(volumes, vec!["ub-cid".to_string()]);
        let allocated: Vec<i32> =
            sqlx::query_scalar("SELECT port FROM ingress_port_pool WHERE is_allocated ORDER BY 1")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(
            allocated,
            vec![10002],
            "only the purged user's port is released"
        );

        // Two users on one slot: refuse, the slot identifies data on the nodes.
        sqlx::query(
            "INSERT INTO users (id, email, full_name, address, wireguard_public_key, client_ip,
                plan_id, user_slot) VALUES ('uc', 'uc@x.dk', 'n', 'a',
                'CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC=', '1.1.1.1', 'p', 60)",
        )
        .execute(&pool)
        .await
        .unwrap();
        assert!(load_target(&pool, "ub").await.is_err());
    }
}
