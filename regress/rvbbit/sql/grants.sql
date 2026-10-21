CREATE ROLE rvbbit_reader;
CREATE BACKEND grant_b TYPE hutch URL 'http://127.0.0.1:9';
CREATE MODEL grant_m ON BACKEND grant_b NAME 'remote';
CREATE SEMANTIC FUNCTION grant_f(body text, criterion text) RETURNS boolean MODEL grant_m;

SELECT has_function_privilege('rvbbit_reader', 'rvbbit.grant_f(text, text)', 'EXECUTE');
REVOKE EXECUTE ON SEMANTIC FUNCTION grant_f(body text, criterion text) FROM public;
SELECT has_function_privilege('rvbbit_reader', 'rvbbit.grant_f(text, text)', 'EXECUTE');
GRANT EXECUTE ON SEMANTIC FUNCTION grant_f(body text, criterion text) TO rvbbit_reader;
SELECT has_function_privilege('rvbbit_reader', 'rvbbit.grant_f(text, text)', 'EXECUTE');

GRANT USAGE ON BACKEND grant_b TO rvbbit_reader;
SELECT acl FROM rvbbit.backend WHERE name = 'grant_b';
REVOKE USAGE ON BACKEND grant_b FROM rvbbit_reader;
SELECT acl FROM rvbbit.backend WHERE name = 'grant_b';

DROP SEMANTIC FUNCTION grant_f;
DROP MODEL grant_m;
DROP BACKEND grant_b;
DROP ROLE rvbbit_reader;
