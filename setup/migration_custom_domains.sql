-- Migration: custom_domains — bring-your-own-domain ingress (MARK II)
--
-- A row per verified customer hostname (e.g. customer1.net or www.customer1.net).
-- The domain_reconciler background task drives each row through the state
-- machine; no HAProxy/pfSense object is ever created before the domain is
-- DNS-verified AND a certificate has been issued.
--
-- Apply with: psql -U garage_user -d garage_cloud -f migration_custom_domains.sql

CREATE TABLE IF NOT EXISTS public.custom_domains (
    id                   serial PRIMARY KEY,
    user_id              character varying(36) NOT NULL REFERENCES public.users(id) ON DELETE CASCADE,
    container_id         character varying(64) NOT NULL,
    -- Normalized: lowercase, punycode (IDNA). Globally unique — one owner per hostname, ever.
    domain               character varying(253) NOT NULL,
    target_port          integer NOT NULL DEFAULT 80,

    -- State machine (driven by domain_reconciler):
    --   pending_dns   waiting for customer to add TXT + A/CNAME records
    --   dns_verified  authoritative DNS checks passed (2 consecutive)
    --   issuing_cert  ACME order in flight
    --   cert_ready    cert issued, not yet uploaded/bound
    --   activating    pfSense objects being created
    --   active        everything verified present via read-back
    --   degraded      DNS no longer points at us; grace period running
    --   error         terminal until user retries or reconciler backoff expires
    --   disabled      torn down (auto after grace, or by user delete keeping history: rows are deleted on user remove)
    status               character varying(20) NOT NULL DEFAULT 'pending_dns',

    -- Ownership proof: customer publishes this at _nordkraft-challenge.<domain> TXT "nk-verify=<token>"
    verification_token   character varying(64) NOT NULL,
    dns_check_successes  integer NOT NULL DEFAULT 0,   -- consecutive successful checks (need 2 to advance)
    verified_at          timestamp with time zone,
    degraded_since       timestamp with time zone,     -- set when an active domain stops resolving to us

    -- Certificate (stored in pfSense cert store; we keep the reference + expiry)
    cert_refid           character varying(100),
    cert_expires_at      timestamp with time zone,

    -- pfSense/HAProxy object names — derived from id, never from raw user input
    haproxy_backend_name character varying(100),
    haproxy_acl_name     character varying(100),
    haproxy_server_name  character varying(100),
    static_route_created boolean NOT NULL DEFAULT false,
    target_ip            character varying(45),

    -- Reconciler bookkeeping
    last_checked_at      timestamp with time zone,
    last_error           text,
    retry_count          integer NOT NULL DEFAULT 0,
    next_retry_at        timestamp with time zone,

    created_at           timestamp with time zone NOT NULL DEFAULT now(),
    updated_at           timestamp with time zone NOT NULL DEFAULT now(),

    CONSTRAINT custom_domains_domain_unique UNIQUE (domain),
    CONSTRAINT custom_domains_status_check CHECK (
        status IN ('pending_dns','dns_verified','issuing_cert','cert_ready',
                   'activating','active','degraded','error','disabled')
    ),
    CONSTRAINT custom_domains_target_port_check CHECK (target_port > 0 AND target_port <= 65535)
);

CREATE INDEX IF NOT EXISTS idx_custom_domains_user ON public.custom_domains (user_id);
CREATE INDEX IF NOT EXISTS idx_custom_domains_container ON public.custom_domains (container_id);
-- The reconciler polls by status + next_retry_at
CREATE INDEX IF NOT EXISTS idx_custom_domains_pending ON public.custom_domains (status, next_retry_at)
    WHERE status NOT IN ('disabled');

-- Reuse the existing updated_at trigger function
DROP TRIGGER IF EXISTS trigger_custom_domains_updated_at ON public.custom_domains;
CREATE TRIGGER trigger_custom_domains_updated_at
    BEFORE UPDATE ON public.custom_domains
    FOR EACH ROW EXECUTE FUNCTION public.update_ingress_timestamp();

GRANT ALL ON TABLE public.custom_domains TO garage_user;
GRANT ALL ON SEQUENCE public.custom_domains_id_seq TO garage_user;
