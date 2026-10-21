-- CREATE / ALTER / DROP for every catalog kind. Command tags reuse the
-- pinned PG 18.3 tags, so psql prints CREATE FUNCTION and friends.
CREATE BACKEND b1 TYPE hutch URL 'http://127.0.0.1:1' AUTH ENV 'RVBBIT_TEST_TOKEN' WITH (batch_size = 8, max_concurrent = 2, timeout = '1500ms', retries = 3);
CREATE EMBEDDING MODEL embed1 ON BACKEND b1 NAME 'embed-remote' DIMENSIONS 8;
CREATE MODEL chat1 ON BACKEND b1 NAME 'chat-remote' COST (input = 1, output = 2);
CREATE SEMANTIC FUNCTION likes(body text, criterion text) RETURNS boolean MODEL chat1 PROMPT 'does it' TEMPERATURE 0 CACHE memoize VERSION 1;
CREATE SEMANTIC OPERATOR likes (LEFTARG = text, RIGHTARG = text, FUNCTION = likes, SYMBOL = '~~?', NEGATOR = NOT likes);
CREATE DECISION TREE route1 (ticket text) MODEL chat1 AS ASK 'urgent?' YES: 'page' NO: 'queue';
CREATE WORKFLOW bundle1 (ticket text) RETURNS text VERSION 1 AS $$SELECT 1$$;
ALTER BACKEND b1 SET URL 'http://127.0.0.1:2';
ALTER BACKEND b1 SET (max_concurrent = 3);
ALTER MODEL chat1 SET NAME 'chat-remote-2';
ALTER SEMANTIC FUNCTION likes(body text, criterion text) SET PROMPT 'still';
ALTER SEMANTIC OPERATOR likes SET FUNCTION likes;
ALTER DECISION TREE route1 SET MODEL chat1;
ALTER WORKFLOW bundle1 VERSION 1 AS $$SELECT 2$$;
COMMENT ON BACKEND b1 IS 'primary';
COMMENT ON MODEL chat1 IS 'chat';
COMMENT ON SEMANTIC FUNCTION likes IS 'predicate';
COMMENT ON SEMANTIC OPERATOR likes IS 'infix';
COMMENT ON DECISION TREE route1 IS 'tree';
COMMENT ON WORKFLOW bundle1 IS 'flow';

SELECT name, kind, url, auth_env, batch_size, max_concurrent, timeout_ms, retries, version, comment FROM rvbbit.backends ORDER BY name;
SELECT name, backend, remote_name, kind, dimensions, cost_input, cost_output, version, comment FROM rvbbit.models ORDER BY name;
SELECT name, rettype, model_name, temperature, cache_mode, version, comment FROM rvbbit.semantic_functions ORDER BY name;
SELECT word, symbol, lefttype, righttype, func_name, negator, version, comment FROM rvbbit.semantic_operators ORDER BY word;
SELECT name, model_name, version, comment FROM rvbbit.decision_trees ORDER BY name;
SELECT name, version, args, returns, comment FROM rvbbit.workflows ORDER BY name, version;
SELECT description FROM pg_description d JOIN rvbbit.backend b ON b.oid = d.objoid WHERE b.name = 'b1';
SELECT p.proname, n.nspname, p.prosrc FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace WHERE n.nspname = 'rvbbit' AND p.proname = 'likes';
SELECT rvbbit.pg_get_semanticfunctiondef('likes') LIKE '%still%';

\dt rvbbit.*
\dv rvbbit.*
\df rvbbit.likes

DROP WORKFLOW bundle1 VERSION 1;
DROP SEMANTIC OPERATOR likes;
DROP DECISION TREE route1;
DROP SEMANTIC FUNCTION likes(body text, criterion text);
DROP MODEL embed1;
DROP MODEL chat1;
DROP BACKEND b1;
SELECT count(*) FROM rvbbit.backend;
SELECT count(*) FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace WHERE n.nspname = 'rvbbit' AND p.proname = 'likes';
