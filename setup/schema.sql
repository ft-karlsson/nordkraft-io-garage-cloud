--
-- PostgreSQL database dump
--

\restrict S1AtI0fvXDZrGcXCsmdjM2f2y6soVb7NHnF8lWdk563D7I47e81bE9uxPhhDQ0d

-- Dumped from database version 16.13 (Ubuntu 16.13-0ubuntu0.24.04.1)
-- Dumped by pg_dump version 16.13 (Ubuntu 16.13-0ubuntu0.24.04.1)

SET statement_timeout = 0;
SET lock_timeout = 0;
SET idle_in_transaction_session_timeout = 0;
SET client_encoding = 'UTF8';
SET standard_conforming_strings = on;
SELECT pg_catalog.set_config('search_path', '', false);
SET check_function_bodies = false;
SET xmloption = content;
SET client_min_messages = warning;
SET row_security = off;

--
-- Name: allocate_ingress_port(); Type: FUNCTION; Schema: public; Owner: garage_user
--

CREATE FUNCTION public.allocate_ingress_port() RETURNS integer
    LANGUAGE plpgsql
    AS $$
DECLARE
    selected_port INTEGER;
BEGIN
    SELECT port INTO selected_port
    FROM tcp_port_allocations
    WHERE is_allocated = FALSE
    ORDER BY port
    LIMIT 1
    FOR UPDATE SKIP LOCKED;
    
    IF selected_port IS NULL THEN
        RAISE EXCEPTION 'No TCP ports available';
    END IF;
    
    UPDATE tcp_port_allocations
    SET is_allocated = TRUE, allocated_at = NOW()
    WHERE port = selected_port;
    
    RETURN selected_port;
END;
$$;


ALTER FUNCTION public.allocate_ingress_port() OWNER TO garage_user;

--
-- Name: allocate_user_ipv6(character varying, integer, character varying, character varying, character varying, integer[]); Type: FUNCTION; Schema: public; Owner: garage_user
--

CREATE FUNCTION public.allocate_user_ipv6(p_user_id character varying, p_user_slot integer, p_garage_id character varying, p_container_id character varying DEFAULT NULL::character varying, p_container_name character varying DEFAULT NULL::character varying, p_ports integer[] DEFAULT '{80,443}'::integer[]) RETURNS text
    LANGUAGE plpgsql
    AS $_$
DECLARE
    v_next_index INTEGER;
    v_ipv6_address INET;
    v_prefix TEXT := '2a05:f6c3:444e:';
BEGIN
    IF p_user_slot < 1 OR p_user_slot > 255 THEN
        RAISE EXCEPTION 'Invalid user_slot: %. Must be 1-255', p_user_slot;
    END IF;

    SELECT COALESCE(MAX(
        (regexp_match(host(ia.ipv6_address), ':([0-9a-f]+)::[^:]*$'))[1]::INTEGER
    ), 0) + 1
    INTO v_next_index
    FROM ipv6_allocations ia
    WHERE ia.user_slot = p_user_slot;

    IF v_next_index IS NULL THEN
        SELECT COUNT(*) + 1 INTO v_next_index
        FROM ipv6_allocations ia
        WHERE ia.user_slot = p_user_slot;
    END IF;

    v_ipv6_address := (v_prefix || '0:' || p_user_slot::TEXT || ':' || v_next_index::TEXT || '::')::inet;

    INSERT INTO ipv6_allocations (
        ipv6_address, container_id, container_name,
        user_id, garage_id, user_slot, exposed_ports, allocated_at
    ) VALUES (
        v_ipv6_address, NULL, p_container_name,
        p_user_id, p_garage_id, p_user_slot, p_ports, NOW()
    );

    RETURN host(v_ipv6_address);

EXCEPTION
    WHEN unique_violation THEN
        RAISE EXCEPTION 'IPv6 address already allocated: %', v_ipv6_address;
END;
$_$;


ALTER FUNCTION public.allocate_user_ipv6(p_user_id character varying, p_user_slot integer, p_garage_id character varying, p_container_id character varying, p_container_name character varying, p_ports integer[]) OWNER TO garage_user;

--
-- Name: FUNCTION allocate_user_ipv6(p_user_id character varying, p_user_slot integer, p_garage_id character varying, p_container_id character varying, p_container_name character varying, p_ports integer[]); Type: COMMENT; Schema: public; Owner: garage_user
--

COMMENT ON FUNCTION public.allocate_user_ipv6(p_user_id character varying, p_user_slot integer, p_garage_id character varying, p_container_id character varying, p_container_name character varying, p_ports integer[]) IS 'Atomically allocates next available IPv6 for user. Returns IPv6 as TEXT.';


--
-- Name: assign_next_allocation(character varying); Type: FUNCTION; Schema: public; Owner: garage_user
--

CREATE FUNCTION public.assign_next_allocation(target_node_id character varying) RETURNS TABLE(allocation_id integer, user_ip character varying, container_cidr character varying)
    LANGUAGE plpgsql
    AS $$
DECLARE
    selected_allocation RECORD;
BEGIN
    -- Find and lock next available allocation
    SELECT na.id, na.user_ip, na.container_cidr
    INTO selected_allocation
    FROM network_allocations na
    WHERE na.node_id = target_node_id 
      AND na.is_allocated = FALSE
    ORDER BY na.allocation_slot
    LIMIT 1
    FOR UPDATE SKIP LOCKED;
    
    IF NOT FOUND THEN
        RAISE EXCEPTION 'No available network allocations for node %', target_node_id;
    END IF;
    
    -- Mark as allocated
    UPDATE network_allocations 
    SET is_allocated = TRUE, 
        allocated_at = NOW()
    WHERE id = selected_allocation.id;
    
    -- Return allocation info
    RETURN QUERY SELECT selected_allocation.id, selected_allocation.user_ip, selected_allocation.container_cidr;
END;
$$;


ALTER FUNCTION public.assign_next_allocation(target_node_id character varying) OWNER TO garage_user;

--
-- Name: is_subdomain_available(character varying); Type: FUNCTION; Schema: public; Owner: garage_user
--

CREATE FUNCTION public.is_subdomain_available(p_subdomain character varying) RETURNS boolean
    LANGUAGE plpgsql
    AS $$
BEGIN
    -- Check reserved list
    IF EXISTS (SELECT 1 FROM reserved_subdomains WHERE subdomain = lower(p_subdomain)) THEN
        RETURN FALSE;
    END IF;
    
    -- Check existing routes
    IF EXISTS (SELECT 1 FROM ingress_routes WHERE subdomain = lower(p_subdomain)) THEN
        RETURN FALSE;
    END IF;
    
    RETURN TRUE;
END;
$$;


ALTER FUNCTION public.is_subdomain_available(p_subdomain character varying) OWNER TO garage_user;

--
-- Name: release_ingress_port(integer); Type: FUNCTION; Schema: public; Owner: garage_user
--

CREATE FUNCTION public.release_ingress_port(release_port integer) RETURNS void
    LANGUAGE plpgsql
    AS $$
BEGIN
    UPDATE tcp_port_allocations
    SET is_allocated = FALSE, allocated_to = NULL, allocated_at = NULL
    WHERE port = release_port;
END;
$$;


ALTER FUNCTION public.release_ingress_port(release_port integer) OWNER TO garage_user;

--
-- Name: release_ipv6_allocation(character varying); Type: FUNCTION; Schema: public; Owner: garage_user
--

CREATE FUNCTION public.release_ipv6_allocation(p_container_id character varying) RETURNS text
    LANGUAGE plpgsql
    AS $$
DECLARE
    v_ipv6 TEXT;
BEGIN
    UPDATE ipv6_allocations
    SET released_at = NOW()
    WHERE container_id = p_container_id
      AND released_at IS NULL
    RETURNING ipv6_address::TEXT INTO v_ipv6;

    RETURN v_ipv6;
END;
$$;


ALTER FUNCTION public.release_ipv6_allocation(p_container_id character varying) OWNER TO garage_user;

--
-- Name: FUNCTION release_ipv6_allocation(p_container_id character varying); Type: COMMENT; Schema: public; Owner: garage_user
--

COMMENT ON FUNCTION public.release_ipv6_allocation(p_container_id character varying) IS 'Releases IPv6 allocation. Returns released IPv6 as TEXT or NULL.';


--
-- Name: update_ingress_timestamp(); Type: FUNCTION; Schema: public; Owner: garage_user
--

CREATE FUNCTION public.update_ingress_timestamp() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
BEGIN
    NEW.updated_at = NOW();
    RETURN NEW;
END;
$$;


ALTER FUNCTION public.update_ingress_timestamp() OWNER TO garage_user;

--
-- Name: update_user_client_ip(character varying, character varying); Type: FUNCTION; Schema: public; Owner: garage_user
--

CREATE FUNCTION public.update_user_client_ip(target_user_id character varying, new_client_ip character varying) RETURNS boolean
    LANGUAGE plpgsql
    AS $$
BEGIN
    UPDATE users 
    SET client_ip = new_client_ip,
        client_ip_updated_at = NOW(),
        updated_at = NOW()
    WHERE id = target_user_id;
    
    RETURN FOUND;
END;
$$;


ALTER FUNCTION public.update_user_client_ip(target_user_id character varying, new_client_ip character varying) OWNER TO garage_user;

SET default_tablespace = '';

SET default_table_access_method = heap;

--
-- Name: container_config; Type: TABLE; Schema: public; Owner: postgres
--

CREATE TABLE public.container_config (
    container_name text NOT NULL,
    user_id text NOT NULL,
    config jsonb NOT NULL,
    revision integer DEFAULT 1 NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL
);


ALTER TABLE public.container_config OWNER TO postgres;

--
-- Name: container_volumes; Type: TABLE; Schema: public; Owner: garage_user
--

CREATE TABLE public.container_volumes (
    id integer NOT NULL,
    container_id character varying(64) NOT NULL,
    volume_name character varying(100) NOT NULL,
    host_path text NOT NULL,
    container_path text NOT NULL,
    size_limit_mb integer DEFAULT 1024,
    read_only boolean DEFAULT false,
    created_at timestamp with time zone DEFAULT now()
);


ALTER TABLE public.container_volumes OWNER TO garage_user;

--
-- Name: container_volumes_id_seq; Type: SEQUENCE; Schema: public; Owner: garage_user
--

CREATE SEQUENCE public.container_volumes_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


ALTER SEQUENCE public.container_volumes_id_seq OWNER TO garage_user;

--
-- Name: container_volumes_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: garage_user
--

ALTER SEQUENCE public.container_volumes_id_seq OWNED BY public.container_volumes.id;


--
-- Name: containers; Type: TABLE; Schema: public; Owner: garage_user
--

CREATE TABLE public.containers (
    id integer NOT NULL,
    container_id character varying(64) NOT NULL,
    user_id character varying(36) NOT NULL,
    node_id character varying(50) NOT NULL,
    container_name character varying(100) NOT NULL,
    image character varying(200) NOT NULL,
    internal_ip character varying(15),
    exposed_ports jsonb DEFAULT '[]'::jsonb,
    status character varying(20) DEFAULT 'created'::character varying,
    public_url character varying(255),
    ingress_enabled boolean DEFAULT false,
    ingress_port integer,
    labels jsonb DEFAULT '{}'::jsonb,
    environment jsonb DEFAULT '{}'::jsonb,
    created_at timestamp with time zone DEFAULT now(),
    updated_at timestamp with time zone DEFAULT now(),
    ipv6_address inet,
    ipv6_enabled boolean DEFAULT false,
    macvlan_attached boolean DEFAULT false,
    cpu_limit double precision DEFAULT 0.5,
    memory_limit_mb bigint DEFAULT 512,
    volume_size_mb bigint DEFAULT 0,
    CONSTRAINT containers_status_check CHECK (((status)::text = ANY (ARRAY[('created'::character varying)::text, ('running'::character varying)::text, ('stopped'::character varying)::text, ('deleted'::character varying)::text, ('error'::character varying)::text])))
);


ALTER TABLE public.containers OWNER TO garage_user;

--
-- Name: COLUMN containers.ipv6_address; Type: COMMENT; Schema: public; Owner: garage_user
--

COMMENT ON COLUMN public.containers.ipv6_address IS 'Globally routable IPv6 address when enabled';


--
-- Name: COLUMN containers.ipv6_enabled; Type: COMMENT; Schema: public; Owner: garage_user
--

COMMENT ON COLUMN public.containers.ipv6_enabled IS 'Whether container has IPv6 exposed to internet';


--
-- Name: COLUMN containers.macvlan_attached; Type: COMMENT; Schema: public; Owner: garage_user
--

COMMENT ON COLUMN public.containers.macvlan_attached IS 'Whether container is attached to macvlan network';


--
-- Name: containers_id_seq; Type: SEQUENCE; Schema: public; Owner: garage_user
--

CREATE SEQUENCE public.containers_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


ALTER SEQUENCE public.containers_id_seq OWNER TO garage_user;

--
-- Name: containers_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: garage_user
--

ALTER SEQUENCE public.containers_id_seq OWNED BY public.containers.id;


--
-- Name: garage_container_allocations; Type: TABLE; Schema: public; Owner: garage_user
--

CREATE TABLE public.garage_container_allocations (
    id integer NOT NULL,
    user_id character varying(36),
    garage_id character varying(50),
    user_slot integer NOT NULL,
    container_subnet character varying(18) NOT NULL,
    is_active boolean DEFAULT true,
    created_at timestamp with time zone DEFAULT now(),
    CONSTRAINT allocation_slot_limit CHECK (((user_slot >= 1) AND (user_slot <= 255)))
);


ALTER TABLE public.garage_container_allocations OWNER TO garage_user;

--
-- Name: garage_container_allocations_id_seq; Type: SEQUENCE; Schema: public; Owner: garage_user
--

CREATE SEQUENCE public.garage_container_allocations_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


ALTER SEQUENCE public.garage_container_allocations_id_seq OWNER TO garage_user;

--
-- Name: garage_container_allocations_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: garage_user
--

ALTER SEQUENCE public.garage_container_allocations_id_seq OWNED BY public.garage_container_allocations.id;


--
-- Name: garages; Type: TABLE; Schema: public; Owner: garage_user
--

CREATE TABLE public.garages (
    garage_id character varying(50) NOT NULL,
    name character varying(100) NOT NULL,
    location character varying(100) NOT NULL,
    country character varying(2) DEFAULT 'DK'::character varying NOT NULL,
    vpn_endpoint character varying(100) NOT NULL,
    wireguard_public_key text,
    container_subnet_base character varying(18) NOT NULL,
    status character varying(20) DEFAULT 'active'::character varying,
    max_users integer DEFAULT 256,
    created_at timestamp with time zone DEFAULT now()
);


ALTER TABLE public.garages OWNER TO garage_user;

--
-- Name: ingress_port_pool; Type: TABLE; Schema: public; Owner: garage_user
--

CREATE TABLE public.ingress_port_pool (
    port integer NOT NULL,
    is_allocated boolean DEFAULT false,
    allocated_at timestamp with time zone,
    CONSTRAINT ingress_port_pool_port_check CHECK (((port >= 10000) AND (port <= 10999)))
);


ALTER TABLE public.ingress_port_pool OWNER TO garage_user;

--
-- Name: ingress_routes; Type: TABLE; Schema: public; Owner: garage_user
--

CREATE TABLE public.ingress_routes (
    id integer NOT NULL,
    user_id character varying(36) NOT NULL,
    container_id character varying(64) NOT NULL,
    subdomain character varying(63) NOT NULL,
    full_domain character varying(255) NOT NULL,
    mode character varying(10) DEFAULT 'http'::character varying NOT NULL,
    target_ip character varying(45) NOT NULL,
    target_port integer NOT NULL,
    public_port integer,
    haproxy_backend_name character varying(100),
    haproxy_frontend_name character varying(100),
    haproxy_acl_name character varying(100),
    haproxy_server_name character varying(100),
    pfsense_rule_id character varying(100),
    firewall_open boolean DEFAULT false,
    is_active boolean DEFAULT true,
    created_at timestamp with time zone DEFAULT now(),
    updated_at timestamp with time zone DEFAULT now(),
    pfsense_static_route_id character varying(100),
    ip_version character varying(4) DEFAULT 'ipv6'::character varying,
    CONSTRAINT ingress_routes_mode_check CHECK (((mode)::text = ANY ((ARRAY['http'::character varying, 'https'::character varying, 'tcp'::character varying])::text[]))),
    CONSTRAINT ingress_routes_target_port_check CHECK (((target_port > 0) AND (target_port <= 65535)))
);


ALTER TABLE public.ingress_routes OWNER TO garage_user;

--
-- Name: TABLE ingress_routes; Type: COMMENT; Schema: public; Owner: garage_user
--

COMMENT ON TABLE public.ingress_routes IS 'HTTP/TCP ingress routes via HAProxy';


--
-- Name: COLUMN ingress_routes.mode; Type: COMMENT; Schema: public; Owner: garage_user
--

COMMENT ON COLUMN public.ingress_routes.mode IS 'http = Host header routing on port 80, tcp = dedicated port routing';


--
-- Name: COLUMN ingress_routes.public_port; Type: COMMENT; Schema: public; Owner: garage_user
--

COMMENT ON COLUMN public.ingress_routes.public_port IS 'Only used for TCP mode - allocated from ingress_port_pool';


--
-- Name: COLUMN ingress_routes.pfsense_rule_id; Type: COMMENT; Schema: public; Owner: garage_user
--

COMMENT ON COLUMN public.ingress_routes.pfsense_rule_id IS 'Only used for TCP mode - HTTP uses shared port 80 rule';


--
-- Name: ingress_routes_id_seq; Type: SEQUENCE; Schema: public; Owner: garage_user
--

CREATE SEQUENCE public.ingress_routes_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


ALTER SEQUENCE public.ingress_routes_id_seq OWNER TO garage_user;

--
-- Name: ingress_routes_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: garage_user
--

ALTER SEQUENCE public.ingress_routes_id_seq OWNED BY public.ingress_routes.id;


--
-- Name: users; Type: TABLE; Schema: public; Owner: garage_user
--

CREATE TABLE public.users (
    id character varying(36) NOT NULL,
    email character varying(255) NOT NULL,
    full_name character varying(255) NOT NULL,
    address text NOT NULL,
    wireguard_public_key text NOT NULL,
    wireguard_ip character varying(15),
    client_ip character varying(15) NOT NULL,
    client_ip_updated_at timestamp with time zone DEFAULT now(),
    node_id character varying(50),
    container_network character varying(18),
    plan_id character varying(50) NOT NULL,
    account_status character varying(20) DEFAULT 'active'::character varying,
    last_vpn_connection timestamp with time zone,
    created_at timestamp with time zone DEFAULT now(),
    updated_at timestamp with time zone DEFAULT now(),
    primary_garage_id character varying(50),
    user_slot integer,
    CONSTRAINT users_account_status_check CHECK (((account_status)::text = ANY (ARRAY[('active'::character varying)::text, ('suspended'::character varying)::text, ('pending'::character varying)::text]))),
    CONSTRAINT users_slot_limit CHECK (((user_slot >= 1) AND (user_slot <= 255)))
);


ALTER TABLE public.users OWNER TO garage_user;

--
-- Name: ingress_status; Type: VIEW; Schema: public; Owner: garage_user
--

CREATE VIEW public.ingress_status AS
 SELECT ir.id,
    ir.user_id,
    ir.container_id,
    ir.subdomain,
    ir.full_domain,
    ir.mode,
    ir.target_ip,
    ir.target_port,
    ir.public_port,
    ir.firewall_open,
    ir.is_active,
    ir.created_at,
    c.container_name,
    c.status AS container_status,
    c.image,
    u.email AS user_email
   FROM ((public.ingress_routes ir
     LEFT JOIN public.containers c ON (((ir.container_id)::text = (c.container_id)::text)))
     LEFT JOIN public.users u ON (((ir.user_id)::text = (u.id)::text)))
  WHERE (ir.is_active = true);


ALTER VIEW public.ingress_status OWNER TO garage_user;

--
-- Name: VIEW ingress_status; Type: COMMENT; Schema: public; Owner: garage_user
--

COMMENT ON VIEW public.ingress_status IS 'Active ingress routes with container and user info';


--
-- Name: ipv6_firewall_rules; Type: TABLE; Schema: public; Owner: garage_user
--

CREATE TABLE public.ipv6_firewall_rules (
    id integer NOT NULL,
    container_name character varying(100) NOT NULL,
    user_id character varying(36) NOT NULL,
    exposed_ports integer[] DEFAULT '{80,443}'::integer[],
    pfsense_rule_id character varying(50),
    pfsense_rule_synced boolean DEFAULT false,
    created_at timestamp with time zone DEFAULT now(),
    updated_at timestamp with time zone DEFAULT now()
);


ALTER TABLE public.ipv6_firewall_rules OWNER TO garage_user;

--
-- Name: ipv6_firewall_rules_id_seq; Type: SEQUENCE; Schema: public; Owner: garage_user
--

CREATE SEQUENCE public.ipv6_firewall_rules_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


ALTER SEQUENCE public.ipv6_firewall_rules_id_seq OWNER TO garage_user;

--
-- Name: ipv6_firewall_rules_id_seq; Type: SEQUENCE OWNED BY; Schema: public; Owner: garage_user
--

ALTER SEQUENCE public.ipv6_firewall_rules_id_seq OWNED BY public.ipv6_firewall_rules.id;


--
-- Name: nodes; Type: TABLE; Schema: public; Owner: garage_user
--

CREATE TABLE public.nodes (
    node_id character varying(50) NOT NULL,
    name character varying(100) NOT NULL,
    location character varying(100) NOT NULL,
    ip_range character varying(18) NOT NULL,
    wireguard_public_key text,
    wireguard_endpoint character varying(100),
    status character varying(20) DEFAULT 'active'::character varying,
    max_users integer DEFAULT 255,
    current_users integer DEFAULT 0,
    created_at timestamp with time zone DEFAULT now(),
    updated_at timestamp with time zone DEFAULT now(),
    garage_id character varying(50),
    hardware_type character varying(50),
    architecture character varying(20),
    cpu_cores integer,
    memory_gb integer,
    storage_type character varying(20),
    power_consumption_watts integer,
    tags jsonb,
    internal_ip character varying(15),
    network_interface character varying(20) DEFAULT 'eth0'::character varying,
    lan_ip character varying(45),
    CONSTRAINT check_current_users_not_exceed_max CHECK ((current_users <= max_users)),
    CONSTRAINT nodes_current_users_check CHECK ((current_users >= 0)),
    CONSTRAINT nodes_max_users_check CHECK ((max_users > 0)),
    CONSTRAINT nodes_status_check CHECK (((status)::text = ANY (ARRAY[('active'::character varying)::text, ('maintenance'::character varying)::text, ('inactive'::character varying)::text])))
);


ALTER TABLE public.nodes OWNER TO garage_user;

--
-- Name: COLUMN nodes.internal_ip; Type: COMMENT; Schema: public; Owner: garage_user
--

COMMENT ON COLUMN public.nodes.internal_ip IS 'Internal IP for routing between controller and agent (e.g., 10.88.0.1)';


--
-- Name: COLUMN nodes.network_interface; Type: COMMENT; Schema: public; Owner: garage_user
--

COMMENT ON COLUMN public.nodes.network_interface IS 'Network interface for routing (e.g., eth0, ens3)';


--
-- Name: plans; Type: TABLE; Schema: public; Owner: garage_user
--

CREATE TABLE public.plans (
    id character varying(50) NOT NULL,
    name character varying(100) NOT NULL,
    display_name character varying(100) NOT NULL,
    cpu character varying(20) NOT NULL,
    memory character varying(20) NOT NULL,
    storage character varying(20) NOT NULL,
    price character varying(20) NOT NULL,
    description text,
    is_active boolean DEFAULT true,
    created_at timestamp with time zone DEFAULT now()
);


ALTER TABLE public.plans OWNER TO garage_user;

--
-- Name: reserved_subdomains; Type: TABLE; Schema: public; Owner: garage_user
--

CREATE TABLE public.reserved_subdomains (
    subdomain character varying(63) NOT NULL,
    reason character varying(255),
    created_at timestamp with time zone DEFAULT now()
);


ALTER TABLE public.reserved_subdomains OWNER TO garage_user;

--
-- Name: user_network_info; Type: VIEW; Schema: public; Owner: garage_user
--

CREATE VIEW public.user_network_info AS
 SELECT u.id AS user_id,
    u.email,
    u.full_name,
    u.node_id AS assigned_node_id,
    n.name AS node_name,
    n.location AS node_location,
    '172.20.0.10'::character varying AS api_server_ip,
    u.container_network AS subnet_cidr,
    ((u.wireguard_ip)::text || '/32'::text) AS wireguard_single_ip,
    u.container_network AS container_range,
    count(c.id) AS container_count
   FROM ((public.users u
     LEFT JOIN public.nodes n ON (((u.node_id)::text = (n.node_id)::text)))
     LEFT JOIN public.containers c ON ((((u.id)::text = (c.user_id)::text) AND ((c.status)::text <> 'deleted'::text))))
  GROUP BY u.id, u.email, u.full_name, u.node_id, n.name, n.location, u.container_network, u.wireguard_ip;


ALTER VIEW public.user_network_info OWNER TO garage_user;

--
-- Name: container_volumes id; Type: DEFAULT; Schema: public; Owner: garage_user
--

ALTER TABLE ONLY public.container_volumes ALTER COLUMN id SET DEFAULT nextval('public.container_volumes_id_seq'::regclass);


--
-- Name: containers id; Type: DEFAULT; Schema: public; Owner: garage_user
--

ALTER TABLE ONLY public.containers ALTER COLUMN id SET DEFAULT nextval('public.containers_id_seq'::regclass);


--
-- Name: garage_container_allocations id; Type: DEFAULT; Schema: public; Owner: garage_user
--

ALTER TABLE ONLY public.garage_container_allocations ALTER COLUMN id SET DEFAULT nextval('public.garage_container_allocations_id_seq'::regclass);


--
-- Name: ingress_routes id; Type: DEFAULT; Schema: public; Owner: garage_user
--

ALTER TABLE ONLY public.ingress_routes ALTER COLUMN id SET DEFAULT nextval('public.ingress_routes_id_seq'::regclass);


--
-- Name: ipv6_firewall_rules id; Type: DEFAULT; Schema: public; Owner: garage_user
--

ALTER TABLE ONLY public.ipv6_firewall_rules ALTER COLUMN id SET DEFAULT nextval('public.ipv6_firewall_rules_id_seq'::regclass);


--
-- Name: container_config container_config_pkey; Type: CONSTRAINT; Schema: public; Owner: postgres
--

ALTER TABLE ONLY public.container_config
    ADD CONSTRAINT container_config_pkey PRIMARY KEY (container_name);


--
-- Name: container_volumes container_volumes_container_id_volume_name_key; Type: CONSTRAINT; Schema: public; Owner: garage_user
--

ALTER TABLE ONLY public.container_volumes
    ADD CONSTRAINT container_volumes_container_id_volume_name_key UNIQUE (container_id, volume_name);


--
-- Name: container_volumes container_volumes_pkey; Type: CONSTRAINT; Schema: public; Owner: garage_user
--

ALTER TABLE ONLY public.container_volumes
    ADD CONSTRAINT container_volumes_pkey PRIMARY KEY (id);


--
-- Name: containers containers_container_id_key; Type: CONSTRAINT; Schema: public; Owner: garage_user
--

ALTER TABLE ONLY public.containers
    ADD CONSTRAINT containers_container_id_key UNIQUE (container_id);


--
-- Name: containers containers_pkey; Type: CONSTRAINT; Schema: public; Owner: garage_user
--

ALTER TABLE ONLY public.containers
    ADD CONSTRAINT containers_pkey PRIMARY KEY (id);


--
-- Name: containers containers_public_url_key; Type: CONSTRAINT; Schema: public; Owner: garage_user
--

ALTER TABLE ONLY public.containers
    ADD CONSTRAINT containers_public_url_key UNIQUE (public_url);


--
-- Name: garage_container_allocations garage_container_allocations_garage_id_user_slot_key; Type: CONSTRAINT; Schema: public; Owner: garage_user
--

ALTER TABLE ONLY public.garage_container_allocations
    ADD CONSTRAINT garage_container_allocations_garage_id_user_slot_key UNIQUE (garage_id, user_slot);


--
-- Name: garage_container_allocations garage_container_allocations_pkey; Type: CONSTRAINT; Schema: public; Owner: garage_user
--

ALTER TABLE ONLY public.garage_container_allocations
    ADD CONSTRAINT garage_container_allocations_pkey PRIMARY KEY (id);


--
-- Name: garage_container_allocations garage_container_allocations_user_id_garage_id_key; Type: CONSTRAINT; Schema: public; Owner: garage_user
--

ALTER TABLE ONLY public.garage_container_allocations
    ADD CONSTRAINT garage_container_allocations_user_id_garage_id_key UNIQUE (user_id, garage_id);


--
-- Name: garages garages_pkey; Type: CONSTRAINT; Schema: public; Owner: garage_user
--

ALTER TABLE ONLY public.garages
    ADD CONSTRAINT garages_pkey PRIMARY KEY (garage_id);


--
-- Name: ingress_routes ingress_container_unique; Type: CONSTRAINT; Schema: public; Owner: garage_user
--

ALTER TABLE ONLY public.ingress_routes
    ADD CONSTRAINT ingress_container_unique UNIQUE (container_id);


--
-- Name: ingress_routes ingress_full_domain_unique; Type: CONSTRAINT; Schema: public; Owner: garage_user
--

ALTER TABLE ONLY public.ingress_routes
    ADD CONSTRAINT ingress_full_domain_unique UNIQUE (full_domain);


--
-- Name: ingress_port_pool ingress_port_pool_pkey; Type: CONSTRAINT; Schema: public; Owner: garage_user
--

ALTER TABLE ONLY public.ingress_port_pool
    ADD CONSTRAINT ingress_port_pool_pkey PRIMARY KEY (port);


--
-- Name: ingress_routes ingress_public_port_unique; Type: CONSTRAINT; Schema: public; Owner: garage_user
--

ALTER TABLE ONLY public.ingress_routes
    ADD CONSTRAINT ingress_public_port_unique UNIQUE (public_port);


--
-- Name: ingress_routes ingress_routes_pkey; Type: CONSTRAINT; Schema: public; Owner: garage_user
--

ALTER TABLE ONLY public.ingress_routes
    ADD CONSTRAINT ingress_routes_pkey PRIMARY KEY (id);


--
-- Name: ingress_routes ingress_subdomain_unique; Type: CONSTRAINT; Schema: public; Owner: garage_user
--

ALTER TABLE ONLY public.ingress_routes
    ADD CONSTRAINT ingress_subdomain_unique UNIQUE (subdomain);


--
-- Name: ipv6_firewall_rules ipv6_firewall_rules_container_name_key; Type: CONSTRAINT; Schema: public; Owner: garage_user
--

ALTER TABLE ONLY public.ipv6_firewall_rules
    ADD CONSTRAINT ipv6_firewall_rules_container_name_key UNIQUE (container_name);


--
-- Name: ipv6_firewall_rules ipv6_firewall_rules_pkey; Type: CONSTRAINT; Schema: public; Owner: garage_user
--

ALTER TABLE ONLY public.ipv6_firewall_rules
    ADD CONSTRAINT ipv6_firewall_rules_pkey PRIMARY KEY (id);


--
-- Name: nodes nodes_pkey; Type: CONSTRAINT; Schema: public; Owner: garage_user
--

ALTER TABLE ONLY public.nodes
    ADD CONSTRAINT nodes_pkey PRIMARY KEY (node_id);


--
-- Name: plans plans_pkey; Type: CONSTRAINT; Schema: public; Owner: garage_user
--

ALTER TABLE ONLY public.plans
    ADD CONSTRAINT plans_pkey PRIMARY KEY (id);


--
-- Name: reserved_subdomains reserved_subdomains_pkey; Type: CONSTRAINT; Schema: public; Owner: garage_user
--

ALTER TABLE ONLY public.reserved_subdomains
    ADD CONSTRAINT reserved_subdomains_pkey PRIMARY KEY (subdomain);


--
-- Name: users users_email_key; Type: CONSTRAINT; Schema: public; Owner: garage_user
--

ALTER TABLE ONLY public.users
    ADD CONSTRAINT users_email_key UNIQUE (email);


--
-- Name: users users_pkey; Type: CONSTRAINT; Schema: public; Owner: garage_user
--

ALTER TABLE ONLY public.users
    ADD CONSTRAINT users_pkey PRIMARY KEY (id);


--
-- Name: idx_container_config_user; Type: INDEX; Schema: public; Owner: postgres
--

CREATE INDEX idx_container_config_user ON public.container_config USING btree (user_id);


--
-- Name: idx_containers_ipv6_enabled; Type: INDEX; Schema: public; Owner: garage_user
--

CREATE INDEX idx_containers_ipv6_enabled ON public.containers USING btree (ipv6_enabled) WHERE (ipv6_enabled = true);


--
-- Name: idx_containers_node; Type: INDEX; Schema: public; Owner: garage_user
--

CREATE INDEX idx_containers_node ON public.containers USING btree (node_id);


--
-- Name: idx_containers_status; Type: INDEX; Schema: public; Owner: garage_user
--

CREATE INDEX idx_containers_status ON public.containers USING btree (status);


--
-- Name: idx_containers_user; Type: INDEX; Schema: public; Owner: garage_user
--

CREATE INDEX idx_containers_user ON public.containers USING btree (user_id);


--
-- Name: idx_containers_user_active; Type: INDEX; Schema: public; Owner: garage_user
--

CREATE INDEX idx_containers_user_active ON public.containers USING btree (user_id, status) WHERE ((status)::text <> 'deleted'::text);


--
-- Name: idx_ingress_container; Type: INDEX; Schema: public; Owner: garage_user
--

CREATE INDEX idx_ingress_container ON public.ingress_routes USING btree (container_id);


--
-- Name: idx_ingress_port_pool_available; Type: INDEX; Schema: public; Owner: garage_user
--

CREATE INDEX idx_ingress_port_pool_available ON public.ingress_port_pool USING btree (is_allocated) WHERE (NOT is_allocated);


--
-- Name: idx_ingress_routes_active; Type: INDEX; Schema: public; Owner: garage_user
--

CREATE INDEX idx_ingress_routes_active ON public.ingress_routes USING btree (is_active) WHERE (is_active = true);


--
-- Name: idx_ingress_routes_container; Type: INDEX; Schema: public; Owner: garage_user
--

CREATE INDEX idx_ingress_routes_container ON public.ingress_routes USING btree (container_id);


--
-- Name: idx_ingress_routes_subdomain; Type: INDEX; Schema: public; Owner: garage_user
--

CREATE INDEX idx_ingress_routes_subdomain ON public.ingress_routes USING btree (subdomain);


--
-- Name: idx_ingress_routes_user; Type: INDEX; Schema: public; Owner: garage_user
--

CREATE INDEX idx_ingress_routes_user ON public.ingress_routes USING btree (user_id);


--
-- Name: idx_ingress_subdomain; Type: INDEX; Schema: public; Owner: garage_user
--

CREATE INDEX idx_ingress_subdomain ON public.ingress_routes USING btree (subdomain) WHERE (is_active = true);


--
-- Name: idx_ingress_user; Type: INDEX; Schema: public; Owner: garage_user
--

CREATE INDEX idx_ingress_user ON public.ingress_routes USING btree (user_id);


--
-- Name: idx_unique_container_ingress; Type: INDEX; Schema: public; Owner: garage_user
--

CREATE UNIQUE INDEX idx_unique_container_ingress ON public.ingress_routes USING btree (container_id) WHERE (is_active = true);


--
-- Name: idx_unique_subdomain_active; Type: INDEX; Schema: public; Owner: garage_user
--

CREATE UNIQUE INDEX idx_unique_subdomain_active ON public.ingress_routes USING btree (subdomain) WHERE (is_active = true);


--
-- Name: idx_users_account_status; Type: INDEX; Schema: public; Owner: garage_user
--

CREATE INDEX idx_users_account_status ON public.users USING btree (account_status);


--
-- Name: idx_users_client_ip; Type: INDEX; Schema: public; Owner: garage_user
--

CREATE INDEX idx_users_client_ip ON public.users USING btree (client_ip);


--
-- Name: idx_users_email; Type: INDEX; Schema: public; Owner: garage_user
--

CREATE INDEX idx_users_email ON public.users USING btree (email);


--
-- Name: ingress_routes trigger_ingress_updated_at; Type: TRIGGER; Schema: public; Owner: garage_user
--

CREATE TRIGGER trigger_ingress_updated_at BEFORE UPDATE ON public.ingress_routes FOR EACH ROW EXECUTE FUNCTION public.update_ingress_timestamp();


--
-- Name: container_volumes container_volumes_container_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: garage_user
--

ALTER TABLE ONLY public.container_volumes
    ADD CONSTRAINT container_volumes_container_id_fkey FOREIGN KEY (container_id) REFERENCES public.containers(container_id);


--
-- Name: containers containers_node_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: garage_user
--

ALTER TABLE ONLY public.containers
    ADD CONSTRAINT containers_node_id_fkey FOREIGN KEY (node_id) REFERENCES public.nodes(node_id);


--
-- Name: containers containers_user_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: garage_user
--

ALTER TABLE ONLY public.containers
    ADD CONSTRAINT containers_user_id_fkey FOREIGN KEY (user_id) REFERENCES public.users(id) ON DELETE CASCADE;


--
-- Name: garage_container_allocations garage_container_allocations_garage_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: garage_user
--

ALTER TABLE ONLY public.garage_container_allocations
    ADD CONSTRAINT garage_container_allocations_garage_id_fkey FOREIGN KEY (garage_id) REFERENCES public.garages(garage_id);


--
-- Name: garage_container_allocations garage_container_allocations_user_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: garage_user
--

ALTER TABLE ONLY public.garage_container_allocations
    ADD CONSTRAINT garage_container_allocations_user_id_fkey FOREIGN KEY (user_id) REFERENCES public.users(id);


--
-- Name: ingress_routes ingress_routes_public_port_fkey; Type: FK CONSTRAINT; Schema: public; Owner: garage_user
--

ALTER TABLE ONLY public.ingress_routes
    ADD CONSTRAINT ingress_routes_public_port_fkey FOREIGN KEY (public_port) REFERENCES public.ingress_port_pool(port);


--
-- Name: ingress_routes ingress_routes_user_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: garage_user
--

ALTER TABLE ONLY public.ingress_routes
    ADD CONSTRAINT ingress_routes_user_id_fkey FOREIGN KEY (user_id) REFERENCES public.users(id) ON DELETE CASCADE;


--
-- Name: nodes nodes_garage_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: garage_user
--

ALTER TABLE ONLY public.nodes
    ADD CONSTRAINT nodes_garage_id_fkey FOREIGN KEY (garage_id) REFERENCES public.garages(garage_id);


--
-- Name: users users_node_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: garage_user
--

ALTER TABLE ONLY public.users
    ADD CONSTRAINT users_node_id_fkey FOREIGN KEY (node_id) REFERENCES public.nodes(node_id);


--
-- Name: users users_plan_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: garage_user
--

ALTER TABLE ONLY public.users
    ADD CONSTRAINT users_plan_id_fkey FOREIGN KEY (plan_id) REFERENCES public.plans(id);


--
-- Name: users users_primary_garage_id_fkey; Type: FK CONSTRAINT; Schema: public; Owner: garage_user
--

ALTER TABLE ONLY public.users
    ADD CONSTRAINT users_primary_garage_id_fkey FOREIGN KEY (primary_garage_id) REFERENCES public.garages(garage_id);


--
-- Name: SCHEMA public; Type: ACL; Schema: -; Owner: pg_database_owner
--

GRANT ALL ON SCHEMA public TO garage_user;


--
-- Name: TABLE container_config; Type: ACL; Schema: public; Owner: postgres
--

GRANT ALL ON TABLE public.container_config TO garage_user;


--
-- Name: DEFAULT PRIVILEGES FOR SEQUENCES; Type: DEFAULT ACL; Schema: public; Owner: postgres
--

ALTER DEFAULT PRIVILEGES FOR ROLE postgres IN SCHEMA public GRANT ALL ON SEQUENCES TO garage_user;


--
-- Name: DEFAULT PRIVILEGES FOR FUNCTIONS; Type: DEFAULT ACL; Schema: public; Owner: postgres
--

ALTER DEFAULT PRIVILEGES FOR ROLE postgres IN SCHEMA public GRANT ALL ON FUNCTIONS TO garage_user;


--
-- Name: DEFAULT PRIVILEGES FOR TABLES; Type: DEFAULT ACL; Schema: public; Owner: postgres
--

ALTER DEFAULT PRIVILEGES FOR ROLE postgres IN SCHEMA public GRANT ALL ON TABLES TO garage_user;


--
-- PostgreSQL database dump complete
--

\unrestrict S1AtI0fvXDZrGcXCsmdjM2f2y6soVb7NHnF8lWdk563D7I47e81bE9uxPhhDQ0d

