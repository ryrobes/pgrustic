CREATE BACKEND dep_b TYPE hutch URL 'http://127.0.0.1:9';
CREATE MODEL dep_m ON BACKEND dep_b NAME 'remote';
CREATE SEMANTIC FUNCTION dep_f(body text, criterion text) RETURNS boolean MODEL dep_m;
CREATE SEMANTIC OPERATOR dep_op (LEFTARG = text, RIGHTARG = text, FUNCTION = dep_f);
CREATE DECISION TREE dep_t (ticket text) MODEL dep_m AS CHOOSE 'which?' WHEN 'a' THEN 'left' WHEN 'b' MEANING 'other' THEN 'right';

SELECT c.relname AS dependent, r.relname AS referenced, d.deptype
FROM pg_depend d
JOIN pg_class c ON c.oid = d.classid
JOIN pg_namespace nc ON nc.oid = c.relnamespace
JOIN pg_class r ON r.oid = d.refclassid
WHERE nc.nspname = 'rvbbit'
ORDER BY 1, 2, 3;

-- RESTRICT: the model still points at the backend.
DROP BACKEND dep_b;
DROP MODEL dep_m;
DROP SEMANTIC FUNCTION dep_f;

DROP BACKEND dep_b CASCADE;
SELECT count(*) FROM rvbbit.backend WHERE name = 'dep_b';
SELECT count(*) FROM rvbbit.model WHERE name = 'dep_m';
SELECT count(*) FROM rvbbit.semfn WHERE name = 'dep_f';
SELECT count(*) FROM rvbbit.semop WHERE word = 'dep_op';
SELECT count(*) FROM rvbbit.decision_tree WHERE name = 'dep_t';
SELECT count(*) FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace WHERE n.nspname = 'rvbbit' AND p.proname = 'dep_f';
SELECT count(*) FROM pg_depend d
JOIN pg_class c ON c.oid = d.classid
JOIN pg_namespace nc ON nc.oid = c.relnamespace
WHERE nc.nspname = 'rvbbit';
