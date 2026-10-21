-- Catalog-driven MEANS. No explicit DDL: the first predicate bootstraps
-- backend hutch -> model default -> rvbbit.means -> operator MEANS.
-- Hutch must be the mock on 127.0.0.1:8099 (RVBBIT_HUTCH_URL).
CREATE TABLE rvbbit_means_t (id int, body text);
INSERT INTO rvbbit_means_t VALUES (1, 'hello alpha'), (2, 'nope');

-- The first predicate bootstraps the chain. Inspect the catalog after it.
SELECT body FROM rvbbit_means_t WHERE body MEANS 'alpha' ORDER BY id;

SELECT b.name AS backend, b.url, m.name AS model, m.remote_name, f.name AS func, p.proname, o.word
FROM rvbbit.semop o
JOIN rvbbit.semfn f ON f.name = o.func_name AND f.nsp = 'rvbbit'
JOIN pg_proc p ON p.oid = f.procoid
JOIN rvbbit.model m ON m.name = f.model_name
JOIN rvbbit.backend b ON b.oid = m.backend_oid
WHERE o.word = 'means';

SELECT count(*) FROM rvbbit_means_t WHERE id > 1 AND body MEANS 'alpha';

SET max_parallel_workers_per_gather = 0;
EXPLAIN (COSTS OFF)
SELECT count(*) FROM rvbbit_means_t WHERE body MEANS 'alpha';

\df rvbbit.means

DROP TABLE rvbbit_means_t;
-- Leave the bootstrap chain. Later sessions in this database may use it.
-- Upstream regression uses a different database and never creates it.
