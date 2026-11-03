-- Energy transparency: watts and CO₂ per container. See setup/ENERGY.md.
-- Idempotent; the controller applies it on every start, or by hand:
--   sudo -u postgres psql garage_cloud -f setup/migration_energy.sql

-- What a node drew at the wall, one row per meter reading (about one a
-- minute). Pruned after 30 days; the per-container tables carry the history.
CREATE TABLE IF NOT EXISTS public.node_power_samples (
    node_id character varying(50) NOT NULL,
    ts timestamp with time zone NOT NULL,
    watts double precision NOT NULL,
    energy_wh double precision,
    source character varying(40) NOT NULL,
    PRIMARY KEY (node_id, ts)
);

-- Grid CO₂ intensity from Energinet's Energi Data Service: CO2Emis is the
-- measured 5-minute value, CO2EmisProg the forecast. Gram CO₂ per kWh, per
-- price area (DK1 west of Storebælt, DK2 east).
CREATE TABLE IF NOT EXISTS public.grid_co2 (
    price_area character varying(8) NOT NULL,
    ts timestamp with time zone NOT NULL,
    forecast boolean NOT NULL DEFAULT false,
    g_per_kwh double precision NOT NULL,
    fetched_at timestamp with time zone NOT NULL DEFAULT now(),
    PRIMARY KEY (price_area, ts, forecast)
);

-- One row per container per meter interval: its attributed share of the
-- node's watts, the energy that is over the interval, and the CO₂ at the
-- grid intensity of that moment. Pruned after 7 days (the hourly table
-- keeps the history). Goes with the customer when the account is purged.
CREATE TABLE IF NOT EXISTS public.container_energy_minutes (
    container_id character varying(64) NOT NULL,
    ts timestamp with time zone NOT NULL,
    user_id character varying(36) NOT NULL REFERENCES public.users(id) ON DELETE CASCADE,
    node_id character varying(50) NOT NULL,
    container_name character varying(100) NOT NULL,
    interval_s double precision NOT NULL,
    watts double precision NOT NULL,
    wh double precision NOT NULL,
    co2_g double precision,
    g_per_kwh double precision,
    method character varying(40) NOT NULL,
    PRIMARY KEY (container_id, ts)
);
CREATE INDEX IF NOT EXISTS container_energy_minutes_user_ts_idx
    ON public.container_energy_minutes (user_id, ts);

-- Hourly rollup of the minute rows: the long-term record, and what an
-- invoice line would read.
CREATE TABLE IF NOT EXISTS public.container_energy_hourly (
    container_id character varying(64) NOT NULL,
    hour timestamp with time zone NOT NULL,
    user_id character varying(36) NOT NULL REFERENCES public.users(id) ON DELETE CASCADE,
    node_id character varying(50) NOT NULL,
    container_name character varying(100) NOT NULL,
    samples integer NOT NULL,
    watts_avg double precision NOT NULL,
    wh double precision NOT NULL,
    co2_g double precision,
    PRIMARY KEY (container_id, hour)
);
CREATE INDEX IF NOT EXISTS container_energy_hourly_user_hour_idx
    ON public.container_energy_hourly (user_id, hour);

-- Per node: what it draws with no containers running (the baseline the
-- attribution splits by reserved vCPU; NULL = unknown, everything is then
-- split by CPU time), and which Energinet price area it sits in.
ALTER TABLE public.nodes ADD COLUMN IF NOT EXISTS power_idle_watts double precision;
ALTER TABLE public.nodes ADD COLUMN IF NOT EXISTS price_area character varying(8) NOT NULL DEFAULT 'DK1';

-- Per node and hour, from the node readings, independent of customers: the
-- platform's own record of what a garage drew and emitted, kept forever
-- (24 rows per node per day). Customers' rows come and go with accounts;
-- this one does not. Wh integrates each reading over the time since the
-- previous one (capped at 150 s, the same freshness rule as attribution);
-- co2_g prices each reading with the grid value at that time, and
-- `unpriced` counts readings that had none.
CREATE TABLE IF NOT EXISTS public.node_energy_hourly (
    node_id character varying(50) NOT NULL,
    hour timestamp with time zone NOT NULL,
    samples integer NOT NULL,
    unpriced integer NOT NULL DEFAULT 0,
    watts_avg double precision NOT NULL,
    watts_max double precision NOT NULL,
    wh double precision NOT NULL,
    co2_g double precision NOT NULL,
    PRIMARY KEY (node_id, hour)
);
