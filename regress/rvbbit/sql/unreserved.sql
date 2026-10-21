-- rvbbit DDL words stay unreserved in ordinary SQL. This session must
-- not create schema rvbbit.
SELECT 1 AS backend;
SELECT 1 AS model;
SELECT 1 AS semantic;
SELECT 1 AS means;
SELECT means FROM (SELECT 2 AS means) s;
CREATE TEMP TABLE means_ident (means int);
INSERT INTO means_ident VALUES (3);
SELECT means FROM means_ident;
SELECT nspname FROM pg_namespace WHERE nspname = 'rvbbit';
