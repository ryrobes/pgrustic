# pg_rvbbit call styles on the native syntax

pg_rvbbit had three ways to call a semantic operation: an infix operator,
a function in schema `rvbbit`, and `rvbbit.flow` for a stored workflow.
This tree keeps all three. The work inside each call is the native syntax
in [GRAMMAR.md](GRAMMAR.md): `MEANS`, `ABOUT`, `CLASSIFY`, `EXTRACT`,
`|>`, and a decision tree. The signatures below are the ones this catalog
creates. They are not copied from Calliope.

The schema, including these shims, is created the first time a statement
parses `MEANS` or `ABOUT`. A database that never does either, and never
runs rvbbit DDL, has no `rvbbit` schema. `rvbbit.flow`, `rvbbit.extract`,
and the two-argument `rvbbit.classify` are created with that schema.
`rvbbit.means(text, text)` is the operator's function and is created with
it. `rvbbit.means_score(text, text)` is created in the same step unless a
function of that name is already there.

## Operator

| Call | What it runs |
| --- | --- |
| `expr MEANS 'criterion'` | Decision. True when the yes-probability clears the cutoff (server default 0.5, or the function or operator `THRESHOLD`). |
| `expr ABOUT 'criterion'` | Cosine of the two embeddings. Default cutoff 0.30. Not a MEANS decision. |
| `NOT (expr ABOUT 'criterion')` | The predicate negated. There is no catalog negator named `ABOUT`. |

`MEANS` and `ABOUT` are the words the lexer treats as operators. A
`CREATE SEMANTIC OPERATOR` row for any other word is stored, dumped, and
granted, and it is not an infix operator. Call its function.

`|>>` is the geometric operator. A semantic pipe is `|>`.

## Function call

These are the common names. Each one is the function-call form of a native
expression. A name the user has already created is not replaced.

| Call | Native expression |
| --- | --- |
| `rvbbit.means(expr, 'criterion')` | `expr MEANS 'criterion'` |
| `rvbbit.means_score(expr, 'criterion')` | the yes-probability. `expr \|> means_score 'criterion'` is the same call |
| `rvbbit.extract(expr, 'kind')` | `EXTRACT(kind FROM expr)`. The second argument is the kind (`creature`, `int`, `text`, …). A model or parse failure is an error; `ON ERROR` is the native form |
| `rvbbit.classify(expr, 'a,b')` | `CLASSIFY(expr INTO 'a', 'b')`. Commas separate labels. A comma inside a label uses the same U+001F separator the native rewrite uses |
| `rvbbit.classify(expr, labels, fallback)` | `CLASSIFY(expr INTO …) ON ERROR fallback`. `labels` is already U+001F-separated. The result is the bare label |
| `rvbbit.decide('tree', expr)` | the stored `CREATE DECISION TREE` |
| `rvbbit.summarize(expr, clauses)` | `SUMMARIZE`. `clauses` is the tail (`WITH TAKES 3`), or `''` |
| `rvbbit.consensus(expr, 'criterion', clauses)` | `CONSENSUS` |
| `rvbbit.rerank(expr, 'query')` | `expr \|> rerank 'query'` |
| `rvbbit.semantic_similarity(a, b)` | the cosine `ABOUT` compares to 0.30 |

A catalog `CREATE SEMANTIC FUNCTION … RETURNS text LABELS (...)` still
returns JSON `{"label","score","scores"}`. Native `CLASSIFY` and
`rvbbit.classify` return the label text. A catalog `RETURNS text` without
labels is still `rvbbit_extract` and still wants JSON. The shim
`rvbbit.extract(input, kind)` is the native scalar extract, and it is
skipped when a function named `extract` already exists.

`WHERE expr MEANS 'criterion'` and `WHERE expr ABOUT 'criterion'` batch.
The three-argument `rvbbit.classify`, `rvbbit.rerank`, and
`rvbbit.semantic_similarity` batch on a Semantic Project. The
two-argument `rvbbit.classify` and `rvbbit.extract` call those same
builtins one row at a time. `rvbbit.means` and `rvbbit.means_score` in a
target list do too. That is the builtin the operator uses when the
predicate is not a scan filter.

## Flow

`CREATE WORKFLOW` stores a name, a version, an argument list, and a SQL
body. `rvbbit.flow` runs that body.

```sql
CREATE WORKFLOW route (ticket text) RETURNS text AS $$
  CLASSIFY({{ticket}} INTO 'biped', 'invoice')
$$;

SELECT rvbbit.flow('route', notes) FROM notes;
SELECT rvbbit.flow('route', notes, 2) FROM notes;  -- that version
```

`{{input}}` and the first argument name (`{{ticket}}` above) are replaced
with one quoted literal. A later argument is not bound. The body is one
statement. An expression is run as `SELECT (expr)`. A `SELECT`, `WITH`, or
`VALUES` body is run as written. The first column of the first row is the
text result, so a boolean body comes back as `t` or `f`.

`MEANS` inside the body stays an operator. `CLASSIFY`, `EXTRACT`, and
`|>` are rewritten to the same functions a query uses. `rvbbit.flow`
runs that one statement. A body that scans a table batches the same way
the statement would on its own. A call that passes one input runs one row.

A pipe that should inline per row, without a catalog lookup, is a
semantic function rather than a workflow:

```sql
CREATE SEMANTIC FUNCTION route(ticket text) RETURNS text AS PIPE $$
  {{ticket}} |> CLASSIFY INTO 'biped', 'invoice'
$$;
```

`notes |> route` is that function. The planner inlines the body.

## Side by side

```sql
-- operator
SELECT count(*) FROM mail WHERE body MEANS 'a deadline was missed';
SELECT count(*) FROM mail WHERE body ABOUT 'a deadline was missed';

-- function call
SELECT rvbbit.means(body, 'a deadline was missed') FROM mail;
SELECT rvbbit.means_score(body, 'a deadline was missed') FROM mail;
SELECT rvbbit.classify(body, 'deadline,fyi,other') FROM mail;
SELECT rvbbit.extract(body, 'date') FROM mail;
SELECT rvbbit.semantic_similarity(body, 'a deadline was missed') FROM mail;

-- flow
CREATE WORKFLOW deadline (body text) RETURNS boolean AS $$
  {{body}} MEANS 'a deadline was missed'
$$;
SELECT rvbbit.flow('deadline', body) FROM mail;
```

`rvbbit.calls` and `EXPLAIN` use the same cost rows for the operator and
the function. A flow does not add a second meter; the statement it runs
does.
