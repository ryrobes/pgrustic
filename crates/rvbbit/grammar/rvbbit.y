/* rvbbit DDL grammar. Standalone: these tokens are NOT added to
   PostgreSQL gram.y, so none of the words become reserved in ordinary SQL.
   `bison -Wcounterexamples --expect=0` must report zero conflicts.
   The engine parses this language with crates/rvbbit/rvbbit_parse (the
   vendored gram.c is bison 2.3 and is not regenerated). */
%{
int yylex(void);
void yyerror(const char *s);
%}
%define parse.error verbose
%expect 0

%token CREATE ALTER DROP GRANT REVOKE COMMENT
%token OR REPLACE IF NOT EXISTS
%token BACKEND MODEL EMBEDDING SEMANTIC FUNCTION OPERATOR
%token DECISION TREE WORKFLOW
%token TYPE URL AUTH ENV SECRET WITH ON NAME COST INPUT OUTPUT DIMENSIONS
%token RETURNS SYSTEM PROMPT MAX TOKENS TEMPERATURE CACHE TEST EXPECT FOR
%token LEFTARG RIGHTARG SYMBOL NEGATOR
%token ASK YES NO CHOOSE WHEN THEN MEANING THRESHOLD USING
%token AS VERSION SET CASCADE RESTRICT
%token USAGE EXECUTE TO FROM IS NULLKW
%token IDENT SCONST ICONST FCONST

/* WHEN after THEN CHOOSE … binds to the inner CHOOSE (shift). */
%nonassoc LOW_WHEN
%nonassoc WHEN

%%

stmt:
    create
  | alter
  | drop
  | grant
  | revoke
  | comment
  ;

create:
    CREATE opt_or_replace opt_if_not_exists BACKEND IDENT TYPE IDENT URL SCONST opt_auth opt_with opt_version
  | CREATE opt_or_replace opt_if_not_exists MODEL IDENT ON BACKEND IDENT NAME SCONST opt_cost opt_version
  | CREATE opt_or_replace opt_if_not_exists EMBEDDING MODEL IDENT ON BACKEND IDENT NAME SCONST DIMENSIONS number opt_version
  | CREATE opt_or_replace SEMANTIC FUNCTION IDENT '(' arglist ')' RETURNS typename fn_clauses
  | CREATE opt_or_replace SEMANTIC OPERATOR IDENT '(' op_items ')' opt_version
  | CREATE opt_or_replace DECISION TREE IDENT '(' arglist ')' opt_model AS tree
  | CREATE opt_or_replace WORKFLOW IDENT opt_paren_blob opt_returns opt_version AS body
  ;

alter:
    ALTER BACKEND IDENT SET URL SCONST
  | ALTER BACKEND IDENT SET AUTH auth_spec
  | ALTER BACKEND IDENT SET '(' with_items ')'
  | ALTER MODEL IDENT ON BACKEND IDENT
  | ALTER MODEL IDENT SET NAME SCONST
  | ALTER SEMANTIC FUNCTION IDENT opt_arglist SET MODEL IDENT
  | ALTER SEMANTIC FUNCTION IDENT opt_arglist SET PROMPT SCONST
  | ALTER SEMANTIC FUNCTION IDENT opt_arglist SET SYSTEM SCONST
  | ALTER SEMANTIC OPERATOR IDENT SET FUNCTION IDENT
  | ALTER DECISION TREE IDENT SET MODEL IDENT
  | ALTER WORKFLOW IDENT VERSION ICONST AS body
  ;

drop:
    DROP opt_if_exists object_head opt_version opt_drop_behavior
  ;

grant:
    GRANT privilege ON object_head TO IDENT
  ;

revoke:
    REVOKE privilege ON object_head FROM IDENT
  ;

comment:
    COMMENT ON object_head IS comment_text
  ;

opt_or_replace: OR REPLACE | /* empty */ ;
opt_if_not_exists: IF NOT EXISTS | /* empty */ ;
opt_if_exists: IF EXISTS | /* empty */ ;
opt_drop_behavior: CASCADE | RESTRICT | /* empty */ ;
opt_version: VERSION ICONST | /* empty */ ;
opt_model: MODEL IDENT | /* empty */ ;
opt_auth: AUTH auth_spec | /* empty */ ;
auth_spec: ENV SCONST | ENV IDENT | SECRET SCONST | SECRET IDENT ;
opt_with: WITH '(' with_items ')' | /* empty */ ;
opt_cost: COST '(' cost_items ')' | /* empty */ ;
opt_arglist: '(' arglist ')' | /* empty */ ;
opt_paren_blob: '(' blob ')' | /* empty */ ;
opt_returns: RETURNS blob_returns | /* empty */ ;
privilege: USAGE | EXECUTE ;
comment_text: SCONST | NULLKW ;
body: SCONST ;

object_head:
    BACKEND IDENT
  | MODEL IDENT
  | SEMANTIC FUNCTION IDENT opt_arglist
  | SEMANTIC OPERATOR IDENT
  | DECISION TREE IDENT
  | WORKFLOW IDENT
  ;

arglist: /* empty */ | arg | arglist ',' arg ;
arg: IDENT typename ;

with_items: with_item | with_items ',' with_item ;
with_item: IDENT '=' timeout ;
timeout: SCONST | ICONST ;

cost_items: cost_item | cost_items ',' cost_item | cost_items IDENT ;
cost_item: INPUT '=' number | OUTPUT '=' number ;
number: ICONST | FCONST ;

op_items: op_item | op_items ',' op_item ;
op_item:
    LEFTARG '=' typename
  | RIGHTARG '=' typename
  | FUNCTION '=' IDENT
  | SYMBOL '=' SCONST
  | NEGATOR '=' negator
  ;
negator: IDENT | IDENT IDENT ;

fn_clauses: /* empty */ | fn_clauses fn_clause ;
fn_clause:
    MODEL IDENT
  | SYSTEM SCONST
  | PROMPT SCONST
  | MAX TOKENS ICONST
  | TEMPERATURE number
  | CACHE IDENT
  | TEST IDENT EXPECT IDENT FOR '(' blob ')'
  | VERSION ICONST
  ;

tree:
    ASK SCONST opt_threshold opt_using YES opt_colon tree NO opt_colon tree
  | CHOOSE SCONST when_list %prec LOW_WHEN
  | SCONST
  ;
opt_threshold: THRESHOLD number | /* empty */ ;
opt_using: USING '(' name_list ')' | /* empty */ ;
opt_colon: ':' | /* empty */ ;
when_list: when | when_list when ;
when: WHEN SCONST opt_meaning THEN tree ;
opt_meaning: MEANING SCONST | /* empty */ ;
name_list: IDENT | name_list ',' IDENT ;

/* A type is one or more identifiers, optionally with a parenthesized modifier.
   Stop words of the surrounding production are not included here; each
   production lists the type where a following keyword is a distinct token
   so LALR can see the boundary (RETURNS typename MODEL, LEFTARG = typename ','). */
typename: IDENT | IDENT IDENT | IDENT IDENT IDENT | IDENT '(' blob ')' ;

/* blob consumes a parenthesized list without caring what is inside. */
blob: /* empty */ | blob blob_atom ;
blob_atom: IDENT | SCONST | ICONST | FCONST | '(' blob ')' | ',' | '=' ;

/* RETURNS text runs until VERSION or AS, which are not blob_returns atoms. */
blob_returns: /* empty */ | blob_returns blob_atom ;

%%
void yyerror(const char *s) { (void)s; }
int yylex(void) { return 0; }
