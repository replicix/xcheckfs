-- Hundreds of relation files: 200 hash partitions, each with a heap, a TOAST
-- table + index, a primary key, a secondary index, and FSM/VM forks.
CREATE TABLE xc_part (
    k       int  PRIMARY KEY,
    n       int  NOT NULL DEFAULT 0,
    payload text NOT NULL
) PARTITION BY HASH (k);
DO $$
BEGIN
    FOR i IN 0..199 LOOP
        EXECUTE format('CREATE TABLE xc_part_%s PARTITION OF xc_part FOR VALUES WITH (MODULUS 200, REMAINDER %s)', i, i);
    END LOOP;
END $$;
CREATE INDEX xc_part_n ON xc_part (n);
INSERT INTO xc_part SELECT g, 0, repeat(md5(g::text), 1 + g % 60) FROM generate_series(1, 200000) g;
-- Steady-state insert/delete queue (autovacuum truncates and reuses its pages).
CREATE TABLE xc_churn (id bigserial PRIMARY KEY, ts timestamptz NOT NULL DEFAULT now(), payload text NOT NULL);
INSERT INTO xc_churn (payload) SELECT repeat('c', 500) FROM generate_series(1, 20000);
VACUUM ANALYZE;
