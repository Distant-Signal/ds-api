-- The tables of ds-core migrations up to 20261120130200 that the PITR train
-- extraction touches (pg_dump -s), plus a minimal users table for the FK.
CREATE TABLE public.users (id text PRIMARY KEY);
CREATE TABLE public.train_current_state (
    status text DEFAULT 'awaiting_activation'::text NOT NULL,
    last_reported_location text,
    last_event_type text,
    delay_minutes integer,
    next_calling_point text,
    eta_next timestamp with time zone,
    eta_source text,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    id bigint NOT NULL,
    trains_id bigint,
    event_time timestamp with time zone,
    CONSTRAINT train_current_state_eta_source_check CHECK ((eta_source = ANY (ARRAY['trust-propagated'::text, 'darwin-estimated'::text]))),
    CONSTRAINT train_current_state_status_check CHECK ((status = ANY (ARRAY['awaiting_activation'::text, 'en_route'::text, 'cancelled'::text, 'completed'::text])))
);
CREATE TABLE public.train_subscriptions (
    id bigint CONSTRAINT tracked_trains_id_not_null NOT NULL,
    user_id text CONSTRAINT tracked_trains_user_id_not_null NOT NULL,
    service_date date CONSTRAINT tracked_trains_service_date_not_null NOT NULL,
    pin_origin_crs text,
    pin_scheduled_departure timestamp with time zone,
    pin_destination_crs text,
    pin_operator text,
    resolution_status text DEFAULT 'pending'::text CONSTRAINT tracked_trains_resolution_status_not_null NOT NULL,
    tracked_at timestamp with time zone DEFAULT now() CONSTRAINT tracked_trains_tracked_at_not_null NOT NULL,
    custom_name text,
    trains_id bigint,
    notifications_enabled boolean DEFAULT true CONSTRAINT tracked_trains_notifications_enabled_not_null NOT NULL,
    pin_skipped_stations text[] DEFAULT '{}'::text[] NOT NULL,
    pin_platform text,
    pin_planned_platform text,
    unresolved_from text,
    tracked_standalone boolean DEFAULT false NOT NULL,
    pin_origin_ref text,
    pin_destination_ref text,
    public_id text DEFAULT ('t_'::text || replace((gen_random_uuid())::text, '-'::text, ''::text)),
    CONSTRAINT tracked_trains_resolution_status_check CHECK ((resolution_status = ANY (ARRAY['pending'::text, 'schedule_matched'::text, 'resolved'::text, 'unresolved'::text]))),
    CONSTRAINT train_subscriptions_pin_ref_check CHECK ((((pin_origin_ref IS NULL) OR (pin_origin_ref ~ '^[a-z]{2,8}:[A-Z0-9]{1,12}$'::text)) AND ((pin_destination_ref IS NULL) OR (pin_destination_ref ~ '^[a-z]{2,8}:[A-Z0-9]{1,12}$'::text)))),
    CONSTRAINT train_subscriptions_public_id_format CHECK (((public_id IS NOT NULL) AND (public_id ~ '^t_[0-9a-f]{32}$'::text)))
);
CREATE TABLE public.trains (
    id bigint NOT NULL,
    train_uid text NOT NULL,
    service_date date NOT NULL,
    origin_crs text,
    scheduled_departure timestamp with time zone,
    destination_crs text,
    calling_points jsonb,
    matched_line_id text,
    schedule_matched_at timestamp with time zone,
    train_id text,
    resolved_at timestamp with time zone,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    skipped_stations text[] DEFAULT '{}'::text[] NOT NULL,
    platform text,
    planned_platform text,
    origin_name text,
    destination_name text,
    CONSTRAINT trains_endpoint_names_check CHECK ((((origin_name IS NULL) OR ((char_length(origin_name) >= 1) AND (char_length(origin_name) <= 100))) AND ((destination_name IS NULL) OR ((char_length(destination_name) >= 1) AND (char_length(destination_name) <= 100)))))
);
CREATE SEQUENCE public.tracked_trains_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;
ALTER SEQUENCE public.tracked_trains_id_seq OWNED BY public.train_subscriptions.id;
CREATE SEQUENCE public.train_current_state_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;
ALTER SEQUENCE public.train_current_state_id_seq OWNED BY public.train_current_state.id;
CREATE TABLE public.train_movement_events (
    id bigint NOT NULL,
    dedup_key text NOT NULL,
    msg_type text NOT NULL,
    event_type text,
    loc_stanox text,
    loc_crs text,
    planned_timestamp timestamp with time zone,
    actual_timestamp timestamp with time zone,
    variation_status text,
    raw_body jsonb NOT NULL,
    received_at timestamp with time zone DEFAULT now() NOT NULL,
    trains_id bigint,
    gbtt_timestamp timestamp with time zone,
    platform text
)
WITH (autovacuum_vacuum_scale_factor='0.02', autovacuum_analyze_scale_factor='0.02');
CREATE SEQUENCE public.train_movement_events_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;
ALTER SEQUENCE public.train_movement_events_id_seq OWNED BY public.train_movement_events.id;
CREATE TABLE public.train_reasons (
    trains_id bigint NOT NULL,
    msg_type text NOT NULL,
    reason_code text NOT NULL,
    canx_type text,
    loc_stanox text,
    event_at timestamp with time zone,
    received_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT train_reasons_msg_type_check CHECK ((msg_type = ANY (ARRAY['0002'::text, '0005'::text, '0006'::text])))
);
CREATE TABLE public.train_service_labels (
    trains_id bigint NOT NULL,
    service_class text NOT NULL,
    toc_id text,
    CONSTRAINT train_service_labels_service_class_check CHECK ((service_class = ANY (ARRAY['passenger_express'::text, 'passenger_ordinary'::text, 'ecs'::text, 'ecs_or_parcels'::text, 'freight'::text, 'freight_or_engineering'::text, 'light_loco'::text, 'special'::text, 'unknown'::text]))),
    CONSTRAINT train_service_labels_toc_id_check CHECK (((toc_id IS NULL) OR ((char_length(toc_id) >= 1) AND (char_length(toc_id) <= 4))))
);
CREATE SEQUENCE public.trains_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;
ALTER SEQUENCE public.trains_id_seq OWNED BY public.trains.id;
ALTER TABLE ONLY public.train_current_state ALTER COLUMN id SET DEFAULT nextval('public.train_current_state_id_seq'::regclass);
ALTER TABLE ONLY public.train_movement_events ALTER COLUMN id SET DEFAULT nextval('public.train_movement_events_id_seq'::regclass);
ALTER TABLE ONLY public.train_subscriptions ALTER COLUMN id SET DEFAULT nextval('public.tracked_trains_id_seq'::regclass);
ALTER TABLE ONLY public.trains ALTER COLUMN id SET DEFAULT nextval('public.trains_id_seq'::regclass);
ALTER TABLE ONLY public.train_subscriptions
    ADD CONSTRAINT tracked_trains_pkey PRIMARY KEY (id);
ALTER TABLE ONLY public.train_current_state
    ADD CONSTRAINT train_current_state_pkey PRIMARY KEY (id);
ALTER TABLE ONLY public.train_movement_events
    ADD CONSTRAINT train_movement_events_pkey PRIMARY KEY (id);
ALTER TABLE ONLY public.train_reasons
    ADD CONSTRAINT train_reasons_pkey PRIMARY KEY (trains_id, msg_type);
ALTER TABLE ONLY public.train_service_labels
    ADD CONSTRAINT train_service_labels_pkey PRIMARY KEY (trains_id);
ALTER TABLE ONLY public.trains
    ADD CONSTRAINT trains_pkey PRIMARY KEY (id);
ALTER TABLE ONLY public.trains
    ADD CONSTRAINT trains_train_uid_service_date_key UNIQUE (train_uid, service_date);
CREATE UNIQUE INDEX train_current_state_trains_id ON public.train_current_state USING btree (trains_id) WHERE (trains_id IS NOT NULL);
CREATE INDEX train_movement_events_trains_id ON public.train_movement_events USING btree (trains_id, received_at) WHERE (trains_id IS NOT NULL);
CREATE UNIQUE INDEX train_movement_events_trains_id_dedup ON public.train_movement_events USING btree (trains_id, dedup_key) WHERE (trains_id IS NOT NULL);
CREATE UNIQUE INDEX train_subscriptions_public_id ON public.train_subscriptions USING btree (public_id);
CREATE INDEX train_subscriptions_resolution_status ON public.train_subscriptions USING btree (resolution_status);
CREATE INDEX train_subscriptions_trains_id ON public.train_subscriptions USING btree (trains_id);
CREATE INDEX train_subscriptions_user_id ON public.train_subscriptions USING btree (user_id);
CREATE INDEX trains_service_date ON public.trains USING btree (service_date);
CREATE UNIQUE INDEX trains_train_id_service_date ON public.trains USING btree (train_id, service_date) WHERE (train_id IS NOT NULL);
ALTER TABLE ONLY public.train_subscriptions
    ADD CONSTRAINT tracked_trains_trains_id_fkey FOREIGN KEY (trains_id) REFERENCES public.trains(id) ON DELETE SET NULL;
ALTER TABLE ONLY public.train_subscriptions
    ADD CONSTRAINT tracked_trains_user_id_fkey FOREIGN KEY (user_id) REFERENCES public.users(id) ON UPDATE CASCADE ON DELETE CASCADE;
ALTER TABLE ONLY public.train_current_state
    ADD CONSTRAINT train_current_state_trains_id_fkey FOREIGN KEY (trains_id) REFERENCES public.trains(id) ON DELETE CASCADE;
ALTER TABLE ONLY public.train_movement_events
    ADD CONSTRAINT train_movement_events_trains_id_fkey FOREIGN KEY (trains_id) REFERENCES public.trains(id) ON DELETE CASCADE;
ALTER TABLE ONLY public.train_reasons
    ADD CONSTRAINT train_reasons_trains_id_fkey FOREIGN KEY (trains_id) REFERENCES public.trains(id) ON DELETE CASCADE;
ALTER TABLE ONLY public.train_service_labels
    ADD CONSTRAINT train_service_labels_trains_id_fkey FOREIGN KEY (trains_id) REFERENCES public.trains(id) ON DELETE CASCADE;
