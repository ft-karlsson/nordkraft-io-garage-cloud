-- Account lifecycle: email verification codes and customer-requested deletions.
-- See setup/ACCOUNT_LIFECYCLE.md. Idempotent; run once against garage_cloud:
--   sudo -u postgres psql garage_cloud -f setup/migration_account_lifecycle.sql

-- One row per code sent to a customer's address. The code itself is never
-- stored, only its SHA-256. A row is spent when verified_at is set, dead when
-- expires_at has passed or attempts reached the limit (5, enforced in code).
CREATE TABLE IF NOT EXISTS public.email_verifications (
    id character varying(36) NOT NULL PRIMARY KEY,
    user_id character varying(36) NOT NULL,
    purpose character varying(20) NOT NULL,
    code_hash character(64) NOT NULL,
    attempts integer DEFAULT 0 NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    expires_at timestamp with time zone NOT NULL,
    verified_at timestamp with time zone,
    CONSTRAINT email_verifications_purpose_check
        CHECK (purpose::text = ANY (ARRAY['activate', 'delete']::text[]))
);
CREATE INDEX IF NOT EXISTS email_verifications_user_idx
    ON public.email_verifications (user_id, purpose);

-- A customer's request to leave. Deliberately no foreign key to users: the
-- row must outlive the users row so there is a record that the purge finished
-- (and when), which is what the final email and GDPR need. The email column
-- exists only to send that final mail; it is cleared once the mail is sent.
CREATE TABLE IF NOT EXISTS public.account_deletions (
    id character varying(36) NOT NULL PRIMARY KEY,
    user_id character varying(36) NOT NULL,
    user_slot integer,
    email character varying(255),
    full_name character varying(255),
    requested_at timestamp with time zone DEFAULT now() NOT NULL,
    scheduled_for timestamp with time zone NOT NULL,
    cancelled_at timestamp with time zone,
    started_at timestamp with time zone,
    completed_at timestamp with time zone,
    attempts integer DEFAULT 0 NOT NULL,
    last_error text
);
-- At most one open (neither cancelled nor completed) request per user.
CREATE UNIQUE INDEX IF NOT EXISTS account_deletions_open_idx
    ON public.account_deletions (user_id)
    WHERE cancelled_at IS NULL AND completed_at IS NULL;
