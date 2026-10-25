-- Migration: child peers — extra WireGuard peers owned by an account, each
-- scoped to the owner's containers (all of them, or a named subset with
-- optional ports). See setup/CHILD_PEERS.md.
--
-- A child peer is a network identity only: no users row, no API access. The
-- PeerReconciler turns these rows into `wg set` peers and the generated
-- nftables ruleset; nothing here is read on the request path except by the
-- (phase 3) /api/peers endpoints.
--
-- Idempotent. container-api applies it on every start (bootstrap.rs), and it
-- can be applied by hand:
--   psql -U garage_user -d garage_cloud -f migration_child_peers.sql

CREATE TABLE IF NOT EXISTS public.tenant_peers (
    id              character varying(36)  PRIMARY KEY,
    user_id         character varying(36)  NOT NULL REFERENCES public.users(id) ON DELETE CASCADE,
    -- Shown in `nordkraft peer ls` and in the nft rule comment: [a-z0-9-]{1,63}
    name            character varying(63)  NOT NULL,
    -- VPN address is 172.20.<owner slot>.<peer_index>; .0, .1 and .255 are never used
    peer_index      smallint               NOT NULL CHECK (peer_index BETWEEN 2 AND 254),
    -- Real WireGuard public key (44 chars, base64). Never a claim token.
    public_key      text                   NOT NULL UNIQUE,
    -- 'all'        → the owner's whole 172.21.<slot>.0/24
    -- 'containers' → exactly the rows in tenant_peer_targets
    scope           character varying(20)  NOT NULL CHECK (scope IN ('all', 'containers')),
    -- 'disabled' keeps the row but removes the peer and its rules
    status          character varying(20)  NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'disabled')),
    created_at      timestamp with time zone NOT NULL DEFAULT now(),
    updated_at      timestamp with time zone NOT NULL DEFAULT now(),
    UNIQUE (user_id, name),
    UNIQUE (user_id, peer_index)
);

-- Keyed by container NAME on purpose: the reconciler resolves name →
-- containers.internal_ip on every run, so a redeploy that changes the IP, or a
-- new container with the same name, keeps the peer's access. A target whose
-- container does not exist right now yields no rule ("dangling").
CREATE TABLE IF NOT EXISTS public.tenant_peer_targets (
    peer_id         character varying(36)  NOT NULL REFERENCES public.tenant_peers(id) ON DELETE CASCADE,
    container_name  character varying(100) NOT NULL,
    -- NULL = every port; otherwise accepted TCP destination ports
    ports           integer[],
    PRIMARY KEY (peer_id, container_name)
);

CREATE INDEX IF NOT EXISTS idx_tenant_peers_user ON public.tenant_peers (user_id);

ALTER TABLE public.plans ADD COLUMN IF NOT EXISTS max_peers integer NOT NULL DEFAULT 5;
